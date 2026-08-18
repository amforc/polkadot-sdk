//! Stablecoin-wide and per-market debt ceilings.

use crate::{mock::*, tests::rate_pct};
use frame::traits::fungibles::Mutate;

// A stablecoin whose global ceiling is `0` cannot be borrowed, even though a market can exist.
#[test]
fn stablecoin_with_zero_ceiling_cannot_be_borrowed() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Governance pins the global ceiling back to 0 (allow-list off).
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 0));
		assert_noop!(
			open(1, DOT, PUSD, 10_000, 2_000, rate_pct(5, 100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
	});
}

// A borrow that fits the branch ceiling but breaches the stablecoin-wide ceiling is rejected.
#[test]
fn borrow_breaching_global_ceiling_is_rejected() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 2_500));

		// Principal and its small upfront fee fit.
		assert_ok!(open(1, DOT, PUSD, 100_000, 2_000, rate_pct(5, 100)));
		// Another 1_000 PUSD breaches the stablecoin-wide limit.
		assert_noop!(
			open(2, DOT, PUSD, 100_000, 1_000, rate_pct(5, 100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
		// A 400 PUSD borrow keeps the total below 2_500 and fits.
		assert_ok!(open(2, DOT, PUSD, 100_000, 400, rate_pct(5, 100)));
	});
}

// Markets issuing the same stablecoin share its ceiling across different collateral assets.
#[test]
fn global_ceiling_is_shared_across_a_stablecoins_markets() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(TOKEN_X, PUSD);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 2_600));

		assert_ok!(open(1, DOT, PUSD, 100_000, 2_000, rate_pct(5, 100)));
		assert_noop!(
			open(2, TOKEN_X, PUSD, 100_000, 1_000, rate_pct(5, 100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
		assert_ok!(open(2, TOKEN_X, PUSD, 100_000, 500, rate_pct(5, 100)));
	});
}

// Stablecoins with different denominations never consume one another's ceiling.
#[test]
fn global_ceiling_is_independent_across_stablecoins() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(DOT, EUSD);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 2_100));
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), EUSD, 600));
		assert_ok!(open(1, DOT, PUSD, 100_000, 2_000, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, EUSD, 100_000, 500, rate_pct(5, 100)));
	});
}

// The global ceiling must include principal and minted interest.
#[test]
fn global_ceiling_counts_all_outstanding_debt() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 2_000));
		// Add non-principal debt that makes the next borrow exceed the ceiling.
		mutate_branch_state(DOT, PUSD, |state| {
			state.debt.minted_interest = 600;
		});
		// 1_500 principal + 600 non-principal debt exceeds the 2_000 cap.
		assert_noop!(
			open(1, DOT, PUSD, 100_000, 1_500, rate_pct(1, 1_000)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
		// Clearing the non-principal debt frees the headroom; the same borrow lands.
		mutate_branch_state(DOT, PUSD, |state| {
			state.debt.minted_interest = 0;
		});
		assert_ok!(open(1, DOT, PUSD, 100_000, 1_500, rate_pct(1, 1_000)));
	});
}

// Repaying lowers stablecoin-wide debt, freeing global-ceiling headroom.
#[test]
fn repaying_frees_global_ceiling_headroom() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 2_500));
		assert_ok!(open(1, DOT, PUSD, 100_000, 2_000, rate_pct(5, 100)));
		assert_noop!(
			open(2, DOT, PUSD, 100_000, 1_000, rate_pct(5, 100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);

		// Repay most of owner 1's debt, dropping the collateral debt well below the cap.
		<VaultStableAssets as Mutate<AccountId>>::mint_into(PUSD, &1, 10_000).unwrap();
		assert_ok!(Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, 1_500));

		// The freed headroom now admits the previously-rejected borrow.
		assert_ok!(open(2, DOT, PUSD, 100_000, 1_000, rate_pct(5, 100)));
	});
}

/// Recomputes realized debt across all markets issuing one stable asset.
fn recomputed_stablecoin_debt(stable: StableId) -> Balance {
	crate::pallet::Branches::<Test>::iter()
		.filter(|(_, branch_stable, _)| *branch_stable == stable)
		.fold(0, |acc, (_, _, branch)| acc + branch.state.debt.outstanding())
}

#[track_caller]
fn assert_aggregate_matches(stable: StableId) {
	assert_eq!(
		crate::pallet::StablecoinDebt::<Test>::get(stable).outstanding,
		recomputed_stablecoin_debt(stable),
		"StablecoinDebt aggregate diverged from the branch recomputation"
	);
	#[cfg(feature = "try-runtime")]
	crate::try_state::do_try_state::<Test>().expect("all aggregate identities hold");
}

