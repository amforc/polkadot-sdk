//! Internal accounting and value-transfer contracts of the dispatchable calls.
//!
//! Each row settles before a change. This rule makes the result independent of account touch
//! order.
//!
//! Accounting plans complete before their storage commits. Cross-pallet value movement follows
//! transactional interface contracts. These rules prevent partial accounting state.

use crate::{
	interfaces::OffsetReservation,
	math,
	pallet::{
		BalanceOf, CohortCheckpointOf, CohortCheckpoints, CollateralCreditOf, CollateralIdOf,
		Config, DepositOf, Deposits, Error, Event, Pallet, PoolStateOf, PoolSumsStore, Pools,
		StabilityPoolConfigOf, StabilityPoolOf, StableCreditOf, StableIdOf,
	},
	types::{
		Accumulators, CohortCheckpoint, CohortId, Deposit, DepositSnapshot, Leg, OpenCohort,
		PUpdate, PendingDeposit, PoolSums, Realized, RecoveryOffsetSource, SumsWindow,
		WithdrawalRequest,
	},
};
use frame::{
	prelude::*,
	traits::{
		fungibles::{Balanced as _, Inspect as _, Mutate as _},
		tokens::{Fortitude, Precision, Preservation, Provenance},
		Defensive, DefensiveOption, Time,
	},
};
use pusd_primitives::{
	debit_preservation, reducible_debit, BranchMode, BranchModeProvider, Millis,
	RecoveryOffsetInterface, RecoveryOffsetResult,
};

/// Asset type paid by [`Pallet::do_claim`].
#[derive(Clone, Copy)]
pub(crate) enum ClaimKind {
	Collateral,
	Yield,
}

/// Validated accounting state of one offset leg.
///
/// A complete plan makes the subsequent value movement and storage commit atomic.
struct OffsetPlan<Balance> {
	new_sums: PoolSums,
	new_unclaimed: Balance,
	new_total: Balance,
	new_coords: Accumulators,
}

/// Result of resolving one due cohort against the current pending leg.
pub(crate) struct CohortRoll<Balance> {
	id: CohortId,
	deadline: Millis,
	/// Capital moved from the pending total to the active total.
	survived: Balance,
	/// Unclamped cohort value recorded in the checkpoint.
	///
	/// This value is not less than the sum of the downward-rounded member claims.
	activated: Balance,
	members: u32,
}

/// Due cohorts of one activation, in deadline order.
pub(crate) type CohortRolls<Balance> = BoundedVec<CohortRoll<Balance>, ConstU32<2>>;

