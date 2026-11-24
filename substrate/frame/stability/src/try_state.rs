//! `try-runtime` checks for stability-pool accounting invariants.
//!
//! These checks protect five contracts:
//!
//! - Accumulators stay within the configured precision limits.
//! - Each leg's sums rows run gap-free within an epoch and never lead the leg's coordinates.
//! - Pool custody covers the recorded stablecoin and collateral totals.
//! - The sum of user positions does not exceed each pool total, and no stored row is empty.
//! - Cohort aggregates and checkpoints cover all member rows that reference them.

use crate::{
	pallet::{
		BalanceOf, CohortCheckpoints, CollateralIdOf, Config, Deposits, Pallet, PoolSumsStore,
		Pools, StableIdOf,
	},
	types::{CohortId, Leg, LegCoords},
};
use alloc::collections::BTreeMap;
use frame::{
	arithmetic::{FixedU128, One, Saturating, Zero},
	deps::{frame_support::traits::fungibles::Inspect as _, sp_runtime::TryRuntimeError},
	prelude::ensure,
};

/// Member counts and claims derived from the deposit rows of one branch.
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
		let mut active_min_scales = BTreeMap::<u32, u32>::new();
		let mut pending_min_scales = BTreeMap::<u32, u32>::new();
		let record_min = |map: &mut BTreeMap<u32, u32>, pos: LegCoords| {
			map.entry(pos.epoch)
				.and_modify(|min| *min = (*min).min(pos.scale))
				.or_insert(pos.scale);
		};
		record_min(&mut active_min_scales, state.coords.position());
		record_min(&mut pending_min_scales, state.pending_coords.position());
		for (_, deposit) in Deposits::<T>::iter_prefix((collateral_id.clone(), stable_id.clone())) {
			record_min(&mut active_min_scales, deposit.snapshot.coords.position());
			if let Some(pending) = &deposit.pending_deposit {
				record_min(&mut pending_min_scales, pending.snapshot.coords.position());
			}
		}
		for (_, checkpoint) in
			CohortCheckpoints::<T>::iter_prefix((collateral_id.clone(), stable_id.clone()))
		{
			record_min(&mut pending_min_scales, checkpoint.pending_end.coords.position());
			record_min(&mut active_min_scales, checkpoint.active_start.coords.position());
		}

		for leg in Leg::ALL {
			let coords = state.coords(leg);
			ensure!(coords.p <= FixedU128::one(), "`P` above one");
			ensure!(coords.p >= crate::math::P_MIN, "`P` below the precision floor");
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
			let min_scales = match leg {
				Leg::Active => &active_min_scales,
				Leg::Pending => &pending_min_scales,
			};
			check_sums_rows::<T>(&collateral_id, &stable_id, leg, coords.position(), min_scales)?;
		}

		// Direct transfers can add unowned surplus at any time. Obligations and the refundable
		// custody float must remain fully backed; donations leave through the removal sweep.
		let pool_account = Pallet::<T>::pool_account(&collateral_id, &stable_id);
		let stable_held = T::StableAssets::balance(stable_id.clone(), &pool_account);
		let stable_owed = state
			.total_active_deposits
			.saturating_add(state.total_pending_deposits)
			.saturating_add(state.total_yield_unclaimed)
			.saturating_add(pool.custody_stable);
		ensure!(stable_held >= stable_owed, "pool stablecoin balance below tracked totals");
		let collateral_held = T::CollateralAssets::balance(collateral_id.clone(), &pool_account);
		let collateral_owed =
			state.total_collateral_gains_unclaimed.saturating_add(pool.custody_collateral);
		ensure!(collateral_held >= collateral_owed, "pool collateral balance below tracked totals");
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
		let mut remaining_principal_sum = BalanceOf::<T>::zero();
		// A claim subtracts the stored claimable balance plus every gain the next settlement
		// realizes, so both count against the unclaimed totals.
		let mut collateral_sum = BalanceOf::<T>::zero();
		let mut yield_sum = BalanceOf::<T>::zero();
		let mut tallies = CohortTallies::<BalanceOf<T>>::default();
		for (_, deposit) in Deposits::<T>::iter_prefix((collateral_id.clone(), stable_id.clone())) {
			collateral_sum.saturating_accrue(deposit.claimable_collateral);
			yield_sum.saturating_accrue(deposit.claimable_yield);
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
						pending_sum.saturating_accrue(realized.remaining_principal);
						collateral_sum.saturating_accrue(realized.collateral_gain);
						tallies
							.claims
							.entry(pending.cohort)
							.or_default()
							.saturating_accrue(realized.remaining_principal);
					},
					(None, Some(checkpoint)) => {
						let phase_one = Pallet::<T>::realize_between(
							&collateral_id,
							&stable_id,
							Leg::Pending,
							pending.amount,
							&pending.snapshot,
							&checkpoint.pending_end,
						)
						.map_err(|_| "checkpoint window has no sums row")?;
						ensure!(
							phase_one.yield_gain.is_zero(),
							"checkpointed tranche realized a yield gain"
						);
						let claims = tallies.claims.entry(pending.cohort).or_default();
						claims.saturating_accrue(phase_one.remaining_principal);
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
							phase_one.remaining_principal,
							&checkpoint.active_start,
						)
						.map_err(|_| "checkpoint active start has no sums row")?;
						remaining_principal_sum.saturating_accrue(phase_two.remaining_principal);
						collateral_sum.saturating_accrue(phase_one.collateral_gain);
						collateral_sum.saturating_accrue(phase_two.collateral_gain);
						yield_sum.saturating_accrue(phase_two.yield_gain);
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
			remaining_principal_sum.saturating_accrue(realized.remaining_principal);
			collateral_sum.saturating_accrue(realized.collateral_gain);
			yield_sum.saturating_accrue(realized.yield_gain);
		}
		ensure!(
			pending_sum <= state.total_pending_deposits,
			"realized pending deposits exceed `total_pending_deposits`"
		);
		ensure!(
			remaining_principal_sum <= state.total_active_deposits,
			"realized deposits exceed `total_active_deposits`"
		);
		ensure!(
			collateral_sum <= state.total_collateral_gains_unclaimed,
			"claimable collateral exceeds `total_collateral_gains_unclaimed`"
		);
		ensure!(
			yield_sum <= state.total_yield_unclaimed,
			"claimable yield exceeds `total_yield_unclaimed`"
		);

		// The cohort bookkeeping must cover the rows that reference it: member counts move in
		// step with the rows, and the ceiling-compounded aggregate never falls below the floored
		// member claims it will have to pay at advancement.
		ensure!(
			state.cohorts_covered(&state.pending_coords, state.total_pending_deposits),
			"cohort reserves exceed total pending deposits"
		);
		for cohort in &state.open_cohorts {
			ensure!(
				tallies.members.get(&cohort.id).copied().unwrap_or(0) == cohort.members,
				"open cohort member count diverges from its rows"
			);
			let cover =
				crate::math::compound_ceil(cohort.amount, &cohort.coords, &state.pending_coords);
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

	// A pool row is what makes a branch registered, so nothing may outlive one.
	for ((collateral_id, stable_id, _who), deposit) in Deposits::<T>::iter() {
		let state = Pools::<T>::get(&collateral_id, &stable_id)
			.ok_or("deposit row without a pool row")?
			.state;
		// The deposit commit removes a row once it holds no value, so none may be stored.
		ensure!(!deposit.is_empty(), "empty deposit row was not removed");
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

	// Branch removal clears every sums row: no row may outlive its pool.
	for (collateral_id, stable_id, _leg, _epoch, _scale) in PoolSumsStore::<T>::iter_keys() {
		ensure!(
			Pools::<T>::contains_key(&collateral_id, &stable_id),
			"sums row without a pool row"
		);
	}

	// No checkpoint may outlive the rows that reference it: every stored checkpoint belongs to a
	// registered branch, and the per-branch loop above already matched member counts.
	for (collateral_id, stable_id, _id) in CohortCheckpoints::<T>::iter_keys() {
		ensure!(
			Pools::<T>::contains_key(&collateral_id, &stable_id),
			"cohort checkpoint without a pool row"
		);
	}
	Ok(())
}

/// Checks the sums rows of one leg against the leg's current `position`.
///
/// Within each epoch referenced by the live leg or a stored deposit/checkpoint, scales must run
/// without a gap from the minimum referenced scale to the epoch's highest stored scale, because
/// `Pallet::sums_window` ends a settlement window at the first absent scale: a gap would silently
/// drop the gains recorded beyond it. Offsets only ever seed rows up to the coordinates they move
/// the leg to, so no row may lie ahead of `position`.
fn check_sums_rows<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
	leg: Leg,
	position: LegCoords,
	min_referenced_scales: &BTreeMap<u32, u32>,
) -> Result<(), TryRuntimeError> {
	// Row iteration is hashed, so collect each epoch's stored scales first.
	let mut epochs = BTreeMap::<u32, alloc::collections::BTreeSet<u32>>::new();
	for ((epoch, scale), sums) in
		PoolSumsStore::<T>::iter_prefix((collateral_id.clone(), stable_id.clone(), leg))
	{
		ensure!(LegCoords { epoch, scale } <= position, "sums row ahead of the leg's coordinates");
		match leg {
			// Pending deposits earn no yield. Sharing the row type with the active leg must never
			// let a yield sum in through the back door.
			Leg::Pending => {
				ensure!(sums.g_yield.is_zero(), "pending sums row carries a nonzero `G`");
			},
			Leg::Active => {},
		}
		epochs.entry(epoch).or_default().insert(scale);
	}
	for (&epoch, &min_scale) in min_referenced_scales {
		let Some(scales) = epochs.get(&epoch) else {
			return Err("sums rows of an epoch have a scale gap".into());
		};
		let Some(&scale_max) = scales.last() else {
			return Err("sums rows of an epoch have a scale gap".into());
		};
		for scale in min_scale..=scale_max {
			ensure!(scales.contains(&scale), "sums rows of an epoch have a scale gap");
		}
	}
	Ok(())
}
