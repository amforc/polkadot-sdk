//! Epoch bumps and scale crossings: the product-sum coordinates under full
//! depletion and extreme partial offsets.

use crate::mock::*;

/// Deposit and immediately activate `amount` for `who`.
fn seed_active(who: AccountId, amount: Balance) {
	seed_deposit(who, amount);
	activate_all(&[who]);
}

fn set_min_active_pool(min: Balance) {
	let mut config = default_pool_config();
	config.minimum_active_pool_balance = min;
	assert_ok!(Stability::set_stability_pool_config(RuntimeOrigin::root(), DOT, PUSD, config));
}

#[test]
fn full_depletion_pays_old_epoch_and_starts_fresh() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 600);
		mint_stable(PUSD, 2, 400);
		assert_ok!(deposit(1, DOT, PUSD, 600));
		assert_ok!(deposit(2, DOT, PUSD, 400));
		advance_time(5_000);
		assert_ok!(activate(1, DOT, PUSD));
		assert_ok!(activate(2, DOT, PUSD));

		let (result, _) = simulate_offset(DOT, PUSD, 1_000, 800);
		assert_eq!(result.debt_offset, 1_000);

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords.epoch, 1);
		assert_eq!(state.coords.scale, 0);
		assert_eq!(state.coords.p, FixedU128::one());
		assert_eq!(state.total_active_deposits, 0);
		// The new epoch's sums row is seeded; the old one keeps the gains:
		// delta_S = floor(800 * 1e18 / 1000) = 8e17.
		let old = crate::PoolSumsStore::<Test>::get((DOT, PUSD, 0u32, 0u32)).expect("kept");
		assert_eq!(old.s_collateral, FixedU128::from_inner(800_000_000_000_000_000));
		let fresh = crate::PoolSumsStore::<Test>::get((DOT, PUSD, 1u32, 0u32)).expect("seeded");
		assert_eq!(fresh.s_collateral, FixedU128::zero());

		// Old-epoch depositors realize to zero active but keep their epoch's
		// gains: floor(600 * 0.8) = 480 and floor(400 * 0.8) = 320.
		// (Deltas: DOT is native, and accounts hold genesis native balance.)
		let before_1 = collateral_balance(DOT, 1);
		let before_2 = collateral_balance(DOT, 2);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_ok!(claim_collateral(2, DOT, PUSD, 2));
		assert_eq!(collateral_balance(DOT, 1) - before_1, 480);
		assert_eq!(collateral_balance(DOT, 2) - before_2, 320);
		// The emptied rows are gone.
		assert!(deposit_row(DOT, PUSD, 1).is_none());
		assert!(deposit_row(DOT, PUSD, 2).is_none());

		// A fresh epoch-1 depositor is untouched by epoch-0 history.
		mint_stable(PUSD, 3, 500);
		assert_ok!(deposit(3, DOT, PUSD, 500));
		advance_time(5_000);
		assert_ok!(activate(3, DOT, PUSD));
		let before_3 = collateral_balance(DOT, 3);
		assert_eq!(simulate_offset(DOT, PUSD, 250, 100).0.debt_offset, 250);
		assert_ok!(claim_collateral(3, DOT, PUSD, 3));
		// floor(500 * floor(100 * 1e18 / 500) / 1e18) = 100, and the
		// compounded deposit is floor(500 * 0.5) = 250.
		assert_eq!(collateral_balance(DOT, 3) - before_3, 100);
		assert_ok!(withdraw(3, DOT, PUSD, 1_000, 3));
		assert_eq!(stable_balance(PUSD, 3), 250);
	});
}

