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
	deps::{frame_support::traits::fungibles::Inspect as _, sp_runtime::TryRuntimeError},
	prelude::ensure,
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
		ensure!(config.is_valid(), "stored stability-pool config fails `is_valid`");
		for leg in Leg::ALL {
			let coords = state.coords(leg);
			ensure!(coords.p <= FixedU128::one(), "`P` above one");
			ensure!(coords.p >= config.precision.p_min, "`P` below the configured precision floor");
			ensure!(
				PoolSumsStore::<T>::contains_key((
					&collateral_id,
					&stable_id,
					leg,
					coords.epoch,
					coords.scale,
				)),
				"current `(epoch, scale)` has no sums row"
			);
		}
		// Pending deposits earn no yield. Sharing the row type with the active leg must never let
		// a yield sum in through the back door.
		for (_, sums) in PoolSumsStore::<T>::iter_prefix((
			collateral_id.clone(),
			stable_id.clone(),
			Leg::Pending,
		)) {
			ensure!(sums.g_yield.is_zero(), "pending sums row carries a nonzero `G`");
		}

		// This holds as an equality, not as an inequality: every stablecoin that enters or leaves
		// the pool account moves exactly one total with it, and a rounding remainder stays inside
		// those totals rather than outside them. The custody float sits on top and only moves at
		// removal.
		let pool_account = Pallet::<T>::pool_account(&collateral_id, &stable_id);
		let stable_held = T::StableAssets::balance(stable_id.clone(), &pool_account);
		let stable_owed = state
			.total_active_deposits
			.saturating_add(state.total_pending_deposits)
			.saturating_add(state.total_yield_unclaimed)
			.saturating_add(pool.custody_stable);
		ensure!(stable_held == stable_owed, "pool stablecoin balance diverges from tracked totals");
		let collateral_held = T::CollateralAssets::balance(collateral_id.clone(), &pool_account);
		let collateral_owed =
			state.total_collateral_gains_unclaimed.saturating_add(pool.custody_collateral);
		ensure!(
			collateral_held == collateral_owed,
			"pool collateral balance diverges from tracked totals"
		);
		// The float is what lets every claim leave the minimum balance behind. A minimum raised
		// after registration is the one case the float does not cover, and it must be visible.
		ensure!(
			pool.custody_stable >= T::StableAssets::minimum_balance(stable_id.clone()),
			"stablecoin custody float below the asset minimum balance"
		);
		ensure!(
			pool.custody_collateral >= T::CollateralAssets::minimum_balance(collateral_id.clone()),
			"collateral custody float below the asset minimum balance"
		);

		// The open cohorts themselves: live ids, increasing deadlines, no memberless row, and an
		// aggregate valued at coordinates the pending leg has actually reached.
		for cohort in &state.open_cohorts {
			ensure!(cohort.id < state.next_cohort_id, "open cohort id at or past `next_cohort_id`");
			ensure!(cohort.members > 0, "open cohort with no members was not cleared");
			ensure!(
				cohort.coords.position() <= state.pending_coords.position(),
				"open cohort coordinates ahead of the pending leg"
			);
		}
		ensure!(
			!state
				.open_cohorts
				.windows(2)
				.any(|pair| pair[0].id == pair[1].id || pair[0].deadline > pair[1].deadline),
			"open cohorts have duplicate ids or unordered deadlines"
		);

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
						ensure!(
							pending.snapshot.coords.position() <= cohort.coords.position(),
							"member snapshot ahead of its cohort's coordinates"
						);
						let (realized, _) = Pallet::<T>::realize_leg(
							&collateral_id,
							&stable_id,
							Leg::Pending,
							&pool,
							pending.amount,
							&pending.snapshot,
						)
						.map_err(|_| "pending snapshot has no sums row")?;
						ensure!(
							realized.yield_gain.is_zero(),
							"pending deposit realized a yield gain"
						);
						pending_sum.saturating_accrue(realized.compounded);
						tallies
							.claims
							.entry(pending.cohort)
							.or_default()
							.saturating_accrue(realized.compounded);
					},
					(None, Some(checkpoint)) => {
						let phase_one = Pallet::<T>::realize_between(
							&collateral_id,
							&stable_id,
							Leg::Pending,
							pending.amount,
							&pending.snapshot,
							&checkpoint.pending_end,
							&config.precision,
						)
						.map_err(|_| "checkpoint window has no sums row")?;
						ensure!(
							phase_one.yield_gain.is_zero(),
							"checkpointed tranche realized a yield gain"
						);
						let claims = tallies.claims.entry(pending.cohort).or_default();
						claims.saturating_accrue(phase_one.compounded);
						ensure!(
							*claims <= checkpoint.activated,
							"member claims exceed the activated cohort capital"
						);
						// The survivor is active capital since the checkpoint: it counts against
						// the active total, not the pending one.
						let (phase_two, _) = Pallet::<T>::realize_leg(
							&collateral_id,
							&stable_id,
							Leg::Active,
							&pool,
							phase_one.compounded,
							&checkpoint.active_start,
						)
						.map_err(|_| "checkpoint active start has no sums row")?;
						compounded_sum.saturating_accrue(phase_two.compounded);
					},
				}
				*tallies.members.entry(pending.cohort).or_default() += 1;
			}

			let (realized, _) = Pallet::<T>::realize_leg(
				&collateral_id,
				&stable_id,
				Leg::Active,
				&pool,
				deposit.active_deposit,
				&deposit.snapshot,
			)
			.map_err(|_| "deposit snapshot has no sums row")?;
			compounded_sum.saturating_accrue(realized.compounded);
		}
		ensure!(
			pending_sum <= state.total_pending_deposits,
			"realized pending deposits exceed `total_pending_deposits`"
		);
		ensure!(
			compounded_sum <= state.total_active_deposits,
			"realized deposits exceed `total_active_deposits`"
		);

		// The cohort bookkeeping must cover the rows that reference it: member counts move in
		// step with the rows, and the ceiling-compounded aggregate never falls below the floored
		// member claims it will have to pay at advancement.
		for cohort in &state.open_cohorts {
			ensure!(
				tallies.members.get(&cohort.id).copied().unwrap_or(0) == cohort.members,
				"open cohort member count diverges from its rows"
			);
			let cover = crate::math::compound_ceil(
				cohort.amount,
				&cohort.coords,
				&state.pending_coords,
				config.precision.scale_factor(),
			);
			let claims =
				tallies.claims.get(&cohort.id).copied().unwrap_or_else(BalanceOf::<T>::zero);
			ensure!(claims <= cover, "member claims exceed the cohort aggregate");
		}
		for (id, checkpoint) in
			CohortCheckpoints::<T>::iter_prefix((collateral_id.clone(), stable_id.clone()))
		{
			ensure!(checkpoint.members > 0, "memberless checkpoint was not removed");
			ensure!(
				tallies.members.get(&id).copied().unwrap_or(0) == checkpoint.members,
				"checkpoint member count diverges from its rows"
			);
			ensure!(state.cohort(id).is_none(), "checkpointed cohort is still open");
			for snapshot in [&checkpoint.pending_end, &checkpoint.active_start] {
				ensure!(
					snapshot.coords.epoch <= state.coords.epoch.max(state.pending_coords.epoch),
					"checkpoint snapshot epoch ahead of the pool"
				);
			}
		}
	}

	// A pool row is what makes a market registered, so nothing may outlive one.
	for ((collateral_id, stable_id, _who), deposit) in Deposits::<T>::iter() {
		let state = Pools::<T>::get(&collateral_id, &stable_id)
			.ok_or("deposit row without a pool row")?
			.state;
		ensure!(
			deposit.snapshot.coords.epoch <= state.coords.epoch,
			"deposit snapshot epoch ahead of the pool"
		);
		if deposit.snapshot.coords.epoch == state.coords.epoch {
			ensure!(
				deposit.snapshot.coords.scale <= state.coords.scale,
				"deposit snapshot scale ahead of the pool"
			);
		}

		let Some(pending) = &deposit.pending_deposit else {
			continue;
		};
		ensure!(
			pending.snapshot.coords.epoch <= state.pending_coords.epoch,
			"pending snapshot epoch ahead of the pending accumulators"
		);
		if pending.snapshot.coords.epoch == state.pending_coords.epoch {
			ensure!(
				pending.snapshot.coords.scale <= state.pending_coords.scale,
				"pending snapshot scale ahead of the pending accumulators"
			);
		}
		ensure!(
			PoolSumsStore::<T>::contains_key((
				&collateral_id,
				&stable_id,
				Leg::Pending,
				pending.snapshot.coords.epoch,
				pending.snapshot.coords.scale,
			)),
			"pending snapshot references a pruned sums row"
		);
	}

	// Market removal clears every sums row: no row may outlive its pool.
	for (collateral_id, stable_id, _leg, _epoch, _scale) in PoolSumsStore::<T>::iter_keys() {
		ensure!(
			Pools::<T>::contains_key(&collateral_id, &stable_id),
			"sums row without a pool row"
		);
	}

	// No checkpoint may outlive the rows that reference it: every stored checkpoint belongs to a
	// registered market, and the per-market loop above already matched member counts.
	for (collateral_id, stable_id, _id) in CohortCheckpoints::<T>::iter_keys() {
		ensure!(
			Pools::<T>::contains_key(&collateral_id, &stable_id),
			"cohort checkpoint without a pool row"
		);
	}
	Ok(())
}
