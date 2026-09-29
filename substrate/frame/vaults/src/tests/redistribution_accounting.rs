//! Tests for the redistribution / aggregate-interest accounting identities and
//! the low-level liquidation accounting.
//!
//! Conventions:
//! - Eligible vaults use snapshot-corrected stake. Only `FinalRecovery` vaults have zero stake.
//! - "Recipient rate" means the recipient vault's `annual_rate`, not the liquidated vault's rate.
//! - Stake calculations are checked by the `try_state` identities, which `build_and_execute` runs
//!   after every test.

use crate::{
	mock::*,
	tests::{liquidation_outcome, rate_pct, ONE_DAY_MS, ONE_YEAR_MS},
};
use pusd_primitives::CollateralRatio;

/// `floor(x * rate)` for the recipient-rate assertions.
fn accrual_rate(x: Balance, rate: FixedU128) -> Balance {
	rate.saturating_mul_int(x)
}

// Touch order must not change mixed-rate allocation or pending residue.
#[test]
fn later_touch_order_cannot_change_mixed_rate_liquidation_allocations() {
	let run = |first, second| {
		new_test_ext().execute_with(|| {
			register_market(DOT, PUSD);
			assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(10, 100)));
			assert_ok!(open(2, DOT, PUSD, 999, 500, rate_pct(90, 100)));
			assert_ok!(open(3, DOT, PUSD, 200, 200, rate_pct(5, 100)));
			let before_1 = vault(DOT, PUSD, 1).debt.principal;
			let before_2 = vault(DOT, PUSD, 2).debt.principal;
			let liquidated_debt = vault(DOT, PUSD, 3).debt.total();

			set_price(DOT, FixedU128::from_rational(1u128, 1u128));
			assert_ok!(liquidate(99, DOT, PUSD, 3, 0, 0));
			assert_eq!(vault(DOT, PUSD, 1).debt.principal, before_1);
			assert_eq!(vault(DOT, PUSD, 2).debt.principal, before_2);

			advance_time(ONE_YEAR_MS);
			assert_ok!(poke(99, DOT, PUSD, first));
			assert_ok!(poke(99, DOT, PUSD, second));
			let final_state = branch_state(DOT, PUSD).unwrap();
			let allocated_1 = vault(DOT, PUSD, 1).debt.principal - before_1;
			let allocated_2 = vault(DOT, PUSD, 2).debt.principal - before_2;
			// The nondividing stakes must leave one debt unit in the pending pool.
			assert_eq!(liquidated_debt, 202);
			assert_eq!(allocated_1, 101);
			assert_eq!(allocated_2, 100);
			assert_eq!(final_state.debt.pending_redistribution_principal, 1);
			crate::try_state::do_try_state::<Test>().expect("post-test invariants hold");
			(
				allocated_1,
				allocated_2,
				final_state.debt.accrual_rate,
				final_state.debt.outstanding(),
				final_state.debt.pending_redistribution_principal,
			)
		})
	};

	assert_eq!(run(1, 2), run(2, 1));
}

// Claims after a delay must leave the same vaults and pool in both touch orders.
#[test]
fn delayed_redistribution_residue_is_touch_order_independent() {
	let run = |first, second| {
		new_test_ext().execute_with(|| {
			register_market(DOT, PUSD);
			assert_ok!(open(1, DOT, PUSD, 1_001, 500, rate_pct(7, 100)));
			assert_ok!(open(2, DOT, PUSD, 997, 500, rate_pct(23, 100)));
			assert_ok!(open(3, DOT, PUSD, 200, 202, rate_pct(5, 100)));

			advance_time(1_000);
			set_price(DOT, FixedU128::from_rational(1u128, 1u128));
			assert_eq!(redistribute_for_test(DOT, PUSD, 3, 0).unwrap(), 204);

			assert_ok!(poke(9, DOT, PUSD, first));
			assert_ok!(poke(9, DOT, PUSD, second));
			crate::try_state::do_try_state::<Test>().expect("post-test invariants hold");
			(branch_state(DOT, PUSD).unwrap(), vault(DOT, PUSD, 1), vault(DOT, PUSD, 2))
		})
	};

	assert_eq!(run(1, 2), run(2, 1));
}

