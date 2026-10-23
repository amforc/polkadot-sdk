//! What branch registration seeds, what teardown removes, and what governance may change
//! afterwards.

use crate::{mock::*, Error};
use frame::traits::{fungibles::Inspect as _, tokens::Provenance};

/// The accumulator rows and the cohort checkpoints stored under the default branch, on either
/// leg and at any coordinates.
fn default_market_rows() -> (usize, usize) {
	(
		crate::PoolSumsStore::<Test>::iter_prefix((DOT, STABLE)).count(),
		crate::CohortCheckpoints::<Test>::iter_prefix((DOT, STABLE)).count(),
	)
}

#[test]
fn branch_registration_seeds_rows_and_removal_tears_them_down() {
	ExtBuilder::default().build_and_execute(|| {
		assert!(crate::Pools::<Test>::get(DOT, STABLE).is_none());

		register_branch(DOT, STABLE, default_branch_config());

		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.coords, Accumulators { p: FixedU128::one(), epoch: 0, scale: 0 });
		assert_eq!(state.total_collateral_gains_unclaimed, 0);
		assert_eq!(state.total_yield_unclaimed, 0);
		assert!(crate::PoolSumsStore::<Test>::contains_key((DOT, STABLE, Leg::Active, 0u32, 0u32)));
		assert_eq!(active_sums(0, 0), PoolSums::default());

		let config = crate::Pools::<Test>::get(DOT, STABLE)
			.expect("pool seeded on registration")
			.config;
		assert_eq!(config, default_pool_config());

		// Registration seeded one sums row per leg.
		assert_eq!(default_market_rows(), (2, 0));
		// The provider reference keeps the pool account alive without ED, and the float is a
		// second provider. Registration spam is not this pallet's concern: rows only appear
		// through vaults' `create_branch`, which gates on `CreateOrigin` (the stable asset's
		// owner) and takes a held creation deposit (`Consideration`), pricing the storage.
		let pool = Stability::pool_account(&DOT, &STABLE);
		assert_eq!(System::providers(&pool), 2);

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
		stability_events_since_last_call();

		assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), DOT, STABLE));

		assert!(crate::Pools::<Test>::get(DOT, STABLE).is_none());
		assert_eq!(default_market_rows(), (0, 0));
		// The refund took the float's provider with it, and the release took the pallet's.
		assert_eq!(System::providers(&pool), 0);
		// A never-used pool holds no dust; the zero-dust path stays silent.
		assert_eq!(stability_events_since_last_call(), vec![]);
	});
}

/// The pool's custody float is charged to the same account Vaults charges for the branch's
/// collateral custody: the depositor when a signed creation paid one, and the full admin
/// otherwise. Removal returns it to that account.
fn assert_custody_float_follows(origin: RuntimeOrigin, funder: AccountId) {
	ExtBuilder::default().build_and_execute(|| {
		// The funder also seeds the branch's redistribution custody. Both withdrawals run under
		// `Preserve`, so the funder needs three collateral minimums and two stablecoin minimums
		// on hand.
		mint_collateral(HIGH_MIN, funder, 3_000);
		mint_stable(STABLE, funder, 2);
		set_price(HIGH_MIN, REGISTRATION_PRICE);
		let pool = Stability::pool_account(&HIGH_MIN, &STABLE);

		let collateral_before = collateral_balance(HIGH_MIN, funder);
		let stable_before = stable_balance(STABLE, funder);

		assert_ok!(create_branch_as(origin, HIGH_MIN));

		let registered = crate::Pools::<Test>::get(&HIGH_MIN, STABLE).expect("pool registered");
		assert_eq!(registered.custody_stable, 1);
		assert_eq!(registered.custody_collateral, 1_000);
		assert_eq!(collateral_balance(HIGH_MIN, pool), 1_000);
		assert_eq!(stable_balance(STABLE, pool), 1);
		// Vaults' redistribution seed is the other 1_000 the funder paid.
		assert_eq!(collateral_balance(HIGH_MIN, funder), collateral_before - 2_000);
		assert_eq!(stable_balance(STABLE, funder), stable_before - 1);
		assert_ok!(Assets::can_deposit(HIGH_MIN_ID, &pool, 1, Provenance::Extant).into_result());

		assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), HIGH_MIN, STABLE));

		assert_eq!(collateral_balance(HIGH_MIN, pool), 0);
		assert_eq!(stable_balance(STABLE, pool), 0);
		assert_eq!(collateral_balance(HIGH_MIN, funder), collateral_before);
		assert_eq!(stable_balance(STABLE, funder), stable_before);
		// The float went back whole, so no dust was swept.
		assert_eq!(stability_events_since_last_call(), vec![]);
	});
}

