//! Epoch bumps and scale crossings: the coordinates under a full depletion and under offsets
//! extreme enough to exhaust the precision of `P`.
//!
//! A scale crossing needs `P` to fall to `p_min`, which is 1e-9 here, so these tests lower the
//! post-offset floor to single digits first. A production floor never needs to be large: only a
//! step that shrinks the pool by more than 1e18 is refused, so any floor above
//! `total_supply / 1e18` is enough.

use crate::mock::*;
use pusd_primitives::StabilityPoolInspect;

#[test]
fn full_depletion_pays_old_epoch_and_starts_fresh() {
	build_with_default_market(|| {
		seed_matured_deposit(1, 600);
		seed_matured_deposit(2, 400);
		// Yield before the depletion: G(0,0) = 100/1000 = 0.1.
		drop(distribute_yield(DOT, PUSD, 100));

		assert_eq!(simulate_offset(DOT, PUSD, 1_000, 800).0, 1_000);

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords, Accumulators { p: FixedU128::one(), epoch: 1, scale: 0 });
		assert_eq!(state.total_active_deposits, 0);
		// The old epoch's sums row keeps the gains, delta_S = 800 * (1/1000) = 0.8, and the new
		// epoch's row is seeded empty.
		assert_eq!(active_sums(0, 0).s_collateral, FixedU128::from_inner(800_000_000_000_000_000));
		assert!(crate::PoolSumsStore::<Test>::contains_key((DOT, PUSD, Leg::Active, 1u32, 0u32)));
		assert_eq!(active_sums(1, 0), PoolSums::default());

		// Old-epoch depositors realize to zero active but keep their epoch's gains — normalized
		// stakes (D0/P0) times delta_S: (600/1) * 0.8 = 480 and (400/1) * 0.8 = 320 collateral,
		// and floor(600 * 0.1) = 60 and floor(400 * 0.1) = 40 yield.
		assert_claim_collateral(1, 480);
		assert_claim_collateral(2, 320);
		assert_claim_yield(1, 60);
		assert_claim_yield(2, 40);
		// The emptied rows are gone.
		assert!(deposit_row(DOT, PUSD, 1).is_none());
		assert!(deposit_row(DOT, PUSD, 2).is_none());

		// A fresh epoch-1 depositor is untouched by epoch-0 history. Yield on epoch 1:
		// G(1,0) = 50/500 = 0.1. The 250 debt seizes 250 / 1.25 = 200 collateral at the
		// registration price: P = floor(P * new_A / A) = floor(1 * 250/500) = 0.5,
		// delta_S(1,0) = 200 * (1/500) = 0.4.
		seed_matured_deposit(3, 500);
		drop(distribute_yield(DOT, PUSD, 50));
		assert_eq!(simulate_offset(DOT, PUSD, 250, 200).0, 250);
		// gain = (500/1) * 0.4 = 200; yield = floor(500 * 0.1) = 50; compounded = (500/1) * 0.5
		// = 250.
		assert_claim_collateral(3, 200);
		assert_claim_yield(3, 50);
		assert_ok!(withdraw(3, DOT, PUSD, 1_000, 3));
		assert_eq!(stable_balance(PUSD, 3), 300);

		// Collateral 480 + 320 + 200 = 1000 and yield 60 + 40 + 50 = 150: nothing stranded across
		// the epoch boundary.
		assert_pool_fully_drained();
	});
}

/// A full depletion at a compounded `P`: the epoch closes and the accumulators reset, while the
/// closing epoch's collateral stays claimable to a depositor who joined at that `P`.
///
/// `P = 0.42` is reached with two collateral-free offsets. The epoch and scale indices are
/// labels, and the arithmetic depends only on `P` and the pool total, so this asserts that the
/// epoch increments rather than that it reaches any particular value.
#[test]
fn full_depletion_at_a_compounded_p_closes_the_epoch() {
	build_with_default_market(|| {
		seed_matured_deposit(1, 1_000);

		// 1_000 → 600 gives P = 0.6, then 600 → 420 gives P = 0.6 * 0.7 = 0.42.
		assert_eq!(simulate_offset(DOT, PUSD, 400, 0).0, 400);
		assert_eq!(simulate_offset(DOT, PUSD, 180, 0).0, 180);
		assert_eq!(pool_state(DOT, PUSD).coords.p, FixedU128::from_rational(42, 100));

		// The depositor this follows joins at P = 0.42 with 600, and a third tops the pool up to
		// 1_500.
		seed_matured_deposit(2, 600);
		seed_matured_deposit(3, 480);
		assert_eq!(Stability::reducible_active(&DOT, &PUSD, 1_500), 1_500);
		let epoch_before = pool_state(DOT, PUSD).coords.epoch;

		// Deplete: S rises by 900 * 0.42 / 1_500 = 0.252 on the closing epoch, which is where
		// the gains stay claimable from, and the new epoch starts from zeroed sums.
		assert_eq!(simulate_offset(DOT, PUSD, 1_500, 900).0, 1_500);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(
			state.coords,
			Accumulators { p: FixedU128::one(), epoch: epoch_before + 1, scale: 0 }
		);
		assert_eq!(active_sums(epoch_before, 0).s_collateral, FixedU128::from_rational(252, 1_000));
		assert_eq!(active_sums(epoch_before + 1, 0), PoolSums::default());

		// The 600 depositor: compounded is zero an epoch behind, but its collateral
		// 600 * 0.252 / 0.42 = 360 stays claimable.
		assert_claim_collateral(2, 360);
	});
}

