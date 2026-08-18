use crate::{
	interfaces::OffsetReservation,
	math,
	pallet::{
		BalanceOf, CollateralCreditOf, CollateralIdOf, Config, DepositOf, Deposits, Error, Event,
		Pallet, PoolStateOf, PoolSumsStore, Pools, StabilityPoolConfigOf, StabilityPoolOf,
		StableCreditOf, StableIdOf,
	},
	types::{
		Accumulators, Deposit, DepositSnapshot, Leg, PUpdate, PendingDeposit, PoolSums, Realized,
		RecoveryOffsetSource, SumsWindow, WithdrawalRequest,
	},
};
use frame::{
	prelude::*,
	traits::{
		fungibles::{Balanced as _, Inspect as _, Mutate as _},
		tokens::{Fortitude, Precision, Preservation, Provenance},
		Defensive, Time,
	},
};
use pusd_primitives::{
	debit_preservation, reducible_debit, BranchMode, BranchModeProvider, Millis,
	RecoveryOffsetInterface, RecoveryOffsetResult,
};

/// Which realized gain a claim pays out; the two sides share one flow
/// ([`Pallet::do_claim`]).
#[derive(Clone, Copy)]
pub(crate) enum ClaimKind {
	Collateral,
	Yield,
}

/// The fully-materialized post-state of a product-sum offset on either
/// leg: all fallible math runs in
/// [`Pallet::plan_offset`] before any value moves; [`Pallet::commit_offset`]
/// then only writes.
struct OffsetPlan<Balance> {
	new_sums: PoolSums,
	new_unclaimed: Balance,
	new_total: Balance,
	new_coords: Accumulators,
}

