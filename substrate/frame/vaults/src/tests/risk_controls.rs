//! Stablecoin-wide and per-market debt ceilings.

use crate::{
	mock::*,
	tests::{
		assert_event, assert_invariants, assert_ok_and_invariants, rate_pct, ONE_DAY_MS,
		ONE_YEAR_MS,
	},
};
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
		assert_ok!(Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, Some(1_500)));

		// The freed headroom now admits the previously-rejected borrow.
		assert_ok!(open(2, DOT, PUSD, 100_000, 1_000, rate_pct(5, 100)));
	});
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
		assert_invariants();

		// A year at 100% accrues 2_000 of pending aggregate interest. The
		// second open projects 2_300 principal + 39 + 2_000 + 6 fee = 4_345
		// PUSD, though its 2_345 stored-debt view would have fit.
		advance_time(ONE_YEAR_MS);
		assert_noop!(
			open(2, DOT, PUSD, 1_000, 300, rate_pct(100, 100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD, 4_345));
		assert_ok!(open(2, DOT, PUSD, 1_000, 300, rate_pct(100, 100)));
	});
}

// Check after every write, before later operations can reconcile a broken aggregate.
#[test]
fn stablecoin_debt_aggregate_tracks_every_write() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(DOT, EUSD);
		assert_invariants();
		assert_ok_and_invariants(open(1, DOT, PUSD, 100_000, 2_000, rate_pct(5, 100)));
		assert_ok_and_invariants(open(2, DOT, EUSD, 100_000, 1_000, rate_pct(5, 100)));
		assert_ok_and_invariants(borrow(1, DOT, PUSD, 500, None));
		// A premature rate change charges the fee into the aggregate.
		assert_ok_and_invariants(change_rate(1, DOT, PUSD, rate_pct(6, 100)));
		advance_time(30 * ONE_DAY_MS);
		assert_ok_and_invariants(poke(9, DOT, PUSD, 1));
		assert_ok_and_invariants(repay(1, DOT, PUSD, 1, Some(500)));
		assert_ok_and_invariants(redeem_from(DOT, PUSD, 1, 9, 300));
		assert_ok_and_invariants(open(3, DOT, PUSD, 40, 300, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(8u128, 1u128));
		assert_ok_and_invariants(liquidate(DOT, PUSD, 3));
		advance_time(ONE_DAY_MS);
		// Freezing flushes pending aggregate interest into stored state.
		assert_ok_and_invariants(set_governance_frozen(ADMIN, DOT, PUSD, true));
		assert_ok_and_invariants(set_governance_frozen(ADMIN, DOT, PUSD, false));
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

// The ceiling shares `CreateOrigin` with market registration: the stablecoin's
// owner manages its own limit, every other signer stays locked out.
#[test]
fn asset_owner_sets_the_global_ceiling() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(
			RuntimeOrigin::signed(PUSD_OWNER),
			PUSD,
			2_000
		));
		assert_event(crate::Event::GlobalDebtCeilingSet { stable_id: PUSD, ceiling: 2_000 });
		assert_ok!(open(1, DOT, PUSD, 100_000, 1_500, rate_pct(5, 100)));
		assert_noop!(
			Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::signed(2), PUSD, Balance::MAX),
			DispatchError::BadOrigin
		);
	});
}
