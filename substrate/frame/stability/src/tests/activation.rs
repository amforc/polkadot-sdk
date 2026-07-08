//! `activate_deposit`: entry-delay maturity and the pending → active move.

use crate::{mock::*, Error};

#[test]
fn activate_before_maturity_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, DOT, PUSD, 400));

		// Deposited at t = 1_000, activatable at 6_000; t = 5_999 is one
		// millisecond short.
		advance_time(4_999);
		assert_noop!(activate(1, DOT, PUSD), Error::<Test>::PendingDepositNotMatured);
	});
}

#[test]
fn activate_at_exact_boundary_succeeds() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, DOT, PUSD, 400));

		advance_time(5_000);
		assert_ok!(activate(1, DOT, PUSD));

		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.active_deposit, 400);
		assert!(row.pending_deposit.is_none());
		// Activation joins at the current accumulators.
		assert_eq!(row.snapshot.p, FixedU128::one());
		assert_eq!(row.snapshot.epoch, 0);
		assert_eq!(row.snapshot.scale, 0);

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 400);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(pending_count(DOT, PUSD), 0);
		assert!(!pending_contains(DOT, PUSD, 1));

		// Activation moves no funds; the pool already held the stablecoin.
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 400);
		assert_eq!(stable_balance(PUSD, 1), 600);

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
fn activate_without_row_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_noop!(activate(1, DOT, PUSD), Error::<Test>::DepositNotFound);
	});
}

#[test]
fn activate_with_nothing_pending_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, DOT, PUSD, 400));
		advance_time(5_000);
		assert_ok!(activate(1, DOT, PUSD));

		// The row still exists (active 400) but nothing is pending.
		assert_noop!(activate(1, DOT, PUSD), Error::<Test>::NoPendingDeposit);
	});
}

#[test]
fn activate_on_unregistered_branch_reverts() {
	build_and_execute(|| {
		assert_noop!(activate(1, DOT, PUSD), Error::<Test>::BranchNotRegistered);
	});
}
