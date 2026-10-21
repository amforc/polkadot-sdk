//! Vault liquidation against the real Stability Pool and its prepared collateral custody.

use crate::mock::*;
use frame::traits::{fungibles::Inspect as _, tokens::Provenance};
use stablecoin_primitives::{OffsetLegs, StabilityPoolInspect};

#[test]
fn vault_liquidation_uses_the_real_stability_pool() {
	build_and_execute(|| {
		register_branch(
			DOT,
			STABLE,
			pallet_vaults::BranchConfig { upfront_fee_period: 0, ..default_branch_config() },
		);
		assert_ok!(open_vault(1, DOT, STABLE, 600, 500));
		assert_ok!(open_vault(2, DOT, STABLE, 2_000, 500));
		// The deposit only matures here: the liquidation itself activates it, so no row touch
		// stands between a depositor and the offset that spends their capital.
		seed_matured_deposit(3, 500);
		set_price(DOT, FixedU128::from_rational(9u128, 10u128));

		let owner_before = collateral_balance(DOT, 1);
		let keeper_before = collateral_balance(DOT, 4);
		let pool_before = pool_collateral_held(DOT, STABLE);

		assert_ok!(liquidate(4, DOT, 1));

		assert!(pallet_vaults::pallet::Vaults::<Test>::get((DOT, STABLE, 1)).is_none());
		assert_eq!(pool_state(DOT, STABLE).total_active_deposits, 0);
		assert_eq!(pool_collateral_held(DOT, STABLE) - pool_before, 571);
		assert_eq!(collateral_balance(DOT, 4) - keeper_before, 12);
		// Terminal interest makes debt 501 and reduces the owner surplus to 14.
		assert_eq!(collateral_balance(DOT, 1) - owner_before, 14);
	});
}

#[test]
fn sub_minimum_first_gain_settles_into_seeded_pool_account() {
	build_and_execute(|| {
		create_high_min_collateral();
		register_branch(
			HIGH_MIN,
			STABLE,
			pallet_vaults::BranchConfig {
				upfront_fee_period: 0,
				..branch_config_for(HIGH_MIN, STABLE)
			},
		);
		for owner in [1, 2] {
			mint_collateral(HIGH_MIN, owner, 5_000);
			assert_ok!(open_vault(owner, HIGH_MIN, STABLE, 3_000, 500));
		}
		// The keeper's reward is itself below this asset's 1_000 minimum, so give account 4 an
		// account to receive it. The pool's gain is what this test is about.
		mint_collateral(HIGH_MIN, 4, 1_000);
		// Registration seeded custody with the asset minimum, which a hold's
		// `Protect` preservation keeps free.
		let redistribution = Vaults::redistribution_account(&HIGH_MIN, &STABLE);
		mint_stable(STABLE, 3, 100);
		assert_ok!(deposit_and_mature(3, HIGH_MIN, STABLE, 100));
		// floor(0.18 * 3_000) = 540 of value: CR 1.08 sits below the 1.10 MCR.
		set_price(HIGH_MIN, FixedU128::from_rational(9u128, 50u128));

		let owner_free_before = collateral_balance(HIGH_MIN, 1);
		let pool_account = Stability::pool_account(&HIGH_MIN, &STABLE);
		// Registration parked the asset minimum in the pool account, so even a one-unit gain can
		// enter it.
		assert_ok!(
			Assets::can_deposit(HIGH_MIN_ID, &pool_account, 1, Provenance::Extant).into_result()
		);

		// The active-pool gain is below the asset minimum to test prepared custody.
		// The seeded account accepts it, so the normal active-first waterfall remains intact.
		assert_ok!(liquidate(4, HIGH_MIN, 1));

		// The expected split includes terminal interest and both 5% penalties.
		assert!(pallet_vaults::pallet::Vaults::<Test>::get((HIGH_MIN, STABLE, 1)).is_none());
		let state = pool_state(HIGH_MIN, STABLE);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.total_collateral_gains_unclaimed, 571);
		assert_eq!(pool_stable_held(HIGH_MIN, STABLE), 0);
		assert_eq!(pool_collateral_held(HIGH_MIN, STABLE), 571);
		// Redistribution collateral remains in custody until the recipient is touched.
		assert_eq!(vault_collateral_on_hold(HIGH_MIN, redistribution), 2_299);
		assert_eq!(vault_collateral_on_hold(HIGH_MIN, 2), 3_000);
		assert_eq!(
			pallet_vaults::pallet::Vaults::<Test>::get((HIGH_MIN, STABLE, 2))
				.unwrap()
				.vault
				.collateral,
			3_000
		);
		assert_ok!(Vaults::poke(RuntimeOrigin::signed(4), HIGH_MIN, STABLE, 2));
		assert_eq!(vault_collateral_on_hold(HIGH_MIN, redistribution), 0);
		assert_eq!(vault_collateral_on_hold(HIGH_MIN, 2), 5_299);
		assert_eq!(collateral_balance(HIGH_MIN, redistribution), 1_000);
		assert_eq!(collateral_balance(HIGH_MIN, 4), 1_000 + 58);
		// The owner receives all collateral not required by the liquidation: 2_928 of the 3_000
		// it pledged was seized. (Account 1 also created the market, so its balance carries the
		// minimum the seed withdrawal preserved.)
		assert_eq!(collateral_balance(HIGH_MIN, 1), owner_free_before + 3_000 - 2_928);
	});
}

