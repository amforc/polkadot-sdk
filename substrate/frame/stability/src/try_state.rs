//! `try-runtime` invariant checks: the accounting identities of SPEC.md §12
//! that must hold after every operation.

use crate::pallet::{BalanceOf, Config, Deposits, Pallet, PoolSumsStore, Pools};
use frame::{
	arithmetic::{FixedU128, One, Saturating, Zero},
	deps::frame_support::traits::fungibles::Inspect as _,
	try_runtime::TryRuntimeError,
};
use linked_list_interface::SortedListInterface;
use pusd_primitives::StableListId;

pub(crate) fn do_try_state<T: Config>() -> Result<(), TryRuntimeError> {
	for (collateral_id, stable_id, pool) in Pools::<T>::iter() {
		let state = &pool.state;
		let config = &pool.config;
		if !config.is_valid() {
			return Err("stored stability-pool config fails `is_valid`".into());
		}
		if state.coords.p > FixedU128::one() {
			return Err("`P` above one".into());
		}
		if state.coords.p < config.precision.p_min {
			return Err("`P` below the configured precision floor".into());
		}
		if !PoolSumsStore::<T>::contains_key((
			&collateral_id,
			&stable_id,
			state.coords.epoch,
			state.coords.scale,
		)) {
			return Err("current `(epoch, scale)` has no sums row".into());
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

		// The FIFO and the rows must be a bijection: exactly the rows with a
		// pending amount are queued. Alongside, no realized deposit set may
		// exceed the pool aggregate (flooring keeps compounded values at or
		// below it; the excess is stranded dust).
		let fifo = StableListId::StabilityPending(collateral_id.clone(), stable_id.clone());
		let mut pending_sum = BalanceOf::<T>::zero();
		let mut pending_rows: u32 = 0;
		let mut compounded_sum = BalanceOf::<T>::zero();
		for (who, deposit) in Deposits::<T>::iter_prefix((collateral_id.clone(), stable_id.clone()))
		{
			let in_fifo = T::PendingLists::contains(&fifo, &who);
			if deposit.pending_deposit.is_some() != in_fifo {
				return Err("pending-deposit FIFO membership diverges from the row".into());
			}
			if let Some(pending) = &deposit.pending_deposit {
				pending_sum = pending_sum.saturating_add(pending.amount);
				pending_rows = pending_rows.saturating_add(1);
			}

			let window = Pallet::<T>::sums_window(&collateral_id, &stable_id, &deposit.snapshot);
			let realized = crate::math::realize(
				deposit.active_deposit,
				&deposit.snapshot,
				&state.coords,
				&window,
				&config.precision,
			);
			compounded_sum = compounded_sum.saturating_add(realized.compounded);
		}
		if pending_sum != state.total_pending_deposits {
			return Err("sum of pending deposits diverges from `total_pending_deposits`".into());
		}
		if T::PendingLists::count(&fifo) != pending_rows {
			return Err("pending-deposit FIFO length diverges from the pending rows".into());
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
			deposit.snapshot.coords.epoch,
			deposit.snapshot.coords.scale,
		)) {
			return Err("deposit snapshot references a pruned sums row".into());
		}
	}
	Ok(())
}