#[test]
fn scale_crossing_preserves_older_deposits() {
	build_with_default_market(|| {
		// One floor for the whole scenario, set before any capital moves.
		set_min_active_pool(10);
		let unit: Balance = 10_000_000_000_000; // 1e13
		seed_matured_deposit(1, unit);

		// Offset all but 100: the survival ratio 1e-11 pushes P below p_min
		// once, so it crosses one scale:
		// P = floor(1e18 * 1e9 * 100 / 1e13) = 1e16 (0.01), scale 1.
		let (debt_offset, _) = simulate_offset(DOT, PUSD, unit - 100, 5_000_000_000_000);
		assert_eq!(debt_offset, unit - 100);
		let state = pool_state(DOT, PUSD);
		assert_eq!(
			state.coords,
			Accumulators { p: FixedU128::from_inner(10_000_000_000_000_000), epoch: 0, scale: 1 }
		);
		assert_eq!(state.total_active_deposits, 100);
		assert!(crate::PoolSumsStore::<Test>::contains_key((DOT, PUSD, Leg::Active, 0u32, 1u32)));
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

		// A second offset on the new scale leaves 50 of the 100:
		// delta_S(0,1) = 40 * (0.01/100) = 4e-3 (inner 4e15),
		// P = floor(1e16 * 50 / 100) = 5e15.
		assert_eq!(simulate_offset(DOT, PUSD, 50, 40).0, 50);

		// The scale-0 deposit realizes one scale behind: each scale
		// crossed adds a `scale_factor` divisor, so
		// compounded = (D0/P0) * P / sf = 1e13 * 5e15 / (1e18 * 1e9) = 50;
		// gain = (D0/P0) * (delta_S(0,0) + delta_S(0,1) / sf)
		//      = 1e13 * (0.5 + 4e-3/1e9) = 5_000_000_000_000 + 40.
		assert_claim_collateral(1, 5_000_000_000_040);
		assert_eq!(pool_state(DOT, PUSD).total_collateral_gains_unclaimed, 0);
		// Nearly the whole 1e13 deposit has been offset away by now; only the
		// compounded 50 remains to withdraw.
		assert_ok!(withdraw(1, DOT, PUSD, unit, 1));
		assert_eq!(stable_balance(PUSD, 1), 50);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 0);
	});
}

#[test]
fn deposit_two_scales_behind_realizes_through_the_squared_divisor() {
	build_with_default_market(|| {
		set_min_active_pool(5);
		let unit: Balance = 10_000_000_000_000_000_000; // 1e19
		seed_matured_deposit(1, unit);

		// Leaving 5 of 1e19 is a survival ratio of 5e-19 < 1e-18: two
		// crossings in one offset, P = floor(1e36 * 5 / 1e19) = 5e17 (0.5).
		let (debt_offset, _) = simulate_offset(DOT, PUSD, unit - 5, 8_000_000_000_000_000_000);
		assert_eq!(debt_offset, unit - 5);
		let state = pool_state(DOT, PUSD);
		assert_eq!(
			state.coords,
			Accumulators { p: FixedU128::from_rational(1, 2), epoch: 0, scale: 2 }
		);
		assert_eq!(state.total_active_deposits, 5);

		// Two scales behind, the sf² divisor still prices the survivor
		// exactly: compounded = (D0/P0) * P / sf²
		// = 1e19 * 0.5 / (1e9)² = 5 — the whole remaining pool, nothing
		// stranded. The window gains survive alongside:
		// gain = (D0/P0) * delta_S(0,0) = 1e19 * 0.8 = 8e18.
		assert_claim_collateral(1, 8_000_000_000_000_000_000);
		assert_ok!(withdraw(1, DOT, PUSD, unit, 1));
		assert_eq!(stable_balance(PUSD, 1), 5);
		// The emptied row is gone and the aggregate holds no dust.
		assert!(deposit_row(DOT, PUSD, 1).is_none());
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 0);
	});
}

#[test]
fn offset_beyond_supported_precision_steps_aside_untouched() {
	build_with_default_market(|| {
		set_min_active_pool(1);
		let unit: Balance = 10_000_000_000_000_000_000_000_000_000; // 1e28
		seed_matured_deposit(1, unit);
		advance_matured_cohorts(DOT, PUSD);

		// A survival ratio of 1e-28 needs more than two crossings:
		// floor(1e36 * 1 / 1e28) = 1e8 < p_min even at the cap. The pool
		// declines the offset and returns the whole credit. The plan failed before any value
		// moved, so there is nothing to roll back.
		assert_storage_noop!(assert_eq!(simulate_offset(DOT, PUSD, unit - 1, unit), (0, unit)));
	});
}