// Governance can register an issued-collateral branch too.
#[test]
fn custody_float_of_a_governance_branch_is_charged_to_the_admin() {
	assert_custody_float_follows(RuntimeOrigin::root(), ADMIN);
}

#[test]
fn custody_float_of_a_signed_branch_is_charged_to_its_creator() {
	assert_custody_float_follows(RuntimeOrigin::signed(1), 1);
}

#[test]
fn branch_registration_error_paths() {
	ExtBuilder::default().build_and_execute(|| {
		hypothetically!({
			let bad = StabilityPoolConfig { minimum_deposit: 0, ..default_pool_config() };
			set_price(DOT, REGISTRATION_PRICE);

			// The whole registration rolls back, vaults side included.
			assert_noop!(
				Vaults::create_branch(
					RuntimeOrigin::root(),
					DOT,
					STABLE,
					branch_admins(ADMIN, EMERGENCY_ADMIN),
					default_branch_config(),
					(Some(default_redemption_config()), bad),
				),
				Error::<Test>::InvalidStabilityPoolConfig
			);
		});

		hypothetically!({
			set_price(HIGH_MIN, REGISTRATION_PRICE);
			// Enough for Vaults' redistribution seed, but the pool float would dust the payer.
			mint_collateral(HIGH_MIN, 1, 2_000);
			mint_stable(STABLE, 1, 2);

			assert_noop!(
				create_branch_as(RuntimeOrigin::signed(1), HIGH_MIN),
				Error::<Test>::CustodySeedUnavailable
			);
		});

		hypothetically!({
			// Whoever creates the branch funds its redistribution seed: the depositor when there
			// is one, the full admin otherwise. The withdrawal preserves the payer, so it needs two
			// minimum balances on hand.
			mint_collateral(HIGH_MIN, 1, 2_000);
			let pool = Stability::pool_account(&HIGH_MIN, &STABLE);
			mint_collateral(HIGH_MIN, pool, 1_000);
			set_price(HIGH_MIN, REGISTRATION_PRICE);

			assert_noop!(
				create_branch_as(RuntimeOrigin::signed(1), HIGH_MIN),
				Error::<Test>::PoolAccountNotEmpty
			);
		});
	});
}

#[test]
fn branch_removal_blocked_while_depositor_rows_exist() {
	ExtBuilder::default()
		.default_branch()
		.matured_deposit(5, 400)
		.build_and_execute(|| {
			assert_noop!(
				Vaults::remove_branch(RuntimeOrigin::root(), DOT, STABLE),
				Error::<Test>::PoolNotEmpty
			);

			// The full withdrawal prunes the only row, which unblocks the removal.
			assert_ok!(withdraw(5, DOT, STABLE, 400, 5));
			assert!(deposit_row(DOT, STABLE, 5).is_none());
			assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), DOT, STABLE));
		});
}