/// The engine sizes its pool legs through the pool's own quotes and settles them in one offset.
///
/// One liquidation that reaches both pool legs and redistribution runs through `Vaults::liquidate`.
/// From the same state, the quotes must size the same legs, and a single `offset` of the legs and
/// collateral the liquidation reported must leave the pool exactly where the liquidation did.
#[test]
fn liquidation_pool_legs_match_the_quotes_and_one_offset() {
	let setup = || {
		register_branch(
			DOT,
			STABLE,
			pallet_vaults::BranchConfig { upfront_fee_period: 0, ..default_branch_config() },
		);
		assert_ok!(open_vault(1, DOT, STABLE, 600, 500));
		assert_ok!(open_vault(2, DOT, STABLE, 2_000, 500));
		seed_matured_deposit(3, 300);
		seed_deposit(5, 150);
		set_price(DOT, FixedU128::from_rational(9u128, 10u128));
	};
	let pool_view = || {
		(
			pool_state(DOT, STABLE),
			active_sums(0, 0),
			pending_sums(0, 0),
			pool_stable_held(DOT, STABLE),
			pool_collateral_held(DOT, STABLE),
		)
	};

	let mut real = None;
	build_and_execute(|| {
		setup();
		assert_ok!(liquidate(4, DOT, 1));
		let outcome = System::events()
			.into_iter()
			.find_map(|record| match record.event {
				RuntimeEvent::Vaults(pallet_vaults::Event::VaultLiquidated { outcome, .. }) => {
					Some(outcome)
				},
				_ => None,
			})
			.expect("the liquidation reports its outcome");
		real = Some((outcome, pool_view()));
	});
	let (outcome, real_pool) = real.expect("the real liquidation ran");
	// The scenario is only worth comparing if it reaches both pool legs and leaves debt over.
	assert_eq!(outcome.keeper_jit.debt, 0);
	assert!(outcome.active_pool.debt > 0);
	assert!(outcome.pending_pool.debt > 0);
	assert!(outcome.redistribution.debt > 0);

	build_and_execute(|| {
		setup();
		let (active, pending) = (outcome.active_pool, outcome.pending_pool);
		let debt = active.debt + pending.debt + outcome.redistribution.debt;
		let quote = market_quote(&DOT, &STABLE);
		assert_eq!(Stability::quote_active(&quote, debt), active.debt);
		assert_eq!(Stability::quote_pending(&quote, debt - active.debt, active.debt), pending.debt);

		let burned = offset_with(
			(DOT, STABLE),
			branch_snapshot(&DOT, &STABLE),
			OffsetLegs { active: active.debt, pending: pending.debt },
			OffsetLegs { active: (DOT, active.collateral), pending: (DOT, pending.collateral) },
		)
		.expect("the reported legs settle");
		assert_eq!(burned.map_or(0, |credit| credit.peek()), active.debt + pending.debt);
		assert_eq!(pool_view(), real_pool);
	});
}
