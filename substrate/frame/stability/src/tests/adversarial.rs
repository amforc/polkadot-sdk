//! What the pallet does when something goes wrong: a caller acting on a stale reading, storage
//! that disagrees with itself, and value that fails to move after the arithmetic is already
//! done.

use crate::mock::*;
use frame::traits::{
	fungibles::Balanced as FungiblesBalanced,
	tokens::{Fortitude, Precision, Preservation},
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
		assert_ok!(deposit_and_mature(1, DOT, USDX, USDX_MIN_BALANCE));

		let pool = Stability::pool_account(&DOT, &USDX);
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
		// The due cohort still advanced — that transition does not depend on the
		// distribution — while the distribution itself left no trace.
		let state = pool_state(DOT, USDX);
		assert_eq!(state.total_active_deposits, USDX_MIN_BALANCE);
		assert_eq!(state.total_pending_deposits, 0);
		assert!(state.open_cohorts.is_empty());
		assert_eq!(state.total_yield_unclaimed, 0);
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
	build_with_default_market(|| {
		mint_stable(PUSD, 1, 400);
		assert_ok!(deposit_and_mature(1, DOT, PUSD, 400));

		// The credit's asset names the market: a USDX credit targets the
		// unregistered (DOT, USDX) pair and comes back whole, while the
		// funded PUSD pool never sees it.
		let credit = issue_stable(USDX, 20_000);
		let returned = storage_noop(|| {
			Stability::distribute_yield(&DOT, branch_snapshot(&DOT, &USDX), credit)
		});
		assert_eq!(returned.asset(), USDX);
		assert_eq!(returned.peek(), 20_000);
		drop(returned);
	});
}

#[test]
fn offset_apis_reject_a_credit_for_another_collateral() {
	build_with_default_market(|| {
		mint_stable(PUSD, 1, 400);
		assert_ok!(deposit_and_mature(1, DOT, PUSD, 400));
		mint_stable(PUSD, 2, 200);
		assert_ok!(deposit(2, DOT, PUSD, 200));

		let quote = market_quote(&DOT, &PUSD);
		assert_eq!(Stability::quote_active(&quote, 200), 200);
		assert_err!(
			hypothetically!(Stability::offset(
				&DOT,
				&PUSD,
				branch_snapshot(&DOT, &PUSD),
				OffsetLegs { active: 200, pending: 0 },
				OffsetLegs {
					active: issue_collateral(TOKEN_X, 100),
					pending: issue_collateral(DOT, 0)
				},
			)),
			crate::Error::<Test>::OffsetSettlementFailed,
		);

		assert_eq!(Stability::quote_pending(&quote, 200, 0), 200);
		assert_err!(
			hypothetically!(Stability::offset(
				&DOT,
				&PUSD,
				branch_snapshot(&DOT, &PUSD),
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
fn stable_shortfall_quotes_nothing_and_try_state_reports_it() {
	build_and_execute(|| {
		register_branch(TOKEN_X, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 400);
		assert_ok!(deposit_and_mature(1, TOKEN_X, PUSD, 400));
		mint_stable(PUSD, 2, 200);
		assert_ok!(deposit(2, TOKEN_X, PUSD, 200));

		// Both legs hold capital and quote it while custody backs the accounting.
		let quote = market_quote(&TOKEN_X, &PUSD);
		assert_eq!(Stability::quote_active(&quote, 200), 200);
		assert_eq!(Stability::quote_pending(&quote, 100, 0), 100);

		// Break the stable-balance identity: the accounting still tracks 400 active and 200
		// pending, but the account that would pay for a burn holds nothing.
		let pool = Stability::pool_account(&TOKEN_X, &PUSD);
		burn_stable(PUSD, pool, 600);
		assert_eq!(
			crate::try_state::do_try_state::<Test>(),
			Err("pool stablecoin balance diverges from tracked totals".into())
		);

		// Sizing reads custody, so neither leg quotes anything, and a caller that settles past
		// its quote is refused.
		let quote = market_quote(&TOKEN_X, &PUSD);
		assert_eq!(Stability::quote_active(&quote, 200), 0);
		assert_eq!(Stability::quote_pending(&quote, 100, 0), 0);
		assert_err!(
			hypothetically!(Stability::offset(
				&TOKEN_X,
				&PUSD,
				branch_snapshot(&TOKEN_X, &PUSD),
				OffsetLegs { active: 200, pending: 0 },
				OffsetLegs {
					active: issue_collateral(TOKEN_X, 100),
					pending: issue_collateral(TOKEN_X, 0)
				},
			)),
			crate::Error::<Test>::OffsetSettlementFailed,
		);
		assert_storage_noop!(assert_eq!(
			simulate_offset(TOKEN_X, PUSD, 200, 100),
			SimulatedOffset { active: 0, pending: 0, leftover: 100 }
		));

		// Minting the burned 600 back restores the identity, so the invariant check that runs
		// when the test exits passes again.
		mint_stable(PUSD, pool, 600);
		assert_ok!(crate::try_state::do_try_state::<Test>());
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
		assert_eq!(simulate_offset(DOT, PUSD, 300, 500).debt(), 300);
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

		assert_claim_collateral(1, 500);
		assert!(deposit_row(DOT, PUSD, 1).is_none());
	});
}
