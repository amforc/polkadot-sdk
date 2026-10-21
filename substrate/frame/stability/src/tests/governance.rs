//! What market registration seeds, what teardown removes, and what governance may change
//! afterwards.

use crate::{mock::*, Error};
use frame::traits::{fungibles::Inspect as _, tokens::Provenance};

fn providers(who: AccountId) -> u32 {
	System::providers(&who)
}

/// The accumulator rows and the cohort checkpoints stored under the default market, on either
/// leg and at any coordinates.
fn default_market_rows() -> (usize, usize) {
	(
		crate::PoolSumsStore::<Test>::iter_prefix((DOT, PUSD)).count(),
		crate::CohortCheckpoints::<Test>::iter_prefix((DOT, PUSD)).count(),
	)
}

#[test]
fn branch_registration_seeds_pool_rows() {
	build_and_execute(|| {
		assert!(crate::Pools::<Test>::get(DOT, PUSD).is_none());

		register_branch(DOT, PUSD, default_branch_config());

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.coords, Accumulators { p: FixedU128::one(), epoch: 0, scale: 0 });
		assert_eq!(state.total_collateral_gains_unclaimed, 0);
		assert_eq!(state.total_yield_unclaimed, 0);
		assert!(crate::PoolSumsStore::<Test>::contains_key((DOT, PUSD, Leg::Active, 0u32, 0u32)));
		assert_eq!(active_sums(0, 0), PoolSums::default());

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

		// Native collateral has no asset account to touch: `can_deposit(1)` fails against a
		// production ED and `Balances::touch` is a no-op. The provider reference is what lets the
		// account receive its own minimum.
		assert_ok!(PoolCollateralAssets::can_deposit(
			DOT,
			&pool,
			PoolCollateralAssets::minimum_balance(DOT),
			Provenance::Extant,
		)
		.into_result());
	});
}

// The pool's custody float is charged to the same account Vaults charges for the market's
// collateral custody: the depositor when a signed creation paid one, and the full admin
// otherwise. Removal returns it to that account. Governance can therefore register an
// issued-collateral market too.
#[test]
fn custody_float_follows_the_custody_funder() {
	build_and_execute(|| {
		create_high_min_collateral();
		// The funder also seeds the market's redistribution custody. Both withdrawals run under
		// `Preserve`, so each candidate needs three collateral minimums and two stablecoin
		// minimums on hand.
		for funder in [1, ADMIN] {
			mint_collateral(HIGH_MIN, funder, 3_000);
			mint_stable(PUSD, funder, 2);
		}
		set_price(HIGH_MIN, REGISTRATION_PRICE);
		let pool = Stability::pool_account(&HIGH_MIN, &PUSD);

		for (origin, funder) in [(RuntimeOrigin::root(), ADMIN), (RuntimeOrigin::signed(1), 1)] {
			let collateral_before = collateral_balance(HIGH_MIN, funder);
			let stable_before = stable_balance(PUSD, funder);
			assert_ok!(create_branch_as(origin, HIGH_MIN));
			let registered = crate::Pools::<Test>::get(&HIGH_MIN, PUSD).expect("pool registered");
			assert_eq!(registered.custody_stable, 1);
			assert_eq!(registered.custody_collateral, 1_000);
			assert_eq!(collateral_balance(HIGH_MIN, pool), 1_000);
			assert_eq!(stable_balance(PUSD, pool), 1);
			// Vaults' redistribution seed is the other 1_000 the funder paid.
			assert_eq!(collateral_balance(HIGH_MIN, funder), collateral_before - 2_000);
			assert_eq!(stable_balance(PUSD, funder), stable_before - 1);
			assert_ok!(Assets::can_deposit(HIGH_MIN_ID, &pool, 1, Provenance::Extant).into_result());

			assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), HIGH_MIN, PUSD));
			assert_eq!(collateral_balance(HIGH_MIN, pool), 0);
			assert_eq!(stable_balance(PUSD, pool), 0);
			assert_eq!(collateral_balance(HIGH_MIN, funder), collateral_before);
			assert_eq!(stable_balance(PUSD, funder), stable_before);
		}
	});
}

