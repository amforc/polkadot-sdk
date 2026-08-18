//! What each accumulator responds to.
//!
//! An offset shrinks `P` and adds to `S`. Yield adds to `G` and leaves the loss side alone. The
//! pending pair resets when an offset empties the pending stock, and not merely when the last
//! pending deposit matures.
//!
//! `epoch_scale` covers the epoch and scale transitions, and `claimable_accrual` covers the
//! depositor snapshots.

use crate::{
	mock::*,
	types::{Leg, PoolSums},
};

fn sums_at(epoch: u32, scale: u32) -> PoolSums {
	crate::PoolSumsStore::<Test>::get((DOT, PUSD, Leg::Active, epoch, scale))
}

fn pending_sums_at(epoch: u32, scale: u32) -> PoolSums {
	crate::PoolSumsStore::<Test>::get((DOT, PUSD, Leg::Pending, epoch, scale))
}

/// Queue a pending (unactivated) deposit for `who`.
fn seed_pending(who: AccountId, amount: Balance) {
	mint_stable(PUSD, who, amount);
	assert_ok!(deposit(who, DOT, PUSD, amount));
}

/// Deposit and immediately activate `amount` for `who`.
fn seed_active(who: AccountId, amount: Balance) {
	seed_deposit(who, amount);
	activate_all(&[who]);
}

#[test]
fn offsets_move_p_and_s_yield_moves_g() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 1_000);

		// Offset 1: P = 800/1000 = 0.8, delta_S = 160 * (1/1000) = 0.16;
		// G is untouched.
		assert_eq!(simulate_offset(DOT, PUSD, 200, 160).0, 200);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords.p, FixedU128::from_rational(4, 5));
		assert_eq!(sums_at(0, 0).s_collateral, FixedU128::from_inner(160_000_000_000_000_000));
		assert_eq!(sums_at(0, 0).g_yield, FixedU128::zero());

		// Yield: delta_G = 80 * (0.8/800) = 0.08; P and S are untouched.
		drop(distribute_yield(DOT, PUSD, 80));
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords.p, FixedU128::from_rational(4, 5));
		assert_eq!(sums_at(0, 0).s_collateral, FixedU128::from_inner(160_000_000_000_000_000));
		assert_eq!(sums_at(0, 0).g_yield, FixedU128::from_inner(80_000_000_000_000_000));

		// Offset 2 compounds P multiplicatively and adds to S:
		// P = 0.8 * (400/800) = 0.4, delta_S = 320 * (0.8/800) = 0.32,
		// so S = 0.48; G is untouched again.
		assert_eq!(simulate_offset(DOT, PUSD, 400, 320).0, 400);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords.p, FixedU128::from_rational(2, 5));
		assert_eq!(sums_at(0, 0).s_collateral, FixedU128::from_inner(480_000_000_000_000_000));
		assert_eq!(sums_at(0, 0).g_yield, FixedU128::from_inner(80_000_000_000_000_000));
		// Coordinates never moved: everything above stayed on (0, 0).
		assert_eq!(state.coords.epoch, 0);
		assert_eq!(state.coords.scale, 0);
	});
}

/// The pending pair resets only when an offset empties the pending stock. Emptying it any other
/// way, such as every row maturing, leaves the accumulators where they stand, which is what makes
/// the snapshot of a later deposit mean anything.
#[test]
fn pending_accumulators_reset_on_depletion_not_on_an_empty_queue() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_pending(1, 200);

		// Burn 100 of the 200 pending: P_pending = 100/200 = 0.5 and
		// delta_S = 50 * (1/200) = 0.25.
		assert_eq!(simulate_pending_offset(DOT, PUSD, 100, 50).0, 100);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.pending_coords.p, FixedU128::from_rational(1, 2));
		assert_eq!(pending_sums_at(0, 0).s_collateral, FixedU128::from_rational(1, 4));

		// The surviving floor(200 * 0.5) = 100 matures into the active pool.
		// The pending stock is empty, yet nothing resets: no depletion
		// happened.
		activate_all(&[1]);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.total_active_deposits, 100);
		assert_eq!(state.pending_coords.p, FixedU128::from_rational(1, 2));
		assert_eq!(state.pending_coords.epoch, 0);
		assert_eq!(state.pending_coords.scale, 0);
		assert_eq!(pending_sums_at(0, 0).s_collateral, FixedU128::from_rational(1, 4));

		// A fresh deposit snapshots that `P_pending`, so it is measured from
		// where the accumulators stand rather than from `P = 1`: untouched,
		// it realizes its full 400.
		seed_pending(2, 400);
		assert_eq!(realized_pending(DOT, PUSD, 2), 400);

		// Depletion is what resets the pair: a new epoch at `P = 1`, scale 0,
		// with zeroed sums.
		assert_eq!(simulate_pending_offset(DOT, PUSD, 400, 80).0, 400);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.pending_coords.epoch, 1);
		assert_eq!(state.pending_coords.scale, 0);
		assert_eq!(state.pending_coords.p, FixedU128::one());
		assert_eq!(pending_sums_at(1, 0).s_collateral, FixedU128::zero());
		// The closing epoch keeps its row: 0.25 + 80 * (0.5/400) = 0.35, the
		// window user 2 still claims through.
		assert_eq!(pending_sums_at(0, 0).s_collateral, FixedU128::from_rational(35, 100));
	});
}
