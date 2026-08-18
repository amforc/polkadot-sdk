//! Epoch bumps and scale crossings: the coordinates under a full depletion and under offsets
//! extreme enough to exhaust the precision of `P`.
//!
//! A scale crossing needs `P` to fall to `p_min`, which is 1e-9 here, so these tests lower the
//! post-offset floor to single digits first. A production floor never needs to be large: only a
//! step that shrinks the pool by more than 1e18 is refused, so any floor above
//! `total_supply / 1e18` is enough.

use crate::{mock::*, types::Leg};

/// Deposit and immediately activate `amount` for `who`.
fn seed_active(who: AccountId, amount: Balance) {
	seed_deposit(who, amount);
	activate_all(&[who]);
}

#[test]
fn full_depletion_pays_old_epoch_and_starts_fresh() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 600);
		mint_stable(PUSD, 2, 400);
		assert_ok!(deposit(1, DOT, PUSD, 600));
		assert_ok!(deposit(2, DOT, PUSD, 400));
		activate_all(&[1, 2]);

		let (debt_offset, _) = simulate_offset(DOT, PUSD, 1_000, 800);
		assert_eq!(debt_offset, 1_000);

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords.epoch, 1);
		assert_eq!(state.coords.scale, 0);
		assert_eq!(state.coords.p, FixedU128::one());
		assert_eq!(state.total_active_deposits, 0);
		// The new epoch's sums row is seeded; the old one keeps the gains:
		// delta_S = 800 * (1/1000) = 0.8.
		let old = crate::PoolSumsStore::<Test>::get((DOT, PUSD, Leg::Active, 0u32, 0u32));
		assert_eq!(old.s_collateral, FixedU128::from_inner(800_000_000_000_000_000));
		assert!(crate::PoolSumsStore::<Test>::contains_key((DOT, PUSD, Leg::Active, 1u32, 0u32)));
		let fresh = crate::PoolSumsStore::<Test>::get((DOT, PUSD, Leg::Active, 1u32, 0u32));
		assert_eq!(fresh.s_collateral, FixedU128::zero());

		// Old-epoch depositors realize to zero active but keep their epoch's
		// gains — normalized stakes (D0/P0) times delta_S:
		// (600/1) * 0.8 = 480 and (400/1) * 0.8 = 320.
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

		// A fresh epoch-1 depositor is untouched by epoch-0 history. The 250
		// debt seizes 250 / 1.25 = 200 collateral at the registration price:
		// P = floor(P * new_A / A) = floor(1 * 250/500) = 0.5,
		// delta_S(1,0) = 200 * (1/500) = 0.4.
		mint_stable(PUSD, 3, 500);
		assert_ok!(deposit(3, DOT, PUSD, 500));
		activate_all(&[3]);
		let before_3 = collateral_balance(DOT, 3);
		assert_eq!(simulate_offset(DOT, PUSD, 250, 200).0, 250);
		assert_ok!(claim_collateral(3, DOT, PUSD, 3));
		// gain = (500/1) * 0.4 = 200; compounded = (500/1) * 0.5 = 250.
		assert_eq!(collateral_balance(DOT, 3) - before_3, 200);
		assert_ok!(withdraw(3, DOT, PUSD, 1_000, 3));
		assert_eq!(stable_balance(PUSD, 3), 250);
	});
}

#[test]
fn scale_crossing_preserves_older_deposits() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// One floor for the whole scenario, set before any capital moves.
		set_min_active_pool(10);
		let unit: Balance = 10_000_000_000_000; // 1e13
		seed_active(1, unit);

		// Offset all but 100: the survival ratio 1e-11 pushes P below p_min
		// once, so it crosses one scale:
		// P = floor(1e18 * 1e9 * 100 / 1e13) = 1e16 (0.01), scale 1.
		let (debt_offset, _) = simulate_offset(DOT, PUSD, unit - 100, 5_000_000_000_000);
		assert_eq!(debt_offset, unit - 100);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords.epoch, 0);
		assert_eq!(state.coords.scale, 1);
		assert_eq!(state.coords.p, FixedU128::from_inner(10_000_000_000_000_000));
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
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 5_000_000_000_040);
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
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		set_min_active_pool(5);
		let unit: Balance = 10_000_000_000_000_000_000; // 1e19
		seed_active(1, unit);

		// Leaving 5 of 1e19 is a survival ratio of 5e-19 < 1e-18: two
		// crossings in one offset, P = floor(1e36 * 5 / 1e19) = 5e17 (0.5).
		let (debt_offset, _) = simulate_offset(DOT, PUSD, unit - 5, 8_000_000_000_000_000_000);
		assert_eq!(debt_offset, unit - 5);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords.scale, 2);
		assert_eq!(state.coords.p, FixedU128::from_rational(1, 2));
		assert_eq!(state.total_active_deposits, 5);

		// Two scales behind, the sf² divisor still prices the survivor
		// exactly: compounded = (D0/P0) * P / sf²
		// = 1e19 * 0.5 / (1e9)² = 5 — the whole remaining pool, nothing
		// stranded. The window gains survive alongside:
		// gain = (D0/P0) * delta_S(0,0) = 1e19 * 0.8 = 8e18.
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
		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, unit - 1, unit);
		assert_eq!(debt_offset, 0);
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
