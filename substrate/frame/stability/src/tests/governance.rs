//! Branch lifecycle seeding and `set_stability_pool_config` governance.

use crate::{mock::*, types::PoolSums, Error};
use frame::traits::Get;

fn providers(who: AccountId) -> u32 {
	System::providers(&who)
}

fn empty_deposit_row() -> crate::pallet::DepositOf<Test> {
	crate::types::Deposit::fresh(crate::types::DepositSnapshot::fresh())
}

#[test]
fn branch_registration_seeds_pool_rows() {
	build_and_execute(|| {
		assert!(crate::PoolStates::<Test>::get(DOT, PUSD).is_none());

		register_branch(DOT, PUSD, default_branch_config());

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.p, FixedU128::one());
		assert_eq!(state.epoch, 0);
		assert_eq!(state.scale, 0);
		assert_eq!(state.total_collateral_gains_unclaimed, 0);
		assert_eq!(state.total_yield_unclaimed, 0);

		let sums = crate::PoolSumsStore::<Test>::get((DOT, PUSD, 0u32, 0u32))
			.expect("epoch 0 / scale 0 sums row seeded on registration");
		assert_eq!(sums, PoolSums::default());

		let config = crate::StabilityPoolConfigs::<Test>::get(DOT, PUSD)
			.expect("config seeded on registration");
		assert_eq!(config, default_pool_config());

		// The provider reference keeps the pool account alive without ED.
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert!(providers(pool) >= 1);
	});
}

#[test]
fn branch_registration_rejects_invalid_default_config() {
	build_and_execute(|| {
		let mut bad = default_pool_config();
		bad.minimum_deposit = 0;
		DefaultStabilityPoolConfig::set(bad);

		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		assert_noop!(
			Vaults::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
			),
			Error::<Test>::InvalidStabilityPoolConfig
		);
		// The whole registration rolled back, vaults side included.
		assert!(crate::PoolStates::<Test>::get(DOT, PUSD).is_none());
		assert!(crate::StabilityPoolConfigs::<Test>::get(DOT, PUSD).is_none());
		assert!(Vaults::branch_tcr(DOT, PUSD).is_none());
	});
}

#[test]
fn branch_removal_blocked_while_depositor_rows_exist() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		crate::Deposits::<Test>::insert((DOT, PUSD, 5u128), empty_deposit_row());

		assert_noop!(
			Vaults::remove_branch(RuntimeOrigin::root(), DOT, PUSD),
			Error::<Test>::PoolNotEmpty
		);
		assert!(crate::PoolStates::<Test>::get(DOT, PUSD).is_some());

		crate::Deposits::<Test>::remove((DOT, PUSD, 5u128));
		assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), DOT, PUSD));
	});
}

#[test]
fn branch_removal_tears_down_pool_rows() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		let pool = Stability::pool_account(&DOT, &PUSD);
		let providers_before = providers(pool);

		assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), DOT, PUSD));

		assert!(crate::PoolStates::<Test>::get(DOT, PUSD).is_none());
		assert!(crate::StabilityPoolConfigs::<Test>::get(DOT, PUSD).is_none());
		assert!(!crate::PoolSumsStore::<Test>::contains_key((DOT, PUSD, 0u32, 0u32)));
		assert_eq!(providers(pool), providers_before - 1);
	});
}

#[test]
fn set_stability_pool_config_origin_matrix() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		let mut config = default_pool_config();
		config.minimum_deposit = 250;

		// A stranger is neither governance nor the stored full admin.
		assert_noop!(
			Stability::set_stability_pool_config(
				RuntimeOrigin::signed(7),
				DOT,
				PUSD,
				config.clone()
			),
			BadOrigin
		);

		// The market's full admin may update its pool config.
		assert_ok!(Stability::set_stability_pool_config(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			config.clone()
		));

		// Root is the governance override.
		config.minimum_deposit = 300;
		assert_ok!(Stability::set_stability_pool_config(RuntimeOrigin::root(), DOT, PUSD, config));
		let stored = crate::StabilityPoolConfigs::<Test>::get(DOT, PUSD).expect("stored");
		assert_eq!(stored.minimum_deposit, 300);
	});
}

#[test]
fn set_stability_pool_config_requires_registered_branch() {
	build_and_execute(|| {
		assert_noop!(
			Stability::set_stability_pool_config(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				default_pool_config()
			),
			Error::<Test>::BranchNotRegistered
		);
	});
}

#[test]
fn set_stability_pool_config_rejects_invalid_config() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		let mut config = default_pool_config();
		config.minimum_active_pool_balance = 0;
		assert_noop!(
			Stability::set_stability_pool_config(RuntimeOrigin::root(), DOT, PUSD, config),
			Error::<Test>::InvalidStabilityPoolConfig
		);
	});
}

#[test]
fn set_stability_pool_config_freezes_precision_parameters() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());

		// Still a valid config on its own (0.5e-9 * 1e9 = 0.5 <= 1), so it
		// reaches the immutability check rather than failing validation.
		let mut config = default_pool_config();
		config.precision.p_min = FixedU128::from_inner(500_000_000);
		assert_noop!(
			Stability::set_stability_pool_config(RuntimeOrigin::root(), DOT, PUSD, config),
			Error::<Test>::AccumulatorParamsImmutable
		);

		// Same for the scale factor (1e8 is inside the validity bounds).
		let mut config = default_pool_config();
		config.precision.scale_factor = FixedU128::from_u32(100_000_000);
		assert_noop!(
			Stability::set_stability_pool_config(RuntimeOrigin::root(), DOT, PUSD, config),
			Error::<Test>::AccumulatorParamsImmutable
		);
	});
}

#[test]
fn set_stability_pool_config_updates_and_emits() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		let mut config = default_pool_config();
		config.minimum_deposit = 250;
		config.entry_delay = 10_000;

		assert_ok!(Stability::set_stability_pool_config(
			RuntimeOrigin::root(),
			DOT,
			PUSD,
			config.clone()
		));

		let stored = crate::StabilityPoolConfigs::<Test>::get(DOT, PUSD).expect("stored");
		assert_eq!(stored, config);
		System::assert_last_event(
			crate::Event::StabilityPoolConfigUpdated { collateral_id: DOT, stable_id: PUSD }.into(),
		);
	});
}

#[test]
fn pool_accounts_are_distinct_across_markets_and_pallets() {
	build_and_execute(|| {
		let dot_pusd = Stability::pool_account(&DOT, &PUSD);
		let token_usdx = Stability::pool_account(&TOKEN_X, &USDX);
		let dot_usdx = Stability::pool_account(&DOT, &USDX);
		assert_ne!(dot_pusd, token_usdx);
		assert_ne!(dot_pusd, dot_usdx);
		assert_ne!(token_usdx, dot_usdx);
		// Distinct from vaults' sub-account for the same market: the two
		// pallets must never share a balance.
		assert_ne!(dot_pusd, Vaults::redistribution_account(&DOT, &PUSD));
	});
}

#[test]
fn default_config_matches_integrity_expectations() {
	// The same predicate `integrity_test` enforces at runtime-build time.
	let config: crate::types::StabilityPoolConfig<Balance> =
		<Test as crate::Config>::DefaultStabilityPoolConfig::get();
	assert!(config.is_valid());
	let max_iterations: u32 = <Test as crate::Config>::MaxPendingOffsetIterations::get();
	assert!(max_iterations > 0);
}
