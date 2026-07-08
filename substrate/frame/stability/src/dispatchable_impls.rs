use crate::{
	math,
	pallet::{
		BalanceOf, Config, DepositOf, Deposits, Error, Event, Pallet, PoolStateOf, PoolStates,
		PoolSumsStore, StabilityPoolConfigOf, StabilityPoolConfigs, StableCreditOf,
	},
	pending,
	types::{
		Deposit, PendingDeposit, PendingOffsetResult, PoolOffsetResult, PoolSums,
		RecoveryOffsetSource, WithdrawalRequest,
	},
};
use frame::{
	prelude::*,
	traits::{
		fungibles::{Balanced as FungiblesBalanced, Mutate as FungiblesMutate},
		tokens::{Fortitude, Precision, Preservation},
	},
};
use pallet_linked_list::SortedListInterface;
use pusd_primitives::{
	BranchMode, BranchModeProvider, RecoveryOffsetInterface, RecoveryOffsetQuote,
};

/// Which realized gain a claim pays out; the two sides share one flow
/// ([`Pallet::do_claim`]).
pub(crate) enum ClaimKind {
	Collateral,
	Yield,
}

impl<T: Config> Pallet<T> {
	/// The shared entry-point prologue: a branch is registered iff both its
	/// state and config rows exist.
	fn load_branch(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Result<(PoolStateOf<T>, StabilityPoolConfigOf<T>), DispatchError> {
		let state = PoolStates::<T>::get(collateral_id, stable_id)
			.ok_or(Error::<T>::BranchNotRegistered)?;
		let config = StabilityPoolConfigs::<T>::get(collateral_id, stable_id)
			.ok_or(Error::<T>::BranchNotRegistered)?;
		Ok((state, config))
	}

	/// The realization pair every value-moving entry point runs before its own
	/// change: settle gains/losses into the row, then fold in a matured
	/// pending deposit. Returns whether an activation happened (i.e. whether
	/// `state` changed).
	fn realize_and_activate(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		who: &T::AccountId,
		state: &mut PoolStateOf<T>,
		config: &StabilityPoolConfigOf<T>,
		deposit: &mut DepositOf<T>,
	) -> Result<bool, DispatchError> {
		Self::realize_deposit(collateral_id, stable_id, state, config, deposit)?;
		Self::activate_matured_pending(collateral_id, stable_id, who, state, deposit)
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
		let (mut state, config) = Self::load_branch(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		ensure!(amount >= config.minimum_deposit, Error::<T>::DepositTooSmall);

		let mut deposit = Self::load_or_fresh_deposit(&collateral_id, &stable_id, &who, &state);
		Self::realize_and_activate(
			&collateral_id,
			&stable_id,
			&who,
			&mut state,
			&config,
			&mut deposit,
		)?;

		let used_for_recovery = Self::try_incoming_recovery(
			&collateral_id,
			&stable_id,
			&who,
			&mut state,
			&mut deposit,
			amount,
		)?;
		let pending_amount =
			amount.checked_sub(&used_for_recovery).ok_or(ArithmeticError::Underflow)?;

		if !pending_amount.is_zero() {
			let pool_account = Self::pool_account(&collateral_id, &stable_id);
			T::StableAssets::transfer(
				stable_id.clone(),
				&who,
				&pool_account,
				pending_amount,
				Preservation::Expendable,
			)?;
			let activatable_at =
				frame_system::Pallet::<T>::block_number().saturating_add(config.entry_delay_blocks);
			match deposit.pending_deposit.as_mut() {
				Some(pending) => {
					// Merging a top-up resets the whole pending amount's
					// entry delay — a top-up must never shorten the wait —
					// and keeps the existing FIFO slot.
					pending.amount = pending
						.amount
						.checked_add(&pending_amount)
						.ok_or(ArithmeticError::Overflow)?;
					pending.activatable_at = activatable_at;
				},
				None => {
					deposit.pending_deposit =
						Some(PendingDeposit { amount: pending_amount, activatable_at });
					pending::append::<T>(&collateral_id, &stable_id, who.clone())?;
				},
			}
			state.total_pending_deposits = state
				.total_pending_deposits
				.checked_add(&pending_amount)
				.ok_or(ArithmeticError::Overflow)?;
		}

		// A fully-settled deposit may leave nothing but the recovery
		// collateral credit on the row (or, if that floored to zero,
		// nothing at all).
		Self::store_or_prune_deposit(&collateral_id, &stable_id, &who, deposit);
		PoolStates::<T>::insert(&collateral_id, &stable_id, state);
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
		state: &mut PoolStateOf<T>,
		deposit: &mut DepositOf<T>,
		amount: BalanceOf<T>,
	) -> Result<BalanceOf<T>, DispatchError> {
		let quote = T::RecoveryOffsets::preview_recovery_offset(collateral_id, stable_id, amount)?;
		let capacity = match quote {
			RecoveryOffsetQuote::NoTarget => return Ok(BalanceOf::<T>::zero()),
			RecoveryOffsetQuote::BelowPar => {
				return Err(Error::<T>::RecoveryOffsetBelowPar.into());
			},
			RecoveryOffsetQuote::Available { debt } => debt,
		};
		let used = amount.min(capacity);
		if used.is_zero() {
			return Ok(used);
		}

		let pool_account = Self::pool_account(collateral_id, stable_id);
		let outcome = T::RecoveryOffsets::apply_recovery_offset(
			collateral_id,
			stable_id,
			who,
			&pool_account,
			used,
		)?;
		// Same dispatch, same oracle price: the quote and the execution see
		// one snapshot, so the settlement matches the sized amount exactly.
		ensure!(outcome.debt_cancelled == used, Error::<T>::InvalidRecoveryOffsetSnapshot);

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
		Ok(used)
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
		let (mut state, config) = Self::load_branch(&collateral_id, &stable_id)?;
		// Recovery offsets are settlement operations: allowed in Safety
		// Mode, halted only by Frozen.
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;

		let quote =
			T::RecoveryOffsets::preview_recovery_offset(&collateral_id, &stable_id, max_stable_in)?;
		let capacity = match quote {
			RecoveryOffsetQuote::NoTarget => {
				return Err(Error::<T>::RecoveryVaultNotFound.into());
			},
			RecoveryOffsetQuote::BelowPar => {
				return Err(Error::<T>::RecoveryOffsetBelowPar.into());
			},
			RecoveryOffsetQuote::Available { debt } => debt,
		};
		// Size the burn before touching anything: pool depth and the §6.5
		// floor cap what the settlement may take.
		let debt = math::clamp_offset_debt(
			max_stable_in.min(capacity),
			state.total_active_deposits,
			config.minimum_active_pool_balance,
		);
		ensure!(!debt.is_zero(), Error::<T>::NoRecoveryOffsetPerformed);

		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		let outcome = T::RecoveryOffsets::apply_recovery_offset(
			&collateral_id,
			&stable_id,
			&pool_account,
			&pool_account,
			debt,
		)?;
		// Same dispatch, same oracle price: the settlement burns exactly
		// what the clamp sized, keeping the §6.5 floor intact.
		ensure!(outcome.debt_cancelled == debt, Error::<T>::InvalidRecoveryOffsetSnapshot);

		Self::apply_active_offset(
			&collateral_id,
			&stable_id,
			&mut state,
			&config,
			outcome.debt_cancelled,
			outcome.collateral_out,
		)?;
		PoolStates::<T>::insert(&collateral_id, &stable_id, state);
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
		let (mut state, config) = Self::load_branch(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;
		let pending = deposit.pending_deposit.as_ref().ok_or(Error::<T>::NoPendingDeposit)?;
		ensure!(
			frame_system::Pallet::<T>::block_number() >= pending.activatable_at,
			Error::<T>::PendingDepositNotMatured
		);

		Self::realize_and_activate(
			&collateral_id,
			&stable_id,
			&who,
			&mut state,
			&config,
			&mut deposit,
		)?;
		debug_assert!(deposit.pending_deposit.is_none());

		Self::store_or_prune_deposit(&collateral_id, &stable_id, &who, deposit);
		PoolStates::<T>::insert(&collateral_id, &stable_id, state);
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
		let (mut state, config) = Self::load_branch(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;
		ensure!(!amount.is_zero(), Error::<T>::NoActiveDeposit);

		let activated = Self::realize_and_activate(
			&collateral_id,
			&stable_id,
			&who,
			&mut state,
			&config,
			&mut deposit,
		)?;

		let now = frame_system::Pallet::<T>::block_number();
		let executable_at = now.saturating_add(config.safety_withdrawal_delay);
		deposit.withdrawal_request = Some(WithdrawalRequest { amount, executable_at });

		Self::store_or_prune_deposit(&collateral_id, &stable_id, &who, deposit);
		// Requests live on the row; the pool state only changed if a pending
		// deposit activated along the way.
		if activated {
			PoolStates::<T>::insert(&collateral_id, &stable_id, state);
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
		let (mut state, config) = Self::load_branch(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;

		Self::realize_and_activate(
			&collateral_id,
			&stable_id,
			&who,
			&mut state,
			&config,
			&mut deposit,
		)?;

		let mode = Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let now = frame_system::Pallet::<T>::block_number();
		let take = Self::resolve_withdrawal(mode, now, amount, &mut deposit)?;
		ensure!(!take.is_zero(), Error::<T>::NoActiveDeposit);

		// `resolve_withdrawal` bounds `take` by the realized active deposit,
		// which flooring keeps at or below the pool aggregate.
		deposit.active_deposit =
			deposit.active_deposit.checked_sub(&take).ok_or(ArithmeticError::Underflow)?;
		state.total_active_deposits = state
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
		PoolStates::<T>::insert(&collateral_id, &stable_id, state);
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
		let (mut state, config) = Self::load_branch(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;

		Self::realize_and_activate(
			&collateral_id,
			&stable_id,
			&who,
			&mut state,
			&config,
			&mut deposit,
		)?;

		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		// Underflows on the unclaimed totals would mean a claimable exceeding
		// the tracked aggregate.
		let event = match kind {
			ClaimKind::Collateral => {
				let amount = deposit.claimable_collateral;
				ensure!(!amount.is_zero(), Error::<T>::NoClaimableCollateral);
				deposit.claimable_collateral = BalanceOf::<T>::zero();
				state.total_collateral_gains_unclaimed = state
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
				Event::CollateralClaimed {
					collateral_id: collateral_id.clone(),
					stable_id: stable_id.clone(),
					depositor: who.clone(),
					recipient,
					amount,
				}
			},
			ClaimKind::Yield => {
				let amount = deposit.claimable_yield;
				ensure!(!amount.is_zero(), Error::<T>::NoClaimableYield);
				deposit.claimable_yield = BalanceOf::<T>::zero();
				state.total_yield_unclaimed = state
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
				Event::YieldClaimed {
					collateral_id: collateral_id.clone(),
					stable_id: stable_id.clone(),
					depositor: who.clone(),
					recipient,
					amount,
				}
			},
		};

		Self::store_or_prune_deposit(&collateral_id, &stable_id, &who, deposit);
		PoolStates::<T>::insert(&collateral_id, &stable_id, state);
		Self::deposit_event(event);
		Ok(())
	}

	/// SPEC.md §7.1: burn active-pool stablecoin against ordinary liquidation
	/// debt, distributing the pro-rata collateral share to active depositors
	/// through `S`. Returns the actual offset, capped by pool depth and the
	/// `minimum_active_pool_balance` floor (§6.5); a zero-capacity pool
	/// no-ops with a zeroed result.
	///
	/// The caller must deliver exactly `collateral_to_pool` to
	/// [`Pallet::pool_account`] within the same transaction, and must run the
	/// call inside a storage layer so it rolls back entirely on error — the
	/// `StabilityPoolOffsetApi` impl (`interfaces.rs`) wraps it accordingly.
	pub(crate) fn do_offset_liquidation(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		max_debt_to_offset: BalanceOf<T>,
		collateral_for_pool: BalanceOf<T>,
	) -> Result<PoolOffsetResult<BalanceOf<T>>, DispatchError> {
		let (mut state, config) = Self::load_branch(collateral_id, stable_id)?;
		// Defense in depth: the vault engine already refuses to liquidate
		// on a frozen branch.
		Self::ensure_not_frozen(collateral_id, stable_id)?;

		let sp_offset_debt = math::clamp_offset_debt(
			max_debt_to_offset,
			state.total_active_deposits,
			config.minimum_active_pool_balance,
		);
		if sp_offset_debt.is_zero() {
			return Ok(PoolOffsetResult {
				debt_offset: BalanceOf::<T>::zero(),
				collateral_to_pool: BalanceOf::<T>::zero(),
			});
		}
		let sp_offset_collateral =
			math::pro_rata_floor(collateral_for_pool, sp_offset_debt, max_debt_to_offset);

		Self::burn_pool_stable(collateral_id, stable_id, sp_offset_debt)?;
		Self::apply_active_offset(
			collateral_id,
			stable_id,
			&mut state,
			&config,
			sp_offset_debt,
			sp_offset_collateral,
		)?;

		Self::deposit_event(Event::PoolOffsetApplied {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			debt_burned: sp_offset_debt,
			collateral_gain: sp_offset_collateral,
			epoch: state.epoch,
			scale: state.scale,
		});
		PoolStates::<T>::insert(collateral_id, stable_id, state);
		Ok(PoolOffsetResult {
			debt_offset: sp_offset_debt,
			collateral_to_pool: sp_offset_collateral,
		})
	}

	/// SPEC.md §7.2 / §6.8: the last-resort backstop — consume pending
	/// deposits oldest-first against liquidation debt that survived the
	/// active pool and JIT liquidity. Collateral is credited directly to the
	/// consumed depositors; `P`/`S`/`G` are never touched (invariant 11).
	/// An empty queue or zero remaining debt no-ops with a zeroed result.
	///
	/// Same delivery and atomicity contract as
	/// [`Pallet::do_offset_liquidation`].
	pub(crate) fn do_offset_pending_liquidation(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		remaining_debt: BalanceOf<T>,
		remaining_collateral: BalanceOf<T>,
		max_pending_iterations: u32,
	) -> Result<PendingOffsetResult<BalanceOf<T>>, DispatchError> {
		let mut state = PoolStates::<T>::get(collateral_id, stable_id)
			.ok_or(Error::<T>::BranchNotRegistered)?;
		Self::ensure_not_frozen(collateral_id, stable_id)?;
		let fifo = pending::list_id::<T>(collateral_id, stable_id);
		let cap = max_pending_iterations.min(T::MaxPendingOffsetIterations::get());

		let mut debt_left = remaining_debt;
		let mut collateral_left = remaining_collateral;
		let mut debt_burned = BalanceOf::<T>::zero();
		let mut collateral_credited = BalanceOf::<T>::zero();
		let mut iterations: u32 = 0;

		// Bounded by `cap <= MaxPendingOffsetIterations`
		while iterations < cap {
			if debt_left.is_zero() {
				break;
			}
			let Some(oldest) = T::PendingLists::tail(&fifo) else {
				break;
			};
			iterations = iterations.saturating_add(1);

			let mut row = Deposits::<T>::get((collateral_id, stable_id, &oldest))
				.ok_or(Error::<T>::PendingFifoInvariantBroken)?;
			let pending =
				row.pending_deposit.as_mut().ok_or(Error::<T>::PendingFifoInvariantBroken)?;

			// §6.8: each step prices against the remainders at its start.
			let step_debt = pending.amount.min(debt_left);
			let step_collateral = math::pro_rata_floor(collateral_left, step_debt, debt_left);

			pending.amount =
				pending.amount.checked_sub(&step_debt).ok_or(ArithmeticError::Underflow)?;
			if pending.amount.is_zero() {
				row.pending_deposit = None;
				pending::remove::<T>(collateral_id, stable_id, &oldest)?;
			}
			row.claimable_collateral = row
				.claimable_collateral
				.checked_add(&step_collateral)
				.ok_or(ArithmeticError::Overflow)?;
			state.total_pending_deposits = state
				.total_pending_deposits
				.checked_sub(&step_debt)
				.ok_or(ArithmeticError::Underflow)?;
			// Direct credits must still enter the unclaimed total, or claims
			// would break the pool-balance identity (SPEC.md §6.8 gap).
			state.total_collateral_gains_unclaimed = state
				.total_collateral_gains_unclaimed
				.checked_add(&step_collateral)
				.ok_or(ArithmeticError::Overflow)?;
			// Flooring can zero the credit; a fully-consumed row with no
			// other value must not linger.
			Self::store_or_prune_deposit(collateral_id, stable_id, &oldest, row);

			debt_left = debt_left.checked_sub(&step_debt).ok_or(ArithmeticError::Underflow)?;
			collateral_left = collateral_left
				.checked_sub(&step_collateral)
				.ok_or(ArithmeticError::Underflow)?;
			debt_burned = debt_burned.checked_add(&step_debt).ok_or(ArithmeticError::Overflow)?;
			collateral_credited = collateral_credited
				.checked_add(&step_collateral)
				.ok_or(ArithmeticError::Overflow)?;
		}

		if !debt_burned.is_zero() {
			// One aggregate burn instead of per-step burns: same result,
			// fewer balance mutations.
			Self::burn_pool_stable(collateral_id, stable_id, debt_burned)?;
			PoolStates::<T>::insert(collateral_id, stable_id, state);
			Self::deposit_event(Event::PendingDepositOffsetApplied {
				collateral_id: collateral_id.clone(),
				stable_id: stable_id.clone(),
				debt_burned,
				collateral_gain: collateral_credited,
				iterations,
			});
		}
		Ok(PendingOffsetResult {
			debt_offset: debt_burned,
			collateral_to_pool: collateral_credited,
			remaining_debt: debt_left,
			remaining_collateral: collateral_left,
			iterations_used: iterations,
		})
	}

	/// The shared active-pool accumulator update for ordinary liquidation
	/// and recovery offsets (invariant 8 by construction): record `delta_S`
	/// from the pre-offset totals FIRST (invariant 5, so gains distribute
	/// over the deposits that absorbed the loss), then shrink `P` — crossing
	/// scales or starting a new epoch as §6.4 requires, seeding a zero sums
	/// row for every new coordinate.
	///
	/// Accounting only: the caller has already removed the offset
	/// stablecoin from the pool account (a direct burn for liquidation
	/// offsets, the settlement's payer-side burn for recovery offsets).
	fn apply_active_offset(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		state: &mut PoolStateOf<T>,
		config: &StabilityPoolConfigOf<T>,
		debt: BalanceOf<T>,
		collateral: BalanceOf<T>,
	) -> DispatchResult {
		debug_assert!(!debt.is_zero());
		debug_assert!(debt <= state.total_active_deposits);

		let delta_s = math::delta_sum(collateral, state.p, state.total_active_deposits)
			.ok_or(ArithmeticError::Overflow)?;
		let mut sums =
			PoolSumsStore::<T>::get((collateral_id, stable_id, state.epoch, state.scale))
				.unwrap_or_default();
		sums.s_collateral =
			sums.s_collateral.checked_add(&delta_s).ok_or(ArithmeticError::Overflow)?;
		PoolSumsStore::<T>::insert((collateral_id, stable_id, state.epoch, state.scale), sums);
		state.total_collateral_gains_unclaimed = state
			.total_collateral_gains_unclaimed
			.checked_add(&collateral)
			.ok_or(ArithmeticError::Overflow)?;

		let update = math::update_p_after_offset(
			state.p,
			state.total_active_deposits,
			debt,
			&config.precision(),
		)
		.ok_or(Error::<T>::UnsupportedOffsetPrecision)?;
		match update {
			math::PUpdate::Depleted => {
				state.total_active_deposits = BalanceOf::<T>::zero();
				state.epoch = state.epoch.checked_add(1).ok_or(ArithmeticError::Overflow)?;
				state.scale = 0;
				state.p = FixedU128::one();
				PoolSumsStore::<T>::insert(
					(collateral_id, stable_id, state.epoch, 0u32),
					PoolSums::default(),
				);
			},
			math::PUpdate::Updated { new_p, scales_crossed } => {
				state.total_active_deposits = state
					.total_active_deposits
					.checked_sub(&debt)
					.ok_or(ArithmeticError::Underflow)?;
				state.p = new_p;
				// Bounded by `math::MAX_SCALE_CROSSINGS`.
				for _ in 0..scales_crossed {
					state.scale = state.scale.checked_add(1).ok_or(ArithmeticError::Overflow)?;
					PoolSumsStore::<T>::insert(
						(collateral_id, stable_id, state.epoch, state.scale),
						PoolSums::default(),
					);
				}
			},
		}
		Ok(())
	}

	/// Burn `amount` stablecoin held by the pool account: withdraw it as a
	/// credit and drop the credit, rescinding issuance. The pool-balance
	/// identity guarantees the balance covers every offset this pallet
	/// authorizes.
	fn burn_pool_stable(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		let pool_account = Self::pool_account(collateral_id, stable_id);
		let credit = <T::StableAssets as FungiblesBalanced<T::AccountId>>::withdraw(
			stable_id.clone(),
			&pool_account,
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
	/// credit when the branch is unknown, the active pool is empty, or the
	/// deposit into the pool account fails — so the caller routes the
	/// remainder to its fee destination. Infallible by design: this runs on
	/// the vault engine's commit paths, which must not fail over yield
	/// routing. The vault engine reaches it through the `OnBranchYield`
	/// impl (`interfaces.rs`), which takes the `yield_share` cut first.
	pub(crate) fn do_distribute_yield(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		credit: StableCreditOf<T>,
	) -> StableCreditOf<T> {
		let amount = credit.peek();
		if amount.is_zero() {
			return credit;
		}
		let Some(mut state) = PoolStates::<T>::get(collateral_id, stable_id) else {
			return credit;
		};
		if state.total_active_deposits.is_zero() {
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
		let Some(delta_g) = math::delta_sum(amount, state.p, state.total_active_deposits) else {
			return credit;
		};
		let mut sums =
			PoolSumsStore::<T>::get((collateral_id, stable_id, state.epoch, state.scale))
				.unwrap_or_default();
		let Some(new_g) = sums.g_yield.checked_add(&delta_g) else {
			return credit;
		};
		let Some(new_total_yield) = state.total_yield_unclaimed.checked_add(&amount) else {
			return credit;
		};

		let pool_account = Self::pool_account(collateral_id, stable_id);
		let credit = match T::StableAssets::resolve(&pool_account, credit) {
			Ok(()) => StableCreditOf::<T>::zero(stable_id.clone()),
			Err(credit) => return credit,
		};

		sums.g_yield = new_g;
		PoolSumsStore::<T>::insert((collateral_id, stable_id, state.epoch, state.scale), sums);
		state.total_yield_unclaimed = new_total_yield;
		PoolStates::<T>::insert(collateral_id, stable_id, state);
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
		let (mut state, config) = Self::load_branch(&collateral_id, &stable_id)?;
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &who))
			.ok_or(Error::<T>::DepositNotFound)?;

		Self::realize_and_activate(
			&collateral_id,
			&stable_id,
			&who,
			&mut state,
			&config,
			&mut deposit,
		)?;

		let take = amount.min(deposit.claimable_yield);
		ensure!(!take.is_zero(), Error::<T>::NoYieldToCompound);
		deposit.claimable_yield =
			deposit.claimable_yield.checked_sub(&take).ok_or(ArithmeticError::Underflow)?;
		deposit.active_deposit =
			deposit.active_deposit.checked_add(&take).ok_or(ArithmeticError::Overflow)?;
		state.total_active_deposits = state
			.total_active_deposits
			.checked_add(&take)
			.ok_or(ArithmeticError::Overflow)?;
		// An underflow would mean a claimable exceeding the tracked total.
		state.total_yield_unclaimed = state
			.total_yield_unclaimed
			.checked_sub(&take)
			.ok_or(ArithmeticError::Underflow)?;

		Self::store_or_prune_deposit(&collateral_id, &stable_id, &who, deposit);
		PoolStates::<T>::insert(&collateral_id, &stable_id, state);
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
		let (state, config) = Self::load_branch(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &owner))
			.ok_or(Error::<T>::DepositNotFound)?;

		Self::realize_deposit(&collateral_id, &stable_id, &state, &config, &mut deposit)?;
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
		now: BlockNumberFor<T>,
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
		state: &PoolStateOf<T>,
		config: &StabilityPoolConfigOf<T>,
		deposit: &mut DepositOf<T>,
	) -> DispatchResult {
		let snapshot = math::DepositSnapshot {
			p: deposit.snapshot_p,
			s: deposit.snapshot_s,
			g: deposit.snapshot_g,
			epoch: deposit.snapshot_epoch,
			scale: deposit.snapshot_scale,
		};
		let current = PoolSumsStore::<T>::get((collateral_id, stable_id, state.epoch, state.scale))
			.unwrap_or_default();
		// A snapshot already at the live coordinates realizes against the
		// current row alone — no row above the live scale can exist — which
		// makes the snapshot-reset read below cover the whole window.
		let window =
			if deposit.snapshot_epoch == state.epoch && deposit.snapshot_scale == state.scale {
				math::SumsWindow {
					s_snap: current.s_collateral,
					g_snap: current.g_yield,
					s_next: FixedU128::zero(),
					g_next: FixedU128::zero(),
				}
			} else {
				Self::sums_window(
					collateral_id,
					stable_id,
					deposit.snapshot_epoch,
					deposit.snapshot_scale,
				)
			};
		let realized = math::realize(
			deposit.active_deposit,
			&snapshot,
			&state.accumulators(),
			&window,
			&config.precision(),
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

		deposit.snapshot_p = state.p;
		deposit.snapshot_s = current.s_collateral;
		deposit.snapshot_g = current.g_yield;
		deposit.snapshot_epoch = state.epoch;
		deposit.snapshot_scale = state.scale;
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
	) -> Result<bool, DispatchError> {
		debug_assert!(deposit.snapshot_p == state.p);
		debug_assert!(deposit.snapshot_epoch == state.epoch);
		let Some(pending) = &deposit.pending_deposit else {
			return Ok(false);
		};
		if frame_system::Pallet::<T>::block_number() < pending.activatable_at {
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
		pending::remove::<T>(collateral_id, stable_id, who)?;
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
			let current =
				PoolSumsStore::<T>::get((collateral_id, stable_id, state.epoch, state.scale))
					.unwrap_or_default();
			Deposit::fresh(state.p, &current, state.epoch, state.scale)
		})
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

	/// The two sums rows a snapshot realizes against: its own `(epoch,
	/// scale)` row and the next scale's (zero while absent). Missing snapshot
	/// rows read as zero, which floors gains instead of overpaying them.
	pub(crate) fn sums_window(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		epoch: u32,
		scale: u32,
	) -> math::SumsWindow {
		let snap =
			PoolSumsStore::<T>::get((collateral_id, stable_id, epoch, scale)).unwrap_or_default();
		let next =
			PoolSumsStore::<T>::get((collateral_id, stable_id, epoch, scale.saturating_add(1)))
				.unwrap_or_default();
		math::SumsWindow {
			s_snap: snap.s_collateral,
			g_snap: snap.g_yield,
			s_next: next.s_collateral,
			g_next: next.g_yield,
		}
	}
}