impl<T: Config> Pallet<T> {
	/// Returns the registered pool of a market.
	fn load_pool(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<StabilityPoolOf<T>, DispatchError> {
		let pool =
			Pools::<T>::get(collateral_id, stable_id).ok_or(Error::<T>::PoolNotRegistered)?;
		Ok(pool)
	}

	/// Applies all cohort deadlines at or before `now` to the supplied pool state.
	///
	/// The function changes no storage. Thus, read-only capacity inspection can use the same result
	/// that [`Pallet::advance_cohorts`] commits in settlement.
	pub(crate) fn roll_due_cohorts(
		pool: &mut StabilityPoolOf<T>,
		now: Millis,
	) -> Result<CohortRolls<BalanceOf<T>>, DispatchError> {
		let sf_int = pool.config.precision.scale_factor();
		let state = &mut pool.state;
		let mut rolls = CohortRolls::default();
		// Deadlines are ordered, so the due cohorts are a prefix of the open set.
		while let Some(cohort) = state.open_cohorts.first() {
			if cohort.deadline > now {
				break;
			}
			// The revaluation ceilings can overstate the aggregate by a sliver; the clamp keeps
			// the moved capital inside what the pending total actually tracks. The roll keeps
			// the unclamped resolution for the checkpoint: it stays at or above the floored
			// member claims even in the corner where the clamp bites.
			let resolved = math::compound_ceil::<BalanceOf<T>>(
				cohort.amount,
				&cohort.coords,
				&state.pending_coords,
				sf_int,
			);
			let cohort = state.open_cohorts.remove(0);
			let survived = resolved.min(state.total_pending_deposits);
			state.total_pending_deposits = state
				.total_pending_deposits
				.checked_sub(&survived)
				.ok_or(ArithmeticError::Underflow)?;
			state.total_active_deposits = state
				.total_active_deposits
				.checked_add(&survived)
				.ok_or(ArithmeticError::Overflow)?;
			rolls
				.try_push(CohortRoll {
					id: cohort.id,
					deadline: cohort.deadline,
					survived,
					activated: resolved,
					members: cohort.members,
				})
				.map_err(|_| DispatchError::Corruption)
				.defensive_proof("open cohorts are bounded to the same length as the rolls")?;
		}
		Ok(rolls)
	}

	/// Activates every due cohort and records checkpoints for later member settlement.
	///
	/// A frozen or unavailable mode prevents activation because activation changes capital risk.
	/// Row-local settlement remains available because it only records prior economic changes.
	///
	/// An error occurs before a storage write. The caller must not persist `pool` after an error.
	pub(crate) fn advance_cohorts(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		pool: &mut StabilityPoolOf<T>,
		now: Millis,
	) -> Result<bool, DispatchError> {
		if Self::ensure_not_frozen(collateral_id, stable_id).is_err() {
			return Ok(false);
		}
		let rolls = Self::roll_due_cohorts(pool, now)?;
		if rolls.is_empty() {
			return Ok(false);
		}
		// Every cohort due now materializes at this same instant, so one snapshot pair serves
		// them all: the pending leg ends here, and the active leg starts here.
		let state = &pool.state;
		let pending_end = {
			let sums = Self::sums_at(collateral_id, stable_id, Leg::Pending, &state.pending_coords);
			state.snapshot(Leg::Pending, &sums)
		};
		let active_start = {
			let sums = Self::sums_at(collateral_id, stable_id, Leg::Active, &state.coords);
			state.snapshot(Leg::Active, &sums)
		};
		for roll in rolls {
			// A memberless cohort would have been cleared on its last removal.
			debug_assert!(roll.members > 0);
			if roll.members > 0 {
				CohortCheckpoints::<T>::insert(
					(collateral_id, stable_id, roll.id),
					CohortCheckpoint {
						pending_end,
						active_start,
						activated: roll.activated,
						members: roll.members,
					},
				);
			}
			Self::deposit_event(Event::CohortActivated {
				collateral_id: collateral_id.clone(),
				stable_id: stable_id.clone(),
				cohort: roll.id,
				deadline: roll.deadline,
				amount: roll.survived,
			});
		}
		Ok(true)
	}

	/// Settles one market and deposit row at `now`.
	///
	/// The function applies due activations before active and pending settlement. This order makes
	/// later changes use the current risk classification.
	///
	/// The caller must persist the returned changes to `pool` after success.
	fn refresh_row(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		who: &T::AccountId,
		pool: &mut StabilityPoolOf<T>,
		deposit: &mut DepositOf<T>,
		now: Millis,
	) -> DispatchResult {
		Self::advance_cohorts(collateral_id, stable_id, pool, now)?;
		Self::realize_deposit(collateral_id, stable_id, pool, deposit)?;
		Self::settle_pending(collateral_id, stable_id, who, pool, deposit)
	}

	/// Applies a stablecoin deposit to recovery debt and then to the stability pool.
	pub(crate) fn do_deposit(
		who: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		ensure!(amount >= pool.config.minimum_deposit, Error::<T>::DepositTooSmall);

		let now = T::TimeProvider::now();
		let mut deposit =
			Self::load_or_fresh_deposit(&collateral_id, &stable_id, &who, &pool.state);
		Self::refresh_row(&collateral_id, &stable_id, &who, &mut pool, &mut deposit, now)?;

		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		// One withdrawal funds both halves: the recovery settlement takes its slice from the
		// credit, and the change becomes the pending deposit. `Expendable` only on a full drain,
		// so the withdrawal itself rejects an amount that would leave the depositor below the
		// minimum balance.
		let preservation =
			debit_preservation::<T::StableAssets, _>(stable_id.clone(), &who, amount);
		let payment = T::StableAssets::withdraw(
			stable_id.clone(),
			&who,
			amount,
			Precision::Exact,
			preservation,
			Fortitude::Polite,
		)?;
		let change = Self::try_incoming_recovery(
			&collateral_id,
			&stable_id,
			&pool_account,
			&mut pool.state,
			&mut deposit,
			payment,
		)?;
		// The settlement can only spend value the credit carried, so the difference is what it
		// spent.
		let pending_amount = change.peek();
		let used_for_recovery = amount.saturating_sub(pending_amount);

		if let Err(change) = change.drop_zero() {
			T::StableAssets::can_deposit(
				stable_id.clone(),
				&pool_account,
				pending_amount,
				Provenance::Extant,
			)
			.into_result()?;
			let _ = T::StableAssets::resolve(&pool_account, change)
				.defensive_proof("`can_deposit` just passed; qed");
			Self::queue_pending(
				&collateral_id,
				&stable_id,
				&mut pool,
				&mut deposit,
				pending_amount,
				now,
			)?;
		}

		// A deposit spent in full leaves nothing but the recovery collateral on the row, or
		// nothing at all if that collateral rounded to zero.
		Self::store_or_prune_deposit(&collateral_id, &stable_id, &who, deposit);
		Pools::<T>::insert(&collateral_id, &stable_id, pool);
		Self::deposit_event(Event::DepositReceived {
			collateral_id,
			stable_id,
			depositor: who,
			amount,
			used_for_recovery,
			pending_amount,
		});
		Ok(())
	}

	/// Applies an incoming deposit to the `FinalRecovery` queue head and returns the unused credit.
	///
	/// The depositor receives the collateral from this settlement. The settled stablecoin does not
	/// enter pool custody, so it has no claim on prior pool gains.
	///
	/// A below-par queue head rejects the deposit because discounted settlement belongs to the
	/// redemption path.
	fn try_incoming_recovery(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		pool_account: &T::AccountId,
		state: &mut PoolStateOf<T>,
		deposit: &mut DepositOf<T>,
		payment: StableCreditOf<T>,
	) -> Result<StableCreditOf<T>, DispatchError> {
		let amount = payment.peek();
		let (result, change) =
			T::RecoveryOffsets::execute_recovery_offset(collateral_id, payment, pool_account)?;
		let collateral_out = match result {
			RecoveryOffsetResult::NoTarget => {
				debug_assert_eq!(change.peek(), amount);
				return Ok(change);
			},
			RecoveryOffsetResult::BelowPar => {
				// The dropped change unwinds with the failing extrinsic.
				return Err(Error::<T>::RecoveryOffsetBelowPar.into());
			},
			RecoveryOffsetResult::Applied { collateral_out } => collateral_out,
		};
		// The settlement can only spend value the credit carried, so the difference is what it
		// spent.
		let used_for_recovery = amount.saturating_sub(change.peek());

		deposit.claimable_collateral = deposit
			.claimable_collateral
			.checked_add(&collateral_out)
			.ok_or(ArithmeticError::Overflow)?;
		state.total_collateral_gains_unclaimed = state
			.total_collateral_gains_unclaimed
			.checked_add(&collateral_out)
			.ok_or(ArithmeticError::Overflow)?;
		Self::deposit_event(Event::RecoveryOffsetApplied {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			debt_burned: used_for_recovery,
			collateral_gain: collateral_out,
			source: RecoveryOffsetSource::IncomingDeposit,
		});
		Ok(change)
	}

	/// Adds funded stablecoin to pending capital or directly to active capital when the delay is
	/// zero.
	///
	/// The caller must first use [`Pallet::refresh_row`]. This requirement updates the current
	/// pending amount before the merge.
	///
	/// A merge restarts the entry delay for all pending capital of the depositor.
	fn queue_pending(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		pool: &mut StabilityPoolOf<T>,
		deposit: &mut DepositOf<T>,
		pending_amount: BalanceOf<T>,
		now: Millis,
	) -> DispatchResult {
		debug_assert!(!pending_amount.is_zero());
		if pool.config.entry_delay == 0 {
			// There is no wait to enforce: the capital is active from the start. The refresh
			// already reset the row snapshot to the live active coordinates.
			deposit.active_deposit = deposit
				.active_deposit
				.checked_add(&pending_amount)
				.ok_or(ArithmeticError::Overflow)?;
			pool.state.total_active_deposits = pool
				.state
				.total_active_deposits
				.checked_add(&pending_amount)
				.ok_or(ArithmeticError::Overflow)?;
			return Ok(());
		}

		// A merge pulls the old tranche out of its cohort first. Its snapshot stands at the live
		// coordinates, so the revalued aggregate carries exactly `pending.amount` for it.
		let sf_int = pool.config.precision.scale_factor();
		let live = pool.state.pending_coords;
		let merged = match deposit.pending_deposit.take() {
			Some(pending) => {
				// An open tranche always has its cohort.
				let cohort = pool
					.state
					.cohort_mut(pending.cohort)
					.defensive_ok_or(DispatchError::Corruption)?;
				cohort.revalue(&live, sf_int);
				cohort.amount =
					cohort.amount.checked_sub(&pending.amount).ok_or(ArithmeticError::Underflow)?;
				cohort.members = cohort.members.saturating_sub(1);
				if cohort.members == 0 {
					pool.state.remove_cohort(pending.cohort);
				}
				pending.amount.checked_add(&pending_amount).ok_or(ArithmeticError::Overflow)?
			},
			None => pending_amount,
		};

		let deadline = math::cohort_deadline(now, pool.config.entry_delay);
		let snapshot = {
			let current = Self::sums_at(collateral_id, stable_id, Leg::Pending, &live);
			pool.state.snapshot(Leg::Pending, &current)
		};
		let cohort = Self::join_cohort(&mut pool.state, deadline, live)?;
		cohort.revalue(&live, sf_int);
		cohort.members = cohort.members.checked_add(1).ok_or(ArithmeticError::Overflow)?;
		cohort.amount = cohort.amount.checked_add(&merged).ok_or(ArithmeticError::Overflow)?;
		deposit.pending_deposit =
			Some(PendingDeposit { amount: merged, cohort: cohort.id, snapshot });
		pool.state.total_pending_deposits = pool
			.state
			.total_pending_deposits
			.checked_add(&pending_amount)
			.ok_or(ArithmeticError::Overflow)?;
		Ok(())
	}

	/// Returns the open cohort that can satisfy the `required` activation deadline.
	///
	/// The function selects the earliest sufficient deadline to minimize the safe wait. If the
	/// bounded set is full, it can move only the last deadline later.
	///
	/// A later deadline preserves the minimum entry delay for all members after a configuration
	/// change.
	fn join_cohort(
		state: &mut PoolStateOf<T>,
		required: Millis,
		coords: Accumulators,
	) -> Result<&mut OpenCohort<BalanceOf<T>>, DispatchError> {
		if let Some(index) =
			state.open_cohorts.iter().position(|cohort| cohort.deadline >= required)
		{
			return Ok(&mut state.open_cohorts[index]);
		}

		if state.open_cohorts.is_full() {
			let cohort = state.open_cohorts.last_mut().expect("a full set is nonempty; qed");
			cohort.deadline = required;
			return Ok(cohort);
		}

		let id = state.next_cohort_id;
		state.next_cohort_id = CohortId(id.0.checked_add(1).ok_or(ArithmeticError::Overflow)?);
		state
			.open_cohorts
			.try_push(OpenCohort::fresh(id, required, coords))
			.map_err(|_| DispatchError::Corruption)
			.defensive_proof("capacity was checked above")?;
		Ok(state.open_cohorts.last_mut().expect("a cohort was just pushed; qed"))
	}

	/// Settles active pool stablecoin against the `FinalRecovery` queue head.
	///
	/// The function applies the active offset rules so liquidation and recovery settlement give
	/// depositors the same accounting and rounding treatment.
	pub(crate) fn do_offset_recovery(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		max_stable_in: BalanceOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		// Settling recovery debt reduces risk, so Safety Mode allows it. Only a freeze stops it.
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		// Matured capital must stand in the active total before the offset is sized against it.
		Self::advance_cohorts(&collateral_id, &stable_id, &mut pool, T::TimeProvider::now())?;

		// Size the burn before touching anything. Pool depth and the post-offset floor cap the
		// accounting, and what the account may actually pay caps that in turn, so a minimum
		// balance rounds the offset down instead of stranding the pool account.
		let accounting_cap = math::clamp_offset_debt(
			max_stable_in,
			pool.state.total_active_deposits,
			pool.config.minimum_active_pool_balance,
		);
		ensure!(!accounting_cap.is_zero(), Error::<T>::NoRecoveryOffsetPerformed);
		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		let (funded, preservation) =
			reducible_debit::<T::StableAssets, _>(stable_id.clone(), &pool_account, accounting_cap);
		ensure!(!funded.is_zero(), Error::<T>::NoRecoveryOffsetPerformed);

		let payment = T::StableAssets::withdraw(
			stable_id.clone(),
			&pool_account,
			funded,
			Precision::Exact,
			preservation,
			Fortitude::Polite,
		)?;
		let (result, change) =
			T::RecoveryOffsets::execute_recovery_offset(&collateral_id, payment, &pool_account)?;
		let collateral_out = match result {
			// The dropped change unwinds with the failing extrinsic.
			RecoveryOffsetResult::NoTarget => {
				return Err(Error::<T>::RecoveryVaultNotFound.into());
			},
			RecoveryOffsetResult::BelowPar => {
				return Err(Error::<T>::RecoveryOffsetBelowPar.into());
			},
			RecoveryOffsetResult::Applied { collateral_out } => collateral_out,
		};
		// The settlement can only spend value the credit carried, so the difference is what it
		// spent.
		let debt_cancelled = funded.saturating_sub(change.peek());
		ensure!(!debt_cancelled.is_zero(), Error::<T>::NoRecoveryOffsetPerformed);
		if let Err(change) = change.drop_zero() {
			// Put the unspent slice back. This can only fail after a full drain whose head took
			// less than everything and left a change below the minimum balance. Failing here asks
			// the caller to size `max_stable_in` from the preview, which is better than stranding
			// the pool account.
			T::StableAssets::can_deposit(
				stable_id.clone(),
				&pool_account,
				change.peek(),
				Provenance::Extant,
			)
			.into_result()?;
			let _ = T::StableAssets::resolve(&pool_account, change)
				.defensive_proof("`can_deposit` just passed; qed");
		}

		Self::apply_active_offset(
			&collateral_id,
			&stable_id,
			&mut pool,
			debt_cancelled,
			collateral_out,
		)?;
		Pools::<T>::insert(&collateral_id, &stable_id, pool);
		Self::deposit_event(Event::RecoveryOffsetApplied {
			collateral_id,
			stable_id,
			debt_burned: debt_cancelled,
			collateral_gain: collateral_out,
			source: RecoveryOffsetSource::ActivePool,
		});
		Ok(())
	}

	/// Records a Safety-mode withdrawal request or withdraws immediately in Normal mode.
	///
	/// A new Safety-mode request replaces the current request. Normal mode needs no request, so the
	/// function pays the caller directly.
	pub(crate) fn do_request_withdraw(
		who: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		let mode = Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		ensure!(!amount.is_zero(), Error::<T>::ZeroAmount);
		match mode {
			BranchMode::Normal => {
				return Self::do_withdraw(who.clone(), collateral_id, stable_id, amount, who);
			},
			BranchMode::Safety => {},
			BranchMode::Frozen => return Err(Error::<T>::BranchFrozen.into()),
		}
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;

		let now = T::TimeProvider::now();
		Self::refresh_row(&collateral_id, &stable_id, &who, &mut pool, &mut deposit, now)?;

		let executable_at = now.saturating_add(pool.config.safety_withdrawal_delay);
		deposit.withdrawal_request = Some(WithdrawalRequest { amount, executable_at });

		Self::store_or_prune_deposit(&collateral_id, &stable_id, &who, deposit);
		Pools::<T>::insert(&collateral_id, &stable_id, pool);
		Self::deposit_event(Event::WithdrawalRequested {
			collateral_id,
			stable_id,
			depositor: who,
			amount,
			executable_at,
		});
		Ok(())
	}

	/// Pays authorized active stablecoin from pool custody.
	pub(crate) fn do_withdraw(
		who: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
		recipient: T::AccountId,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;
		ensure!(!amount.is_zero(), Error::<T>::ZeroAmount);

		let now = T::TimeProvider::now();
		Self::refresh_row(&collateral_id, &stable_id, &who, &mut pool, &mut deposit, now)?;

		let mode = Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let take = Self::resolve_withdrawal(mode, now, amount, &mut deposit)?;
		ensure!(!take.is_zero(), Error::<T>::NoActiveDeposit);

		// `resolve_withdrawal` bounds `take` by the realized active deposit, which rounding keeps
		// at or below the pool total.
		deposit.active_deposit =
			deposit.active_deposit.checked_sub(&take).ok_or(ArithmeticError::Underflow)?;
		pool.state.total_active_deposits = pool
			.state
			.total_active_deposits
			.checked_sub(&take)
			.ok_or(ArithmeticError::Underflow)?;

		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		// `Expendable` only on a full drain, so the transfer itself rejects a payout that would
		// leave the pool account below the minimum balance.
		let preservation =
			debit_preservation::<T::StableAssets, _>(stable_id.clone(), &pool_account, take);
		T::StableAssets::transfer(
			stable_id.clone(),
			&pool_account,
			&recipient,
			take,
			preservation,
		)?;

		Self::store_or_prune_deposit(&collateral_id, &stable_id, &who, deposit);
		Pools::<T>::insert(&collateral_id, &stable_id, pool);
		Self::deposit_event(Event::WithdrawalExecuted {
			collateral_id,
			stable_id,
			depositor: who,
			recipient,
			amount: take,
		});
		Ok(())
	}

	/// Pays one claimable asset from pool custody.
	///
	/// The claim reduces the user balance and its corresponding pool total by the same amount.
	pub(crate) fn do_claim(
		who: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		recipient: T::AccountId,
		kind: ClaimKind,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;

		Self::refresh_row(
			&collateral_id,
			&stable_id,
			&who,
			&mut pool,
			&mut deposit,
			T::TimeProvider::now(),
		)?;

		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		let amount = match kind {
			ClaimKind::Collateral => {
				let amount = Self::take_claim(
					&mut deposit.claimable_collateral,
					&mut pool.state.total_collateral_gains_unclaimed,
					Error::<T>::NoClaimableCollateral,
				)?;
				Self::pay_claim::<T::CollateralAssets>(
					collateral_id.clone(),
					&pool_account,
					&recipient,
					amount,
				)?;
				amount
			},
			ClaimKind::Yield => {
				let amount = Self::take_claim(
					&mut deposit.claimable_yield,
					&mut pool.state.total_yield_unclaimed,
					Error::<T>::NoClaimableYield,
				)?;
				Self::pay_claim::<T::StableAssets>(
					stable_id.clone(),
					&pool_account,
					&recipient,
					amount,
				)?;
				amount
			},
		};

		Self::store_or_prune_deposit(&collateral_id, &stable_id, &who, deposit);
		Pools::<T>::insert(&collateral_id, &stable_id, pool);
		Self::deposit_event(match kind {
			ClaimKind::Collateral => Event::CollateralClaimed {
				collateral_id,
				stable_id,
				depositor: who,
				recipient,
				amount,
			},
			ClaimKind::Yield => {
				Event::YieldClaimed { collateral_id, stable_id, depositor: who, recipient, amount }
			},
		});
		Ok(())
	}

	/// Returns the debt that `leg` can cancel and the required `Preservation` rule.
	///
	/// Four limits apply:
	///
	/// - The capital available in the leg.
	/// - The required balance after a partial offset.
	/// - The reducible pool-account balance.
	/// - The supported precision range of `P`.
	///
	/// `reserved` is debt that another leg will burn from the same account first. This input keeps
	/// both reservations valid against one custody balance.
	///
	/// Capacity includes the precision limit so a caller does not allocate collateral to an invalid
	/// burn. The active leg must settle before a reserved pending leg.
	///
	/// `minimum_active_pool_balance` also protects pending `P` because both legs use the same
	/// precision parameters.
	///
	/// [`Pallet::offset`]: pusd_primitives::StabilityPoolOffset::offset
	pub(crate) fn size_offset(
		pool: &StabilityPoolOf<T>,
		stable_id: &StableIdOf<T>,
		pool_account: &T::AccountId,
		leg: Leg,
		max_debt: BalanceOf<T>,
		reserved: BalanceOf<T>,
	) -> Option<(BalanceOf<T>, Preservation)> {
		let total = pool.state.total(leg);
		let accounting_cap =
			math::clamp_offset_debt(max_debt, total, pool.config.minimum_active_pool_balance);
		if accounting_cap.is_zero() {
			return None;
		}
		// What the account may pay caps the accounting, so a minimum balance rounds the offset
		// down instead of stranding the pool account.
		let (headroom, preservation) = reducible_debit::<T::StableAssets, _>(
			stable_id.clone(),
			pool_account,
			accounting_cap.saturating_add(reserved),
		);
		let debt = headroom.saturating_sub(reserved).min(accounting_cap);
		if debt.is_zero() {
			return None;
		}
		math::update_p_after_offset(pool.state.coords(leg).p, total, debt, &pool.config.precision)?;
		Some((debt, preservation))
	}

	/// Removes a claimable balance and the same amount from its pool obligation.
	///
	/// An underflow indicates that the user claim exceeds the recorded pool obligation.
	fn take_claim(
		claimable: &mut BalanceOf<T>,
		unclaimed_total: &mut BalanceOf<T>,
		empty_error: Error<T>,
	) -> Result<BalanceOf<T>, DispatchError> {
		let amount = *claimable;
		ensure!(!amount.is_zero(), empty_error);
		*claimable = BalanceOf::<T>::zero();
		*unclaimed_total =
			unclaimed_total.checked_sub(&amount).ok_or(ArithmeticError::Underflow)?;
		Ok(amount)
	}

	/// Pays an exact claim from the pool account.
	///
	/// The transfer uses `Expendable` only for a full drain. A partial claim cannot leave the pool
	/// account below the asset minimum balance.
	fn pay_claim<Assets>(
		asset_id: Assets::AssetId,
		pool_account: &T::AccountId,
		recipient: &T::AccountId,
		amount: BalanceOf<T>,
	) -> DispatchResult
	where
		Assets: frame::traits::fungibles::Mutate<T::AccountId, Balance = BalanceOf<T>>,
	{
		let preservation = debit_preservation::<Assets, _>(asset_id.clone(), pool_account, amount);
		Assets::transfer(asset_id, pool_account, recipient, amount, preservation)?;
		Ok(())
	}

	/// Settles one exact debt and collateral reservation on `leg`.
	///
	/// The function burns the stablecoin and allocates all supplied collateral through `S`. A
	/// reservation mismatch aborts the complete transaction.
	pub(crate) fn settle_offset(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		pool_account: &T::AccountId,
		leg: Leg,
		pool: &mut StabilityPoolOf<T>,
		reservation: OffsetReservation<BalanceOf<T>>,
		collateral: CollateralCreditOf<T>,
	) -> DispatchResult {
		ensure!(collateral.asset() == *collateral_id, Error::<T>::OffsetSettlementFailed);
		let plan = Self::plan_offset(
			collateral_id,
			stable_id,
			leg,
			pool,
			reservation.debt,
			collateral.peek(),
		)?;
		let collateral_amount =
			Self::settle_reservation_exact(stable_id, pool_account, reservation, collateral)?;
		Self::commit_offset(collateral_id, stable_id, leg, &mut pool.state, plan);
		let coords = pool.state.coords(leg);
		Self::deposit_event(match leg {
			Leg::Active => Event::PoolOffsetApplied {
				collateral_id: collateral_id.clone(),
				stable_id: stable_id.clone(),
				debt_burned: reservation.debt,
				collateral_gain: collateral_amount,
				epoch: coords.epoch,
				scale: coords.scale,
			},
			Leg::Pending => Event::PendingDepositOffsetApplied {
				collateral_id: collateral_id.clone(),
				stable_id: stable_id.clone(),
				debt_burned: reservation.debt,
				collateral_gain: collateral_amount,
				epoch: coords.epoch,
				scale: coords.scale,
			},
		});
		Ok(())
	}

	/// Burns the reserved stablecoin and resolves all collateral into pool custody.
	///
	/// Exact movement keeps debt cancellation equal to the quoted amount. The function returns the
	/// collateral amount received.
	fn settle_reservation_exact(
		stable_id: &StableIdOf<T>,
		pool_account: &T::AccountId,
		reservation: OffsetReservation<BalanceOf<T>>,
		collateral: CollateralCreditOf<T>,
	) -> Result<BalanceOf<T>, DispatchError> {
		let collateral_amount = collateral.peek();
		let stable_credit = T::StableAssets::withdraw(
			stable_id.clone(),
			pool_account,
			reservation.debt,
			Precision::Exact,
			reservation.preservation,
			Fortitude::Polite,
		)
		.map_err(|_| Error::<T>::OffsetSettlementFailed)?;
		debug_assert_eq!(stable_credit.peek(), reservation.debt);
		// A zero credit is dropped without touching the account, whose only possible failure is a
		// zero deposit into an account that does not exist yet. A real failure means a first gain
		// below the minimum balance.
		if let Err(collateral) = collateral.drop_zero() {
			T::CollateralAssets::resolve(pool_account, collateral)
				.map_err(|_| Error::<T>::OffsetSettlementFailed)?;
		}
		// Dropping the withdrawn credit is what cancels the debt.
		drop(stable_credit);
		Ok(collateral_amount)
	}

	/// Returns the validated state after an offset without a storage write.
	///
	/// `S` uses the deposit total before the offset. `P` then records the principal loss. This
	/// order gives collateral to the capital that paid the debt.
	fn plan_offset(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		pool: &StabilityPoolOf<T>,
		debt: BalanceOf<T>,
		collateral: BalanceOf<T>,
	) -> Result<OffsetPlan<BalanceOf<T>>, DispatchError> {
		let state = &pool.state;
		let coords = state.coords(leg);
		let total = state.total(leg);
		debug_assert!(!debt.is_zero());
		debug_assert!(debt <= total);

		let mut sums = Self::sums_at(collateral_id, stable_id, leg, coords);
		let delta_s =
			math::delta_sum(collateral, coords.p, total).ok_or(ArithmeticError::Overflow)?;
		sums.s_collateral =
			sums.s_collateral.checked_add(&delta_s).ok_or(ArithmeticError::Overflow)?;
		let new_unclaimed = state
			.total_collateral_gains_unclaimed
			.checked_add(&collateral)
			.ok_or(ArithmeticError::Overflow)?;

		let update = math::update_p_after_offset(coords.p, total, debt, &pool.config.precision)
			.ok_or(Error::<T>::UnsupportedOffsetPrecision)?;
		let (new_coords, new_total) = match update {
			PUpdate::Depleted => (
				Accumulators {
					p: FixedU128::one(),
					epoch: coords.epoch.checked_add(1).ok_or(ArithmeticError::Overflow)?,
					scale: 0,
				},
				BalanceOf::<T>::zero(),
			),
			PUpdate::Updated { new_p, scales_crossed } => (
				Accumulators {
					p: new_p,
					epoch: coords.epoch,
					scale: coords
						.scale
						.checked_add(scales_crossed)
						.ok_or(ArithmeticError::Overflow)?,
				},
				total.checked_sub(&debt).ok_or(ArithmeticError::Underflow)?,
			),
		};
		Ok(OffsetPlan { new_sums: sums, new_unclaimed, new_total, new_coords })
	}

	/// Commits a validated [`OffsetPlan`] and creates required accumulator rows.
	///
	/// [`Pallet::plan_offset`] completes all fallible arithmetic before this function.
	fn commit_offset(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		state: &mut PoolStateOf<T>,
		plan: OffsetPlan<BalanceOf<T>>,
	) {
		let coords = *state.coords(leg);
		PoolSumsStore::<T>::insert(
			(collateral_id, stable_id, leg, coords.epoch, coords.scale),
			plan.new_sums,
		);
		if plan.new_coords.epoch == coords.epoch {
			// Bounded by `math::MAX_SCALE_CROSSINGS`.
			for scale in coords.scale.saturating_add(1)..=plan.new_coords.scale {
				PoolSumsStore::<T>::insert(
					(collateral_id, stable_id, leg, coords.epoch, scale),
					PoolSums::default(),
				);
			}
		} else {
			PoolSumsStore::<T>::insert(
				(collateral_id, stable_id, leg, plan.new_coords.epoch, 0u32),
				PoolSums::default(),
			);
		}
		*state.coords_mut(leg) = plan.new_coords;
		*state.total_mut(leg) = plan.new_total;
		state.total_collateral_gains_unclaimed = plan.new_unclaimed;
	}

	/// Applies an active-leg recovery offset after the recovery interface moves the value.
	fn apply_active_offset(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		pool: &mut StabilityPoolOf<T>,
		debt: BalanceOf<T>,
		collateral: BalanceOf<T>,
	) -> DispatchResult {
		let plan =
			Self::plan_offset(collateral_id, stable_id, Leg::Active, pool, debt, collateral)?;
		Self::commit_offset(collateral_id, stable_id, Leg::Active, &mut pool.state, plan);
		Ok(())
	}

	/// Allocates stablecoin yield through `G` and returns the unallocated credit.
	///
	/// Yield routing must not fail the operation that produced the yield. If the pool cannot accept
	/// the credit, the caller receives it unchanged.
	///
	/// The `OnBranchYield` implementation applies `yield_share` before it calls this function.
	pub(crate) fn do_distribute_yield(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		mut pool: StabilityPoolOf<T>,
		credit: StableCreditOf<T>,
	) -> StableCreditOf<T> {
		if credit.peek().is_zero() {
			return credit;
		}
		// A frozen market, or one whose mode cannot be read, takes no yield.
		match T::BranchModes::branch_mode(collateral_id, stable_id) {
			Ok(BranchMode::Normal) | Ok(BranchMode::Safety) => {},
			Ok(BranchMode::Frozen) | Err(_) => return credit,
		}

		// Matured capital enters the denominator before the new yield is shared out, so a
		// deposit earns everything distributed after its deadline and nothing before it.
		// Advancement is a complete transition of its own: it persists even when the
		// distribution behind it bails out.
		let advanced = match Self::advance_cohorts(
			collateral_id,
			stable_id,
			&mut pool,
			T::TimeProvider::now(),
		) {
			Ok(advanced) => advanced,
			Err(_) => return credit,
		};

		let (credit, pool_changed) =
			match Self::distribute_into_sums(collateral_id, stable_id, &mut pool, credit) {
				Ok(zero) => (zero, true),
				Err(credit) => (credit, advanced),
			};
		if pool_changed {
			Pools::<T>::insert(collateral_id, stable_id, pool);
		}
		credit
	}

	/// Adds a yield credit to the current `G` row and pool obligations.
	///
	/// All validation occurs before credit resolution. An error therefore returns the complete
	/// credit and leaves pool custody unchanged.
	///
	/// After successful resolution, the caller must persist `pool`.
	fn distribute_into_sums(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		pool: &mut StabilityPoolOf<T>,
		credit: StableCreditOf<T>,
	) -> Result<StableCreditOf<T>, StableCreditOf<T>> {
		let amount = credit.peek();
		if pool.state.total_active_deposits.is_zero() {
			return Err(credit);
		}
		let Some(delta_g) = pool.state.delta_sum(amount) else {
			return Err(credit);
		};
		let mut sums = Self::sums_at(collateral_id, stable_id, Leg::Active, &pool.state.coords);
		let Some(new_g) = sums.g_yield.checked_add(&delta_g) else {
			return Err(credit);
		};
		let Some(new_total_yield) = pool.state.total_yield_unclaimed.checked_add(&amount) else {
			return Err(credit);
		};

		let pool_account = Self::pool_account(collateral_id, stable_id);
		let credit = match T::StableAssets::resolve(&pool_account, credit) {
			Ok(()) => StableCreditOf::<T>::zero(stable_id.clone()),
			Err(credit) => return Err(credit),
		};

		sums.g_yield = new_g;
		PoolSumsStore::<T>::insert(
			(
				collateral_id,
				stable_id,
				Leg::Active,
				pool.state.coords.epoch,
				pool.state.coords.scale,
			),
			sums,
		);
		pool.state.total_yield_unclaimed = new_total_yield;
		Self::deposit_event(Event::YieldDistributed {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			amount,
		});
		Ok(credit)
	}

	/// Moves claimable yield into the active deposit.
	///
	/// The stablecoin remains in pool custody. The amount joins at the current accumulators so it
	/// receives no gains from before compounding.
	pub(crate) fn do_compound_yield(
		who: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;

		Self::refresh_row(
			&collateral_id,
			&stable_id,
			&who,
			&mut pool,
			&mut deposit,
			T::TimeProvider::now(),
		)?;

		let take = amount.min(deposit.claimable_yield);
		ensure!(!take.is_zero(), Error::<T>::NoYieldToCompound);
		deposit.claimable_yield =
			deposit.claimable_yield.checked_sub(&take).ok_or(ArithmeticError::Underflow)?;
		deposit.active_deposit =
			deposit.active_deposit.checked_add(&take).ok_or(ArithmeticError::Overflow)?;
		pool.state.total_active_deposits = pool
			.state
			.total_active_deposits
			.checked_add(&take)
			.ok_or(ArithmeticError::Overflow)?;
		// An underflow would mean a row claiming more than the pool tracks.
		pool.state.total_yield_unclaimed = pool
			.state
			.total_yield_unclaimed
			.checked_sub(&take)
			.ok_or(ArithmeticError::Underflow)?;

		Self::store_or_prune_deposit(&collateral_id, &stable_id, &who, deposit);
		Pools::<T>::insert(&collateral_id, &stable_id, pool);
		Self::deposit_event(Event::YieldCompounded {
			collateral_id,
			stable_id,
			depositor: who,
			amount: take,
		});
		Ok(())
	}

	/// Settles the position of another account against the current pool state.
	///
	/// The function moves no value. A frozen market prevents new activation because activation
	/// changes risk. Settlement of prior losses and gains remains available.
	pub(crate) fn do_settle_deposit(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &owner))
			.ok_or(Error::<T>::DepositNotFound)?;