#[test]
fn branch_registration_needs_the_funder_to_cover_the_custody_float() {
	build_and_execute(|| {
		create_high_min_collateral();
		set_price(HIGH_MIN, REGISTRATION_PRICE);
		// Enough for Vaults' redistribution seed, but the pool float would dust the payer.
		mint_collateral(HIGH_MIN, 1, 2_000);
		mint_stable(PUSD, 1, 2);
		assert_noop!(
			create_branch_as(RuntimeOrigin::signed(1), HIGH_MIN),
			Error::<Test>::CustodySeedUnavailable
		);
		assert!(crate::Pools::<Test>::get(&HIGH_MIN, PUSD).is_none());
	});
}

#[test]
fn branch_registration_rejects_prefunded_pool_account() {
	build_and_execute(|| {
		create_high_min_collateral();
		// Whoever creates the market funds its redistribution seed: the depositor when there is
		// one, the full admin otherwise. The withdrawal preserves the payer, so both need two
		// minimum balances on hand.
		mint_collateral(HIGH_MIN, 1, 2_000);
		mint_collateral(HIGH_MIN, ADMIN, 2_000);
		let pool = Stability::pool_account(&HIGH_MIN, &PUSD);
		mint_collateral(HIGH_MIN, pool, 1_000);
		set_price(HIGH_MIN, REGISTRATION_PRICE);

		assert_noop!(
			create_branch_as(RuntimeOrigin::signed(1), HIGH_MIN),
			Error::<Test>::PoolAccountNotEmpty
		);
		assert!(crate::Pools::<Test>::get(HIGH_MIN, PUSD).is_none());
		assert_eq!(collateral_balance(HIGH_MIN, pool), 1_000);
	});
}

#[test]
fn branch_registration_rejects_invalid_pool_config() {
	build_and_execute(|| {
		let bad = StabilityPoolConfig { minimum_deposit: 0, ..default_pool_config() };

		set_price(DOT, REGISTRATION_PRICE);
		assert_noop!(
			Vaults::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(Some(default_redemption_config()), bad),
			),
			Error::<Test>::InvalidStabilityPoolConfig
		);
		// The whole registration rolled back, vaults side included.
		assert_err!(Vaults::branch_tcr(DOT, PUSD), pallet_vaults::Error::<Test>::BranchNotFound);
	});
}

#[test]
fn branch_removal_blocked_while_depositor_rows_exist() {
	build_with_default_market(|| {
		seed_matured_deposit(5, 400);

		assert_noop!(
			Vaults::remove_branch(RuntimeOrigin::root(), DOT, PUSD),
			Error::<Test>::PoolNotEmpty
		);

		// The full withdrawal prunes the only row, which unblocks the removal.
		assert_ok!(withdraw(5, DOT, PUSD, 400, 5));
		assert!(deposit_row(DOT, PUSD, 5).is_none());
		assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), DOT, PUSD));
	});
}

#[test]
fn branch_removal_tears_down_pool_rows() {
	build_with_default_market(|| {
		let pool = Stability::pool_account(&DOT, &PUSD);
		// Registration seeded one sums row per leg, and the float is a second provider.
		assert_eq!(default_market_rows(), (2, 0));
		assert_eq!(providers(pool), 2);

		assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), DOT, PUSD));

		assert!(crate::Pools::<Test>::get(DOT, PUSD).is_none());
		assert_eq!(default_market_rows(), (0, 0));
		// The refund took the float's provider with it, and the release took the pallet's.
		assert_eq!(providers(pool), 0);
		// A never-used pool holds no dust; the zero-dust path stays silent.
		assert!(!System::events().iter().any(|record| matches!(
			record.event,
			RuntimeEvent::Stability(crate::Event::DustSwept { .. })
		)));
	});
}

