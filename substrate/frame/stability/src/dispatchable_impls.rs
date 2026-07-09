use crate::{
	math,
	pallet::{
		BalanceOf, CollateralCreditOf, Config, DepositOf, Deposits, Error, Event, Pallet,
		PoolStateOf, PoolSumsStore, Pools, StabilityPoolConfigOf, StabilityPoolOf, StableCreditOf,
	},
	pending,
	types::{
		Accumulators, Deposit, DepositSnapshot, PUpdate, PendingDeposit, PendingOffsetResult,
		PoolOffsetResult, PoolSums, RecoveryOffsetSource, SumsWindow, WithdrawalRequest,
	},
};
use frame::{
	prelude::*,
	traits::{
		fungibles::{Balanced as FungiblesBalanced, Mutate as FungiblesMutate},
		tokens::{Fortitude, Precision, Preservation},
		Defensive, Time,
	},
};
use pallet_linked_list::SortedListInterface;
use pusd_primitives::{
	BranchMode, BranchModeProvider, Millis, RecoveryOffsetInterface, RecoveryOffsetResult,
	StableListId,
};

/// Which realized gain a claim pays out; the two sides share one flow
/// ([`Pallet::do_claim`]).
#[derive(Clone, Copy)]
pub(crate) enum ClaimKind {
	Collateral,
	Yield,
}

/// The fully-materialized post-state of an active-pool offset (SPEC.md §6.4
/// / §7.1): all fallible accumulator math runs in
/// [`Pallet::plan_active_offset`] before any value moves;
/// [`Pallet::commit_active_offset`] then only writes.
struct ActiveOffsetPlan<Balance> {
	new_sums: PoolSums,
	new_unclaimed: Balance,
	new_total_active: Balance,
	new_coords: Accumulators,
}

/// One committed §6.8 backstop step: what it burned and credited.
struct PendingStep<Balance> {
	debt: Balance,
	collateral: Balance,
}

