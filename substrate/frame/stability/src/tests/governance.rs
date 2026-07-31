//! Branch lifecycle seeding and `set_stability_pool_config` governance.

use crate::{mock::*, types::PoolSums, Error};

fn providers(who: AccountId) -> u32 {
	System::providers(&who)
}

fn empty_deposit_row() -> crate::pallet::DepositOf<Test> {
	crate::types::Deposit::fresh(crate::types::DepositSnapshot::fresh())
}

#[test]
fn branch_registration_seeds_pool_rows() {
	build_and_execute(|| {
		assert!(crate::Pools::<Test>::get(DOT, PUSD).is_none());

		register_branch(DOT, PUSD, default_branch_config());

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.coords.p, FixedU128::one());
		assert_eq!(state.coords.epoch, 0);
		assert_eq!(state.coords.scale, 0);
		assert_eq!(state.total_collateral_gains_unclaimed, 0);
		assert_eq!(state.total_yield_unclaimed, 0);
		assert!(crate::PoolSumsStore::<Test>::contains_key((DOT, PUSD, 0u32, 0u32)));
		assert_eq!(crate::PoolSumsStore::<Test>::get((DOT, PUSD, 0u32, 0u32)), PoolSums::default());

		let config = crate::Pools::<Test>::get(DOT, PUSD)
			.expect("pool seeded on registration")
			.config;
		assert_eq!(config, default_pool_config());

		// The provider reference keeps the pool account alive without ED.
		// Registration spam is not this pallet's concern: rows only appear
		// through vaults' `create_branch`, which gates on `CreateOrigin` (the
		// stable asset's owner) and takes a held creation deposit
		// (`Consideration`), pricing the storage.
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
		assert!(crate::Pools::<Test>::get(DOT, PUSD).is_none());
		assert!(Vaults::branch_tcr(DOT, PUSD).is_err());
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
		assert!(crate::Pools::<Test>::get(DOT, PUSD).is_some());

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

		assert!(crate::Pools::<Test>::get(DOT, PUSD).is_none());
		assert!(!crate::PoolSumsStore::<Test>::contains_key((DOT, PUSD, 0u32, 0u32)));
		assert_eq!(providers(pool), providers_before - 1);
		// A never-used pool holds no dust; the zero-dust path stays silent.
		assert!(!System::events().iter().any(|record| matches!(
			record.event,
			RuntimeEvent::Stability(crate::Event::DustSwept { .. })
		)));
	});
}

#[test]
fn branch_removal_sweeps_dust_and_reregistration_starts_clean() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		let pool = Stability::pool_account(&DOT, &PUSD);

		// Two 150-deposits, then amounts indivisible by the two-way split so
		// per-row flooring strands residue in the pool aggregates.
		mint_stable(PUSD, 1, 150);
		mint_stable(PUSD, 2, 150);
		assert_ok!(deposit(1, DOT, PUSD, 150));
		assert_ok!(deposit(2, DOT, PUSD, 150));
		activate_all(&[1, 2]);
		let leftover = distribute_yield(DOT, PUSD, 101);
		assert_eq!(leftover.peek(), 0);
		drop(leftover);
		let (debt_cancelled, remainder) = simulate_offset(DOT, PUSD, 100, 101);
		assert_eq!(debt_cancelled, 100);
		assert_eq!(remainder, 0);

		// Empty every depositor row: full withdrawal plus both claims prune it.
		for who in [1u128, 2u128] {
			assert_ok!(withdraw(who, DOT, PUSD, 1_000, who));
			assert_ok!(claim_collateral(who, DOT, PUSD, who));
			assert_ok!(claim_yield(who, DOT, PUSD, who));
			assert!(deposit_row(DOT, PUSD, who).is_none());
		}

		// The exact residue is accumulator flooring detail; the property under
		// test is conservation, so snapshot it and require the scenario to
		// have actually stranded value on both assets.
		let stable_dust = stable_balance(PUSD, pool);
		let collateral_dust = collateral_balance(DOT, pool);
		assert!(stable_dust > 0);
		assert!(collateral_dust > 0);

		assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), DOT, PUSD));

		// The sweep moved every stranded unit to the dust destination.
		assert_eq!(stable_balance(PUSD, pool), 0);
		assert_eq!(collateral_balance(DOT, pool), 0);
		assert_eq!(stable_balance(PUSD, DUST_DEST), stable_dust);
		assert_eq!(collateral_balance(DOT, DUST_DEST), collateral_dust);
		System::assert_has_event(
			crate::Event::DustSwept {
				collateral_id: DOT,
				stable_id: PUSD,
				stable_amount: stable_dust,
				collateral_amount: collateral_dust,
			}
			.into(),
		);

		// Re-registering the same pair starts from a genuinely clean slate:
		// zero tracked totals against a zero-balance pool account, so the
		// balance↔totals equality (try_state, run on exit) holds again.
		register_branch(DOT, PUSD, default_branch_config());
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.total_yield_unclaimed, 0);
		assert_eq!(state.total_collateral_gains_unclaimed, 0);
		assert_eq!(stable_balance(PUSD, pool), 0);
		assert_eq!(collateral_balance(DOT, pool), 0);
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
		let stored = crate::Pools::<Test>::get(DOT, PUSD).expect("stored").config;
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
			Error::<Test>::PoolNotRegistered
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
		config.precision.set_scale_factor(100_000_000);
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

		let stored = crate::Pools::<Test>::get(DOT, PUSD).expect("stored").config;
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
		// Distinct from vaults' sub-account for the same market — the
		// redistribution account, which parks the collateral and debt shares
		// a liquidation redistributes onto surviving vaults. The two pallets
		// must never share a balance.
		assert_ne!(dot_pusd, Vaults::redistribution_account(&DOT, &PUSD));
	});
}