// Redistributed debt must accrue interest at the recipient's rate, not the liquidated vault's
// rate or the temporary accounting rate.
#[test]
fn aggregate_interest_post_redistribution_accrues_at_recipient_rates() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(20, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let coll_1 = held(DOT, 1);
		assert_ok!(redistribute_for_test(DOT, PUSD, 1, coll_1));

		let state_pre = branch_state(DOT, PUSD).unwrap();
		// Both the owned debt and the pending share use the recipient's rate.
		assert_eq!(state_pre.debt.pending_redistribution_principal, 501);
		let actual: Balance = state_pre.debt.accrual_rate.whole();
		assert_eq!(actual, 200);

		advance_time(ONE_YEAR_MS);
		assert_ok!(poke(99, DOT, PUSD, 2));

		// ceil(1_001 × 20%): the vault rounds the owned and the absorbed debt's interest up.
		let post_minted = branch_state(DOT, PUSD).unwrap().debt.minted_interest;
		assert_eq!(post_minted - state_pre.debt.minted_interest, 201);
	});
}

// A rate change first materializes the assigned share. Past interest keeps the old rate, and the
// new rate applies to all principal after the change.
#[test]
fn recipient_rate_change_after_liquidation_reprices_the_absorbed_share() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100))); // A - reprices
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(50, 100))); // B - holds its share
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(10, 100))); // C - liquidated
		let vault_a_pre = vault(DOT, PUSD, 1);

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let redistributed = redistribute_for_test(DOT, PUSD, 3, held(DOT, 3)).expect("liquidated");

		// Wait past the cooldown to isolate interest from the rate-change fee, and restore the
		// price so the rate change passes the ratio checks.
		advance_time(ONE_YEAR_MS);
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(change_rate(1, DOT, PUSD, rate_pct(30, 100)));

		let vault_a = vault(DOT, PUSD, 1);
		assert_eq!(vault_a.annual_rate, rate_pct(30, 100));
		assert_eq!(vault_a.debt.principal, 500 + 251);
		assert_eq!(vault_a.collateral, 1_000 + 500);
		// Interest before the change uses the old rate for both principal sources.
		assert_eq!(vault_a.debt.interest, vault_a_pre.debt.interest + 38);

		// An untouched recipient keeps its share pending at its own rate.
		let vault_b = vault(DOT, PUSD, 2);
		assert_eq!(vault_b.debt.principal, 500);
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.debt.pending_redistribution_principal, redistributed - 251);
		assert_eq!(state.pending_redistribution_collateral, 500);
		let actual: Balance = state.stakes.accrual_rate.whole();
		assert_eq!(
			actual,
			accrual_rate(1_000, rate_pct(30, 100)) + accrual_rate(1_000, rate_pct(50, 100))
		);

		// Interest after the change uses the new rate for all principal.
		advance_time(ONE_YEAR_MS);
		assert_ok!(poke(9, DOT, PUSD, 1));
		let vault_a_post = vault(DOT, PUSD, 1);
		assert_eq!(vault_a_post.debt.interest, vault_a.debt.interest + 225);
	});
}

// A follow-on `borrow` against a recipient first touches the vault to fold in its redistribution
// share, then prices the fee at the market's average rate, which still counts the share left
// pending. The quote and the dispatch must agree on both. The exact aggregates are checked by
// `try_state` when the test ends.
#[test]
fn borrow_after_redistribution_folds_the_share_and_charges_the_quoted_fee() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100))); // A — recipient + borrower
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(50, 100))); // B — recipient
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(10, 100))); // C — liquidated

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let coll_3 = held(DOT, 3);
		// C owes 500 plus an upfront fee of 3, priced at the 21.67% average of the three opens.
		assert_eq!(redistribute_for_test(DOT, PUSD, 3, coll_3), Ok(503));
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));

		let interest_before = vault(DOT, PUSD, 1).debt.interest;
		// A holds half the stake, so the touch folds floor(503 / 2) = 251 into its principal and
		// leaves 252 pending. After the borrow the market accrues, per year,
		//   A: 2_751 · 5% = 137.55, B: 500 · 50% = 250, B's pending share: 251.5 · 50% = 125.75,
		// which is 513 in whole units over 2_751 + 500 + 252 = 3_503 of principal: 14.64%. The
		// fee is ceil(2_000 · 14.64% · 7d / 365.25d) = ceil(5.61) = 6. A's own 5% would give 2.
		let predicted_fee =
			crate::Pallet::<Test>::predict_borrow_upfront_fee(DOT, PUSD, 1, 2_000, None)
				.expect("touch projection and fee calculation succeed");
		assert_eq!(predicted_fee, 6);
		assert_ok!(borrow(1, DOT, PUSD, 2_000, None));
		let vault = vault(DOT, PUSD, 1);
		assert_eq!(vault.debt.principal, 500 + 251 + 2_000);
		assert_eq!(vault.debt.interest, interest_before + 6);
	});
}

