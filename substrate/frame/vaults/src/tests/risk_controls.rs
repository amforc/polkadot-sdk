//! Stablecoin-wide and per-branch debt ceilings.

use crate::{
	mock::*,
	tests::{assert_event, assert_ok_and_invariants, percent, ONE_DAY_MS, ONE_YEAR_MS},
};

// A stablecoin whose global ceiling is `0` cannot be borrowed, even though a branch can exist.
#[test]
fn stablecoin_with_zero_ceiling_cannot_be_borrowed() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// Governance pins the global ceiling back to 0 (allow-list off).
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, 0));

		assert_noop!(
			open(1, DOT, STABLE, 10_000, 2_000, percent(5)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
	});
}

// Branches issuing the same stablecoin share its ceiling across different collateral assets.
#[test]
fn global_ceiling_is_shared_across_a_stablecoins_markets() {
	ExtBuilder::default()
		.branch(DOT, STABLE)
		.branch(TOKEN_X, STABLE)
		.build_and_execute(|| {
			assert_ok!(Pallet::<Test>::set_global_debt_ceiling(
				RuntimeOrigin::root(),
				STABLE,
				2_600
			));

			assert_ok!(open(1, DOT, STABLE, 100_000, 2_000, percent(5)));
			assert_noop!(
				open(2, TOKEN_X, STABLE, 100_000, 1_000, percent(5)),
				Error::<Test>::GlobalDebtCeilingExceeded
			);
			assert_ok!(open(2, TOKEN_X, STABLE, 100_000, 500, percent(5)));
		});
}

// Stablecoins with different denominations never consume one another's ceiling.
#[test]
fn global_ceiling_is_independent_across_stablecoins() {
	ExtBuilder::default()
		.branch(DOT, STABLE)
		.branch(DOT, EUSD)
		.build_and_execute(|| {
			assert_ok!(Pallet::<Test>::set_global_debt_ceiling(
				RuntimeOrigin::root(),
				STABLE,
				2_100
			));
			assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), EUSD, 600));

			assert_ok!(open(1, DOT, STABLE, 100_000, 2_000, percent(5)));
			assert_ok!(open(2, DOT, EUSD, 100_000, 500, percent(5)));
		});
}

// Repaying lowers stablecoin-wide debt, freeing global-ceiling headroom.
#[test]
fn repaying_frees_global_ceiling_headroom() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, 2_500));
		assert_ok!(open(1, DOT, STABLE, 100_000, 2_000, percent(5)));
		assert_noop!(
			open(2, DOT, STABLE, 100_000, 1_000, percent(5)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);

		// Repay most of owner 1's debt, dropping the collateral debt well below the cap.
		mint_stable(STABLE, 1, 10_000);
		assert_ok!(repay(1, DOT, STABLE, 1, Some(1_500)));

		// The freed headroom now admits the previously-rejected borrow.
		assert_ok!(open(2, DOT, STABLE, 100_000, 1_000, percent(5)));
	});
}

// The projected ceiling check counts the upfront fee, not just the proposed
// principal: a borrow whose principal alone fits is rejected once its fee
// tips the total over.
#[test]
fn projected_ceiling_counts_the_upfront_fee() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, 2_000));

		// At 100% the first open's fee is ceil(2_000 · 1.0 · 7d/365.25d) = 39,
		// so the 2_039 projection exceeds the 2_000 limit.
		assert_noop!(
			open(1, DOT, STABLE, 100_000, 2_000, percent(100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);

		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, 2_039));
		assert_ok!(open(1, DOT, STABLE, 100_000, 2_000, percent(100)));
	});
}

// The projected ceiling check counts aggregate interest accrued in memory at
// load — debt the stored aggregate has not minted yet.
#[test]
fn projected_ceiling_counts_accrued_aggregate_interest() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, 2_400));
		// Stored outstanding after the open: 2_000 + 39 fee (derived above).
		assert_ok!(open(1, DOT, STABLE, 100_000, 2_000, percent(100)));
		check_invariants();

		// A year at 100% accrues 2_000 of pending aggregate interest. The
		// second open projects 2_300 principal + 39 + 2_000 + 6 fee = 4_345
		// STABLE, though its 2_345 stored-debt view would have fit.
		advance_time(ONE_YEAR_MS);

		assert_noop!(
			open(2, DOT, STABLE, 1_000, 300, percent(100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, 4_345));
		assert_ok!(open(2, DOT, STABLE, 1_000, 300, percent(100)));
	});
}

