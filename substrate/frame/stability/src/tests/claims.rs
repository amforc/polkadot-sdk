//! `claim_collateral` and `claim_yield`.
//!
//! These tests write the claimable balances into storage directly, which separates payout from
//! the engines that produce the gains. Other modules cover claims earned through the live flows,
//! and the pruning of a row that its last claim empties.

use crate::{mock::*, Error};

#[test]
fn claims_pay_out_clear_and_default_to_the_caller() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();
		// A fresh immature tranche on top of the 400 active, which no claim may touch.
		assert_ok!(deposit(1, DOT, STABLE, 300));
		seed_claimables(1, 70, 55);

		// Collateral goes to the caller here, yield to a named recipient.
		assert_claim_collateral(1, 70);
		System::assert_last_event(market_event!(CollateralClaimed {
			depositor: 1,
			recipient: 1,
			amount: 70,
		}));
		assert_ok!(claim_yield(1, DOT, STABLE, 9));
		assert_eq!(stable_balance(STABLE, 9), 55);
		System::assert_last_event(market_event!(YieldClaimed {
			depositor: 1,
			recipient: 9,
			amount: 55,
		}));

		// Both claimables are cleared; the deposited capital keeps the row alive, on both legs.
		let row = deposit_row(DOT, STABLE, 1).expect("row survives with active deposit");
		assert_eq!(row.claimable_collateral, 0);
		assert_eq!(row.claimable_yield, 0);
		assert_eq!(row.active_deposit, 400);
		assert_eq!(row.pending_deposit.expect("still queued").amount, 300);
		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_collateral_gains_unclaimed, 0);
		assert_eq!(state.total_yield_unclaimed, 0);
		assert_eq!(state.total_pending_deposits, 300);

		// Nothing left to claim.
		assert_noop!(claim_collateral(1, DOT, STABLE, 1), Error::<Test>::NoClaimableCollateral);
		assert_noop!(claim_yield(1, DOT, STABLE, 9), Error::<Test>::NoClaimableYield);

		// A `None` recipient pays the caller on both claim sides.
		seed_claimables(1, 70, 55);
		let coll_before = collateral_balance(DOT, 1);
		assert_ok!(Stability::claim_collateral(RuntimeOrigin::signed(1), DOT, STABLE, None));
		assert_eq!(collateral_balance(DOT, 1) - coll_before, 70);
		let stable_before = stable_balance(STABLE, 1);
		assert_ok!(Stability::claim_yield(RuntimeOrigin::signed(1), DOT, STABLE, None));
		assert_eq!(stable_balance(STABLE, 1) - stable_before, 55);
	});
}

// Regression: without the custody float, the last claims of a drained pool left only a
// sub-minimum remainder behind and `Preserve` refused them until a new liquidation topped the
// account up. The float is what every exact payout leaves behind instead.
#[test]
fn claims_leaving_a_sub_minimum_remainder_still_pay_out() {
	build_and_execute(|| {
		create_high_min_collateral();
		register_branch(HIGH_MIN, STABLE, branch_config_for(HIGH_MIN, STABLE));
		let pool = Stability::pool_account(&HIGH_MIN, &STABLE);
		for who in [1, 2] {
			mint_stable(STABLE, who, 100);
			assert_ok!(deposit(who, HIGH_MIN, STABLE, 100));
		}
		seed_claimables_in(HIGH_MIN, 1, 600, 0);
		seed_claimables_in(HIGH_MIN, 2, 400, 0);
		assert_eq!(collateral_balance(HIGH_MIN, pool), 2_000);
		// Both payouts are below the asset minimum, so the recipients need accounts first.
		for recipient in [8, 9] {
			mint_collateral(HIGH_MIN, recipient, 1_000);
		}

		// 400 of tracked gains stay behind, below the 1_000 minimum; the float sits beneath them.
		assert_ok!(claim_collateral(1, HIGH_MIN, STABLE, 8));
		assert_eq!(collateral_balance(HIGH_MIN, 8), 1_600);
		assert_eq!(pool_collateral_held(HIGH_MIN, STABLE), 400);

		// The last claim empties the tracked gains and leaves exactly the float.
		assert_ok!(claim_collateral(2, HIGH_MIN, STABLE, 9));
		assert_eq!(collateral_balance(HIGH_MIN, 9), 1_400);
		assert_eq!(pool_collateral_held(HIGH_MIN, STABLE), 0);
		assert_eq!(collateral_balance(HIGH_MIN, pool), 1_000);
		assert_noop!(
			claim_collateral(2, HIGH_MIN, STABLE, 2),
			Error::<Test>::NoClaimableCollateral
		);
	});
}
