//! `distribute_yield` and `compound_yield`: the `G` accumulator end to end.
//!
//! Yield is earned through the accumulators here rather than written into storage, so these are
//! the first tests in which realization does real work.

use crate::{mock::*, Error};
use stablecoin_primitives::OnBranchYield;

#[test]
fn yield_without_a_target_returns_the_credit_whole() {
	ExtBuilder::default().default_branch().build_and_execute(|| {
		// The returned credit is the caller's to route: in the runtime the
		// vault engine's `OnBranchYield` plumbing forwards it to the fee
		// destination (`vault_interest_flows_to_pool_through_the_hook`
		// exercises that split live). Here the test drops it, rescinding the
		// issuance.
		// A registered pool with nothing active has no denominator to share over, so nothing
		// is written.
		assert_yield_declined(DOT, STABLE, 100);

		// The credit's asset names the branch: a USDX credit targets the
		// unregistered (DOT, USDX) pair and comes back whole, while the
		// funded STABLE pool never sees it.
		seed_matured_deposit(1, 400);
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
fn distribute_updates_g_exactly() {
	ExtBuilder::default()
		.default_branch()
		.matured_deposit(1, 1_000)
		.build_and_execute(|| {
			let leftover = distribute_yield_leftover(DOT, STABLE, 100);

			assert_eq!(leftover, 0);
			// delta_G = floor(100 * P / A) = floor(100 * 1e18 / 1000) = 1e17.
			let sums = active_sums(0, 0);
			assert_eq!(sums.g_yield, FixedU128::from_rational(1, 10));

			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_yield_unclaimed, 100);
			// Pool holds the 1000 active plus the 100 undistributed yield.
			assert_eq!(pool_stable_held(DOT, STABLE), 1_100);

			assert_eq!(
				stability_events_since_last_call(),
				vec![
					crate::Event::CohortActivated {
						collateral_id: DOT,
						stable_id: STABLE,
						cohort: CohortId(0),
						deadline: 10_000,
						amount: 1_000,
					},
					crate::Event::YieldDistributed {
						collateral_id: DOT,
						stable_id: STABLE,
						amount: 100
					},
				]
			);
		});
}

#[test]
fn flooring_dust_accumulates_until_teardown() {
	ExtBuilder::default()
		.default_branch()
		.matured_deposit(1, 600)
		.matured_deposit(2, 2_400)
		.build_and_execute(|| {
			distribute_all_yield(100);

			// delta_G = floor(100 * 1e18 / 3000) = 33_333_333_333_333_333.
			// Gains: floor(600 * inner / 1e18) = 19 (exactly 19.999...98),
			//        floor(2400 * inner / 1e18) = 79 (exactly 79.999...92).
			assert_claim_yield(1, 19);
			assert_claim_yield(2, 79);
			// 100 - 19 - 79 = 2 units of flooring dust stay pool-owned, keeping
			// the balance identity exact.
			assert_eq!(pool_state(DOT, STABLE).total_yield_unclaimed, 2);

			// Live-branch interactions do not redistribute the residue: no flow
			// reads it back, so later distributions add their own remainder.
			distribute_all_yield(100);
			assert_ok!(claim_yield(1, DOT, STABLE, 1));
			assert_ok!(claim_yield(2, DOT, STABLE, 2));

			assert_eq!(pool_state(DOT, STABLE).total_yield_unclaimed, 4);
			// It backs the pool-balance identity until all depositor rows are gone
			// and branch teardown sweeps it through `StableDustHandler`. The node
			// runtime routes that terminal residue to Treasury; governance.rs pins
			// the sweep and clean re-registration contract.
		});
}

#[test]
fn compound_moves_yield_into_active_and_it_earns() {
	ExtBuilder::default()
		.default_branch()
		.matured_deposit(1, 600)
		.build_and_execute(|| {
			distribute_all_yield(60);

			// Asking for more than the 60 claimable compounds exactly the 60.
			assert_ok!(compound(1, DOT, STABLE, 1_000));

			let row = deposit_row(DOT, STABLE, 1).expect("row exists");
			assert_eq!(row.active_deposit, 660);
			assert_eq!(row.claimable_yield, 0);
			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_active_deposits, 660);
			assert_eq!(state.total_yield_unclaimed, 0);
			// Pure accounting: the pool balance did not move.
			assert_eq!(pool_stable_held(DOT, STABLE), 660);
			System::assert_last_event(market_event!(YieldCompounded { depositor: 1, amount: 60 }));

			// The compounded amount earns from here on: G moves by 66/660 = 0.1
			// and the whole 660 realizes floor(660 * 0.1) = 66.
			distribute_all_yield(66);
			assert_claim_yield(1, 66);
		});
}

#[test]
fn compound_with_nothing_claimable_reverts() {
	ExtBuilder::default()
		.default_branch()
		.matured_deposit(1, 600)
		.build_and_execute(|| {
			assert_noop!(compound(1, DOT, STABLE, 60), Error::<Test>::NoYieldToCompound);

			distribute_all_yield(60);

			// A zero request is equally nothing to compound.
			assert_noop!(compound(1, DOT, STABLE, 0), Error::<Test>::NoYieldToCompound);
		});
}

#[test]
fn vault_interest_flows_to_pool_through_the_hook() {
	ExtBuilder::default()
		.default_branch()
		.matured_deposit(1, 400)
		.build_and_execute(|| {
			// A vault accruing 5%/yr on exactly 500 debt (499 borrowed + 1
			// upfront fee; the fee's 75% share floors to zero, so the whole
			// unit routes to the fee destination).
			mint_collateral(DOT, 5, 2_000);
			assert_ok!(open_vault(5, DOT, STABLE, 1_000, 499));
			assert_eq!(vault_debt(DOT, STABLE, 5), 500);
			assert_eq!(stable_balance(STABLE, FEE_DEST), 1);

			// One exact year of interest, minted on the next branch touch
			// (opening a second vault, whose own upfront fee is another unit
			// for the fee destination). Interest accrues on the 499 principal
			// at the floor-derived weighted rate — weighted sum
			// floor(499 * 0.05) = 24 — so exactly 24 mints.
			advance_time(stablecoin_primitives::MILLIS_PER_YEAR);
			mint_collateral(DOT, 6, 2_000);
			assert_ok!(open_vault(6, DOT, STABLE, 1_000, 499));

			// The hook split the 24: floor(75% * 24) = 18 to the pool; the
			// 6 remainder plus the two upfront fees (1 + 1) go to the fee
			// destination.
			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_yield_unclaimed, 18);
			let sums = active_sums(0, 0);
			// delta_G = floor(18 * 1e18 / 400) = 4.5e16.
			assert_eq!(sums.g_yield, FixedU128::from_rational(45, 1_000));
			assert_eq!(pool_stable_held(DOT, STABLE), 418);
			assert_eq!(stable_balance(STABLE, FEE_DEST), 8);
			System::assert_has_event(
				crate::Event::YieldDistributed {
					collateral_id: DOT,
					stable_id: STABLE,
					amount: 18,
				}
				.into(),
			);

			// The depositor realizes the full pool share: floor(400 * 0.045).
			assert_claim_yield(1, 18);
		});
}

