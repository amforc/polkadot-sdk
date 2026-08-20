//! `try-runtime` checks for stability-pool accounting invariants.
//!
//! These checks protect four contracts:
//!
//! - Accumulators stay within the configured precision limits.
//! - Pool custody equals the recorded stablecoin and collateral totals.
//! - The sum of user positions does not exceed each pool total.
//! - Cohort aggregates and checkpoints cover all member rows that reference them.

use crate::{
	pallet::{BalanceOf, CohortCheckpoints, Config, Deposits, Pallet, PoolSumsStore, Pools},
	types::{CohortId, Leg},
};
use alloc::collections::BTreeMap;
use frame::{
	arithmetic::{FixedU128, One, Saturating, Zero},
	deps::frame_support::traits::fungibles::Inspect as _,
	try_runtime::TryRuntimeError,
};

/// Member counts and claims derived from the deposit rows of one market.
///
/// The checks compare these values with cohort aggregates and checkpoints.
#[derive(Default)]
struct CohortTallies<Balance> {
	/// Number of rows that reference each open cohort or checkpoint.
	members: BTreeMap<CohortId, u32>,
	/// Sum of downward-rounded member claims for each cohort.
	///
	/// An open cohort uses the current pending leg. An activated cohort uses its checkpoint.
	claims: BTreeMap<CohortId, Balance>,
}

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
		// Pending deposits earn no yield. Sharing the row type with the active leg must never let
		// a yield sum in through the back door.
		for (_, sums) in PoolSumsStore::<T>::iter_prefix((
			collateral_id.clone(),
			stable_id.clone(),
			Leg::Pending,
		)) {
			if !sums.g_yield.is_zero() {
				return Err("pending sums row carries a nonzero `G`".into());
			}
		}

		// This holds as an equality, not as an inequality: every stablecoin that enters or leaves
		// the pool account moves exactly one total with it, and a rounding remainder stays inside
		// those totals rather than outside them.
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

		// The open cohorts themselves: live ids, increasing deadlines, no memberless row, and an
		// aggregate valued at coordinates the pending leg has actually reached.
		for cohort in &state.open_cohorts {
			if cohort.id >= state.next_cohort_id {
				return Err("open cohort id at or past `next_cohort_id`".into());
			}
			if cohort.members == 0 {
				return Err("open cohort with no members was not cleared".into());
			}
			if (cohort.coords.epoch, cohort.coords.scale) >
				(state.pending_coords.epoch, state.pending_coords.scale)
			{
				return Err("open cohort coordinates ahead of the pending leg".into());
			}
		}
		if state
			.open_cohorts
			.windows(2)
			.any(|pair| pair[0].id == pair[1].id || pair[0].deadline > pair[1].deadline)
		{
			return Err("open cohorts have duplicate ids or unordered deadlines".into());
		}

		// No set of rows may add up to more than its pool total, on either leg. Rounding keeps
		// every realized value at or below the total, and the difference belongs to nobody; it
		// leaves with an epoch reset or with the teardown sweep.
		let mut pending_sum = BalanceOf::<T>::zero();
		let mut compounded_sum = BalanceOf::<T>::zero();
		let mut tallies = CohortTallies::<BalanceOf<T>>::default();
		for (_, deposit) in Deposits::<T>::iter_prefix((collateral_id.clone(), stable_id.clone())) {
			if let Some(pending) = &deposit.pending_deposit {
				let open = state.cohort(pending.cohort);
				let checkpoint =
					CohortCheckpoints::<T>::get((&collateral_id, &stable_id, pending.cohort));
				match (open, checkpoint) {
					(Some(_), Some(_)) => {
						return Err("cohort is both open and checkpointed".into());
					},
					(None, None) => {
						return Err("pending row references an unknown cohort".into());
					},
					(Some(cohort), None) => {
						let coords = pending.snapshot.coords;
						if (coords.epoch, coords.scale) > (cohort.coords.epoch, cohort.coords.scale)
						{
							return Err("member snapshot ahead of its cohort's coordinates".into());
						}
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

						let claims = tallies.claims.entry(pending.cohort).or_default();
						*claims = claims.saturating_add(realized.compounded);
					},
					(None, Some(checkpoint)) => {
						let window = Pallet::<T>::checkpoint_window(
							&collateral_id,
							&stable_id,
							&pending.snapshot,
							&checkpoint.pending_end,
						);
						let phase_one = crate::math::realize(
							pending.amount,
							&pending.snapshot,
							&checkpoint.pending_end.coords,
							&window,
							&config.precision,
						);
						if !phase_one.yield_gain.is_zero() {
							return Err("checkpointed tranche realized a yield gain".into());
						}
						let claims = tallies.claims.entry(pending.cohort).or_default();
						*claims = claims.saturating_add(phase_one.compounded);
						if *claims > checkpoint.activated {
							return Err("member claims exceed the activated cohort capital".into());
						}
						// The survivor is active capital since the checkpoint: it counts against
						// the active total, not the pending one.
						let window = Pallet::<T>::sums_window(
							&collateral_id,
							&stable_id,
							Leg::Active,
							&checkpoint.active_start,
						);
						let phase_two = crate::math::realize(
							phase_one.compounded,
							&checkpoint.active_start,
							&state.coords,
							&window,
							&config.precision,
						);
						compounded_sum = compounded_sum.saturating_add(phase_two.compounded);
					},
				}
				*tallies.members.entry(pending.cohort).or_default() += 1;
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

		// The cohort bookkeeping must cover the rows that reference it: member counts move in
		// step with the rows, and the ceiling-compounded aggregate never falls below the floored
		// member claims it will have to pay at advancement.
		for cohort in &state.open_cohorts {
			if tallies.members.get(&cohort.id).copied().unwrap_or(0) != cohort.members {
				return Err("open cohort member count diverges from its rows".into());
			}
			let cover = crate::math::compound_ceil(
				cohort.amount,
				&cohort.coords,
				&state.pending_coords,
				config.precision.scale_factor(),
			);
			let claims =
				tallies.claims.get(&cohort.id).copied().unwrap_or_else(BalanceOf::<T>::zero);
			if claims > cover {
				return Err("member claims exceed the cohort aggregate".into());
			}
		}
		for (id, checkpoint) in
			CohortCheckpoints::<T>::iter_prefix((collateral_id.clone(), stable_id.clone()))
		{
			if checkpoint.members == 0 {
				return Err("memberless checkpoint was not removed".into());
			}
			if tallies.members.get(&id).copied().unwrap_or(0) != checkpoint.members {
				return Err("checkpoint member count diverges from its rows".into());
			}
			if state.cohort(id).is_some() {
				return Err("checkpointed cohort is still open".into());
			}
			for snapshot in [&checkpoint.pending_end, &checkpoint.active_start] {
				if snapshot.coords.epoch > state.coords.epoch.max(state.pending_coords.epoch) {
					return Err("checkpoint snapshot epoch ahead of the pool".into());
				}
			}
		}
	}

	// A pool row is what makes a market registered, so nothing may outlive one.
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
		// Realization reads the sums row the snapshot points at, so that row must still exist.
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

	// No checkpoint may outlive the rows that reference it: every stored checkpoint belongs to a
	// registered market, and the per-market loop above already matched member counts.
	for ((collateral_id, stable_id, _id), _checkpoint) in CohortCheckpoints::<T>::iter() {
		if Pools::<T>::get(&collateral_id, &stable_id).is_none() {
			return Err("cohort checkpoint without a pool row".into());
		}
	}
	Ok(())
}