// Check after every write, before later operations can reconcile a broken aggregate.
#[test]
fn stablecoin_debt_aggregate_tracks_every_write() {
	ExtBuilder::default()
		.branch(DOT, STABLE)
		.branch(DOT, EUSD)
		.build_and_execute(|| {
			check_invariants();
			assert_ok_and_invariants(open(1, DOT, STABLE, 100_000, 2_000, percent(5)));
			assert_ok_and_invariants(open(2, DOT, EUSD, 100_000, 1_000, percent(5)));
			assert_ok_and_invariants(borrow(1, DOT, STABLE, 500, None));
			// A premature rate change charges the fee into the aggregate.
			assert_ok_and_invariants(change_rate(1, DOT, STABLE, percent(6)));
			advance_time(30 * ONE_DAY_MS);
			assert_ok_and_invariants(poke(9, DOT, STABLE, 1));
			assert_ok_and_invariants(repay(1, DOT, STABLE, 1, Some(500)));
			assert_ok_and_invariants(redeem_from(DOT, STABLE, 1, 9, 300));
			assert_ok_and_invariants(open(3, DOT, STABLE, 40, 300, percent(5)));
			set_price(DOT, FixedU128::from_u32(8));
			ActiveSpCapacity::set(1_000);
			assert_ok_and_invariants(liquidate(9, DOT, STABLE, 3, 0, 0));
			advance_time(ONE_DAY_MS);
			// Freezing flushes pending aggregate interest into stored state.
			assert_ok_and_invariants(set_governance_frozen(ADMIN, DOT, STABLE, true));
			assert_ok_and_invariants(set_governance_frozen(ADMIN, DOT, STABLE, false));
		});
}

// Resetting the ceiling to `0` removes only the policy row; debt accounting remains separate.
#[test]
fn zero_ceiling_reset_leaves_no_record() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, 2_500));
		assert!(crate::pallet::GlobalDebtCeilings::<Test>::contains_key(STABLE));
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, 0));
		assert!(!crate::pallet::GlobalDebtCeilings::<Test>::contains_key(STABLE));

		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, 100_000));
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));

		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, 0));

		assert!(!crate::pallet::GlobalDebtCeilings::<Test>::contains_key(STABLE));
		// 500 principal + ceil(500 · 0.05 · 7d/365.25d) = 500 + 1 upfront fee.
		assert_eq!(crate::pallet::StablecoinDebt::<Test>::get(STABLE).outstanding, 501);
	});
}

// The ceiling shares `CreateOrigin` with branch registration: the stablecoin's
// owner manages its own limit, every other signer stays locked out.
#[test]
fn asset_owner_sets_the_global_ceiling() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(
			RuntimeOrigin::signed(STABLECOIN_OWNER),
			STABLE,
			2_000
		));

		assert_event(crate::Event::GlobalDebtCeilingSet {
			stable_id: STABLE,
			old_ceiling: GLOBAL_CEILING,
			new_ceiling: 2_000,
		});
		assert_ok!(open(1, DOT, STABLE, 100_000, 1_500, percent(5)));

		assert_noop!(
			Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::signed(2), STABLE, Balance::MAX),
			DispatchError::BadOrigin
		);
	});
}

// A ceiling may precede the stablecoin's first branch, but never its asset: an unknown key
// would leave a limit nothing can borrow against.
#[test]
fn global_ceiling_requires_an_existing_stable_asset() {
	ExtBuilder::default().build_and_execute(|| {
		assert_noop!(
			Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), 9_999, 2_000),
			Error::<Test>::UnknownStable
		);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, 2_000));
		assert_eq!(crate::pallet::GlobalDebtCeilings::<Test>::get(STABLE), 2_000);

		// Clearing stores nothing, so it stays open for the record of a destroyed asset.
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), 9_999, 0));
		assert!(!crate::pallet::GlobalDebtCeilings::<Test>::contains_key(9_999));
	});
}

// The view pairs the stablecoin's debt with the limit borrowing is checked against. Accrual of
// that debt is owned by `stablecoin_debt_accrues_interest_across_untouched_markets`.
#[test]
fn stablecoin_debt_view_pairs_debt_with_ceiling() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_eq!(Pallet::<Test>::stablecoin_debt_and_ceiling(STABLE), (0, GLOBAL_CEILING));

		assert_ok!(open(1, DOT, STABLE, 100_000, 1_000, percent(5)));
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, 5_000));

		// 1_000 principal + ceil(1_000 · 0.05 · 7d/365.25d) = 1_000 + 1 upfront fee.
		assert_eq!(Pallet::<Test>::stablecoin_debt_and_ceiling(STABLE), (1_001, 5_000));
	});
}