/// Every call that changes the capital sharing the yield issues the branch's pending interest
/// first, over the capital active before the call: one year on the 499 vault is 24 units, 18 of
/// which the pool spreads over the 400 already active, whatever the call then does. Dispatches
/// `call` after that year and asserts the distribution it triggered.
fn assert_issues_pending_branch_interest_first(call: impl FnOnce() -> DispatchResult) {
	ExtBuilder::default()
		.default_branch()
		.matured_deposit(1, 400)
		.stable_balance(2, 10_000)
		.build_and_execute(|| {
			// The figures below assume the vault opens after the deposit matures, which the
			// builder's vaults-first order cannot express. Its events are cleared as setup events
			// are, so the assertions below see only what `call` emits.
			mint_collateral(DOT, 5, 1_000);
			assert_ok!(open_vault(5, DOT, STABLE, 1_000, 499));
			System::reset_events();
			advance_time(stablecoin_primitives::MILLIS_PER_YEAR);

			assert_ok!(call());

			System::assert_has_event(
				crate::Event::YieldDistributed {
					collateral_id: DOT,
					stable_id: STABLE,
					amount: 18,
				}
				.into(),
			);
			// delta_G = floor(18 * 1e18 / 400): spread over the 400 active before the call.
			assert_eq!(active_sums(0, 0).g_yield, FixedU128::from_rational(45, 1_000));
		});
}

#[test]
fn deposit_issues_pending_branch_interest_first() {
	assert_issues_pending_branch_interest_first(|| deposit(2, DOT, STABLE, 400));
}

#[test]
fn withdraw_issues_pending_branch_interest_first() {
	assert_issues_pending_branch_interest_first(|| withdraw(1, DOT, STABLE, 400, 1));
}

#[test]
fn request_withdraw_issues_pending_branch_interest_first() {
	assert_issues_pending_branch_interest_first(|| request_withdraw(1, DOT, STABLE, 400));
}

#[test]
fn claim_yield_issues_pending_branch_interest_first() {
	assert_issues_pending_branch_interest_first(|| claim_yield(1, DOT, STABLE, 1));
}

#[test]
fn compound_issues_pending_branch_interest_first() {
	assert_issues_pending_branch_interest_first(|| compound(1, DOT, STABLE, 100));
}