// The projected ceiling check counts the upfront fee, not just the proposed
// principal: a borrow whose principal alone fits is rejected once its fee
// tips the total over.
#[test]
fn projected_ceiling_counts_the_upfront_fee() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 2_000));

		// At 100% the first open's fee is ceil(2_000 · 1.0 · 7d/365.25d) = 39,
		// so the 2_039 projection exceeds the 2_000 limit.
		assert_noop!(
			open(1, DOT, PUSD, 100_000, 2_000, rate_pct(100, 100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 2_039));
		assert_ok!(open(1, DOT, PUSD, 100_000, 2_000, rate_pct(100, 100)));
		assert_aggregate_matches(PUSD);
	});
}

// The projected ceiling check counts aggregate interest accrued in memory at
// load — debt the stored aggregate has not minted yet.
#[test]
fn projected_ceiling_counts_accrued_aggregate_interest() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 2_400));
		// Stored outstanding after the open: 2_000 + 39 fee (derived above).
		assert_ok!(open(1, DOT, PUSD, 100_000, 2_000, rate_pct(100, 100)));
		assert_aggregate_matches(PUSD);

		// A year at 100% accrues 2_000 of pending aggregate interest. The
		// second open projects 2_300 principal + 39 + 2_000 + 6 fee = 4_345
		// PUSD, though its 2_345 stored-debt view would have fit.
		advance_time(pusd_primitives::MILLIS_PER_YEAR);
		assert_noop!(
			open(2, DOT, PUSD, 1_000, 300, rate_pct(100, 100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 4_345));
		assert_ok!(open(2, DOT, PUSD, 1_000, 300, rate_pct(100, 100)));
		assert_aggregate_matches(PUSD);
	});
}

// Each debt write must preserve the stablecoin-wide debt aggregate.
#[test]
fn stablecoin_debt_aggregate_tracks_every_write() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(DOT, EUSD);
		assert_aggregate_matches(PUSD);
		assert_aggregate_matches(EUSD);

		assert_ok!(open(1, DOT, PUSD, 100_000, 2_000, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, EUSD, 100_000, 1_000, rate_pct(5, 100)));
		assert_aggregate_matches(PUSD);
		assert_aggregate_matches(EUSD);

		assert_ok!(Pallet::<Test>::borrow(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			500,
			None,
			None,
			Position::endpoints_only(),
		));
		assert_aggregate_matches(PUSD);

		// Within the cooldown, the rate change mints an upfront fee.
		assert_ok!(Pallet::<Test>::change_rate(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			rate_pct(6, 100),
			Position::endpoints_only(),
		));
		assert_aggregate_matches(PUSD);

		advance_time(30 * 24 * 3_600 * 1_000);
		assert_ok!(Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		assert_aggregate_matches(PUSD);

		assert_ok!(Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, 500));
		assert_aggregate_matches(PUSD);

		assert_ok!(redeem_from(DOT, PUSD, 1, 9, 300));
		assert_aggregate_matches(PUSD);

		// A thin vault, then a price drop below its MCR, then liquidation.
		assert_ok!(open(3, DOT, PUSD, 40, 300, rate_pct(5, 100)));
		assert_aggregate_matches(PUSD);
		set_price(DOT, FixedU128::from_rational(8u128, 1u128));
		assert_ok!(liquidate(DOT, PUSD, 3));
		assert_aggregate_matches(PUSD);

		// Freezing flushes pending aggregate interest into the stored state.
		advance_time(24 * 3_600 * 1_000);
		assert_ok!(Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			true
		));
		assert_aggregate_matches(PUSD);
		assert_ok!(Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			false
		));
	});
}

// Resetting the ceiling to `0` removes only the policy row; debt accounting remains separate.
#[test]
fn zero_ceiling_reset_leaves_no_record() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 2_500));
		assert!(crate::pallet::GlobalDebtCeilings::<Test>::contains_key(PUSD));
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 0));
		assert!(!crate::pallet::GlobalDebtCeilings::<Test>::contains_key(PUSD));

		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 100_000));
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 0));
		assert!(!crate::pallet::GlobalDebtCeilings::<Test>::contains_key(PUSD));
		// 500 principal + ceil(500 · 0.05 · 7d/365.25d) = 500 + 1 upfront fee.
		assert_eq!(crate::pallet::StablecoinDebt::<Test>::get(PUSD).outstanding, 501);
	});
}
