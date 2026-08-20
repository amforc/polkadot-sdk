//! `claim_collateral`, `claim_yield` and `settle_deposit`.
//!
//! These tests write the claimable balances into storage directly, which separates payout and row
//! pruning from the engines that produce the gains. Other modules cover claims earned through the
//! live flows.

use crate::{mock::*, Error};

#[test]
fn claim_collateral_pays_out_and_clears() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();
		seed_claimables(1, 70, 0);

		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));

		assert_eq!(collateral_balance(DOT, 1), before + 70);
		let row = deposit_row(DOT, PUSD, 1).expect("row survives with active deposit");
		assert_eq!(row.claimable_collateral, 0);
		assert_eq!(row.active_deposit, 400);
		assert_eq!(pool_state(DOT, PUSD).total_collateral_gains_unclaimed, 0);

		System::assert_last_event(
			crate::Event::CollateralClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 70,
			}
			.into(),
		);

		// Nothing left to claim.
		assert_noop!(claim_collateral(1, DOT, PUSD, 1), Error::<Test>::NoClaimableCollateral);
	});
}

#[test]
fn claim_yield_pays_out_to_recipient_and_clears() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();
		seed_claimables(1, 0, 55);

		assert_ok!(claim_yield(1, DOT, PUSD, 9));

		assert_eq!(stable_balance(PUSD, 9), 55);
		let row = deposit_row(DOT, PUSD, 1).expect("row survives with active deposit");
		assert_eq!(row.claimable_yield, 0);
		assert_eq!(pool_state(DOT, PUSD).total_yield_unclaimed, 0);

		System::assert_last_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 9,
				amount: 55,
			}
			.into(),
		);

		assert_noop!(claim_yield(1, DOT, PUSD, 9), Error::<Test>::NoClaimableYield);
	});
}

#[test]
fn claims_leave_active_and_pending_untouched() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();
		// A fresh immature pending amount on top of the 400 active.
		assert_ok!(deposit(1, DOT, PUSD, 300));
		seed_claimables(1, 70, 55);

		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_ok!(claim_yield(1, DOT, PUSD, 1));

		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 400);
		assert_eq!(row.pending_deposit.expect("still queued").amount, 300);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 400);
		assert_eq!(state.total_pending_deposits, 300);
	});
}

#[test]
fn final_claim_prunes_an_otherwise_empty_row() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();
		seed_claimables(1, 70, 55);
		// Drain the active deposit; only the two claimables keep the row.
		assert_ok!(withdraw(1, DOT, PUSD, 400, 1));
		assert!(deposit_row(DOT, PUSD, 1).is_some());

		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		// Yield is still claimable, so the row must survive the first claim.
		assert!(deposit_row(DOT, PUSD, 1).is_some());

		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		assert!(deposit_row(DOT, PUSD, 1).is_none());
	});
}

#[test]
fn claims_without_row_or_branch_revert() {
	build_and_execute(|| {
		assert_noop!(claim_collateral(1, DOT, PUSD, 1), Error::<Test>::PoolNotRegistered);
		assert_noop!(claim_yield(1, DOT, PUSD, 1), Error::<Test>::PoolNotRegistered);

		register_branch(DOT, PUSD, default_branch_config());
		assert_noop!(claim_collateral(1, DOT, PUSD, 1), Error::<Test>::DepositNotFound);
		assert_noop!(claim_yield(1, DOT, PUSD, 1), Error::<Test>::DepositNotFound);
	});
}

#[test]
fn claim_activates_matured_pending() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();
		assert_ok!(deposit(1, DOT, PUSD, 300));
		seed_claimables(1, 70, 0);

		// The claim is a caller-initiated operation, so it performs the
		// standard housekeeping: the matured 300 becomes active.
		advance_time(9_000);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));

		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 700);
		assert!(row.pending_deposit.is_none());
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 700);
		assert_eq!(state.total_pending_deposits, 0);
	});
}

#[test]
fn settle_activates_matured_pending() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, DOT, PUSD, 400));

		// Nothing folds a matured pending leg in on its own: the row needs a
		// touch. Long past maturity a third party supplies one, and settlement
		// completes the move.
		advance_time(49_000);
		assert_ok!(settle(7, 1, DOT, PUSD));

		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 400);
		assert!(row.pending_deposit.is_none());
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 400);
	});
}

#[test]
fn claim_recipient_defaults_to_caller() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();
		seed_claimables(1, 70, 55);

		// A `None` recipient pays the caller on both claim sides.
		let coll_before = collateral_balance(DOT, 1);
		assert_ok!(Stability::claim_collateral(RuntimeOrigin::signed(1), DOT, PUSD, None));
		assert_eq!(collateral_balance(DOT, 1) - coll_before, 70);

		let stable_before = stable_balance(PUSD, 1);
		assert_ok!(Stability::claim_yield(RuntimeOrigin::signed(1), DOT, PUSD, None));
		assert_eq!(stable_balance(PUSD, 1) - stable_before, 55);
	});
}

#[test]
fn settle_without_row_or_branch_reverts() {
	build_and_execute(|| {
		assert_noop!(settle(7, 1, DOT, PUSD), Error::<Test>::PoolNotRegistered);
		register_branch(DOT, PUSD, default_branch_config());
		assert_noop!(settle(7, 1, DOT, PUSD), Error::<Test>::DepositNotFound);
	});
}