// A liquidation below accumulator resolution must remain an explicit branch liability.
#[test]
fn sub_resolution_liquidation_remains_explicitly_pending() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		mint_collateral(DOT, 1, 2_000_000_000_000_000_000_000_000);
		mint_collateral(DOT, 2, 3_000_000_000_000_000_000_000_000);
		// Large collateral creates a stake total that exceeds accumulator resolution.
		assert_ok!(open(
			1,
			DOT,
			PUSD,
			1_000_000_000_000_000_000_000_000,
			1_000_000,
			rate_pct(5, 100)
		));
		assert_ok!(open(
			2,
			DOT,
			PUSD,
			2_000_000_000_000_000_000_000_000,
			1_000_000,
			rate_pct(5, 100)
		));
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let debt_3 = vault(DOT, PUSD, 3).debt.total();
		let coll_3 = held(DOT, 3);
		let p1_before = vault(DOT, PUSD, 1).debt.principal;
		let p2_before = vault(DOT, PUSD, 2).debt.principal;
		assert_ok!(redistribute_for_test(DOT, PUSD, 3, coll_3));
		assert_ok!(poke(9, DOT, PUSD, 1));
		assert_ok!(poke(9, DOT, PUSD, 2));

		assert!(debt_3 < 3_000_000, "the event must sit below the index resolution");
		let p1_after = vault(DOT, PUSD, 1).debt.principal;
		let p2_after = vault(DOT, PUSD, 2).debt.principal;
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(
			(p1_after - p1_before) +
				(p2_after - p2_before) +
				state.debt.pending_redistribution_principal,
			debt_3,
		);
		assert_eq!(
			held(DOT, crate::Pallet::<Test>::redistribution_account(&DOT, &PUSD)),
			state.pending_redistribution_collateral,
		);

		// Stake consolidation must give the remaining bearer the exact residue.
		mint_stable(PUSD, 2, 10_000);
		assert_ok!(repay(2, DOT, PUSD, 2, None));
		assert_ok!(close_vault(2, DOT, PUSD, None));
		assert_ok!(poke(9, DOT, PUSD, 1));
		let drained = branch_state(DOT, PUSD).unwrap();
		assert_eq!(drained.debt.pending_redistribution_principal, 0);
		assert_eq!(drained.pending_redistribution_collateral, 0);
		assert_eq!(held(DOT, crate::Pallet::<Test>::redistribution_account(&DOT, &PUSD)), 0,);
	});
}

