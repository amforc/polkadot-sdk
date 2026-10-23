//! Role-exclusivity invariants: the per-stablecoin branch count that pairs
//! with `Branches`' own collateral-first keys to replace the full-registry
//! collision scan, and the atomicity of the cross-pallet branch lifecycle it
//! participates in.

use crate::{mock::*, pallet::StablecoinMarkets};

// Branches are counted per stablecoin: one collateral may back several
// stablecoins and one stablecoin may span several collaterals; removing one of
// several branches keeps the count, removing the last deletes the entry.
// Collateral usage is never indexed — the registry's own keys carry it.
#[test]
fn markets_are_counted_per_stablecoin() {
	ExtBuilder::default()
		.branch(DOT, STABLE)
		.branch(DOT, EUSD)
		.branch(TOKEN_X, STABLE)
		.build_and_execute(|| {
			assert_eq!(StablecoinMarkets::<Test>::get(DOT), None);
			assert_eq!(StablecoinMarkets::<Test>::get(TOKEN_X), None);
			assert_eq!(StablecoinMarkets::<Test>::get(AssetId::WithId(STABLE)), Some(2));
			assert_eq!(StablecoinMarkets::<Test>::get(AssetId::WithId(EUSD)), Some(1));

			// Removing one of STABLE's two branches keeps the count.
			assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, STABLE));

			assert_eq!(StablecoinMarkets::<Test>::get(AssetId::WithId(STABLE)), Some(1));

			// Removing the final branches deletes the entries.
			assert_ok!(Pallet::<Test>::remove_branch(
				RuntimeOrigin::signed(ADMIN),
				TOKEN_X,
				STABLE
			));
			assert_eq!(StablecoinMarkets::<Test>::get(AssetId::WithId(STABLE)), None);
			assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, EUSD));
			assert_eq!(StablecoinMarkets::<Test>::get(AssetId::WithId(EUSD)), None);
			assert_eq!(
				LifecycleLog::get(),
				alloc::vec![
					(DOT, STABLE, Lifecycle::Registered, 1),
					(DOT, EUSD, Lifecycle::Registered, 1),
					(TOKEN_X, STABLE, Lifecycle::Registered, 2),
					(DOT, STABLE, Lifecycle::Deregistered, 1),
					(TOKEN_X, STABLE, Lifecycle::Deregistered, 0),
					(DOT, EUSD, Lifecycle::Deregistered, 0),
				]
			);
		});
}

// Role exclusivity binds live branches only, by design. Removal already
// requires the branch to be empty and debt-free, so a retired stablecoin's
// only residual mint authority is its asset owner's — the same trust every
// listed asset carries, priced when it is listed.
#[test]
fn roles_are_reusable_once_no_live_market_holds_them() {
	ExtBuilder::default().branch(TOKEN_X, STABLE).build_and_execute(|| {
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), TOKEN_X, STABLE));
		assert_eq!(StablecoinMarkets::<Test>::get(AssetId::WithId(STABLE)), None);

		// The retired stablecoin returns as a collateral (ADMIN funds the
		// custody seed of every Root-created branch, so it needs the coin)...
		mint_stable(STABLE, ADMIN, 1_000_000_000_000);
		register_market(AssetId::WithId(STABLE), EUSD);
		// ...and the retired collateral returns as a stablecoin.
		register_market(ETH, TOKEN_X_ID);

		assert_eq!(StablecoinMarkets::<Test>::get(AssetId::WithId(EUSD)), Some(1));
		assert_eq!(StablecoinMarkets::<Test>::get(TOKEN_X), Some(1));
	});
}

// Cross-branch role reuse is rejected in both directions: a live branch's
// stablecoin cannot become a collateral, and a live branch's collateral cannot
// become a stablecoin.
#[test]
fn cross_role_reuse_rejected_in_both_directions() {
	ExtBuilder::default().branch(TOKEN_X, STABLE).build_and_execute(|| {
		// A live stablecoin as a new branch's collateral.
		set_price(AssetId::WithId(STABLE), DEFAULT_PRICE);
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				AssetId::WithId(STABLE),
				EUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			Error::<Test>::StableCollateralCollision
		);

		// A live collateral as a new branch's stablecoin: TOKEN_X's asset id
		// (`TOKEN_X_ID`) exists in `pallet-assets`, and its stable key is the
		// collateral id TOKEN_X itself.
		set_price(ETH, DEFAULT_PRICE);
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
// deposit, the branch count, and the branch row.
#[test]
fn failed_registration_hook_rolls_back_everything() {
	ExtBuilder::default().price(DOT, DEFAULT_PRICE).build_and_execute(|| {
		FailOnRegistered::set(true);

		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::signed(STABLECOIN_OWNER),
				DOT,
				STABLE,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			DispatchError::Other("on_registered failure")
		);

		assert!(crate::pallet::Branches::<Test>::get(DOT, STABLE).is_none());
		assert_eq!(StablecoinMarkets::<Test>::get(AssetId::WithId(STABLE)), None);
		assert_eq!(creation_deposit_held(STABLECOIN_OWNER), 0, "deposit hold rolled back");

		// The same creation lands once the sibling stops failing.
		FailOnRegistered::set(false);

		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::signed(STABLECOIN_OWNER),
			DOT,
			STABLE,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));
		assert_eq!(creation_deposit_held(STABLECOIN_OWNER), MarketDepositBase::get());
	});
}

// A failing sibling `on_deregistered` rolls back the complete removal: the
// branch, its stablecoin count, and the still-locked deposit all survive
// intact.
#[test]
fn failed_deregistration_hook_rolls_back_everything() {
	ExtBuilder::default().price(DOT, DEFAULT_PRICE).build_and_execute(|| {
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::signed(STABLECOIN_OWNER),
			DOT,
			STABLE,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));

		FailOnDeregistered::set(true);
		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, STABLE),
			DispatchError::Other("on_deregistered failure")
		);

		assert!(crate::pallet::Branches::<Test>::get(DOT, STABLE).is_some());
		assert_eq!(StablecoinMarkets::<Test>::get(AssetId::WithId(STABLE)), Some(1));
		assert_eq!(creation_deposit_held(STABLECOIN_OWNER), MarketDepositBase::get());

		// The removal lands once the sibling stops failing.
		FailOnDeregistered::set(false);

		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, STABLE));
		assert_eq!(creation_deposit_held(STABLECOIN_OWNER), 0);
		assert_eq!(StablecoinMarkets::<Test>::get(AssetId::WithId(STABLE)), None);
	});
}
