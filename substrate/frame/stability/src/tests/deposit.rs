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
		assert_eq!(row.snapshot.coords.p, FixedU128::one());
		assert_eq!(row.snapshot.coords.epoch, 0);
		assert_eq!(row.snapshot.coords.scale, 0);
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
		// User 2 was never minted any PUSD, so the asset account itself is
		// missing (a funded-but-short wallet errors `BalanceLow` instead —
		// see `deposit_in_the_wallet_dead_zone_fails_instead_of_dusting`).
		assert_noop!(deposit(2, DOT, PUSD, 400), pallet_assets::Error::<Test>::NoAccount);
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
		// The merge restarts the whole amount's delay: deposited at 1_000,
		// topped up 2_000 later, plus the fresh 5_000 delay.
		assert_eq!(pending.activatable_at, 1_000 + 2_000 + 5_000);
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

// FIFO order is load-bearing in exactly one place: the §7.2 liquidation
// backstop consumes pending deposits oldest-first (`pending_offsets.rs`).
// Worth an integration test once the liquidations pallet drives the full
// waterfall.
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

		// The first depositor's activation removes it from the queue; the
		// second (matured too, but not yet touched) becomes oldest.
		advance_time(5_000);
		assert_ok!(poke(1, 1, DOT, PUSD));
		assert_eq!(pending_oldest(DOT, PUSD), Some(2));
		assert_eq!(pending_count(DOT, PUSD), 1);
	});
}

#[test]
fn deposit_in_the_wallet_dead_zone_fails_instead_of_dusting() {
	build_and_execute(|| {
		register_branch(DOT, USDX, default_branch_config());
		mint_stable(USDX, 1, 50_000);

		// 45_000 would leave 5_000 < the 10_000 USDX minimum in the wallet.
		// The funding withdrawal runs under `Preserve` (not a full drain), so
		// the asset pallet itself rejects it instead of folding the 5_000
		// into the debit. Depositing the whole wallet is the legitimate full
		// expend.
		assert_noop!(deposit(1, DOT, USDX, 45_000), pallet_assets::Error::<Test>::BalanceLow);
		assert_ok!(deposit(1, DOT, USDX, 50_000));
		assert_eq!(stable_balance(USDX, 1), 0);

		let pool = Stability::pool_account(&DOT, &USDX);
		assert_eq!(stable_balance(USDX, pool), 50_000);
	});
}