// Pending redistribution belongs to the branch, not to its recipients at liquidation time. It
// survives recipient changes and moves to a later sole recipient without loss.
#[test]
fn pending_residue_outlives_its_recipients_and_lands_on_a_later_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		mint_collateral(DOT, 1, 2_000_000_000_000_000_000_000_000);
		mint_collateral(DOT, 2, 3_000_000_000_000_000_000_000_000);
		// Large stakes keep the allocation below accumulator resolution.
		assert_ok!(open(
			1,
			DOT,
			PUSD,
			1_000_000_000_000_000_000_000_000,
			1_000_000,
			rate_pct(5, 100)
		));
		assert_ok!(open(
			2,
			DOT,
			PUSD,
			2_000_000_000_000_000_000_000_000,
			1_000_000,
			rate_pct(5, 100)
		));
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(redistribute_for_test(DOT, PUSD, 3, held(DOT, 3)));
		assert_ok!(poke(9, DOT, PUSD, 1));
		assert_ok!(poke(9, DOT, PUSD, 2));
		let seeded = branch_state(DOT, PUSD).unwrap();
		let residue = seeded.debt.pending_redistribution_principal;
		let residue_collateral = seeded.pending_redistribution_collateral;
		// Neither recipient can claim an amount at this accumulator resolution.
		assert_eq!(residue, 501);
		assert_eq!(residue_collateral, 1_000);

		// The new recipient must exist before the last old one closes: a close touches the vault,
		// and a sole stake bearer would absorb the residue itself instead of leaving it pending.
		mint_stable(PUSD, 2, 10_000_000);
		assert_ok!(repay(2, DOT, PUSD, 2, None));
		assert_ok!(close_vault(2, DOT, PUSD, None));
		assert_ok!(open(4, DOT, PUSD, 40_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 1, 10_000_000);
		assert_ok!(repay(1, DOT, PUSD, 1, None));
		assert_ok!(close_vault(1, DOT, PUSD, None));

		let stranded = branch_state(DOT, PUSD).unwrap();
		assert_eq!(stranded.debt.pending_redistribution_principal, residue);
		assert_eq!(stranded.pending_redistribution_collateral, residue_collateral);

		let fresh_before = vault(DOT, PUSD, 4);
		assert_ok!(poke(9, DOT, PUSD, 4));
		let fresh_after = vault(DOT, PUSD, 4);
		assert_eq!(fresh_after.debt.principal - fresh_before.debt.principal, residue);
		assert_eq!(fresh_after.collateral - fresh_before.collateral, residue_collateral);
		let drained = branch_state(DOT, PUSD).unwrap();
		assert_eq!(drained.debt.pending_redistribution_principal, 0);
		assert_eq!(drained.pending_redistribution_collateral, 0);
		assert_eq!(held(DOT, crate::Pallet::<Test>::redistribution_account(&DOT, &PUSD)), 0);
	});
}

// The sole survivor must receive the complete pending-pool complement.
#[test]
fn sole_survivor_receives_the_exact_remainder() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 10_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let debt_2 = vault(DOT, PUSD, 2).debt.total();
		let coll_2 = held(DOT, 2);
		let principal_before = vault(DOT, PUSD, 1).debt.principal;
		assert_ok!(redistribute_for_test(DOT, PUSD, 2, coll_2));
		assert_eq!(vault(DOT, PUSD, 1).debt.principal, principal_before);
		assert_ok!(poke(9, DOT, PUSD, 1));
		let principal_after = vault(DOT, PUSD, 1).debt.principal;
		assert_eq!(principal_after - principal_before, debt_2);
		assert_eq!(held(DOT, 1), 11_000);
	});
}

// A one-unit stake floor keeps a debt-bearing vault eligible for redistribution and liquidation.
#[test]
fn dust_ratio_stake_floors_to_one_unit_and_stays_liquidatable() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// The fixture makes the corrected stake floor to zero before the minimum applies.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 2_000_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(1u128, 10_000u128));
		let coll_2 = held(DOT, 2);
		assert_ok!(redistribute_for_test(DOT, PUSD, 2, coll_2));

		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// The liquidation snapshot is total stake 1_000 over collateral 2_001_000, so the new
		// vault's stake floor(1_000 × 1_000 / 2_001_000) = 0 is lifted to the one-unit minimum.
		assert_eq!(vault(DOT, PUSD, 3).redistribution_stake, 1);
		assert_ok!(poke(9, DOT, PUSD, 3));

		set_price(DOT, FixedU128::from_rational(50u128, 100u128));
		assert_ok!(liquidate(99, DOT, PUSD, 3, 0, 0));
		assert!(!vault_exists(DOT, PUSD, 3));
	});
}

#[test]
fn vault_cr_projects_lazy_redistribution_before_materialization() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let coll_3 = held(DOT, 3);
		assert_ok!(redistribute_for_test(DOT, PUSD, 3, coll_3));
		// Restore price so the view's CR is defined for vault 1.
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));

		let view_pre = crate::Pallet::<Test>::vault_cr(DOT, PUSD, 1).expect("cr");
		// Vault 3 owed 500 plus a fee of 1. Vault 1 holds half the stake, so its share is
		// floor(501 / 2) = 250 of debt and 500 of collateral: 1_500 of collateral worth 15_000
		// against 500 + 250 + 1 of debt, and 15_000 / 751 = 19.973368…, rounded down.
		assert_eq!(
			view_pre,
			CollateralRatio::Ratio(FixedU128::from_inner(19_973_368_841_544_607_190))
		);
		assert_ok!(poke(99, DOT, PUSD, 1));
		let view_post = crate::Pallet::<Test>::vault_cr(DOT, PUSD, 1).expect("cr");
		// Projection must match execution before materialization.
		assert_eq!(view_pre, view_post);
	});
}

