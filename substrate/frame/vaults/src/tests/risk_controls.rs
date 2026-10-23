//! Stablecoin-wide and per-branch debt ceilings.

use crate::{
	mock::*,
	tests::{assert_event, assert_ok_and_invariants, percent, ONE_DAY_MS, ONE_YEAR_MS},
};

// The global ceiling binds per stablecoin: branches issuing one coin share it across collaterals,
// another coin's debt never consumes it, repaying frees headroom, and a ceiling of `0` blocks
// borrowing although the branch exists.
#[test]
fn global_ceiling_is_per_stablecoin_and_tracks_debt() {
	ExtBuilder::default()
		.branch(DOT, STABLE)
		.branch(TOKEN_X, STABLE)
		.branch(DOT, EUSD)
		.build_and_execute(|| {
			let set_ceiling = |stable, ceiling| {
				Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), stable, ceiling)
			};
			assert_ok!(set_ceiling(STABLE, 2_600));
			assert_ok!(set_ceiling(EUSD, 1_000));

			// Every open below at 5% adds ceil(principal · 5% · 7d / 365.25d) = 1 of upfront fee.
			assert_ok!(open(1, DOT, STABLE, 100_000, 2_000, percent(5)));
			// 1_001 fits under 2_600 alone, but not on top of the other collateral's 2_002.
			assert_noop!(
				open(2, TOKEN_X, STABLE, 100_000, 1_000, percent(5)),
				Error::<Test>::GlobalDebtCeilingExceeded
			);
			// STABLE's 2_002 would exceed the EUSD ceiling if it counted against it.
			assert_ok!(open(2, DOT, EUSD, 100_000, 500, percent(5)));

			mint_stable(STABLE, 1, 10_000);
			assert_ok!(repay(1, DOT, STABLE, 1, Some(1_500)));

			// 502 + 1_001 now fits.
			assert_ok!(open(2, TOKEN_X, STABLE, 100_000, 1_000, percent(5)));

			// 501 + 301 would fit under the old 1_000, so only a cleared ceiling rejects it.
			assert_ok!(set_ceiling(EUSD, 0));
			assert!(!crate::pallet::GlobalDebtCeilings::<Test>::contains_key(EUSD));
			assert_noop!(
				open(3, DOT, EUSD, 100_000, 300, percent(5)),
				Error::<Test>::GlobalDebtCeilingExceeded
			);
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

// Check after every write, before later operations can reconcile a broken aggregate. The
// liquidation splits into an offset and a redistribution, so the writes include the later
// touches, repay and overpaying close of a vault holding a redistributed share, and a redemption
// from a vault whose share is still pending. The second coin is backed by the same collateral on
// a branch of its own.
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
			assert_ok_and_invariants(open(4, DOT, STABLE, 1_000, 500, percent(25)));

			// Part offset, the rest redistributed to vaults 1 and 4.
			set_price(DOT, FixedU128::from_u32(8));
			assert_ok_and_invariants(liquidate_with(DOT, STABLE, 3, |_| LiquidationAllocation {
				offset: OffsetAllocation { collateral_recipient: 8, debt: 100, collateral: 13 },
				redistribution_collateral: 27,
				keeper: KeeperCompensation { recipient: 8, collateral: 0 },
			}));

			// The recipient accrues from the redistribution time, and a partial repay swaps its
			// full-contribution accrual rate.
			assert_ok_and_invariants(poke(9, DOT, STABLE, 4));
			assert_ok_and_invariants(repay(4, DOT, STABLE, 4, Some(100)));
			// Vault 1 is still untouched, so the redemption settles its share inside the step.
			assert!(market_state(DOT, STABLE).debt.pending_redistribution_principal > 0);
			assert_ok_and_invariants(redeem_from(DOT, STABLE, 1, 7, 400));
			advance_time(ONE_DAY_MS);
			// Freezing flushes pending aggregate interest into stored state.
			assert_ok_and_invariants(set_governance_frozen(ADMIN, DOT, STABLE, true));
			assert_ok_and_invariants(set_governance_frozen(ADMIN, DOT, STABLE, false));

			// Close the recipient by overpaying.
			assert_ok_and_invariants(
				<Stable as frame::traits::fungible::Mutate<AccountId>>::transfer(
					&1,
					&4,
					stable_balance(STABLE, 1),
					frame::traits::tokens::Preservation::Expendable,
				),
			);
			assert_ok_and_invariants(repay(4, DOT, STABLE, 4, Some(stable_balance(STABLE, 4))));
			assert_ok_and_invariants(close_vault(4, DOT, STABLE, None));
			assert!(!vault_exists(DOT, STABLE, 4));
		});
}

// The ceiling shares `CreateOrigin` with branch registration: the stablecoin's owner manages its
// own limit, every other signer stays locked out. A ceiling may precede the stablecoin's first
// branch, but never its asset: an unknown key would leave a limit nothing can borrow against.
#[test]
fn set_global_debt_ceiling_origin_and_asset_rules() {
	ExtBuilder::default().build_and_execute(|| {
		assert_noop!(
			Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), 9_999, 2_000),
			Error::<Test>::UnknownStable
		);
		// Clearing stores nothing, so it stays open for the record of a destroyed asset.
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), 9_999, 0));
		assert!(!crate::pallet::GlobalDebtCeilings::<Test>::contains_key(9_999));

		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(
			RuntimeOrigin::signed(STABLECOIN_OWNER),
			STABLE,
			2_000
		));

		assert_eq!(crate::pallet::GlobalDebtCeilings::<Test>::get(STABLE), 2_000);
		assert_event(crate::Event::GlobalDebtCeilingSet {
			stable_id: STABLE,
			old_ceiling: 0,
			new_ceiling: 2_000,
		});
		assert_noop!(
			Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::signed(2), STABLE, Balance::MAX),
			DispatchError::BadOrigin
		);

		// A stablecoin without a ceiling cannot be borrowed, so this open proves the ceiling set
		// before the branch existed applies to it.
		create_market(DOT, STABLE, DEFAULT_PRICE, default_branch_config());
		assert_ok!(open(1, DOT, STABLE, 100_000, 1_500, percent(5)));
	});
}