#[test]
fn branch_removal_sweeps_dust_and_reregistration_starts_clean() {
	build_with_default_market(|| {
		// A first depositor takes the active leg across a scale and then an epoch, so that
		// the market holds rows beyond the two that registration seeds. Leaving 100 of 1e13
		// crosses one scale: P = floor(1e18 * 1e9 * 100 / 1e13) = 0.01. Burning those 100
		// depletes the pool and opens epoch 1.
		set_min_active_pool(10);
		let unit: Balance = 10_000_000_000_000; // 1e13
		seed_matured_deposit(3, unit);
		assert_eq!(simulate_offset(DOT, PUSD, unit - 100, 5_000_000_000_000).debt(), unit - 100);
		assert_eq!(simulate_offset(DOT, PUSD, 100, 80).debt(), 100);
		assert_eq!(
			pool_state(DOT, PUSD).coords,
			Accumulators { p: FixedU128::one(), epoch: 1, scale: 0 }
		);
		// delta_S(0,0) = 5e12 / 1e13 = 0.5 and delta_S(0,1) = 80 * (0.01/100) = 8e-3, which the
		// row one scale ahead pays through the divisor: 1e13 * (0.5 + 8e-3/1e9) = 5e12 + 80.
		// Nothing of the deposit survives the depletion, so the claim prunes the row.
		assert_claim_collateral(3, 5_000_000_000_080);
		assert!(deposit_row(DOT, PUSD, 3).is_none());

		// Two 150-deposits, then amounts indivisible by the two-way split so
		// per-row flooring strands residue in the pool aggregates.
		mint_stable(PUSD, 1, 150);
		mint_stable(PUSD, 2, 150);
		assert_ok!(deposit_and_mature(1, DOT, PUSD, 150));
		assert_ok!(deposit_and_mature(2, DOT, PUSD, 150));
		assert_eq!(distribute_yield_leftover(DOT, PUSD, 101), 0);
		let offset = simulate_offset(DOT, PUSD, 100, 101);
		assert_eq!(offset.debt(), 100);
		assert_eq!(offset.leftover, 0);

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
		let stable_dust = pool_stable_held(DOT, PUSD);
		let collateral_dust = pool_collateral_held(DOT, PUSD);
		assert!(stable_dust > 0);
		assert!(collateral_dust > 0);

		// Active rows at (0, 0), (0, 1) and (1, 0), and the pending row at (0, 0). Every
		// checkpoint went with the last settlement of its cohort.
		assert_eq!(default_market_rows(), (4, 0));

		assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), DOT, PUSD));
		assert_eq!(default_market_rows(), (0, 0));

		// The sweep moved every stranded unit to the dust destination, and the float went back
		// to the funder rather than with it.
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 0);
		assert_eq!(collateral_balance(DOT, pool), 0);
		assert_eq!(stable_balance(PUSD, DUST_DEST), stable_dust);
		assert_eq!(collateral_balance(DOT, DUST_DEST), collateral_dust);
		System::assert_has_event(market_event!(DustSwept {
			stable_amount: stable_dust,
			collateral_amount: collateral_dust,
		}));

		// Re-registering the same pair starts from a genuinely clean slate:
		// zero tracked totals against a pool account holding only its float, so the
		// balance↔totals equality (try_state, run on exit) holds again.
		register_branch(DOT, PUSD, default_branch_config());
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.total_yield_unclaimed, 0);
		assert_eq!(state.total_collateral_gains_unclaimed, 0);
		assert_eq!(pool_stable_held(DOT, PUSD), 0);
		assert_eq!(pool_collateral_held(DOT, PUSD), 0);
		// Only the two seeded rows exist, both empty. A row left at (0, 1) would sit one scale
		// ahead of the first snapshot of the new market and pay it gains it never earned.
		assert_eq!(default_market_rows(), (2, 0));
		assert_eq!(active_sums(0, 0), PoolSums::default());
		assert_eq!(pending_sums(0, 0), PoolSums::default());
	});
}

#[test]
fn set_stability_pool_config_checks_the_origin_then_stores_and_emits() {
	build_with_default_market(|| {
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

		// Root is the governance override. The whole config is stored as given.
		config.minimum_deposit = 300;
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
fn set_stability_pool_config_validates_and_freezes_precision_parameters() {
	build_with_default_market(|| {
		let config =
			StabilityPoolConfig { minimum_active_pool_balance: 0, ..default_pool_config() };
		assert_noop!(
			Stability::set_stability_pool_config(RuntimeOrigin::root(), DOT, PUSD, config),
			Error::<Test>::InvalidStabilityPoolConfig
		);

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
		config.precision.scale_factor = 100_000_000;
		assert_noop!(
			Stability::set_stability_pool_config(RuntimeOrigin::root(), DOT, PUSD, config),
			Error::<Test>::AccumulatorParamsImmutable
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
