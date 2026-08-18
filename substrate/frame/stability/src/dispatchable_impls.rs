//! The bodies of the dispatchables, and the internals they share.
//!
//! Two rules shape almost every function here.
//!
//! A row is realized before it changes. Losses and gains are settled against the live
//! accumulators, and the snapshot is reset, so the change that follows applies to a current
//! amount.
//!
//! Value moves last. Every fallible step runs while the pool is untouched, so a failure leaves
//! nothing half done.

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

/// Which gain a claim pays out. The two sides share [`Pallet::do_claim`].
#[derive(Clone, Copy)]
pub(crate) enum ClaimKind {
	Collateral,
	Yield,
}

/// The state an offset leaves behind, computed in full before anything is written.
///
/// [`Pallet::plan_offset`] does all the arithmetic that can fail, and [`Pallet::commit_offset`]
/// then only writes.
struct OffsetPlan<Balance> {
	new_sums: PoolSums,
	new_unclaimed: Balance,
	new_total: Balance,
	new_coords: Accumulators,
}

impl<T: Config> Pallet<T> {
	/// The pool of a market. A market is registered exactly while its row exists.
	fn load_pool(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<StabilityPoolOf<T>, DispatchError> {
		let pool =
			Pools::<T>::get(collateral_id, stable_id).ok_or(Error::<T>::PoolNotRegistered)?;
		Ok(pool)
	}

	/// Brings a row up to date: settle its losses and gains, then activate a matured pending
	/// deposit.
	///
	/// Every operation that moves value runs this before its own change. Returns whether an
	/// activation happened, which is also whether `pool` needs to be written back.
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

	/// Takes stablecoin from `who`, settles what it can against a vault in `FinalRecovery`, and
	/// queues the rest behind the entry delay.
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
			let activatable_at = now.saturating_add(pool.config.entry_delay);
			match deposit.pending_deposit.as_mut() {
				Some(pending) => {
					// The realization above settled earlier backstop losses and reset the
					// snapshot, so the merged amount joins at the current pending accumulators. A
					// top-up restarts the delay for the whole amount; it must never shorten the
					// wait.
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

	/// Settles as much of an incoming deposit as the `FinalRecovery` head can take, and returns
	/// what is left.
	///
	/// The collateral goes straight to the depositor as a claimable balance. The settled
	/// stablecoin never reaches the pool account and never touches `P`, `S` or `G`, so it earns no
	/// share of anything the pool already holds. A head below par rejects the whole deposit,
	/// because settlement at a discount stays exclusive to the redemption path.
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

	/// Settles active pool stablecoin against the `FinalRecovery` head, then runs the ordinary
	/// active-leg offset.
	///
	/// Reusing the liquidation path is what keeps a recovery offset indistinguishable from a
	/// liquidation for the depositors: same accumulators, same rounding, same events on the row.
	pub(crate) fn do_offset_recovery(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		max_stable_in: BalanceOf<T>,
	) -> DispatchResult {
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		// Settling recovery debt reduces risk, so Safety Mode allows it. Only a freeze stops it.
		Self::ensure_not_frozen(&collateral_id, &stable_id)?;

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

	/// Records a Safety-Mode withdrawal request, or withdraws at once in Normal Mode.
	///
	/// A new request replaces any earlier one. In Normal Mode the exit is immediate, so a request
	/// would serve no purpose and the call withdraws to the caller instead.
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
		// The request lives on the row. The pool changed only if a pending deposit activated.
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

	/// Pays active stablecoin out of the pool.
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

	/// Pays a claimable balance out.
	///
	/// The two claim sides differ only in the field they empty, the total they reduce, the asset
	/// they pay in, and the error and event they use.
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

	/// How much debt an offset of at most `max_debt` may cancel on `leg`, and the `Preservation`
	/// that sizing proved valid for the burn.
	///
	/// Four limits apply: the depth of the leg, the post-offset floor, what the pool account may
	/// pay above its minimum balance, and the `P` precision guard. Set `reserved` to the debt
	/// another leg of the same offset will take from the same account first.
	///
	/// The precision guard matters here and not only at settlement. Without it a caller could
	/// allocate collateral to a leg that then declines the burn, and the collateral would be
	/// stranded.
	///
	/// The returned `Preservation` is computed against the combined limit. With a non-zero
	/// `reserved` it stays valid only if the reserved part leaves the account first, which is why
	/// [`Pallet::offset`] settles active before pending.
	///
	/// `minimum_active_pool_balance` bounds the pending leg too. It is the parameter that sizes a
	/// leg against `P` running out of precision, and the pending `P` uses the same precision
	/// parameters as the active one.
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

	/// Empties a claimable field and takes the same amount off its pool total.
	///
	/// An underflow would mean a row claiming more than the pool tracks.
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

	/// Pays a claim out of the pool account.
	///
	/// `Expendable` only on a full drain, so the transfer itself rejects a payout that would leave
	/// the pool account below the minimum balance.
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

	/// Settles one sized reservation on `leg`: burn the stablecoin, take the collateral, and share
	/// it out through that leg's `S`.
	///
	/// The liquidation engine hands over the collateral in full, so any disagreement with the
	/// reservation aborts the whole transaction rather than keep part of it.
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

	/// The value movement both legs share: burn the reserved stablecoin and take the whole
	/// collateral credit into the pool account. Returns how much collateral arrived.
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

	/// Works out the state an offset leaves behind, without writing anything.
	///
	/// `S` grows against the total from before the offset, and only then does `P` shrink.
	/// Reversing the order would pay the depositors a share computed from capital the offset has
	/// already consumed.
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

	/// Writes an [`OffsetPlan`], seeding an empty sums row for every coordinate the offset opened.
	///
	/// Cannot fail: [`Pallet::plan_offset`] already did every calculation.
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

	/// Plans and commits in one step, for the recovery path, which moves no value in between.
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

	/// Shares stablecoin yield out to the active depositors through `G`, and returns what it could
	/// not share.
	///
	/// Cannot fail. The vault engine mints yield on commit paths that must not roll a user
	/// operation back over a routing problem. When the pool cannot take the credit, all of it
	/// comes back and the caller sends it to its fee destination.
	///
	/// The vault engine reaches this through the `OnBranchYield` implementation in `interfaces`,
	/// which reads the pool row once, takes the `yield_share` cut, and hands the row down.
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
		// A frozen market, or one whose mode cannot be read, takes no yield.
		match T::BranchModes::branch_mode(collateral_id, stable_id) {
			Ok(BranchMode::Normal) | Ok(BranchMode::Safety) => {},
			Ok(BranchMode::Frozen) | Err(_) => return credit,
		}

		// Every fallible step runs before the credit is consumed. Once `resolve` succeeds, only
		// plain writes remain.
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

	/// Moves claimable yield into the active deposit.
	///
	/// The stablecoin already sits in the pool account, so only the accounting moves. The
	/// realization that runs first has reset the snapshot, so the amount joins at the live
	/// accumulators and earns nothing that predates the move.
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

	/// Realizes another account's deposit and activates a matured pending deposit of theirs.
	///
	/// Moves no value. Past the entry delay, activation is mechanical, so its outcome does not
	/// depend on who asks for it. A frozen market still realizes but does not activate, because
	/// activation changes how much risk the pool carries.
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
		// Realization lives on the row. The pool changed only if a pending deposit activated.
		if activated {
			Pools::<T>::insert(&collateral_id, &stable_id, pool);
		}
		Ok(())
	}

	/// How much stablecoin a withdrawal may take, per operating mode.
	///
	/// - `Normal`: up to the active deposit. Any outstanding request is ignored, because requests
	///   are Safety-Mode state; one left behind is bounded by the live active deposit and goes with
	///   the row.
	/// - `Safety`: needs a request past its `executable_at`, and takes the amount off it.
	/// - `Frozen`: rejected.
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

	/// Stops an operation while the market is frozen, and reports the live mode.
	///
	/// A market with no usable price is frozen too: the mode provider fails closed.
	pub(crate) fn ensure_not_frozen(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<BranchMode, DispatchError> {
		let mode = T::BranchModes::branch_mode(collateral_id, stable_id)?;
		ensure!(mode != BranchMode::Frozen, Error::<T>::BranchFrozen);
		Ok(mode)
	}

	/// Settles the losses and gains of both legs of a row, and resets its snapshots.
	///
	/// Never touches the pool totals. An offset already changed those when the loss happened; this
	/// only works out which row carries which part of it.
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

	/// The pending leg of [`Pallet::realize_deposit`].
	///
	/// A pending deposit the backstop consumed in full is dropped. Its rounding remainder stays
	/// inside `total_pending_deposits`, as every other remainder does.
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
		// Pending deposits earn no yield, so `G` is zero here by construction.
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

	/// Realizes `amount` on one leg, and returns the settled values with the reset snapshot.
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

	/// Moves a matured pending deposit into the active deposit.
	///
	/// Must run after [`Pallet::realize_deposit`]. Both legs then stand at their live
	/// accumulators, so the amount that moves is already net of backstop losses and cannot collect
	/// gains from offsets older than itself.
	///
	/// Does nothing while the pending deposit is absent or immature. Returns whether it moved,
	/// which is also whether `state` changed.
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
		// An underflow would mean a realized pending deposit above the pool total. Rounding only
		// ever leaves the rows at or below it.
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

	/// The row of a depositor, or an empty one at the live coordinates.
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

	/// Checks and stores replacement pool parameters.
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

	/// Writes a row back, or removes it once it holds no user value.
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

	/// The sums row of `leg` at `coords`. An absent row reads as zero, which rounds gains down
	/// rather than overpay them.
	pub(crate) fn sums_at(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		coords: &Accumulators,
	) -> PoolSums {
		PoolSumsStore::<T>::get((collateral_id, stable_id, leg, coords.epoch, coords.scale))
	}

	/// The sums rows a snapshot realizes against: its own row plus the next `math::SCALE_SPAN`
	/// scales.
	///
	/// Rows are seeded without gaps within an epoch, so the read stops at the first missing one.
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
