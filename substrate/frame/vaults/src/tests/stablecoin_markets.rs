//! Role-exclusivity invariants: the per-stablecoin market count that pairs
//! with `Branches`' own collateral-first keys to replace the full-registry
//! collision scan, and the atomicity of the cross-pallet market lifecycle it
//! participates in.

use crate::{mock::*, pallet::StablecoinMarkets};

fn markets_of(asset: AssetId) -> Option<u32> {
	StablecoinMarkets::<Test>::get(asset)
}

// Markets are counted per stablecoin: one collateral may back several
// stablecoins and one stablecoin may span several collaterals; removing one of
// several markets keeps the count, removing the last deletes the entry.
// Collateral usage is never indexed — the registry's own keys carry it.
#[test]
fn markets_are_counted_per_stablecoin() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(DOT, EUSD);
		register_market(TOKEN_X, PUSD);

		assert_eq!(markets_of(DOT), None);
		assert_eq!(markets_of(TOKEN_X), None);
		assert_eq!(markets_of(AssetId::WithId(PUSD)), Some(2));
		assert_eq!(markets_of(AssetId::WithId(EUSD)), Some(1));

		// Removing one of PUSD's two markets keeps the count.
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert_eq!(markets_of(AssetId::WithId(PUSD)), Some(1));

		// Removing the final markets deletes the entries.
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), TOKEN_X, PUSD));
		assert_eq!(markets_of(AssetId::WithId(PUSD)), None);
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, EUSD));
		assert_eq!(markets_of(AssetId::WithId(EUSD)), None);
		assert_eq!(
			LifecycleLog::get(),
			alloc::vec![
				(DOT, PUSD, true, 1),
				(DOT, EUSD, true, 1),
				(TOKEN_X, PUSD, true, 2),
				(DOT, PUSD, false, 1),
				(TOKEN_X, PUSD, false, 0),
				(DOT, EUSD, false, 0),
			]
		);
	});
}

// Role exclusivity binds live markets only, by design. Removal already
// requires the market to be empty and debt-free, so a retired stablecoin's
// only residual mint authority is its asset owner's — the same trust every
// listed asset carries, priced when it is listed.
#[test]
fn roles_are_reusable_once_no_live_market_holds_them() {
	build_and_execute(|| {
		register_market(TOKEN_X, PUSD);
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), TOKEN_X, PUSD));
		assert_eq!(markets_of(AssetId::WithId(PUSD)), None);

		// The retired stablecoin returns as a collateral (ADMIN funds the
		// custody seed of every Root-created market, so it needs the coin)...
		mint_stable(PUSD, ADMIN, 1_000_000_000_000);
		register_market(AssetId::WithId(PUSD), EUSD);
		// ...and the retired collateral returns as a stablecoin.
		register_market(ETH, TOKEN_X_ID);

		assert_eq!(markets_of(AssetId::WithId(EUSD)), Some(1));
		assert_eq!(markets_of(TOKEN_X), Some(1));
	});
}

// Cross-market role reuse is rejected in both directions: a live market's
// stablecoin cannot become a collateral, and a live market's collateral cannot
// become a stablecoin.
#[test]
fn cross_role_reuse_rejected_in_both_directions() {
	build_and_execute(|| {
		register_market(TOKEN_X, PUSD);

		// A live stablecoin as a new market's collateral.
		set_price(AssetId::WithId(PUSD), FixedU128::from_rational(10u128, 1u128));
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				AssetId::WithId(PUSD),
				EUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			Error::<Test>::StableCollateralCollision
		);

		// A live collateral as a new market's stablecoin: TOKEN_X's asset id
		// (`TOKEN_X_ID`) exists in `pallet-assets`, and its stable key is the
		// collateral id TOKEN_X itself.
		set_price(ETH, FixedU128::from_rational(10u128, 1u128));
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				ETH,
				TOKEN_X_ID,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			Error::<Test>::StableCollateralCollision
		);
	});
}

// A failing sibling `on_registered` rolls back the complete creation: the
// deposit, the market count, and the branch row.
#[test]
fn failed_registration_hook_rolls_back_everything() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		FailOnRegistered::set(true);
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::signed(PUSD_OWNER),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			DispatchError::Other("on_registered failure")
		);
		assert!(crate::pallet::Branches::<Test>::get(DOT, PUSD).is_none());
		assert_eq!(markets_of(AssetId::WithId(PUSD)), None);
		assert_eq!(creation_deposit_held(PUSD_OWNER), 0, "deposit hold rolled back");

		// The same creation lands once the sibling stops failing.
		FailOnRegistered::set(false);
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::signed(PUSD_OWNER),
			DOT,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));
		assert_eq!(creation_deposit_held(PUSD_OWNER), MarketDepositBase::get());
	});
}

// A failing sibling `on_deregistered` rolls back the complete removal: the
// market, its stablecoin count, and the still-locked deposit all survive
// intact.
#[test]
fn failed_deregistration_hook_rolls_back_everything() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::signed(PUSD_OWNER),
			DOT,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));

		FailOnDeregistered::set(true);
		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD),
			DispatchError::Other("on_deregistered failure")
		);
		assert!(crate::pallet::Branches::<Test>::get(DOT, PUSD).is_some());
		assert_eq!(markets_of(AssetId::WithId(PUSD)), Some(1));
		assert_eq!(creation_deposit_held(PUSD_OWNER), MarketDepositBase::get());

		// The removal lands once the sibling stops failing.
		FailOnDeregistered::set(false);
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert_eq!(creation_deposit_held(PUSD_OWNER), 0);
		assert_eq!(markets_of(AssetId::WithId(PUSD)), None);
	});
}