#[test]
fn branch_removal_sweeps_dust_and_reregistration_starts_clean() {
	let unit: Balance = 10_000_000_000_000; // 1e13
	ExtBuilder::default()
		.default_branch()
		.min_active_pool(10)
		.matured_deposit(3, unit)
		.build_and_execute(|| {
			// A first depositor takes the active leg across a scale and then an epoch, so that
			// the branch holds rows beyond the two that registration seeds. Leaving 100 of 1e13
			// crosses one scale: P = floor(1e18 * 1e9 * 100 / 1e13) = 0.01. Burning those 100
			// depletes the pool and opens epoch 1.
			assert_eq!(
				simulate_offset(DOT, STABLE, unit - 100, 5_000_000_000_000).debt(),
				unit - 100
			);
			assert_eq!(simulate_offset(DOT, STABLE, 100, 80).debt(), 100);
			assert_eq!(
				pool_state(DOT, STABLE).coords,
				Accumulators { p: FixedU128::one(), epoch: 1, scale: 0 }
			);
			// delta_S(0,0) = 5e12 / 1e13 = 0.5 and delta_S(0,1) = 80 * (0.01/100) = 8e-3, which the
			// row one scale ahead pays through the divisor: 1e13 * (0.5 + 8e-3/1e9) = 5e12 + 80.
			// Nothing of the deposit survives the depletion, so the claim prunes the row.
			assert_claim_collateral(3, 5_000_000_000_080);
			assert!(deposit_row(DOT, STABLE, 3).is_none());

			// Two 150-deposits, then amounts indivisible by the two-way split so
			// per-row flooring strands residue in the pool aggregates.
			mint_stable(STABLE, 1, 150);
			mint_stable(STABLE, 2, 150);
			assert_ok!(deposit_and_mature(1, DOT, STABLE, 150));
			assert_ok!(deposit_and_mature(2, DOT, STABLE, 150));
			assert_eq!(distribute_yield_leftover(DOT, STABLE, 101), 0);
			let offset = simulate_offset(DOT, STABLE, 100, 101);
			assert_eq!(offset.debt(), 100);
			assert_eq!(offset.leftover, 0);

			// Empty every depositor row: full withdrawal plus both claims prune it.
			for who in [1u128, 2u128] {
				assert_ok!(withdraw(who, DOT, STABLE, 1_000, who));
				assert_ok!(claim_collateral(who, DOT, STABLE, who));
				assert_ok!(claim_yield(who, DOT, STABLE, who));
				assert!(deposit_row(DOT, STABLE, who).is_none());
			}

			// The exact residue is accumulator flooring detail; the property under
			// test is conservation, so snapshot it and require the scenario to
			// have actually stranded value on both assets.
			let stable_dust = pool_stable_held(DOT, STABLE);
			let collateral_dust = pool_collateral_held(DOT, STABLE);
			assert!(stable_dust > 0);
			assert!(collateral_dust > 0);

			// Active rows at (0, 0), (0, 1) and (1, 0), and the pending row at (0, 0). Every
			// checkpoint went with the last settlement of its cohort.
			assert_eq!(default_market_rows(), (4, 0));

			stability_events_since_last_call();

			assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), DOT, STABLE));

			assert_eq!(default_market_rows(), (0, 0));

			// The sweep moved every stranded unit to the dust destination, and the float went back
			// to the funder rather than with it.
			let pool = Stability::pool_account(&DOT, &STABLE);
			assert_eq!(stable_balance(STABLE, pool), 0);
			assert_eq!(collateral_balance(DOT, pool), 0);
			assert_eq!(stable_balance(STABLE, DUST_DEST), stable_dust);
			assert_eq!(collateral_balance(DOT, DUST_DEST), collateral_dust);
			assert_eq!(
				stability_events_since_last_call(),
				vec![crate::Event::DustSwept {
					collateral_id: DOT,
					stable_id: STABLE,
					stable_amount: stable_dust,
					collateral_amount: collateral_dust,
				}]
			);

			// Re-registering the same pair starts from a genuinely clean slate:
			// zero tracked totals against a pool account holding only its float, so the
			// balance↔totals equality (try_state, run on exit) holds again.
			register_branch(DOT, STABLE, default_branch_config());

			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_active_deposits, 0);
			assert_eq!(state.total_pending_deposits, 0);
			assert_eq!(state.total_yield_unclaimed, 0);
			assert_eq!(state.total_collateral_gains_unclaimed, 0);
			assert_eq!(pool_stable_held(DOT, STABLE), 0);
			assert_eq!(pool_collateral_held(DOT, STABLE), 0);
			// Only the two seeded rows exist, both empty. A row left at (0, 1) would sit one scale
			// ahead of the first snapshot of the new branch and pay it gains it never earned.
			assert_eq!(default_market_rows(), (2, 0));
			assert_eq!(active_sums(0, 0), PoolSums::default());
			assert_eq!(pending_sums(0, 0), PoolSums::default());
		});
}