		Self::refresh_row(
			&collateral_id,
			&stable_id,
			&owner,
			&mut pool,
			&mut deposit,
			T::TimeProvider::now(),
		)?;
		Self::store_or_prune_deposit(&collateral_id, &stable_id, &owner, deposit);
		Pools::<T>::insert(&collateral_id, &stable_id, pool);
		Ok(())
	}

	/// Returns the stablecoin amount authorized by the current operating mode.
	///
	/// - `Normal` permits up to the active deposit and ignores a prior Safety-mode request.
	/// - `Safety` requires a request at or after `executable_at` and reduces its authorized amount.
	/// - `Frozen` rejects the withdrawal.
	pub(crate) fn resolve_withdrawal(
		mode: BranchMode,
		now: Millis,
		amount: BalanceOf<T>,
		deposit: &mut DepositOf<T>,
	) -> Result<BalanceOf<T>, DispatchError> {
		match mode {
			BranchMode::Normal => Ok(amount.min(deposit.active_deposit)),
			BranchMode::Safety => {
				let request = deposit
					.withdrawal_request
					.as_mut()
					.ok_or(Error::<T>::WithdrawalRequestMissing)?;
				ensure!(now >= request.executable_at, Error::<T>::SafetyWithdrawalDelayActive);
				let take = amount.min(request.amount).min(deposit.active_deposit);
				request.amount = request.amount.saturating_sub(take);
				if request.amount.is_zero() {
					deposit.withdrawal_request = None;
				}
				Ok(take)
			},
			BranchMode::Frozen => Err(Error::<T>::BranchFrozen.into()),
		}
	}

	/// Returns the current mode or rejects a frozen market.
	///
	/// An unavailable mode also causes rejection. This behavior prevents value movement without
	/// current market-risk information.
	pub(crate) fn ensure_not_frozen(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<BranchMode, DispatchError> {
		let mode = T::BranchModes::branch_mode(collateral_id, stable_id)?;
		ensure!(mode != BranchMode::Frozen, Error::<T>::BranchFrozen);
		Ok(mode)
	}

	/// Settles active principal and gains, then resets the active snapshot.
	///
	/// An offset changes pool totals when it occurs. This function only allocates those changes to
	/// one row, so it must not change the totals again.
	///
	/// [`Pallet::settle_pending`] settles pending capital separately.
	fn realize_deposit(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		pool: &StabilityPoolOf<T>,
		deposit: &mut DepositOf<T>,
	) -> DispatchResult {
		let (realized, snapshot) = Self::realize_leg(
			collateral_id,
			stable_id,
			Leg::Active,
			pool,
			deposit.active_deposit,
			&deposit.snapshot,
		);
		deposit.active_deposit = realized.compounded;
		deposit.claimable_collateral = deposit
			.claimable_collateral
			.checked_add(&realized.collateral_gain)
			.ok_or(ArithmeticError::Overflow)?;
		deposit.claimable_yield = deposit
			.claimable_yield
			.checked_add(&realized.yield_gain)
			.ok_or(ArithmeticError::Overflow)?;
		deposit.snapshot = snapshot;
		Ok(())
	}

	/// Settles the pending position before or after its cohort activation.
	///
	/// The caller must first use [`Pallet::realize_deposit`]. This requirement gives activated
	/// capital the current active snapshot before the merge.
	fn settle_pending(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		who: &T::AccountId,
		pool: &mut StabilityPoolOf<T>,
		deposit: &mut DepositOf<T>,
	) -> DispatchResult {
		let Some(pending) = deposit.pending_deposit.take() else {
			return Ok(());
		};
		match CohortCheckpoints::<T>::get((collateral_id, stable_id, pending.cohort)) {
			Some(checkpoint) => Self::settle_through_checkpoint(
				collateral_id,
				stable_id,
				who,
				pool,
				deposit,
				pending,
				checkpoint,
			),
			None => {
				deposit.pending_deposit =
					Self::realize_open_pending(collateral_id, stable_id, pool, deposit, pending)?;
				Ok(())
			},
		}
	}

	/// Settles an activated position across its pending and active accounting phases.
	///
	/// The checkpoint ends the pending phase at `pending_end` and starts the active phase at
	/// `active_start`. The principal that remains joins the active deposit.
	fn settle_through_checkpoint(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		who: &T::AccountId,
		pool: &StabilityPoolOf<T>,
		deposit: &mut DepositOf<T>,
		pending: PendingDeposit<BalanceOf<T>>,
		checkpoint: CohortCheckpointOf<T>,
	) -> DispatchResult {
		let window = Self::checkpoint_window(
			collateral_id,
			stable_id,
			&pending.snapshot,
			&checkpoint.pending_end,
		);
		let phase_one = math::realize(
			pending.amount,
			&pending.snapshot,
			&checkpoint.pending_end.coords,
			&window,
			&pool.config.precision,
		);
		debug_assert!(phase_one.yield_gain.is_zero());
		debug_assert!(phase_one.compounded <= checkpoint.activated);

		let (phase_two, live) = Self::realize_leg(
			collateral_id,
			stable_id,
			Leg::Active,
			pool,
			phase_one.compounded,
			&checkpoint.active_start,
		);
		debug_assert!(deposit.snapshot.coords.p == live.coords.p);
		deposit.active_deposit = deposit
			.active_deposit
			.checked_add(&phase_two.compounded)
			.ok_or(ArithmeticError::Overflow)?;
		let gains = phase_one
			.collateral_gain
			.checked_add(&phase_two.collateral_gain)
			.ok_or(ArithmeticError::Overflow)?;
		deposit.claimable_collateral = deposit
			.claimable_collateral
			.checked_add(&gains)
			.ok_or(ArithmeticError::Overflow)?;
		deposit.claimable_yield = deposit
			.claimable_yield
			.checked_add(&phase_two.yield_gain)
			.ok_or(ArithmeticError::Overflow)?;

		Self::release_checkpoint(collateral_id, stable_id, pending.cohort, checkpoint);
		Self::deposit_event(Event::PendingDepositActivated {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			depositor: who.clone(),
			amount: phase_one.compounded,
		});
		Ok(())
	}

	/// Returns the sums window of a pending phase that ends at a cohort checkpoint.
	///
	/// Stored rows provide sums before the endpoint. The checkpoint provides the endpoint sum
	/// because the pending leg can continue after activation.
	///
	/// Rows after the endpoint belong to later pending capital and contribute zero.
	pub(crate) fn checkpoint_window(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		snapshot: &DepositSnapshot,
		end: &DepositSnapshot,
	) -> SumsWindow {
		if snapshot.coords.epoch != end.coords.epoch {
			// The pending leg was depleted after the join, and every row of the join epoch froze
			// at that depletion, before the advancement: plain storage reads cover the window.
			return Self::sums_window(collateral_id, stable_id, Leg::Pending, snapshot);
		}
		let snap = if snapshot.coords.scale == end.coords.scale {
			end.sums
		} else {
			Self::sums_row(
				collateral_id,
				stable_id,
				Leg::Pending,
				snapshot.coords.epoch,
				snapshot.coords.scale,
			)
		};
		let mut ahead = [PoolSums::default(); math::SCALE_SPAN as usize];
		let mut scale = snapshot.coords.scale;
		for slot in &mut ahead {
			scale = scale.saturating_add(1);
			if scale > end.coords.scale {
				break;
			}
			*slot = if scale == end.coords.scale {
				end.sums
			} else {
				Self::sums_row(collateral_id, stable_id, Leg::Pending, snapshot.coords.epoch, scale)
			};
		}
		SumsWindow { snap, ahead }
	}

	/// Settles one position and its open cohort at the current pending coordinates.
	///
	/// The function returns `None` when offsets consumed the full position. Its downward-rounding
	/// remainder stays in `total_pending_deposits` as unowned pool value.
	///
	/// Joint settlement keeps the member snapshot at or behind the cohort coordinates. Upward
	/// cohort rounding preserves sufficient aggregate capital for all members.
	fn realize_open_pending(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		pool: &mut StabilityPoolOf<T>,
		deposit: &mut DepositOf<T>,
		mut pending: PendingDeposit<BalanceOf<T>>,
	) -> Result<Option<PendingDeposit<BalanceOf<T>>>, DispatchError> {
		let live = pool.state.pending_coords;
		let joined = pending.snapshot.coords;
		if joined.p == live.p && joined.epoch == live.epoch && joined.scale == live.scale {
			// The pending leg only moves through offsets, which always shrink `P`, so unchanged
			// coordinates mean unchanged sums: nothing to settle.
			return Ok(Some(pending));
		}
		let (realized, snapshot) = Self::realize_leg(
			collateral_id,
			stable_id,
			Leg::Pending,
			pool,
			pending.amount,
			&pending.snapshot,
		);
		debug_assert!(realized.yield_gain.is_zero());
		deposit.claimable_collateral = deposit
			.claimable_collateral
			.checked_add(&realized.collateral_gain)
			.ok_or(ArithmeticError::Overflow)?;

		let sf_int = pool.config.precision.scale_factor();
		// An open tranche always has its cohort.
		let cohort = pool
			.state
			.cohort_mut(pending.cohort)
			.defensive_ok_or(DispatchError::Corruption)?;
		cohort.revalue(&live, sf_int);
		if realized.compounded.is_zero() {
			cohort.members = cohort.members.saturating_sub(1);
			if cohort.members == 0 {
				pool.state.remove_cohort(pending.cohort);
			}
			return Ok(None);
		}
		pending.amount = realized.compounded;
		pending.snapshot = snapshot;
		Ok(Some(pending))
	}

	/// Removes one member reference and deletes an unused checkpoint.
	fn release_checkpoint(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		id: CohortId,
		mut checkpoint: CohortCheckpointOf<T>,
	) {
		checkpoint.members = checkpoint.members.saturating_sub(1);
		if checkpoint.members == 0 {
			CohortCheckpoints::<T>::remove((collateral_id, stable_id, id));
		} else {
			CohortCheckpoints::<T>::insert((collateral_id, stable_id, id), checkpoint);
		}
	}

	/// Settles `amount` on one leg and returns its current principal, gains, and snapshot.
	fn realize_leg(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		pool: &StabilityPoolOf<T>,
		amount: BalanceOf<T>,
		snapshot: &DepositSnapshot,
	) -> (Realized<BalanceOf<T>>, DepositSnapshot) {
		let coords = pool.state.coords(leg);
		let current = Self::sums_at(collateral_id, stable_id, leg, coords);
		// A snapshot already at the live coordinates realizes against the live row alone, because
		// no row above the live scale can exist. The read for the snapshot reset then covers the
		// whole window.
		let window =
			if snapshot.coords.epoch == coords.epoch && snapshot.coords.scale == coords.scale {
				SumsWindow { snap: current, ahead: Default::default() }
			} else {
				Self::sums_window(collateral_id, stable_id, leg, snapshot)
			};
		let realized = math::realize(amount, snapshot, coords, &window, &pool.config.precision);
		debug_assert!(realized.compounded <= amount);
		(realized, pool.state.snapshot(leg, &current))
	}

	/// Returns a depositor row or an empty row at the current active coordinates.
	fn load_or_fresh_deposit(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		who: &T::AccountId,
		state: &PoolStateOf<T>,
	) -> DepositOf<T> {
		Deposits::<T>::get((collateral_id, stable_id, who)).unwrap_or_else(|| {
			let current = Self::sums_at(collateral_id, stable_id, Leg::Active, &state.coords);
			Deposit::fresh(state.snapshot(Leg::Active, &current))
		})
	}

	/// Validates and stores replacement pool parameters.
	pub(crate) fn do_set_stability_pool_config(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		config: StabilityPoolConfigOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		ensure!(config.is_valid(), Error::<T>::InvalidStabilityPoolConfig);
		ensure!(config.precision == pool.config.precision, Error::<T>::AccumulatorParamsImmutable);
		pool.config = config;
		Pools::<T>::insert(&collateral_id, &stable_id, pool);
		Self::deposit_event(Event::StabilityPoolConfigUpdated { collateral_id, stable_id });
		Ok(())
	}

	/// Stores a user position or removes it when no user value remains.
	fn store_or_prune_deposit(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		who: &T::AccountId,
		deposit: DepositOf<T>,
	) {
		if deposit.is_empty() {
			Deposits::<T>::remove((collateral_id, stable_id, who));
		} else {
			Deposits::<T>::insert((collateral_id, stable_id, who), deposit);
		}
	}

	/// Returns the sums row of `leg` at `coords`.
	///
	/// An absent row returns zero. This conservative result prevents an overpayment.
	pub(crate) fn sums_at(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		coords: &Accumulators,
	) -> PoolSums {
		Self::sums_row(collateral_id, stable_id, leg, coords.epoch, coords.scale)
	}

	/// Returns a sums row by raw `(epoch, scale)` coordinates.
	fn sums_row(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		epoch: u32,
		scale: u32,
	) -> PoolSums {
		PoolSumsStore::<T>::get((collateral_id, stable_id, leg, epoch, scale))
	}

	/// Returns the bounded sums window required to settle a snapshot.
	///
	/// Rows have no gaps within an epoch. Therefore, the first absent row ends the window.
	pub(crate) fn sums_window(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		snapshot: &DepositSnapshot,
	) -> SumsWindow {
		let snap = Self::sums_at(collateral_id, stable_id, leg, &snapshot.coords);
		let mut ahead = [PoolSums::default(); math::SCALE_SPAN as usize];
		let mut scale = snapshot.coords.scale;
		for slot in &mut ahead {
			scale = scale.saturating_add(1);
			let Ok(sums) = PoolSumsStore::<T>::try_get((
				collateral_id,
				stable_id,
				leg,
				snapshot.coords.epoch,
				scale,
			)) else {
				break;
			};
			*slot = sums;
		}
		SumsWindow { snap, ahead }
	}
}