#[test]
fn scale_crossing_preserves_older_deposits() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		let unit: Balance = 10_000_000_000_000; // 1e13
		seed_active(1, unit);

		// Offset all but 100 (the exact floor): the survival ratio 1e-11
		// pushes P below p_min once, so it crosses one scale:
		// P = floor(1e18 * 1e9 * 100 / 1e13) = 1e16 (0.01), scale 1.
		let (result, _) = simulate_offset(DOT, PUSD, unit - 100, 5_000_000_000_000);
		assert_eq!(result.debt_offset, unit - 100);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords.epoch, 0);
		assert_eq!(state.coords.scale, 1);
		assert_eq!(state.coords.p, FixedU128::from_inner(10_000_000_000_000_000));
		assert_eq!(state.total_active_deposits, 100);
		assert!(crate::PoolSumsStore::<Test>::contains_key((DOT, PUSD, 0u32, 1u32)));
		System::assert_has_event(
			crate::Event::PoolOffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				debt_burned: unit - 100,
				collateral_gain: 5_000_000_000_000,
				epoch: 0,
				scale: 1,
			}
			.into(),
		);

		// A second offset on the new scale, allowed to leave 50 by lowering
		// the floor: delta_S(0,1) = floor(40 * 0.01 / 100) = 4e15 inner,
		// P = floor(1e16 * 50 / 100) = 5e15.
		set_min_active_pool(10);
		assert_eq!(simulate_offset(DOT, PUSD, 50, 40).0.debt_offset, 50);

		// The scale-0 deposit realizes across the boundary:
		// compounded = floor(1e13 * 5e15 / (1e18 * 1e9)) = 50;
		// gains combine both rows: floor(1e13 * (5e17 + 4e15/1e9) / 1e18)
		//                        = 5_000_000_000_000 + 40.
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 5_000_000_000_040);
		assert_eq!(pool_state(DOT, PUSD).total_collateral_gains_unclaimed, 0);
		assert_ok!(withdraw(1, DOT, PUSD, unit, 1));
		assert_eq!(stable_balance(PUSD, 1), 50);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 0);
	});
}

#[test]
fn deposit_two_scales_behind_realizes_through_the_squared_divisor() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		set_min_active_pool(5);
		let unit: Balance = 10_000_000_000_000_000_000; // 1e19
		seed_active(1, unit);

		// Leaving 5 of 1e19 is a survival ratio of 5e-19 < 1e-18: two
		// crossings in one offset, P = floor(1e36 * 5 / 1e19) = 5e17 (0.5).
		let (result, _) = simulate_offset(DOT, PUSD, unit - 5, 8_000_000_000_000_000_000);
		assert_eq!(result.debt_offset, unit - 5);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords.scale, 2);
		assert_eq!(state.coords.p, FixedU128::from_rational(1, 2));
		assert_eq!(state.total_active_deposits, 5);

		// Two scales behind, the sf² divisor still prices the survivor
		// exactly: compounded = floor(1e19 * 0.5 / (1 * 1e18)) = 5 — the
		// whole remaining pool, nothing stranded. The window gains survive
		// alongside: floor(1e19 * 0.8) = 8e18.
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 8_000_000_000_000_000_000);
		assert_ok!(withdraw(1, DOT, PUSD, unit, 1));
		assert_eq!(stable_balance(PUSD, 1), 5);
		// The emptied row is gone and the aggregate holds no dust.
		assert!(deposit_row(DOT, PUSD, 1).is_none());
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 0);
	});
}

#[test]
fn offset_beyond_supported_precision_steps_aside_untouched() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		set_min_active_pool(1);
		let unit: Balance = 10_000_000_000_000_000_000_000_000_000; // 1e28
		seed_active(1, unit);

		// A survival ratio of 1e-28 needs more than two crossings:
		// floor(1e36 * 1 / 1e28) = 1e8 < p_min even at the cap. The pool
		// declines the offset and returns the whole credit.
		let (result, leftover) = simulate_offset(DOT, PUSD, unit - 1, unit);
		assert_eq!(result.debt_offset, 0);
		assert_eq!(result.collateral_to_pool, 0);
		assert_eq!(leftover, unit);

		// The plan failed before any value moved: nothing to roll back.
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords.p, FixedU128::one());
		assert_eq!(state.total_active_deposits, unit);
		assert_eq!(state.total_collateral_gains_unclaimed, 0);
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), unit);
	});
}