impl<T: Config> Pallet<T> {
	/// The shared entry-point prologue: a branch is registered iff its pool
	/// row exists.
	fn load_pool(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<StabilityPoolOf<T>, DispatchError> {
		let pool =
			Pools::<T>::get(collateral_id, stable_id).ok_or(Error::<T>::PoolNotRegistered)?;
		Ok(pool)
	}

	/// The realization pair every value-moving entry point runs before its own
	/// change: settle gains/losses into the row, then fold in a matured
	/// pending deposit. Returns whether an activation happened (i.e. whether
	/// `state` changed).
	fn realize_and_activate(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		who: &T::AccountId,
		pool: &mut StabilityPoolOf<T>,
		deposit: &mut DepositOf<T>,
		now: Millis,
	) -> Result<bool, DispatchError> {
		Self::realize_deposit(collateral_id, stable_id, pool, deposit)?;
		Self::activate_matured_pending(collateral_id, stable_id, who, &mut pool.state, deposit, now)
	}

	/// Realize, activate any matured pending deposit, attempt an
	/// incoming-deposit recovery offset, and queue whatever the
	/// settlement did not use behind the entry delay.
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
		Self::realize_and_activate(&collateral_id, &stable_id, &who, &mut pool, &mut deposit, now)?;

		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		// One withdrawal funds both halves: the recovery settlement consumes
		// its slice from the credit and the change becomes the pending
		// deposit. `Expendable` only on a full drain, so the withdrawal
		// itself rejects a dead-zone amount instead of folding the dust in.
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
		// Conservation by construction: the settlement can only have burned
		// value the credit carried.
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
			let activatable_at = now.saturating_add(pool.config.entry_delay);
			match deposit.pending_deposit.as_mut() {
				Some(pending) => {
					// The realization above settled earlier backstop losses
					// and reset the snapshot, so the merged amount joins at
					// the current pending accumulators. A top-up resets the
					// whole pending amount's entry delay — it must never
					// shorten the wait.
					pending.amount = pending
						.amount
						.checked_add(&pending_amount)
						.ok_or(ArithmeticError::Overflow)?;
					pending.activatable_at = activatable_at;
				},
				None => {
					let current = Self::sums_at(
						&collateral_id,
						&stable_id,
						Leg::Pending,
						&pool.state.pending_coords,
					);
					deposit.pending_deposit = Some(PendingDeposit {
						amount: pending_amount,
						activatable_at,
						snapshot: pool.state.snapshot(Leg::Pending, &current),
					});
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

	/// Settle up to the incoming deposit credit against an
	/// at-or-above-par `FinalRecovery` head, crediting the priced collateral
	/// directly to the depositor. The used portion never touches the pool's
	/// stablecoin balance or `P`/`S`/`G` (invariant 7); the unconsumed
	/// change returns to the caller to become the pending deposit. A
	/// below-par head rejects the whole deposit. Returns only the unconsumed
	/// change; the caller derives what the settlement used from it.
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
		// Conservation by construction: the settlement can only have burned
		// value the credit carried.
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

	/// Burn active pool stablecoin against the `FinalRecovery`
	/// head at the shared settlement pricing, then run the standard
	/// active-pool accumulator update — the same code path as ordinary
	/// liquidation offsets (invariant 8 by construction).
	pub(crate) fn do_offset_recovery(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		max_stable_in: BalanceOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		// Recovery offsets are settlement operations: allowed in Safety
		// Mode, halted only by Frozen.
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;

		// Size the burn before touching anything: pool depth and the
		// post-offset floor cap the accounting, and the burnable amount caps that — a
		// minimum-balance dead zone rounds the offset down instead of
		// dusting the pool account.
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
		// Conservation by construction: the settlement can only have burned
		// value the credit carried.
		let debt_cancelled = funded.saturating_sub(change.peek());
		ensure!(!debt_cancelled.is_zero(), Error::<T>::NoRecoveryOffsetPerformed);
		if let Err(change) = change.drop_zero() {
			// Return the unburned slice to the pool. Only a full-drain
			// withdrawal whose head cancelled less, leaving a sub-minimum
			// change, can be refused here: the revert asks the offsetter to
			// size `max_stable_in` from the preview instead of dusting the
			// pool.
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

	/// Create or replace the caller's Safety-Mode withdrawal
	/// request, `executable_at` stamped `safety_withdrawal_delay` from now.
	/// In Normal Mode a request has no purpose — the exit is immediate — so
	/// the call forwards to [`Pallet::do_withdraw`] paying the caller.
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

	/// Withdraw active stablecoin — immediately in Normal Mode,
	/// against an executable request in Safety Mode.
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
		// `Expendable` only on a full drain: the transfer itself then rejects
		// a dead-zone payout instead of dusting the pool account.
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

	/// Pay out the caller's realized gains — one flow for both
	/// claim sides, which differ only in the claimed field, its aggregate,
	/// the paying asset surface, and the error/event pair.
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

		Self::realize_and_activate(
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

	/// The debt an offset of at most `max_debt` on `leg` may burn: the
	/// leg-depth and `minimum_active_pool_balance` clamp, the pool
	/// account's minimum-balance dead zone (with `reserved` set aside for
	/// another stage of the same transaction), and the `P`-precision guard.
	/// The guard matters on the capacity side too — without it a
	/// caller could allocate collateral to a stage that then steps aside,
	/// stranding the slice. The returned `Preservation` sizes the burn debit;
	/// with a non-zero `reserved` it is computed against the combined limit and
	/// stays valid only if the reserved tranche is debited from the account
	/// first (the exact offset call settles active before pending).
	///
	/// The `minimum_active_pool_balance` floor applies to the pending leg
	/// too — it is what sizes a leg against `P`-precision exhaustion, and the
	/// pending `P` runs on the same precision parameters.
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
		// The burnable amount caps the accounting: a minimum-balance dead
		// zone rounds the offset down instead of dusting the pool account.
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

	/// Zero a realized claimable field and remove it from its pool aggregate.
	/// An underflow on the aggregate would mean a claimable exceeding the
	/// tracked total.
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

	/// Pay a taken claim out of the pool account. `Expendable` only on a full
	/// drain: the transfer itself then rejects a dead-zone payout instead of
	/// dusting the pool.
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

	/// Settle one previously sized reservation on `leg`
	/// exactly — burn the reserved stablecoin against liquidation debt and
	/// resolve the assigned collateral credit into the pool account,
	/// distributing it to that leg's depositors through its `S`. The
	/// production liquidation contract: the collateral is consumed in full and
	/// any disagreement with the reservation aborts the transaction.
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
		let collateral_amount = Self::settle_reservation_exact(
			collateral_id,
			stable_id,
			pool_account,
			reservation,
			collateral,
		)?;
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

	/// The value movement both legs share: burn the reserved stable
	/// debit and resolve the whole collateral credit into the pool account.
	/// Returns the collateral amount consumed.
	fn settle_reservation_exact(
		collateral_id: &CollateralIdOf<T>,
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
		// A zero credit is dropped without touching the account — a zero
		// deposit into a not-yet-existing account is the only failure it
		// could hit. A real failure means a sub-minimum first gain.
		if let Err(collateral) = collateral.drop_zero() {
			T::CollateralAssets::resolve(pool_account, collateral)
				.map_err(|_| Error::<T>::OffsetSettlementFailed)?;
		}
		// Dropping the withdrawn credit is the debt-cancelling burn.
		drop(stable_credit);
		Ok(collateral_amount)
	}

	/// The accumulator math shared by liquidation offsets on either leg and
	/// by recovery offsets: `delta_S` from the pre-offset total FIRST, then
	/// the `P` shrink. Read-only: the caller commits the plan once its value
	/// movement is through.
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

	/// Write an [`OffsetPlan`] into `leg`'s sums rows and `state`, seeding a
	/// zero sums row for every new coordinate. Infallible: all arithmetic
	/// already ran in [`Pallet::plan_offset`].
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

	/// Plan-and-commit in one step for the extrinsic-transactional recovery
	/// path ([`Pallet::do_offset_recovery`]), which interleaves no value
	/// ops. Accounting only: the settlement already moved the stablecoin
	/// and the collateral.
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

	/// Distribute same-stablecoin yield to active depositors
	/// through `G`, returning whatever could not be distributed — the whole
	/// credit when the active pool is empty, the branch is frozen, or the
	/// deposit into the pool account fails — so the caller routes the
	/// remainder to its fee destination. Infallible by design: this runs on
	/// the vault engine's commit paths, which must not fail over yield
	/// routing. The vault engine reaches it through the `OnBranchYield`
	/// impl (`interfaces.rs`), which loads `pool`, takes the `yield_share`
	/// cut, and hands the row down so the branch is read once.
	pub(crate) fn do_distribute_yield(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
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
		// routes the credit to its fee destination.
		match T::BranchModes::branch_mode(collateral_id, stable_id) {
			Ok(BranchMode::Normal) | Ok(BranchMode::Safety) => {},
			Ok(BranchMode::Frozen) | Err(_) => return credit,
		}

		// Every fallible step runs before the credit is consumed; after
		// `resolve` succeeds only plain writes remain.
		let Some(delta_g) = pool.state.delta_sum(amount) else {
			return credit;
		};
		let mut sums = Self::sums_at(collateral_id, stable_id, Leg::Active, &pool.state.coords);
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
		Pools::<T>::insert(collateral_id, stable_id, pool);
		Self::deposit_event(Event::YieldDistributed {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			amount,
		});
		credit
	}

	/// Move up to `amount` of realized claimable yield into
	/// the active deposit. The funds already sit in the pool account, so
	/// only the accounting moves; the realization that precedes this has
	/// already reset the snapshots, so the compounded amount joins at the
	/// current accumulators.
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

	/// Permissionlessly realize `owner`'s
	/// deposit without moving value, and fold in a matured pending deposit.
	/// A matured pending deposit needs a touch to fold in; past the entry
	/// delay the move is mechanical, so any caller may supply that touch.
	/// A frozen branch skips the activation (it changes offsettable risk,
	/// which the freeze halts) but still realizes.
	pub(crate) fn do_poke_deposit(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		let mut deposit = Deposits::<T>::get((&collateral_id, &stable_id, &owner))
			.ok_or(Error::<T>::DepositNotFound)?;

		Self::realize_deposit(&collateral_id, &stable_id, &pool, &mut deposit)?;
		let activated = if Self::ensure_not_frozen(&collateral_id, &stable_id).is_ok() {
			Self::activate_matured_pending(
				&collateral_id,
				&stable_id,
				&owner,
				&mut pool.state,
				&mut deposit,
				T::TimeProvider::now(),
			)?
		} else {
			false
		};
		Self::store_or_prune_deposit(&collateral_id, &stable_id, &owner, deposit);
		// Realization lives on the row; the pool row only changed if a
		// pending deposit activated along the way.
		if activated {
			Pools::<T>::insert(&collateral_id, &stable_id, pool);
		}
		Ok(())
	}

	/// How much a withdrawal may take, per mode:
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

	/// Every value-moving pool operation halts while the
	/// branch is Frozen (which includes oracle failure — the provider fails
	/// closed). Returns the live mode for operations that differentiate
	/// Normal from Safety.
	pub(crate) fn ensure_not_frozen(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<BranchMode, DispatchError> {
		let mode = T::BranchModes::branch_mode(collateral_id, stable_id)?;
		ensure!(mode != BranchMode::Frozen, Error::<T>::BranchFrozen);
		Ok(mode)
	}

	/// Settle accumulated losses and gains into the row — the active leg AND
	/// the pending leg — and reset its snapshots to the pool's current
	/// coordinates. Never touches pool totals: offsets already
	/// updated the aggregates when the losses happened.
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
		Self::realize_pending(collateral_id, stable_id, pool, deposit)
	}

	/// The pending leg of [`Pallet::realize_deposit`]: settle backstop losses
	/// and direct collateral gains into the row and reset the pending
	/// snapshot. A pending fully consumed by the backstop is dropped — its
	/// flooring residue stays inside `total_pending_deposits` like every
	/// other aggregate residue.
	fn realize_pending(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		pool: &StabilityPoolOf<T>,
		deposit: &mut DepositOf<T>,
	) -> DispatchResult {
		let Some(pending) = deposit.pending_deposit.as_mut() else {
			return Ok(());
		};
		let (realized, snapshot) = Self::realize_leg(
			collateral_id,
			stable_id,
			Leg::Pending,
			pool,
			pending.amount,
			&pending.snapshot,
		);
		// Pending deposits earn no yield: `G` is structurally zero here.
		debug_assert!(realized.yield_gain.is_zero());
		pending.amount = realized.compounded;
		pending.snapshot = snapshot;

		deposit.claimable_collateral = deposit
			.claimable_collateral
			.checked_add(&realized.collateral_gain)
			.ok_or(ArithmeticError::Overflow)?;
		if realized.compounded.is_zero() {
			deposit.pending_deposit = None;
		}
		Ok(())
	}

	/// Realize `amount` (as of `snapshot`) on `leg` against the pool's live
	/// coordinates: the settled values plus the reset snapshot at those
	/// coordinates.
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
		// A snapshot already at the live coordinates realizes against the
		// current row alone — no row above the live scale can exist — which
		// makes the snapshot-reset read cover the whole window.
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

	/// Fold a matured pending deposit into the active deposit.
	/// Must run after [`Self::realize_deposit`] — both legs join at the
	/// current accumulators, so the activated amount is net of backstop
	/// losses and cannot receive gains from offsets that predate its
	/// activation. No-op while immature or absent; returns whether an
	/// activation happened (i.e. whether `state` changed).
	fn activate_matured_pending(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
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
		debug_assert!(pending.snapshot.coords.p == state.pending_coords.p);
		debug_assert!(pending.snapshot.coords.epoch == state.pending_coords.epoch);
		let amount = pending.amount;
		deposit.active_deposit =
			deposit.active_deposit.checked_add(&amount).ok_or(ArithmeticError::Overflow)?;
		state.total_active_deposits = state
			.total_active_deposits
			.checked_add(&amount)
			.ok_or(ArithmeticError::Overflow)?;
		// An underflow would mean a realized pending exceeding the tracked
		// aggregate — flooring only ever leaves the rows at or below it.
		state.total_pending_deposits = state
			.total_pending_deposits
			.checked_sub(&amount)
			.ok_or(ArithmeticError::Underflow)?;
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

	/// Validate and store a replacement pool config.
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

	/// Write the row back, or remove it once it holds no user value.
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

	/// The sums row of `leg` at `coords`; an absent row reads as zero,
	/// which floors gains instead of overpaying them.
	pub(crate) fn sums_at(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		coords: &Accumulators,
	) -> PoolSums {
		PoolSumsStore::<T>::get((collateral_id, stable_id, leg, coords.epoch, coords.scale))
	}

	/// The sums rows a snapshot on `leg` realizes against: its own
	/// `(epoch, scale)` row plus the [`math::SCALE_SPAN`] scales after it.
	/// Rows are seeded contiguously per epoch, so reading stops at the first
	/// gap (`try_get` keeps absence observable through the `ValueQuery`).
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
