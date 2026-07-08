//! `deposit`: transfer, entry-delay queueing, merging, and FIFO ordering.

use crate::{mock::*, Error};

#[test]
fn deposit_moves_funds_and_queues_pending() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);

		assert_ok!(deposit(1, DOT, PUSD, 400));

		assert_eq!(stable_balance(PUSD, 1), 600);
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 400);

		let row = deposit_row(DOT, PUSD, 1).expect("row created");
		assert_eq!(row.active_deposit, 0);
		let pending = row.pending_deposit.expect("queued as pending");
		assert_eq!(pending.amount, 400);
		// Deposited at t = 1_000 with the 5_000 ms entry delay.
		assert_eq!(pending.activatable_at, 6_000);
		assert_eq!(row.claimable_collateral, 0);
		assert_eq!(row.claimable_yield, 0);
		assert_eq!(row.snapshot_p, FixedU128::one());
		assert_eq!(row.snapshot_epoch, 0);
		assert_eq!(row.snapshot_scale, 0);
		assert!(row.withdrawal_request.is_none());

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_pending_deposits, 400);
		assert_eq!(state.total_active_deposits, 0);

		assert!(pending_contains(DOT, PUSD, 1));
		assert_eq!(pending_oldest(DOT, PUSD), Some(1));
		assert_eq!(pending_count(DOT, PUSD), 1);

		System::assert_last_event(
			crate::Event::DepositReceived {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				amount: 400,
				used_for_recovery: 0,
				pending_amount: 400,
			}
			.into(),
		);
	});
}

#[test]
fn deposit_below_minimum_reverts_at_minimum_succeeds() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);

		// The branch minimum is 100.
		assert_noop!(deposit(1, DOT, PUSD, 99), Error::<Test>::DepositTooSmall);
		assert_ok!(deposit(1, DOT, PUSD, 100));
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 100);
	});
}

#[test]
fn deposit_on_unregistered_branch_reverts() {
	build_and_execute(|| {
		mint_stable(PUSD, 1, 1_000);
		assert_noop!(deposit(1, DOT, PUSD, 400), Error::<Test>::BranchNotRegistered);
	});
}

#[test]
fn deposit_without_funds_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert!(deposit(2, DOT, PUSD, 400).is_err());
		// Nothing was queued or transferred.
		assert!(deposit_row(DOT, PUSD, 2).is_none());
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 0);
		assert_eq!(pending_count(DOT, PUSD), 0);
	});
}

#[test]
fn second_deposit_merges_and_resets_delay() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);

		assert_ok!(deposit(1, DOT, PUSD, 400));
		advance_time(2_000);
		assert_ok!(deposit(1, DOT, PUSD, 300));

		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		let pending = row.pending_deposit.expect("still pending");
		assert_eq!(pending.amount, 700);
		// The merge restarts the whole amount's delay: t = 3_000 + 5_000.
		assert_eq!(pending.activatable_at, 8_000);
		// One FIFO slot, not two.
		assert_eq!(pending_count(DOT, PUSD), 1);
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 700);

		System::assert_last_event(
			crate::Event::DepositReceived {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				amount: 300,
				used_for_recovery: 0,
				pending_amount: 300,
			}
			.into(),
		);
	});
}

#[test]
fn deposit_auto_activates_matured_pending() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);

		assert_ok!(deposit(1, DOT, PUSD, 400));
		// Matured at t = 6_000; the next deposit activates it first.
		advance_time(5_000);
		assert_ok!(deposit(1, DOT, PUSD, 300));

		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.active_deposit, 400);
		let pending = row.pending_deposit.expect("new amount queued");
		assert_eq!(pending.amount, 300);
		// Fresh delay for the new amount: t = 6_000 + 5_000.
		assert_eq!(pending.activatable_at, 11_000);

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 400);
		assert_eq!(state.total_pending_deposits, 300);
		// Removed on activation, re-appended for the new pending amount.
		assert_eq!(pending_count(DOT, PUSD), 1);
		assert!(pending_contains(DOT, PUSD, 1));

		System::assert_has_event(
			crate::Event::PendingDepositActivated {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				amount: 400,
			}
			.into(),
		);
	});
}

#[test]
fn fifo_orders_depositors_oldest_first() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		mint_stable(PUSD, 2, 1_000);

		assert_ok!(deposit(1, DOT, PUSD, 200));
		assert_ok!(deposit(2, DOT, PUSD, 300));
		assert_eq!(pending_oldest(DOT, PUSD), Some(1));
		assert_eq!(pending_count(DOT, PUSD), 2);

		// The first depositor leaves the queue; the second becomes oldest.
		advance_time(5_000);
		assert_ok!(activate(1, DOT, PUSD));
		assert_eq!(pending_oldest(DOT, PUSD), Some(2));
		assert_eq!(pending_count(DOT, PUSD), 1);
	});
}