#[test]
fn set_stability_pool_config_origin_validation_and_storage() {
	ExtBuilder::default().default_branch().build_and_execute(|| {
		let set_config = |origin: RuntimeOrigin, config: StabilityPoolConfig<Balance>| {
			Stability::set_stability_pool_config(origin, DOT, STABLE, config)
		};
		let mut config = default_pool_config();
		config.minimum_deposit = 250;

		// A stranger is neither governance nor the stored full admin.
		assert_noop!(set_config(RuntimeOrigin::signed(7), config.clone()), BadOrigin);

		// The branch's full admin may update its pool config.
		assert_ok!(set_config(RuntimeOrigin::signed(ADMIN), config.clone()));

		config.minimum_deposit = 300;
		config.entry_delay = 10_000;
		stability_events_since_last_call();

		// Root is the governance override. The whole config is stored as given.
		assert_ok!(set_config(RuntimeOrigin::root(), config.clone()));

		let stored = crate::Pools::<Test>::get(DOT, STABLE).expect("stored").config;
		assert_eq!(stored, config);
		assert_eq!(
			stability_events_since_last_call(),
			vec![crate::Event::StabilityPoolConfigUpdated {
				collateral_id: DOT,
				stable_id: STABLE,
			}]
		);

		let config =
			StabilityPoolConfig { minimum_active_pool_balance: 0, ..default_pool_config() };
		assert_noop!(
			set_config(RuntimeOrigin::root(), config),
			Error::<Test>::InvalidStabilityPoolConfig
		);

		// Still a valid config on its own (0.5e-9 * 1e9 = 0.5 <= 1), so it
		// reaches the immutability check rather than failing validation.
		let mut config = default_pool_config();
		config.precision.p_min = FixedU128::from_inner(500_000_000);
		assert_noop!(
			set_config(RuntimeOrigin::root(), config),
			Error::<Test>::AccumulatorParamsImmutable
		);

		// Same for the scale factor (1e8 is inside the validity bounds).
		let mut config = default_pool_config();
		config.precision.scale_factor = 100_000_000;
		assert_noop!(
			set_config(RuntimeOrigin::root(), config),
			Error::<Test>::AccumulatorParamsImmutable
		);
	});
}

#[test]
fn pool_accounts_are_distinct_across_markets_and_pallets() {
	ExtBuilder::default().build_and_execute(|| {
		let dot_stable = Stability::pool_account(&DOT, &STABLE);
		let token_usdx = Stability::pool_account(&TOKEN_X, &USDX);
		let dot_usdx = Stability::pool_account(&DOT, &USDX);

		assert_ne!(dot_stable, token_usdx);
		assert_ne!(dot_stable, dot_usdx);
		assert_ne!(token_usdx, dot_usdx);
		// Distinct from vaults' sub-account for the same branch — the
		// redistribution account, which parks the collateral and debt shares
		// a liquidation redistributes onto surviving vaults. The two pallets
		// must never share a balance.
		assert_ne!(dot_stable, Vaults::redistribution_account(&DOT, &STABLE));
	});
}