// Full-lifecycle identity soak: open → liquidate with a redistribution split
// → recipient touches → partial repay → redemption → overpay-close. The
// `try_state` identities (Σ principal exact, Σ floor(rate·stake) exact,
// accrual rate bounds) must hold at every stage, not just at the end.
#[test]
fn full_lifecycle_holds_branch_identities() {
	fn assert_identities() {
		crate::try_state::do_try_state::<Test>().expect("branch identities hold");
	}
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 2_000, 800, rate_pct(25, 100)));
		assert_ok!(open(3, DOT, PUSD, 3_000, 1_000, rate_pct(50, 100)));
		assert_identities();

		// A month of accrual so touches materialise real interest.
		advance_time(30 * ONE_DAY_MS);

		// Liquidate vault 1 through the production three-way path: active
		// Stability Pool, keeper JIT, then redistribution.
		set_price(DOT, FixedU128::from_rational(55u128, 100u128));
		let keeper_8_pre = collateral_balance(DOT, 8);
		let pool_pre = collateral_balance(DOT, SP_ACCOUNT);
		ActiveSpCapacity::set(200);
		mint_stable(PUSD, 8, 200);
		assert_ok!(liquidate(8, DOT, PUSD, 1, 200, 0));
		assert_identities();
		let outcome = liquidation_outcome();
		assert_ne!(outcome.active_pool.debt, 0);
		assert_ne!(outcome.keeper_jit.debt, 0);
		assert_ne!(outcome.redistribution.debt, 0);
		assert_eq!(collateral_balance(DOT, SP_ACCOUNT) - pool_pre, outcome.active_pool.collateral);
		assert_eq!(
			collateral_balance(DOT, 8) - keeper_8_pre,
			outcome.keeper_reward + outcome.keeper_jit.collateral
		);

		// The recipient must accrue interest from the redistribution time.
		assert_ok!(poke(9, DOT, PUSD, 2));
		assert_identities();

		// Partial repay exercises the full-contribution accrual rate swap.
		assert_ok!(repay(2, DOT, PUSD, 2, Some(300)));
		assert_identities();

		// Redemption against the cheapest vault at a healthy price.
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		let recipient_7_pre = collateral_balance(DOT, 7);
		assert_ok!(redeem(DOT, PUSD, 7, 400));
		// At price 10 the redemption releases floor(debt_cancelled / 10) collateral free
		// to the recipient.
		let released = collateral_balance(DOT, 7) - recipient_7_pre;
		assert_eq!(released, 40, "redeemed 400 debt at price 10 releases 40 collateral");
		assert_identities();

		// Touch the remaining whale, then close it by overpaying.
		assert_ok!(poke(9, DOT, PUSD, 3));
		assert_identities();
		assert_ok!(<Pusd as frame::traits::fungible::Mutate<u64>>::transfer(
			&1,
			&3,
			stable_balance(PUSD, 1),
			frame::traits::tokens::Preservation::Expendable,
		));
		assert_ok!(repay(3, DOT, PUSD, 3, Some(stable_balance(PUSD, 3))));
		// Repay-to-zero leaves a husk; close it to release the collateral and end
		// the lifecycle with the row gone.
		assert_ok!(close_vault(3, DOT, PUSD, None));
		assert!(!vault_exists(DOT, PUSD, 3), "vault 3 closed");
		assert_identities();
	});
}