impl<T: Config> Pallet<T> {
	/// The shared entry-point prologue: a branch is registered iff its pool
	/// row exists.
	fn load_pool(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Result<StabilityPoolOf<T>, DispatchError> {
		let pool =
			Pools::<T>::get(collateral_id, stable_id).ok_or(Error::<T>::BranchNotRegistered)?;
		Ok(pool)
	}

	/// The realization pair every value-moving entry point runs before its own
	/// change: settle gains/losses into the row, then fold in a matured
	/// pending deposit. Returns whether an activation happened (i.e. whether
	/// `state` changed).
	fn realize_and_activate(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		who: &T::AccountId,
		pool: &mut StabilityPoolOf<T>,
		deposit: &mut DepositOf<T>,
		now: Millis,
	) -> Result<bool, DispatchError> {
		Self::realize_deposit(collateral_id, stable_id, pool, deposit)?;
		Self::activate_matured_pending(collateral_id, stable_id, who, &mut pool.state, deposit, now)
	}

	/// SPEC.md §6.6: realize, activate any matured pending deposit, attempt
	/// an incoming-deposit recovery offset (§7.4), and queue whatever the
	/// settlement did not use behind the entry delay.
	pub(crate) fn do_deposit(
		who: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		ensure!(amount >= pool.config.minimum_deposit, Error::<T>::DepositTooSmall);

		let now = T::TimeProvider::now();
		let mut deposit =
			Self::load_or_fresh_deposit(&collateral_id, &stable_id, &who, &pool.state);
		Self::realize_and_activate(&collateral_id, &stable_id, &who, &mut pool, &mut deposit, now)?;

		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		let used_for_recovery = Self::try_incoming_recovery(
			&collateral_id,
			&stable_id,
			&who,
			&pool_account,
			&mut pool.state,
			&mut deposit,
			amount,
		)?;
		let pending_amount =
			amount.checked_sub(&used_for_recovery).ok_or(ArithmeticError::Underflow)?;

		if !pending_amount.is_zero() {
			T::StableAssets::transfer(
				stable_id.clone(),
				&who,
				&pool_account,
				pending_amount,
				Preservation::Expendable,
			)?;
			let activatable_at = now.saturating_add(pool.config.entry_delay);
			match deposit.pending_deposit.as_mut() {
				Some(pending) => {
					// Merging a top-up resets the whole pending amount's
					// entry delay — a top-up must never shorten the wait —
					// and keeps the existing FIFO slot: re-appending would
					// let a dust top-up flee the queue's tail right before
					// a lossy backstop consumption (§6.8).
					pending.amount = pending
						.amount
						.checked_add(&pending_amount)
						.ok_or(ArithmeticError::Overflow)?;
					pending.activatable_at = activatable_at;
				},
				None => {
					deposit.pending_deposit =
						Some(PendingDeposit { amount: pending_amount, activatable_at });
					let fifo = pending::list_id::<T>(&collateral_id, &stable_id);
					pending::append::<T>(&fifo, who.clone())?;
				},
			}
			pool.state.total_pending_deposits = pool
				.state
				.total_pending_deposits
				.checked_add(&pending_amount)
				.ok_or(ArithmeticError::Overflow)?;
		}

		// A fully-settled deposit may leave nothing but the recovery
		// collateral credit on the row (or, if that floored to zero,
		// nothing at all).
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

	/// SPEC.md §7.4: burn up to `amount` of the incoming deposit straight
	/// from the depositor against an at-or-above-par `FinalRecovery` head,
	/// crediting the priced collateral directly to the depositor. The used
	/// portion never touches the pool's stablecoin balance or `P`/`S`/`G`
	/// (invariant 7). A below-par head rejects the whole deposit.
	fn try_incoming_recovery(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		who: &T::AccountId,
		pool_account: &T::AccountId,
		state: &mut PoolStateOf<T>,
		deposit: &mut DepositOf<T>,
		amount: BalanceOf<T>,
	) -> Result<BalanceOf<T>, DispatchError> {
		let result = T::RecoveryOffsets::execute_recovery_offset(
			collateral_id,
			stable_id,
			who,
			pool_account,
			amount,
		)?;
		let outcome = match result {
			RecoveryOffsetResult::NoTarget => return Ok(BalanceOf::<T>::zero()),
			RecoveryOffsetResult::BelowPar => {
				return Err(Error::<T>::RecoveryOffsetBelowPar.into());
			},
			RecoveryOffsetResult::Applied(outcome) => outcome,
		};
		// Redemptions caps execution at the requested amount and returns the
		// actually cancelled debt; the remainder becomes pending deposit below.
		ensure!(outcome.debt_cancelled <= amount, Error::<T>::InvalidRecoveryOffsetSnapshot);

		deposit.claimable_collateral = deposit
			.claimable_collateral
			.checked_add(&outcome.collateral_out)
			.ok_or(ArithmeticError::Overflow)?;
		state.total_collateral_gains_unclaimed = state
			.total_collateral_gains_unclaimed
			.checked_add(&outcome.collateral_out)
			.ok_or(ArithmeticError::Overflow)?;
		Self::deposit_event(Event::RecoveryOffsetApplied {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			debt_burned: outcome.debt_cancelled,
			collateral_gain: outcome.collateral_out,
			source: RecoveryOffsetSource::IncomingDeposit,
		});
		Ok(outcome.debt_cancelled)
	}

	/// SPEC.md §7.3: burn active pool stablecoin against the `FinalRecovery`
	/// head at the shared settlement pricing, then run the standard
	/// active-pool accumulator update — the same code path as ordinary
	/// liquidation offsets (invariant 8 by construction).
	pub(crate) fn do_offset_recovery(
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		max_stable_in: BalanceOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		// Recovery offsets are settlement operations: allowed in Safety
		// Mode, halted only by Frozen.
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;

		// Size the burn before touching anything: pool depth and the §6.5
		// floor cap what the settlement may take.
		let debt = math::clamp_offset_debt(
			max_stable_in,
			pool.state.total_active_deposits,
			pool.config.minimum_active_pool_balance,
		);
		ensure!(!debt.is_zero(), Error::<T>::NoRecoveryOffsetPerformed);

		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		let result = T::RecoveryOffsets::execute_recovery_offset(
			&collateral_id,
			&stable_id,
			&pool_account,
			&pool_account,
			debt,
		)?;
		let outcome = match result {
			RecoveryOffsetResult::NoTarget => {
				return Err(Error::<T>::RecoveryVaultNotFound.into());
			},
			RecoveryOffsetResult::BelowPar => {
				return Err(Error::<T>::RecoveryOffsetBelowPar.into());
			},
			RecoveryOffsetResult::Applied(outcome) => outcome,
		};
		// Redemptions caps execution at the clamp result. A smaller burn means
		// the recovery head had less cancellable debt than the active pool could
		// safely spend.
		ensure!(outcome.debt_cancelled <= debt, Error::<T>::InvalidRecoveryOffsetSnapshot);
		ensure!(!outcome.debt_cancelled.is_zero(), Error::<T>::NoRecoveryOffsetPerformed);

		Self::apply_active_offset(
			&collateral_id,
			&stable_id,
			&mut pool,
			outcome.debt_cancelled,
			outcome.collateral_out,
		)?;
		Pools::<T>::insert(&collateral_id, &stable_id, pool);
		Self::deposit_event(Event::RecoveryOffsetApplied {
			collateral_id,
			stable_id,
			debt_burned: outcome.debt_cancelled,
			collateral_gain: outcome.collateral_out,
			source: RecoveryOffsetSource::ActivePool,
		});
		Ok(())
	}

	/// SPEC.md §6.7: explicit activation of a matured pending deposit.
	pub(crate) fn do_activate_deposit(
		who: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;
		let pending = deposit.pending_deposit.as_ref().ok_or(Error::<T>::NoPendingDeposit)?;
		let now = T::TimeProvider::now();
		ensure!(now >= pending.activatable_at, Error::<T>::PendingDepositNotMatured);

		Self::realize_and_activate(&collateral_id, &stable_id, &who, &mut pool, &mut deposit, now)?;
		debug_assert!(deposit.pending_deposit.is_none());

		Self::store_or_prune_deposit(&collateral_id, &stable_id, &who, deposit);
		Pools::<T>::insert(&collateral_id, &stable_id, pool);
		Ok(())
	}

	/// SPEC.md §6.9: create or replace the caller's withdrawal request.
	/// Recorded in every mode with `executable_at` stamped now — a request
	/// made in Normal Mode therefore already satisfies the Safety delay if
	/// the branch turns before execution. Normal-mode withdrawals never read
	/// it, so recording is harmless there.
	pub(crate) fn do_request_withdraw(
		who: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;
		ensure!(!amount.is_zero(), Error::<T>::NoActiveDeposit);

		let now = T::TimeProvider::now();
		let activated = Self::realize_and_activate(
			&collateral_id,
			&stable_id,
			&who,
			&mut pool,
			&mut deposit,
			now,
		)?;

		let executable_at = now.saturating_add(pool.config.safety_withdrawal_delay);
		deposit.withdrawal_request = Some(WithdrawalRequest { amount, executable_at });

		Self::store_or_prune_deposit(&collateral_id, &stable_id, &who, deposit);
		// Requests live on the row; the pool row only changed if a pending
		// deposit activated along the way.
		if activated {
			Pools::<T>::insert(&collateral_id, &stable_id, pool);
		}
		Self::deposit_event(Event::WithdrawalRequested {
			collateral_id,
			stable_id,
			depositor: who,
			amount,
			executable_at,
		});
		Ok(())
	}

	/// SPEC.md §6.9: withdraw active stablecoin — immediately in Normal Mode,
	/// against an executable request in Safety Mode.
	pub(crate) fn do_withdraw(
		who: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		amount: BalanceOf<T>,
		recipient: T::AccountId,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;

		let now = T::TimeProvider::now();
		Self::realize_and_activate(&collateral_id, &stable_id, &who, &mut pool, &mut deposit, now)?;

		let mode = Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let take = Self::resolve_withdrawal(mode, now, amount, &mut deposit)?;
		ensure!(!take.is_zero(), Error::<T>::NoActiveDeposit);

		// `resolve_withdrawal` bounds `take` by the realized active deposit,
		// which flooring keeps at or below the pool aggregate.
		deposit.active_deposit =
			deposit.active_deposit.checked_sub(&take).ok_or(ArithmeticError::Underflow)?;
		pool.state.total_active_deposits = pool
			.state
			.total_active_deposits
			.checked_sub(&take)
			.ok_or(ArithmeticError::Underflow)?;

		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		T::StableAssets::transfer(
			stable_id.clone(),
			&pool_account,
			&recipient,
			take,
			Preservation::Expendable,
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

	/// SPEC.md §6.10: pay out the caller's realized gains — one flow for both
	/// claim sides, which differ only in the claimed field, its aggregate,
	/// the paying asset surface, and the error/event pair.
	pub(crate) fn do_claim(
		who: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		recipient: T::AccountId,
		kind: ClaimKind,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;

		Self::realize_and_activate(
			&collateral_id,
			&stable_id,
			&who,
			&mut pool,
			&mut deposit,
			T::TimeProvider::now(),
		)?;

		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		// Underflows on the unclaimed totals would mean a claimable exceeding
		// the tracked aggregate.
		let amount = match kind {
			ClaimKind::Collateral => {
				let amount = deposit.claimable_collateral;
				ensure!(!amount.is_zero(), Error::<T>::NoClaimableCollateral);
				deposit.claimable_collateral = BalanceOf::<T>::zero();
				pool.state.total_collateral_gains_unclaimed = pool
					.state
					.total_collateral_gains_unclaimed
					.checked_sub(&amount)
					.ok_or(ArithmeticError::Underflow)?;
				T::CollateralAssets::transfer(
					collateral_id.clone(),
					&pool_account,
					&recipient,
					amount,
					Preservation::Expendable,
				)?;
				amount
			},
			ClaimKind::Yield => {
				let amount = deposit.claimable_yield;
				ensure!(!amount.is_zero(), Error::<T>::NoClaimableYield);
				deposit.claimable_yield = BalanceOf::<T>::zero();
				pool.state.total_yield_unclaimed = pool
					.state
					.total_yield_unclaimed
					.checked_sub(&amount)
					.ok_or(ArithmeticError::Underflow)?;
				T::StableAssets::transfer(
					stable_id.clone(),
					&pool_account,
					&recipient,
					amount,
					Preservation::Expendable,
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

	/// SPEC.md §7.1: burn active-pool stablecoin against ordinary liquidation
	/// debt, resolving the pro-rata slice of the offered collateral credit
	/// into the pool account and distributing it to active depositors
	/// through `S`. The offset is capped by pool depth and the
	/// `minimum_active_pool_balance` floor (§6.5); whatever the pool cannot
	/// (or may not) take comes back as the credit remainder, with the result
	/// zeroed on a full step-aside.
	pub(crate) fn do_offset_liquidation(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		max_debt_to_offset: BalanceOf<T>,
		collateral: CollateralCreditOf<T>,
	) -> (PoolOffsetResult<BalanceOf<T>>, CollateralCreditOf<T>) {
		let Ok(mut pool) = Self::load_pool(collateral_id, stable_id) else {
			return (PoolOffsetResult::zero(), collateral);
		};
		// Defense in depth: the vault engine already refuses to liquidate
		// on a frozen branch.
		if Self::ensure_not_frozen(collateral_id, stable_id).is_err() {
			return (PoolOffsetResult::zero(), collateral);
		}

		let sp_offset_debt = math::clamp_offset_debt(
			max_debt_to_offset,
			pool.state.total_active_deposits,
			pool.config.minimum_active_pool_balance,
		);
		if sp_offset_debt.is_zero() {
			return (PoolOffsetResult::zero(), collateral);
		}
		let sp_offset_collateral =
			math::pro_rata_floor(collateral.peek(), sp_offset_debt, max_debt_to_offset);
		let Ok(plan) = Self::plan_active_offset(
			collateral_id,
			stable_id,
			&pool,
			sp_offset_debt,
			sp_offset_collateral,
		) else {
			// Beyond supported precision (§6.4): the pool steps aside and
			// the debt continues to the caller's next stage.
			return (PoolOffsetResult::zero(), collateral);
		};

		let pool_account = Self::pool_account(collateral_id, stable_id);
		let remainder = match Self::resolve_and_burn(
			collateral_id,
			stable_id,
			&pool_account,
			sp_offset_debt,
			sp_offset_collateral,
			collateral,
		) {
			Ok(remainder) => remainder,
			Err(remainder) => return (PoolOffsetResult::zero(), remainder),
		};

		Self::commit_active_offset(collateral_id, stable_id, &mut pool.state, plan);
		Self::deposit_event(Event::PoolOffsetApplied {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			debt_burned: sp_offset_debt,
			collateral_gain: sp_offset_collateral,
			epoch: pool.state.coords.epoch,
			scale: pool.state.coords.scale,
		});
		Pools::<T>::insert(collateral_id, stable_id, pool);
		(
			PoolOffsetResult {
				debt_offset: sp_offset_debt,
				collateral_to_pool: sp_offset_collateral,
			},
			remainder,
		)
	}

	/// SPEC.md §7.2 / §6.8: the last-resort backstop — consume pending
	/// deposits oldest-first against liquidation debt that survived the
	/// active pool and JIT liquidity. Each step burns its stablecoin slice,
	/// resolves its collateral slice into the pool account, and credits the
	/// consumed depositor directly; `P`/`S`/`G` are never touched
	/// (invariant 11). An empty queue or zero remaining debt no-ops with a
	/// zeroed result and the untouched credit.
	pub(crate) fn do_offset_pending_liquidation(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		remaining_debt: BalanceOf<T>,
		max_pending_iterations: u32,
		mut collateral: CollateralCreditOf<T>,
	) -> (PendingOffsetResult<BalanceOf<T>>, CollateralCreditOf<T>) {
		let Some(mut pool) = Pools::<T>::get(collateral_id, stable_id) else {
			return (PendingOffsetResult::zero(remaining_debt), collateral);
		};
		if Self::ensure_not_frozen(collateral_id, stable_id).is_err() {
			return (PendingOffsetResult::zero(remaining_debt), collateral);
		}
		let fifo = pending::list_id::<T>(collateral_id, stable_id);
		let pool_account = Self::pool_account(collateral_id, stable_id);
		let cap = max_pending_iterations.min(T::MaxPendingOffsetIterations::get());

		let mut debt_left = remaining_debt;
		let mut debt_burned = BalanceOf::<T>::zero();
		let mut collateral_credited = BalanceOf::<T>::zero();
		let mut iterations: u32 = 0;

		// Bounded by `cap <= MaxPendingOffsetIterations`.
		while iterations < cap {
			if debt_left.is_zero() {
				break;
			}
			let Some(oldest) = T::PendingLists::tail(&fifo) else {
				break;
			};
			iterations = iterations.saturating_add(1);

			let (step, returned) = Self::offset_pending_step(
				collateral_id,
				stable_id,
				&fifo,
				&pool_account,
				&oldest,
				&mut pool.state,
				debt_left,
				collateral,
			);
			collateral = returned;
			let Some(step) = step else {
				break;
			};
			debug_assert!(step.debt <= debt_left);
			debt_left = debt_left.saturating_sub(step.debt);
			debt_burned = debt_burned.saturating_add(step.debt);
			collateral_credited = collateral_credited.saturating_add(step.collateral);
		}

		if !debt_burned.is_zero() {
			Pools::<T>::insert(collateral_id, stable_id, pool);
			Self::deposit_event(Event::PendingDepositOffsetApplied {
				collateral_id: collateral_id.clone(),
				stable_id: stable_id.clone(),
				debt_burned,
				collateral_gain: collateral_credited,
				iterations,
			});
		}
		(
			PendingOffsetResult {
				debt_offset: debt_burned,
				collateral_to_pool: collateral_credited,
				remaining_debt: debt_left,
				iterations_used: iterations,
			},
			collateral,
		)
	}

	/// One §6.8 backstop step against the FIFO's `oldest` member: price the
	/// slices against the remainders at step start, resolve the collateral,
	/// burn the stablecoin, then commit the row, the FIFO, and the in-memory
	/// aggregates. `None` stops the walk — a broken invariant or an
	/// unresolvable collateral slice — with nothing of this step applied.
	fn offset_pending_step(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		fifo: &StableListId<T::CollateralAssetId, T::StableAssetId>,
		pool_account: &T::AccountId,
		oldest: &T::AccountId,
		state: &mut PoolStateOf<T>,
		debt_left: BalanceOf<T>,
		collateral: CollateralCreditOf<T>,
	) -> (Option<PendingStep<BalanceOf<T>>>, CollateralCreditOf<T>) {
		debug_assert!(!debt_left.is_zero());
		// Reads and checked math first; nothing moves until they all pass.
		let Some(mut row) = Deposits::<T>::get((collateral_id, stable_id, oldest)).defensive()
		else {
			return (None, collateral);
		};
		let Some(pending) = row.pending_deposit.as_ref().defensive() else {
			return (None, collateral);
		};
		let pending_amount = pending.amount;
		let activatable_at = pending.activatable_at;
		let step_debt = pending_amount.min(debt_left);
		let step_collateral = math::pro_rata_floor(collateral.peek(), step_debt, debt_left);
		let Some(new_pending_amount) = pending_amount.checked_sub(&step_debt).defensive() else {
			return (None, collateral);
		};
		let Some(new_claimable) =
			row.claimable_collateral.checked_add(&step_collateral).defensive()
		else {
			return (None, collateral);
		};
		let Some(new_total_pending) =
			state.total_pending_deposits.checked_sub(&step_debt).defensive()
		else {
			return (None, collateral);
		};
		// Direct credits must still enter the unclaimed total, or claims
		// would break the pool-balance identity
		let Some(new_unclaimed) =
			state.total_collateral_gains_unclaimed.checked_add(&step_collateral).defensive()
		else {
			return (None, collateral);
		};

		let remainder = match Self::resolve_and_burn(
			collateral_id,
			stable_id,
			pool_account,
			step_debt,
			step_collateral,
			collateral,
		) {
			Ok(remainder) => remainder,
			Err(remainder) => return (None, remainder),
		};

		// Value moved: commit unconditionally (roll forward). A FIFO-remove
		// failure strands an orphan node and the next iteration's row check stops the
		// walk before it can loop on the same tail.
		row.pending_deposit = if new_pending_amount.is_zero() {
			let _ = pending::remove::<T>(fifo, oldest)
				.defensive_proof("pending FIFO diverged from the rows");
			None
		} else {
			Some(PendingDeposit { amount: new_pending_amount, activatable_at })
		};
		row.claimable_collateral = new_claimable;
		state.total_pending_deposits = new_total_pending;
		state.total_collateral_gains_unclaimed = new_unclaimed;
		// Flooring can zero the credit; a fully-consumed row with no other
		// value must not linger.
		Self::store_or_prune_deposit(collateral_id, stable_id, oldest, row);
		(Some(PendingStep { debt: step_debt, collateral: step_collateral }), remainder)
	}

	/// The shared active-pool accumulator math for ordinary liquidation and
	/// recovery offsets: `delta_S` from the
	/// pre-offset totals FIRST, then the `P` shrink. Read-only: the
	/// caller commits the plan once its value movement is through.
	fn plan_active_offset(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		pool: &StabilityPoolOf<T>,
		debt: BalanceOf<T>,
		collateral: BalanceOf<T>,
	) -> Result<ActiveOffsetPlan<BalanceOf<T>>, DispatchError> {
		let state = &pool.state;
		debug_assert!(!debt.is_zero());
		debug_assert!(debt <= state.total_active_deposits);

		let delta_s = state.delta_sum(collateral).ok_or(ArithmeticError::Overflow)?;
		let mut new_sums = Self::sums_at(collateral_id, stable_id, &state.coords);
		new_sums.s_collateral =
			new_sums.s_collateral.checked_add(&delta_s).ok_or(ArithmeticError::Overflow)?;
		let new_unclaimed = state
			.total_collateral_gains_unclaimed
			.checked_add(&collateral)
			.ok_or(ArithmeticError::Overflow)?;

		let update = math::update_p_after_offset(
			state.coords.p,
			state.total_active_deposits,
			debt,
			&pool.config.precision,
		)
		.ok_or(Error::<T>::UnsupportedOffsetPrecision)?;
		let (new_coords, new_total_active) = match update {
			PUpdate::Depleted => (
				Accumulators {
					p: FixedU128::one(),
					epoch: state.coords.epoch.checked_add(1).ok_or(ArithmeticError::Overflow)?,
					scale: 0,
				},
				BalanceOf::<T>::zero(),
			),
			PUpdate::Updated { new_p, scales_crossed } => (
				Accumulators {
					p: new_p,
					epoch: state.coords.epoch,
					scale: state
						.coords
						.scale
						.checked_add(scales_crossed)
						.ok_or(ArithmeticError::Overflow)?,
				},
				state
					.total_active_deposits
					.checked_sub(&debt)
					.ok_or(ArithmeticError::Underflow)?,
			),
		};
		Ok(ActiveOffsetPlan { new_sums, new_unclaimed, new_total_active, new_coords })
	}

	/// Write an [`ActiveOffsetPlan`] into the sums store and `state`,
	/// seeding a zero sums row for every new coordinate. Infallible: all
	/// arithmetic already ran in [`Pallet::plan_active_offset`].
	fn commit_active_offset(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		state: &mut PoolStateOf<T>,
		plan: ActiveOffsetPlan<BalanceOf<T>>,
	) {
		PoolSumsStore::<T>::insert(
			(collateral_id, stable_id, state.coords.epoch, state.coords.scale),
			plan.new_sums,
		);
		if plan.new_coords.epoch == state.coords.epoch {
			// Bounded by `math::MAX_SCALE_CROSSINGS`.
			for scale in state.coords.scale.saturating_add(1)..=plan.new_coords.scale {
				PoolSumsStore::<T>::insert(
					(collateral_id, stable_id, state.coords.epoch, scale),
					PoolSums::default(),
				);
			}
		} else {
			PoolSumsStore::<T>::insert(
				(collateral_id, stable_id, plan.new_coords.epoch, 0u32),
				PoolSums::default(),
			);
		}
		state.coords = plan.new_coords;
		state.total_active_deposits = plan.new_total_active;
		state.total_collateral_gains_unclaimed = plan.new_unclaimed;
	}

	/// Plan-and-commit in one step for the extrinsic-transactional recovery
	/// path ([`Pallet::do_offset_recovery`]), which interleaves no value
	/// ops. Accounting only: the settlement already moved the stablecoin
	/// and the collateral.
	fn apply_active_offset(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		pool: &mut StabilityPoolOf<T>,
		debt: BalanceOf<T>,
		collateral: BalanceOf<T>,
	) -> DispatchResult {
		let plan = Self::plan_active_offset(collateral_id, stable_id, pool, debt, collateral)?;
		Self::commit_active_offset(collateral_id, stable_id, &mut pool.state, plan);
		Ok(())
	}

	/// The §7 offset value movement: split `collateral_amount` off `credit`
	/// and resolve it into the pool account, then burn `debt` pool
	/// stablecoin. The genuinely-fallible resolve (a sub-minimum first gain)
	/// runs first; the identity-guaranteed burn gets a defensive claw-back.
	/// `Err` hands back the reassembled credit with no value moved.
	fn resolve_and_burn(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		pool_account: &T::AccountId,
		debt: BalanceOf<T>,
		collateral_amount: BalanceOf<T>,
		credit: CollateralCreditOf<T>,
	) -> Result<CollateralCreditOf<T>, CollateralCreditOf<T>> {
		let (to_pool, mut remainder) = credit.split(collateral_amount);
		if let Err(returned) = Self::resolve_pool_collateral(pool_account, to_pool) {
			Self::subsume_returned(&mut remainder, returned);
			return Err(remainder);
		}
		if Self::burn_pool_stable(stable_id, pool_account, debt).defensive().is_err() {
			Self::claw_back_pool_collateral(
				collateral_id,
				pool_account,
				collateral_amount,
				&mut remainder,
			);
			return Err(remainder);
		}
		Ok(remainder)
	}

	/// Resolve an offset's collateral slice into the pool account. A zero
	/// slice is dropped without touching the account (a zero deposit into a
	/// not-yet-existing account is the only failure it could hit); a real
	/// failure means a sub-minimum first gain.
	fn resolve_pool_collateral(
		pool_account: &T::AccountId,
		credit: CollateralCreditOf<T>,
	) -> Result<(), CollateralCreditOf<T>> {
		let credit = match credit.drop_zero() {
			Ok(()) => return Ok(()),
			Err(credit) => credit,
		};
		T::CollateralAssets::resolve(pool_account, credit)
	}

	/// Fold a returned credit back into the remainder. Both halves come
	/// from one split, so a mismatch cannot happen; the defensive arm drops
	/// (burns) the leftover, keeping issuance conservative.
	fn subsume_returned(remainder: &mut CollateralCreditOf<T>, returned: CollateralCreditOf<T>) {
		let _ = remainder.subsume(returned).defensive_proof("collateral credit halves diverged");
	}

	/// Defensive unwind for a burn failure after the collateral resolve:
	/// pull the just-resolved amount back out of the pool account. Only
	/// reachable when the pool-balance identity is already broken.
	fn claw_back_pool_collateral(
		collateral_id: &T::CollateralAssetId,
		pool_account: &T::AccountId,
		amount: BalanceOf<T>,
		remainder: &mut CollateralCreditOf<T>,
	) {
		if amount.is_zero() {
			return;
		}
		let clawed = <T::CollateralAssets as FungiblesBalanced<T::AccountId>>::withdraw(
			collateral_id.clone(),
			pool_account,
			amount,
			Precision::Exact,
			Preservation::Expendable,
			Fortitude::Polite,
		)
		.defensive();
		if let Ok(credit) = clawed {
			Self::subsume_returned(remainder, credit);
		}
	}

	/// Burn `amount` stablecoin held by the pool account: withdraw it as a
	/// credit and drop the credit, rescinding issuance. The pool-balance
	/// identity guarantees the balance covers every offset this pallet
	/// authorizes.
	fn burn_pool_stable(
		stable_id: &T::StableAssetId,
		pool_account: &T::AccountId,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		let credit = <T::StableAssets as FungiblesBalanced<T::AccountId>>::withdraw(
			stable_id.clone(),
			pool_account,
			amount,
			Precision::Exact,
			Preservation::Expendable,
			Fortitude::Polite,
		)
		.map_err(|_| Error::<T>::StablecoinBurnFailed)?;
		drop(credit);
		Ok(())
	}

	/// SPEC.md §6.3: distribute same-stablecoin yield to active depositors
	/// through `G`, returning whatever could not be distributed — the whole
	/// credit when the active pool is empty, the branch is frozen, or the
	/// deposit into the pool account fails — so the caller routes the
	/// remainder to its fee destination. Infallible by design: this runs on
	/// the vault engine's commit paths, which must not fail over yield
	/// routing. The vault engine reaches it through the `OnBranchYield`
	/// impl (`interfaces.rs`), which loads `pool`, takes the `yield_share`
	/// cut, and hands the row down so the branch is read once.
	pub(crate) fn do_distribute_yield(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		mut pool: StabilityPoolOf<T>,
		credit: StableCreditOf<T>,
	) -> StableCreditOf<T> {
		let amount = credit.peek();
		if amount.is_zero() {
			return credit;
		}
		if pool.state.total_active_deposits.is_zero() {
			return credit;
		}
		// A frozen (or mode-unreadable) branch takes no yield; the caller
		// routes the credit to its fee destination (SPEC.md §8.1).
		match T::BranchModes::branch_mode(collateral_id, stable_id) {
			Ok(BranchMode::Normal) | Ok(BranchMode::Safety) => {},
			Ok(BranchMode::Frozen) | Err(_) => return credit,
		}

		// Every fallible step runs before the credit is consumed; after
		// `resolve` succeeds only plain writes remain.
		let Some(delta_g) = pool.state.delta_sum(amount) else {
			return credit;
		};
		let mut sums = Self::sums_at(collateral_id, stable_id, &pool.state.coords);
		let Some(new_g) = sums.g_yield.checked_add(&delta_g) else {
			return credit;
		};
		let Some(new_total_yield) = pool.state.total_yield_unclaimed.checked_add(&amount) else {
			return credit;
		};

		let pool_account = Self::pool_account(collateral_id, stable_id);
		let credit = match T::StableAssets::resolve(&pool_account, credit) {
			Ok(()) => StableCreditOf::<T>::zero(stable_id.clone()),
			Err(credit) => return credit,
		};

		sums.g_yield = new_g;
		PoolSumsStore::<T>::insert(
			(collateral_id, stable_id, pool.state.coords.epoch, pool.state.coords.scale),
			sums,
		);
		pool.state.total_yield_unclaimed = new_total_yield;
		Pools::<T>::insert(collateral_id, stable_id, pool);
		Self::deposit_event(Event::YieldDistributed {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			amount,
		});
		credit
	}

	/// SPEC.md §6.11: move up to `amount` of realized claimable yield into
	/// the active deposit. The funds already sit in the pool account, so
	/// only the accounting moves; the realization that precedes this has
	/// already reset the snapshots, so the compounded amount joins at the
	/// current accumulators.
	pub(crate) fn do_compound_yield(
		who: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;

		Self::realize_and_activate(
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
		// An underflow would mean a claimable exceeding the tracked total.
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

	/// Housekeeping (SPEC.md §8.2): permissionlessly realize `owner`'s
	/// deposit without moving value. Deliberately does NOT activate a matured
	/// pending deposit: pending capital is only the last-resort liquidation
	/// backstop, and whether to expose it to ordinary offsets stays the
	/// owner's call — a third party must not be able to force that right
	/// before a liquidation.
	pub(crate) fn do_poke_deposit(
		owner: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
	) -> DispatchResult {
		let pool = Self::load_pool(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &owner))
			.ok_or(Error::<T>::DepositNotFound)?;

		Self::realize_deposit(&collateral_id, &stable_id, &pool, &mut deposit)?;
		Self::store_or_prune_deposit(&collateral_id, &stable_id, &owner, deposit);
		Ok(())
	}

	/// How much a withdrawal may take, per mode (SPEC.md §6.9 / §8.1):
	/// - `Normal`: up to the active deposit, ignoring any outstanding request (requests are
	///   Safety-Mode state; one left behind is bounded by the live active deposit and dies with the
	///   row);
	/// - `Safety`: requires a request past its `executable_at` and consumes it by the taken amount;
	/// - `Frozen`: rejected outright.
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

	/// SPEC.md §8.1: every value-moving pool operation halts while the
	/// branch is Frozen (which includes oracle failure — the provider fails
	/// closed). Returns the live mode for operations that differentiate
	/// Normal from Safety.
	fn ensure_not_frozen(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Result<BranchMode, DispatchError> {
		let mode = T::BranchModes::branch_mode(collateral_id, stable_id)?;
		ensure!(mode != BranchMode::Frozen, Error::<T>::BranchFrozen);
		Ok(mode)
	}

	/// Settle accumulated losses and gains into the row and reset its
	/// snapshots to the pool's current coordinates (SPEC.md §6.2). Never
	/// touches pool totals: offsets already updated the aggregates when the
	/// losses happened.
	fn realize_deposit(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		pool: &StabilityPoolOf<T>,
		deposit: &mut DepositOf<T>,
	) -> DispatchResult {
		let state = &pool.state;
		let snapshot = deposit.snapshot;
		let current = Self::sums_at(collateral_id, stable_id, &state.coords);
		// A snapshot already at the live coordinates realizes against the
		// current row alone — no row above the live scale can exist — which
		// makes the snapshot-reset read below cover the whole window.
		let window = if snapshot.coords.epoch == state.coords.epoch &&
			snapshot.coords.scale == state.coords.scale
		{
			SumsWindow { snap: current, ahead: Default::default() }
		} else {
			Self::sums_window(collateral_id, stable_id, &snapshot)
		};
		let realized = math::realize(
			deposit.active_deposit,
			&snapshot,
			&state.coords,
			&window,
			&pool.config.precision,
		);
		debug_assert!(realized.compounded <= deposit.active_deposit);

		deposit.active_deposit = realized.compounded;
		deposit.claimable_collateral = deposit
			.claimable_collateral
			.checked_add(&realized.collateral_gain)
			.ok_or(ArithmeticError::Overflow)?;
		deposit.claimable_yield = deposit
			.claimable_yield
			.checked_add(&realized.yield_gain)
			.ok_or(ArithmeticError::Overflow)?;

		deposit.snapshot = state.snapshot(&current);
		Ok(())
	}

	/// SPEC.md §6.7: fold a matured pending deposit into the active deposit.
	/// Must run after [`Self::realize_deposit`] — the pending amount joins at
	/// the current accumulators, so it cannot receive gains from offsets that
	/// predate its activation. No-op while immature or absent; returns whether
	/// an activation happened (i.e. whether `state` changed).
	fn activate_matured_pending(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		who: &T::AccountId,
		state: &mut PoolStateOf<T>,
		deposit: &mut DepositOf<T>,
		now: Millis,
	) -> Result<bool, DispatchError> {
		debug_assert!(deposit.snapshot.coords.p == state.coords.p);
		debug_assert!(deposit.snapshot.coords.epoch == state.coords.epoch);
		let Some(pending) = &deposit.pending_deposit else {
			return Ok(false);
		};
		if now < pending.activatable_at {
			return Ok(false);
		}
		let amount = pending.amount;
		deposit.active_deposit =
			deposit.active_deposit.checked_add(&amount).ok_or(ArithmeticError::Overflow)?;
		state.total_active_deposits = state
			.total_active_deposits
			.checked_add(&amount)
			.ok_or(ArithmeticError::Overflow)?;
		// An underflow would mean the rows and the aggregate disagree.
		state.total_pending_deposits = state
			.total_pending_deposits
			.checked_sub(&amount)
			.ok_or(Error::<T>::PendingFifoInvariantBroken)?;
		let fifo = pending::list_id::<T>(collateral_id, stable_id);
		pending::remove::<T>(&fifo, who)?;
		deposit.pending_deposit = None;
		Self::deposit_event(Event::PendingDepositActivated {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			depositor: who.clone(),
			amount,
		});
		Ok(true)
	}

	/// Load the depositor's row, or start a fresh one snapshotted at the
	/// pool's current coordinates (realization on a fresh row is the
	/// identity).
	fn load_or_fresh_deposit(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		who: &T::AccountId,
		state: &PoolStateOf<T>,
	) -> DepositOf<T> {
		Deposits::<T>::get((collateral_id, stable_id, who)).unwrap_or_else(|| {
			let current = Self::sums_at(collateral_id, stable_id, &state.coords);
			Deposit::fresh(state.snapshot(&current))
		})
	}

	/// Validate and store a replacement pool config.
	pub(crate) fn do_set_stability_pool_config(
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
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

	/// Write the row back, or remove it once it holds no user value.
	fn store_or_prune_deposit(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		who: &T::AccountId,
		deposit: DepositOf<T>,
	) {
		if deposit.is_empty() {
			Deposits::<T>::remove((collateral_id, stable_id, who));
		} else {
			Deposits::<T>::insert((collateral_id, stable_id, who), deposit);
		}
	}

	/// The sums row at `coords`; an absent row reads as zero,
	/// which floors gains instead of overpaying them.
	pub(crate) fn sums_at(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		coords: &Accumulators,
	) -> PoolSums {
		PoolSumsStore::<T>::get((collateral_id, stable_id, coords.epoch, coords.scale))
	}

	/// The sums rows a snapshot realizes against: its own `(epoch, scale)`
	/// row plus the [`math::SCALE_SPAN`] scales after it. Rows are seeded
	/// contiguously per epoch, so reading stops at the first gap (`try_get`
	/// keeps absence observable through the `ValueQuery`).
	pub(crate) fn sums_window(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		snapshot: &DepositSnapshot,
	) -> SumsWindow {
		let snap = Self::sums_at(collateral_id, stable_id, &snapshot.coords);
		let mut ahead = [PoolSums::default(); math::SCALE_SPAN as usize];
		let mut scale = snapshot.coords.scale;
		for slot in &mut ahead {
			scale = scale.saturating_add(1);
			let Ok(sums) = PoolSumsStore::<T>::try_get((
				collateral_id,
				stable_id,
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
