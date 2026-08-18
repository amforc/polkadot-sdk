//! `try-runtime` invariant checks: the accounting identities of SPEC.md §12
//! that must hold after every operation.

use crate::{
	pallet::{BalanceOf, Config, Deposits, Pallet, PoolSumsStore, Pools},
	types::Leg,
};
use frame::{
	arithmetic::{FixedU128, One, Saturating, Zero},
	deps::frame_support::traits::fungibles::Inspect as _,
	try_runtime::TryRuntimeError,
};

pub(crate) fn do_try_state<T: Config>() -> Result<(), TryRuntimeError> {
	for (collateral_id, stable_id, pool) in Pools::<T>::iter() {
		let state = &pool.state;
		let config = &pool.config;
		if !config.is_valid() {
			return Err("stored stability-pool config fails `is_valid`".into());
		}
		for leg in Leg::ALL {
			let coords = state.coords(leg);
			if coords.p > FixedU128::one() {
				return Err("`P` above one".into());
			}
			if coords.p < config.precision.p_min {
				return Err("`P` below the configured precision floor".into());
			}
			if !PoolSumsStore::<T>::contains_key((
				&collateral_id,
				&stable_id,
				leg,
				coords.epoch,
				coords.scale,
			)) {
				return Err("current `(epoch, scale)` has no sums row".into());
			}
		}
		// Pending deposits earn no yield, so the pending leg's `G` is
		// structurally zero — sharing the row type with the active leg must
		// never smuggle a yield sum in.
		for (_, sums) in PoolSumsStore::<T>::iter_prefix((
			collateral_id.clone(),
			stable_id.clone(),
			Leg::Pending,
		)) {
			if !sums.g_yield.is_zero() {
				return Err("pending sums row carries a nonzero `G`".into());
			}
		}

		// Invariant 1 holds as an equality: every stablecoin flow into or
		// out of the pool account mirrors exactly one aggregate, and
		// flooring dust strands inside the unclaimed totals, never outside.
		let pool_account = Pallet::<T>::pool_account(&collateral_id, &stable_id);
		let stable_held = T::StableAssets::balance(stable_id.clone(), &pool_account);
		let stable_owed = state
			.total_active_deposits
			.saturating_add(state.total_pending_deposits)
			.saturating_add(state.total_yield_unclaimed);
		if stable_held != stable_owed {
			return Err("pool stablecoin balance diverges from tracked totals".into());
		}
		let collateral_held = T::CollateralAssets::balance(collateral_id.clone(), &pool_account);
		let collateral_owed = state.total_collateral_gains_unclaimed;
		if collateral_held != collateral_owed {
			return Err("pool collateral balance diverges from tracked totals".into());
		}

		// No realized deposit set may exceed its pool aggregate — on either
		// leg. Flooring keeps realized values at or below the aggregate; the
		// excess is stranded dust that leaves with an epoch reset or the
		// teardown sweep.
		let mut pending_sum = BalanceOf::<T>::zero();
		let mut compounded_sum = BalanceOf::<T>::zero();
		for (_, deposit) in Deposits::<T>::iter_prefix((collateral_id.clone(), stable_id.clone())) {
			if let Some(pending) = &deposit.pending_deposit {
				let window = Pallet::<T>::sums_window(
					&collateral_id,
					&stable_id,
					Leg::Pending,
					&pending.snapshot,
				);
				let realized = crate::math::realize(
					pending.amount,
					&pending.snapshot,
					&state.pending_coords,
					&window,
					&config.precision,
				);
				if !realized.yield_gain.is_zero() {
					return Err("pending deposit realized a yield gain".into());
				}
				pending_sum = pending_sum.saturating_add(realized.compounded);
			}

			let window = Pallet::<T>::sums_window(
				&collateral_id,
				&stable_id,
				Leg::Active,
				&deposit.snapshot,
			);
			let realized = crate::math::realize(
				deposit.active_deposit,
				&deposit.snapshot,
				&state.coords,
				&window,
				&config.precision,
			);
			compounded_sum = compounded_sum.saturating_add(realized.compounded);
		}
		if pending_sum > state.total_pending_deposits {
			return Err("realized pending deposits exceed `total_pending_deposits`".into());
		}
		if compounded_sum > state.total_active_deposits {
			return Err("realized deposits exceed `total_active_deposits`".into());
		}
	}

	// Pool rows are the registration proxy: nothing may outlive them.
	for ((collateral_id, stable_id, _who), deposit) in Deposits::<T>::iter() {
		let state = Pools::<T>::get(&collateral_id, &stable_id)
			.ok_or("deposit row without a pool row")?
			.state;
		if deposit.snapshot.coords.epoch > state.coords.epoch {
			return Err("deposit snapshot epoch ahead of the pool".into());
		}
		if deposit.snapshot.coords.epoch == state.coords.epoch {
			if deposit.snapshot.coords.scale > state.coords.scale {
				return Err("deposit snapshot scale ahead of the pool".into());
			}
		}
		// Pruning guard: realization reads the snapshot's sums row.
		if !PoolSumsStore::<T>::contains_key((
			&collateral_id,
			&stable_id,
			Leg::Active,
			deposit.snapshot.coords.epoch,
			deposit.snapshot.coords.scale,
		)) {
			return Err("deposit snapshot references a pruned sums row".into());
		}

		let Some(pending) = &deposit.pending_deposit else {
			continue;
		};
		if pending.snapshot.coords.epoch > state.pending_coords.epoch {
			return Err("pending snapshot epoch ahead of the pending accumulators".into());
		}
		if pending.snapshot.coords.epoch == state.pending_coords.epoch {
			if pending.snapshot.coords.scale > state.pending_coords.scale {
				return Err("pending snapshot scale ahead of the pending accumulators".into());
			}
		}
		if !PoolSumsStore::<T>::contains_key((
			&collateral_id,
			&stable_id,
			Leg::Pending,
			pending.snapshot.coords.epoch,
			pending.snapshot.coords.scale,
		)) {
			return Err("pending snapshot references a pruned sums row".into());
		}
	}
	Ok(())
}
