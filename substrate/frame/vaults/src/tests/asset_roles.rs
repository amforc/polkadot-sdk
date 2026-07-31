//! Asset-role registry invariants: the reference-counted role map that
//! replaced the full-registry collision scan, and the atomicity of the
//! cross-pallet market lifecycle it participates in.

use crate::{
	mock::*,
	pallet::AssetRoles,
	types::{AssetRole, AssetRoleUsage},
};

fn role_of(asset: AssetId) -> Option<AssetRoleUsage> {
	AssetRoles::<Test>::get(asset)
}

fn usage(role: AssetRole, markets: u32) -> Option<AssetRoleUsage> {
	Some(AssetRoleUsage { role, markets })
}

// Roles are reference-counted per market: one collateral may back several
// stablecoins and one stablecoin may span several collaterals; removing one of
// several references keeps the role, removing the last deletes the entry.
#[test]
fn roles_are_reference_counted_across_shared_markets() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(DOT, EUSD);
		register_market(TOKEN_X, PUSD);

		assert_eq!(role_of(DOT), usage(AssetRole::Collateral, 2));
		assert_eq!(role_of(TOKEN_X), usage(AssetRole::Collateral, 1));
		assert_eq!(role_of(AssetId::WithId(PUSD)), usage(AssetRole::Stable, 2));
		assert_eq!(role_of(AssetId::WithId(EUSD)), usage(AssetRole::Stable, 1));

		// Removing one of DOT's / PUSD's two markets keeps both roles held.
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert_eq!(role_of(DOT), usage(AssetRole::Collateral, 1));
		assert_eq!(role_of(AssetId::WithId(PUSD)), usage(AssetRole::Stable, 1));

		// Removing the final references deletes the entries.
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), TOKEN_X, PUSD));
		assert_eq!(role_of(TOKEN_X), None);
		assert_eq!(role_of(AssetId::WithId(PUSD)), None);
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, EUSD));
		assert_eq!(role_of(DOT), None);
		assert_eq!(role_of(AssetId::WithId(EUSD)), None);
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
				default_branch_config()
			),
			Error::<Test>::StableCollateralCollision
		);

		// A live collateral as a new market's stablecoin: TOKEN_X's asset id
		// (`TOKEN_X_ID`) exists in `pallet-assets`, and its role key is the
		// collateral id TOKEN_X itself.
		set_price(ETH, FixedU128::from_rational(10u128, 1u128));
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				ETH,
				TOKEN_X_ID,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config()
			),
			Error::<Test>::StableCollateralCollision
		);
	});
}

// A failing sibling `on_registered` rolls back the complete creation: the
// deposit, the role counters, and the branch row.
#[test]
fn failed_registration_hook_rolls_back_everything() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		let redistribution = Pallet::<Test>::redistribution_account(&DOT, &PUSD);
		let payer_before = collateral_balance(DOT, PUSD_OWNER);
		FailOnRegistered::set(true);
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::signed(PUSD_OWNER),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config()
			),
			DispatchError::Other("on_registered failure")
		);
		assert!(crate::pallet::Branches::<Test>::get(DOT, PUSD).is_none());
		assert_eq!(role_of(DOT), None);
		assert_eq!(role_of(AssetId::WithId(PUSD)), None);
		assert_eq!(creation_deposit_held(PUSD_OWNER), 0, "deposit hold rolled back");
		assert_eq!(collateral_balance(DOT, PUSD_OWNER), payer_before, "seed funding rolled back");
		assert_eq!(collateral_balance(DOT, redistribution), 0, "seed custody rolled back");
		assert_eq!(System::providers(&redistribution), 0, "provider reference rolled back");

		// The same creation lands once the sibling stops failing.
		FailOnRegistered::set(false);
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::signed(PUSD_OWNER),
			DOT,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config()
		));
		assert_eq!(creation_deposit_held(PUSD_OWNER), MarketDepositBase::get());
	});
}

// A failing sibling `on_deregistered` rolls back the complete removal: the
// market, its roles, and the still-locked deposit all survive intact.
#[test]
fn failed_deregistration_hook_rolls_back_everything() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::signed(PUSD_OWNER),
			DOT,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config()
		));

		FailOnDeregistered::set(true);
		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD),
			DispatchError::Other("on_deregistered failure")
		);
		assert!(crate::pallet::Branches::<Test>::get(DOT, PUSD).is_some());
		assert_eq!(role_of(DOT), usage(AssetRole::Collateral, 1));
		assert_eq!(role_of(AssetId::WithId(PUSD)), usage(AssetRole::Stable, 1));
		assert_eq!(creation_deposit_held(PUSD_OWNER), MarketDepositBase::get());

		// The removal lands once the sibling stops failing.
		FailOnDeregistered::set(false);
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert_eq!(creation_deposit_held(PUSD_OWNER), 0);
		assert_eq!(role_of(DOT), None);
	});
}