// Interest-time debt-time accounting: interest on a redistributed share
// accrues from the liquidation moment t1, not from the branch interest-time
// origin or any absolute origin. Liquidate at t1, touch the recipient at t2, and the
// redistribution part of the accrued interest must equal
// `share · rate · (t2 - t1) / year` to within fixed-point flooring.
#[test]
fn redistributed_principal_accrues_interest_from_liquidation_moment() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 2_000, 800, rate_pct(50, 100)));

		// The 501 debt remains pending until the recipient is touched.
		set_price(DOT, FixedU128::from_rational(55u128, 100u128));
		let v_pre = vault(DOT, PUSD, 2);
		let redistributed = redistribute_for_test(DOT, PUSD, 1, 0).unwrap();
		assert_eq!(redistributed, 501);
		let v_at_record = vault(DOT, PUSD, 2);
		assert_eq!(v_at_record.debt.principal, v_pre.debt.principal);
		let minted_pre = branch_state(DOT, PUSD).unwrap().debt.minted_interest;

		// The expected interest includes own and redistributed principal for two years.
		advance_time(2 * ONE_YEAR_MS);
		let projected =
			<crate::Pallet<Test> as pusd_primitives::VaultInterface>::stablecoin_debt(&PUSD);
		assert_eq!(branch_state(DOT, PUSD).unwrap().debt.minted_interest, minted_pre);
		assert_ok!(poke(9, DOT, PUSD, 2));
		let v_post = vault(DOT, PUSD, 2);
		assert_eq!(v_post.debt.principal, v_at_record.debt.principal + 501);
		assert_eq!(v_post.debt.interest - v_at_record.debt.interest, 1_301);
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.debt.minted_interest - minted_pre, 1_301);
		assert_eq!(state.debt.pending_interest_attribution, 0);
		assert_eq!(projected, state.debt.outstanding());
	});
}

// Seeds pending redistribution debt of 502 with accrual rate 50.2.
fn seed_redistributed_recipient() {
	register_market(DOT, PUSD);
	assert_ok!(open(1, DOT, PUSD, 10_000, 500, rate_pct(10, 100)));
	assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(20, 100)));
	// Only vault 2 must be liquidatable in this fixture.
	set_price(DOT, FixedU128::from_rational(50u128, 100u128));
	let coll_2 = held(DOT, 2);
	assert_ok!(redistribute_for_test(DOT, PUSD, 2, coll_2));
}

// A recipient touch moves pending interest to the vault and preserves projected branch debt.
#[test]
fn recipient_owned_redistribution_interest_stays_in_branch_projection() {
	build_and_execute(|| {
		seed_redistributed_recipient();

		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.debt.principal, 500);
		assert_eq!(state.debt.pending_redistribution_principal, 502);
		let accrued_at_record =
			crate::Pallet::<Test>::accrued_branch_debt(&state, Timestamp::get());
		assert_eq!(accrued_at_record, 1_003);

		advance_time(ONE_YEAR_MS);
		let accrued_after_idle_year = crate::Pallet::<Test>::accrued_branch_debt(
			&branch_state(DOT, PUSD).unwrap(),
			Timestamp::get(),
		);
		assert_eq!(accrued_after_idle_year, 1_104);

		assert_ok!(poke(99, DOT, PUSD, 1));

		let state = branch_state(DOT, PUSD).unwrap();
		let vault = vault(DOT, PUSD, 1);
		assert_eq!(vault.debt.interest, 102, "fee 1 + ceil(1_002 × 10%)");
		assert!(vault.interest_prepaid != 0);
		// The unit the touch rounds up was charged ahead of the aggregate, so the projection nets
		// it.
		assert_eq!(
			crate::Pallet::<Test>::accrued_branch_debt(&state, Timestamp::get()),
			accrued_after_idle_year,
		);
	});
}

// Branch refresh frequency must not change projected debt at the same time.
#[test]
fn branch_debt_projection_is_refresh_cadence_independent() {
	let run = |refreshes: u64| {
		new_test_ext().execute_with(|| {
			seed_redistributed_recipient();
			assert_ok!(open(9, DOT, PUSD, 1_000, 300, rate_pct(5, 100)));

			for step in 0..10u64 {
				advance_time(ONE_YEAR_MS / 10);
				if step < refreshes {
					assert_ok!(poke(99, DOT, PUSD, 9));
				}
			}
			crate::try_state::do_try_state::<Test>().expect("post-test invariants hold");
			crate::Pallet::<Test>::accrued_branch_debt(
				&branch_state(DOT, PUSD).unwrap(),
				Timestamp::get(),
			)
		})
	};
	// The seed leaves 500 of principal, 502 pending, and a fee of 1. Vault 9 adds 300 and a fee of
	// ceil(300 · 8.83% · 7d / 365.25d) = 1, which makes 1_304. A year then accrues
	// 500 · 10% + 502 · 10% + 300 · 5% = 115.2, rounded up to 116.
	let untouched = run(0);
	assert_eq!(untouched, 1_420);
	assert_eq!(run(9), untouched);
}
