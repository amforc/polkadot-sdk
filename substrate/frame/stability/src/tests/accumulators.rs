//! One focused global-accumulator contract: offsets compound `P` and add to
//! `S`, while yield adds to `G` without changing the liquidation side.
//! Epoch/scale transitions and depositor snapshots are already covered by
//! `epoch_scale.rs` and `claimable_accrual.rs`.

use crate::{mock::*, types::PoolSums};

fn sums_at(epoch: u32, scale: u32) -> PoolSums {
	crate::PoolSumsStore::<Test>::get((DOT, PUSD, epoch, scale))
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
