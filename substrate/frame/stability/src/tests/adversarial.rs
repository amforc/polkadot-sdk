//! Adversarial boundaries: stale user intent, corrupted FIFO/storage
//! disagreement, and failed value movement after planning.

use crate::{mock::*, types::Leg};
use frame::{
	testing_prelude::hypothetically,
	traits::{
		fungibles::Balanced as FungiblesBalanced,
		tokens::{Fortitude, Precision, Preservation},
	},
};
use pusd_primitives::{OffsetLegs, OnBranchYield, StabilityPoolInspect, StabilityPoolOffset};

fn burn_stable(stable: StableId, who: AccountId, amount: Balance) {
	let credit = <Assets as FungiblesBalanced<AccountId>>::withdraw(
		stable,
		&who,
		amount,
		Precision::Exact,
		Preservation::Expendable,
		Fortitude::Polite,
	)
	.expect("pool stable balance covers the forced burn");
	drop(credit);
}

#[test]
fn yield_distribution_returns_credit_when_pool_account_cannot_hold_it() {
	build_and_execute(|| {
		register_branch(DOT, USDX, branch_config_for(DOT, USDX));
		mint_stable(USDX, 1, USDX_MIN_BALANCE);
		assert_ok!(deposit(1, DOT, USDX, USDX_MIN_BALANCE));
		advance_time(5_000);
		assert_ok!(poke(1, 1, DOT, USDX));

		let pool = Stability::pool_account(&DOT, &USDX);
		let state_before = pool_state(DOT, USDX);
		let sums_before = crate::PoolSumsStore::<Test>::get((DOT, USDX, Leg::Active, 0u32, 0u32));

		// USDX has a 10_000-unit minimum balance. Emptying the pool asset
		// account makes a sub-minimum yield credit unresolvable, so the
		// infallible hook must hand the credit back untouched.
		burn_stable(USDX, pool, USDX_MIN_BALANCE);
		assert_eq!(stable_balance(USDX, pool), 0);

		let leftover = distribute_yield(DOT, USDX, USDX_MIN_BALANCE - 1);
		assert_eq!(leftover.peek(), USDX_MIN_BALANCE - 1);
		drop(leftover);
		assert_eq!(stable_balance(USDX, pool), 0);
		assert_eq!(pool_state(DOT, USDX), state_before);
		assert_eq!(
			crate::PoolSumsStore::<Test>::get((DOT, USDX, Leg::Active, 0u32, 0u32)),
			sums_before
		);

		// Restore the artificial corruption before the post-test try-state
		// identity check runs.
		mint_stable(USDX, pool, USDX_MIN_BALANCE);
	});
}

#[test]
fn yield_distribution_routes_by_the_credits_own_asset() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 400);
		assert_ok!(deposit(1, DOT, PUSD, 400));
		activate_all(&[1]);

		// The credit's asset names the market: a USDX credit targets the
		// unregistered (DOT, USDX) pair and comes back whole, while the
		// funded PUSD pool never sees it.
		let pool = Stability::pool_account(&DOT, &PUSD);
		let state_before = pool_state(DOT, PUSD);
		let credit = <Assets as FungiblesBalanced<AccountId>>::issue(USDX, 20_000);
		let returned = Stability::distribute_yield(&DOT, credit);

		assert_eq!(returned.asset(), USDX);
		assert_eq!(returned.peek(), 20_000);
		assert_eq!(stable_balance(USDX, pool), 0);
		assert_eq!(pool_state(DOT, PUSD), state_before);
		drop(returned);
	});
}

#[test]
fn offset_apis_reject_a_credit_for_another_collateral() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 400);
		assert_ok!(deposit(1, DOT, PUSD, 400));
		activate_all(&[1]);
		mint_stable(PUSD, 2, 200);
		assert_ok!(deposit(2, DOT, PUSD, 200));

		assert_eq!(Stability::reducible_active(&DOT, &PUSD, 200), 200);
		assert_noop!(
			hypothetically!(Stability::offset(
				&DOT,
				&PUSD,
				OffsetLegs { active: 200, pending: 0 },
				OffsetLegs {
					active: issue_collateral(TOKEN_X, 100),
					pending: issue_collateral(DOT, 0)
				},
			)),
			crate::Error::<Test>::OffsetSettlementFailed,
		);

		assert_eq!(Stability::reducible_pending(&DOT, &PUSD, 200, 0), 200);
		assert_noop!(
			hypothetically!(Stability::offset(
				&DOT,
				&PUSD,
				OffsetLegs { active: 0, pending: 200 },
				OffsetLegs {
					active: issue_collateral(DOT, 0),
					pending: issue_collateral(TOKEN_X, 100)
				},
			)),
			crate::Error::<Test>::OffsetSettlementFailed,
		);
	});
}

#[test]
fn stable_shortfall_steps_aside_without_consuming_collateral() {
	build_and_execute(|| {
		register_branch(TOKEN_X, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 400);
		assert_ok!(deposit(1, TOKEN_X, PUSD, 400));
		advance_time(5_000);
		assert_ok!(poke(1, 1, TOKEN_X, PUSD));

		let pool = Stability::pool_account(&TOKEN_X, &PUSD);
		// Break the stable-balance identity so offset sizing finds nothing
		// burnable and steps aside before any part of the collateral credit
		// is consumed.
		burn_stable(PUSD, pool, 400);

		assert_eq!(simulate_offset(TOKEN_X, PUSD, 200, 100), (0, 100));
		assert_eq!(collateral_balance(TOKEN_X, pool), 0);

		// Repair the deliberate corruption before the post-test invariant check.
		mint_stable(PUSD, pool, 400);
	});
}

#[test]
fn safety_withdraw_after_offset_cannot_overdraw_stale_request() {
	build_and_execute(|| {
		seed_branch_with_debt();
		enter_safety_mode();
		assert_ok!(request_withdraw(1, DOT, PUSD, 400));

		// The request still says 400, but a liquidation offset shrinks the
		// live active deposit to 100 before the request matures. At the 0.6
		// Safety price, the 300 debt seizes 300 / 0.6 = 500 collateral.
		assert_eq!(simulate_offset(DOT, PUSD, 300, 500).0, 300);
		advance_time(600_000);

		assert_ok!(withdraw(1, DOT, PUSD, 400, 1));
		System::assert_has_event(
			crate::Event::WithdrawalExecuted {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 100,
			}
			.into(),
		);
		assert_eq!(stable_balance(PUSD, 1), 700);

		let row = deposit_row(DOT, PUSD, 1).expect("claimable keeps row alive");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.claimable_collateral, 500);
		assert_eq!(row.withdrawal_request.expect("request remainder stays bounded").amount, 300);

		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 500);
		assert!(deposit_row(DOT, PUSD, 1).is_none());
	});
}
