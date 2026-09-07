//! `claim_collateral` and `claim_yield`.
//!
//! These tests write the claimable balances into storage directly, which separates payout and row
//! pruning from the engines that produce the gains. Other modules cover claims earned through the
//! live flows.

use crate::{mock::*, Error};

#[test]
fn claims_pay_out_clear_and_default_to_the_caller() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();
		seed_claimables(1, 70, 55);

		// Collateral goes to the caller here, yield to a named recipient.
		assert_claim_collateral(1, 70);
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
		assert_ok!(claim_yield(1, DOT, PUSD, 9));
		assert_eq!(stable_balance(PUSD, 9), 55);
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

		// Both claimables are cleared; the active deposit keeps the row alive.
		let row = deposit_row(DOT, PUSD, 1).expect("row survives with active deposit");
		assert_eq!(row.claimable_collateral, 0);
		assert_eq!(row.claimable_yield, 0);
		assert_eq!(row.active_deposit, 400);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_collateral_gains_unclaimed, 0);
		assert_eq!(state.total_yield_unclaimed, 0);

		// Nothing left to claim.
		assert_noop!(claim_collateral(1, DOT, PUSD, 1), Error::<Test>::NoClaimableCollateral);
		assert_noop!(claim_yield(1, DOT, PUSD, 9), Error::<Test>::NoClaimableYield);

		// A `None` recipient pays the caller on both claim sides.
		seed_claimables(1, 70, 55);
		let coll_before = collateral_balance(DOT, 1);
		assert_ok!(Stability::claim_collateral(RuntimeOrigin::signed(1), DOT, PUSD, None));
		assert_eq!(collateral_balance(DOT, 1) - coll_before, 70);
		let stable_before = stable_balance(PUSD, 1);
		assert_ok!(Stability::claim_yield(RuntimeOrigin::signed(1), DOT, PUSD, None));
		assert_eq!(stable_balance(PUSD, 1) - stable_before, 55);
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
