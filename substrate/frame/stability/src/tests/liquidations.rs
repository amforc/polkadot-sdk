//! Vault liquidation against the real Stability Pool and its prepared collateral custody.

use crate::mock::*;
use frame::traits::{fungibles::Inspect as _, tokens::Provenance};

#[test]
fn vault_liquidation_uses_the_real_stability_pool() {
	build_and_execute(|| {
		let mut config = default_branch_config();
		config.upfront_fee_period = 0;
		register_branch(DOT, PUSD, config);
		assert_ok!(open_vault(1, DOT, PUSD, 600, 500));
		assert_ok!(open_vault(2, DOT, PUSD, 2_000, 500));
		// The deposit only matures here: the liquidation itself activates it, so no row touch
		// stands between a depositor and the offset that spends their capital.
		seed_matured_deposit(3, 500);
		set_price(DOT, FixedU128::from_rational(9u128, 10u128));

		let owner_before = collateral_balance(DOT, 1);
		let keeper_before = collateral_balance(DOT, 4);
		let pool_account = Stability::pool_account(&DOT, &PUSD);
		let pool_before = collateral_balance(DOT, pool_account);

		assert_ok!(Vaults::liquidate(
			RuntimeOrigin::signed(4),
			DOT,
			PUSD,
			1,
			pallet_vaults::JitTerms { max_stable: 0, min_collateral_out: 0 },
		));

		assert!(pallet_vaults::pallet::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 0);
		assert_eq!(collateral_balance(DOT, pool_account) - pool_before, 571);
		assert_eq!(collateral_balance(DOT, 4) - keeper_before, 12);
		// Terminal interest makes debt 501 and reduces the owner surplus to 14.
		assert_eq!(collateral_balance(DOT, 1) - owner_before, 14);
	});
}

#[test]
fn sub_minimum_first_gain_settles_into_touched_pool_account() {
	build_and_execute(|| {
		assert_ok!(Assets::force_create(RuntimeOrigin::root(), 77, 1, true, 1_000));
		let collateral = AssetId::WithId(77);
		let mut config = branch_config_for(collateral.clone(), PUSD);
		config.upfront_fee_period = 0;
		register_branch(collateral.clone(), PUSD, config);
		for owner in [1, 2] {
			mint_collateral(collateral.clone(), owner, 5_000);
			assert_ok!(open_vault(owner, collateral.clone(), PUSD, 3_000, 500));
		}
		// The keeper's reward is itself below this asset's 1_000 minimum, so give account 4 an
		// account to receive it. The pool's gain is what this test is about.
		mint_collateral(collateral.clone(), 4, 1_000);
		// Registration seeded custody with the asset minimum, which a hold's
		// `Protect` preservation keeps free.
		let redistribution = Vaults::redistribution_account(&collateral, &PUSD);
		mint_stable(PUSD, 3, 100);
		assert_ok!(deposit_and_mature(3, collateral.clone(), PUSD, 100));
		// floor(0.18 * 3_000) = 540 of value: CR 1.08 sits below the 1.10 MCR.
		set_price(collateral.clone(), FixedU128::from_rational(9u128, 50u128));

		let owner_free_before = collateral_balance(collateral.clone(), 1);
		let pool_account = Stability::pool_account(&collateral, &PUSD);
		// Registration touched a zero-balance asset account, so even a one-unit gain can enter it.
		assert_ok!(Assets::can_deposit(77, &pool_account, 1, Provenance::Extant).into_result());

		// The active-pool gain is below the asset minimum to test prepared custody.
		// The pre-created account accepts it, so the normal active-first waterfall remains intact.
		assert_ok!(Vaults::liquidate(
			RuntimeOrigin::signed(4),
			collateral.clone(),
			PUSD,
			1,
			pallet_vaults::JitTerms { max_stable: 0, min_collateral_out: 0 },
		));

		// The expected split includes terminal interest and both 5% penalties.
		assert!(pallet_vaults::pallet::Vaults::<Test>::get((collateral.clone(), PUSD, 1)).is_none());
		let state = pool_state(collateral.clone(), PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.total_collateral_gains_unclaimed, 571);
		assert_eq!(stable_balance(PUSD, pool_account), 0);
		assert_eq!(collateral_balance(collateral.clone(), pool_account), 571);
		// Redistribution collateral remains in custody until the recipient is touched.
		use frame::traits::fungibles::InspectHold;
		assert_eq!(
			<VaultCollateralAssets as InspectHold<AccountId>>::balance_on_hold(
				collateral.clone(),
				&pallet_vaults::HoldReason::VaultCollateral.into(),
				&redistribution,
			),
			2_299
		);
		assert_eq!(
			<VaultCollateralAssets as InspectHold<AccountId>>::balance_on_hold(
				collateral.clone(),
				&pallet_vaults::HoldReason::VaultCollateral.into(),
				&2,
			),
			3_000
		);
		assert_eq!(
			pallet_vaults::pallet::Vaults::<Test>::get((collateral.clone(), PUSD, 2))
				.unwrap()
				.vault
				.collateral,
			3_000
		);
		assert_ok!(Vaults::poke(RuntimeOrigin::signed(4), collateral.clone(), PUSD, 2));
		assert_eq!(
			<VaultCollateralAssets as InspectHold<AccountId>>::balance_on_hold(
				collateral.clone(),
				&pallet_vaults::HoldReason::VaultCollateral.into(),
				&redistribution,
			),
			0
		);
		assert_eq!(
			<VaultCollateralAssets as InspectHold<AccountId>>::balance_on_hold(
				collateral.clone(),
				&pallet_vaults::HoldReason::VaultCollateral.into(),
				&2,
			),
			5_299
		);
		assert_eq!(collateral_balance(collateral.clone(), redistribution), 1_000);
		assert_eq!(collateral_balance(collateral.clone(), 4), 1_000 + 58);
		// The owner receives all collateral not required by the liquidation: 2_928 of the 3_000
		// it pledged was seized. (Account 1 also created the market, so its balance carries the
		// minimum the seed withdrawal preserved.)
		assert_eq!(collateral_balance(collateral, 1), owner_free_before + 3_000 - 2_928);
	});
}
