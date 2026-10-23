//! What the pallet does when something goes wrong: a caller acting on a stale reading, storage
//! that disagrees with itself, and value that fails to move after the arithmetic is already
//! done.

use crate::mock::*;
use frame::traits::{
	fungibles::Balanced as FungiblesBalanced,
	tokens::{Fortitude, Precision, Preservation},
};
use stablecoin_primitives::{OffsetLegs, StabilityPoolInspect};

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
	ExtBuilder::default().branch(DOT, USDX).build_and_execute(|| {
		mint_stable(USDX, 1, USDX_MIN_BALANCE);
		assert_ok!(deposit_and_mature(1, DOT, USDX, USDX_MIN_BALANCE));
		let pool = Stability::pool_account(&DOT, &USDX);
		let sums_before = crate::PoolSumsStore::<Test>::get((DOT, USDX, Leg::Active, 0u32, 0u32));

		// USDX has a 10_000-unit minimum balance. Emptying the pool asset account, deposit and
		// custody float alike, makes a sub-minimum yield credit unresolvable, so the infallible
		// hook must hand the credit back untouched. The corruption rolls back before the
		// post-test invariant check runs.
		hypothetically!({
			burn_stable(USDX, pool, 2 * USDX_MIN_BALANCE);
			assert_eq!(stable_balance(USDX, pool), 0);

			let leftover = distribute_yield_leftover(DOT, USDX, USDX_MIN_BALANCE - 1);

			assert_eq!(leftover, USDX_MIN_BALANCE - 1);
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
		});
	});
}

#[test]
fn stable_shortfall_quotes_nothing_and_refuses_settlement() {
	ExtBuilder::default().branch(TOKEN_X, STABLE).build_and_execute(|| {
		mint_stable(STABLE, 1, 400);
		assert_ok!(deposit_and_mature(1, TOKEN_X, STABLE, 400));
		mint_stable(STABLE, 2, 200);
		assert_ok!(deposit(2, TOKEN_X, STABLE, 200));

		// Both legs hold capital and quote it while custody backs the accounting.
		let quote = market_quote(&TOKEN_X, &STABLE);
		assert_eq!(Stability::quote_active(&quote, 200), 200);
		assert_eq!(Stability::quote_pending(&quote, 100, 0), 100);

		// Break the stable-balance identity: the accounting still tracks 400 active and 200
		// pending, but the account that would pay for a burn holds nothing.
		let pool = Stability::pool_account(&TOKEN_X, &STABLE);
		hypothetically!({
			burn_stable(STABLE, pool, 600);

			// Sizing reads custody, so neither leg quotes anything, and a caller that settles
			// past its quote is refused.
			let quote = market_quote(&TOKEN_X, &STABLE);
			assert_eq!(Stability::quote_active(&quote, 200), 0);
			assert_eq!(Stability::quote_pending(&quote, 100, 0), 0);
			assert_err!(
				hypothetically!(offset_with(
					(TOKEN_X, STABLE),
					branch_snapshot(&TOKEN_X, &STABLE),
					OffsetLegs { active: 200, pending: 0 },
					OffsetLegs { active: (TOKEN_X, 100), pending: (TOKEN_X, 0) }
				)),
				crate::Error::<Test>::OffsetSettlementFailed,
			);
			assert_storage_noop!(assert_eq!(
				simulate_offset(TOKEN_X, STABLE, 200, 100),
				SimulatedOffset { active: 0, pending: 0, leftover: 100 }
			));
		});
	});
}

#[test]
fn safety_withdraw_after_offset_cannot_overdraw_stale_request() {
	ExtBuilder::branch_with_debt().build_and_execute(|| {
		enter_safety_mode();
		assert_ok!(request_withdraw(1, DOT, STABLE, 400));
		// The request still says 400, but a liquidation offset shrinks the
		// live active deposit to 100 before the request matures. At the 0.6
		// Safety price, the 300 debt seizes 300 / 0.6 = 500 collateral.
		assert_eq!(simulate_offset(DOT, STABLE, 300, 500).debt(), 300);
		advance_time(600_000);

		assert_ok!(withdraw(1, DOT, STABLE, 400, 1));

		System::assert_has_event(market_event!(WithdrawalExecuted {
			depositor: 1,
			recipient: 1,
			amount: 100,
		}));
		assert_eq!(stable_balance(STABLE, 1), 700);

		let row = deposit_row(DOT, STABLE, 1).expect("claimable keeps row alive");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.claimable_collateral, 500);
		assert_eq!(row.withdrawal_request.expect("request remainder stays bounded").amount, 300);

		assert_claim_collateral(1, 500);
		assert!(deposit_row(DOT, STABLE, 1).is_none());
	});
}
