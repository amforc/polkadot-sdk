//! Internal accounting and value-transfer contracts of the dispatchable calls.
//!
//! Each row settles before a change. This rule makes the result independent of account touch
//! order.
//!
//! Offset accounting completes its arithmetic before its first storage write. Every offset runs
//! inside a storage transaction of its caller, so a failed value movement rolls the accounting
//! back. These rules prevent partial accounting state.

use crate::{
	interfaces::OffsetReservation,
	math,
	pallet::{
		BalanceOf, CohortCheckpointOf, CohortCheckpoints, CollateralCreditOf, CollateralIdOf,
		Config, DepositOf, Deposits, Error, Event, Pallet, PoolStateOf, PoolSumsStore, Pools,
		StabilityPoolConfigOf, StabilityPoolOf, StableCreditOf, StableIdOf, MAX_PRUNE_SUMS,
	},
	types::{
		Accumulators, CohortCheckpoint, CohortId, Deposit, DepositSnapshot, Leg, LegCoords,
		MaxOpenCohorts, OpenCohort, PUpdate, PendingDeposit, PoolSums, Realized, SumsWindow,
		WithdrawalRequest,
	},
	weights::WeightInfo,
};
use frame::{
	deps::frame_support::{
		dispatch::WithPostDispatchInfo,
		storage::{with_transaction, StorageNMap as _, TransactionOutcome},
	},
	prelude::*,
	traits::{
		fungibles::{Balanced as _, Inspect as _},
		tokens::{Fortitude, Precision, Preservation, Provenance},
		Defensive, DefensiveOption, DefensiveSaturating, Time,
	},
};
use stablecoin_primitives::{
	debit_preservation, BranchInterface, BranchMode, BranchSnapshot, Millis,
	RecoveryOffsetInterface, RecoveryOffsetResult,
};

/// Asset type paid by [`Pallet::do_claim`].
#[derive(Clone, Copy)]
pub(crate) enum ClaimKind {
	Collateral,
	Yield,
}

/// The pool account's stablecoin custody, read once for every debit an operation sizes.
///
/// Only the `Preserve` reducible balance is read: the custody float sits beneath the tracked
/// totals, so that balance is exactly the value the pool may pay out, and an `Expendable` drain
/// would take the float with it. It is valid while nothing moves the account's stablecoin, so an
/// operation reads it after its last change to the account and before its first debit.
///
/// Every debit it sizes therefore withdraws under `Preservation::Preserve`, and no sizing result
/// carries a `Preservation` of its own.
pub(crate) struct PoolCustody<T: Config> {
	account: T::AccountId,
	preserved: BalanceOf<T>,
}

impl<T: Config> PoolCustody<T> {
	pub(crate) fn read(stable_id: &StableIdOf<T>, account: T::AccountId) -> Self {
		let preserved = T::StableAssets::reducible_balance(
			stable_id.clone(),
			&account,
			Preservation::Preserve,
			Fortitude::Polite,
		);
		Self { account, preserved }
	}

	pub(crate) const fn account(&self) -> &T::AccountId {
		&self.account
	}

	/// The greatest debit at or below `limit`.
	fn debit(&self, limit: BalanceOf<T>) -> BalanceOf<T> {
		limit.min(self.preserved)
	}
}

/// Pool and depositor row of one operation, held in memory until commit.
struct DepositOp<'a, T: Config> {
	collateral_id: &'a CollateralIdOf<T>,
	stable_id: &'a StableIdOf<T>,
	owner: &'a T::AccountId,
	pool: StabilityPoolOf<T>,
	deposit: DepositOf<T>,
}

impl<'a, T: Config> DepositOp<'a, T> {
	/// Loads and settles the stored row of `owner`, which must exist.
	fn load(
		collateral_id: &'a CollateralIdOf<T>,
		stable_id: &'a StableIdOf<T>,
		owner: &'a T::AccountId,
		pool: StabilityPoolOf<T>,
		branch: BranchSnapshot,
	) -> Result<Self, DispatchError> {
		let deposit = Deposits::<T>::get((collateral_id, stable_id, owner))
			.ok_or(Error::<T>::DepositNotFound)?;
		Self { collateral_id, stable_id, owner, pool, deposit }.refresh(branch)
	}

	/// Loads and settles the stored row of `owner`. A depositor without a row starts from an
	/// empty row at the current active coordinates, so it carries no share of earlier gains or
	/// losses.
	fn load_or_fresh(
		collateral_id: &'a CollateralIdOf<T>,
		stable_id: &'a StableIdOf<T>,
		owner: &'a T::AccountId,
		pool: StabilityPoolOf<T>,
		branch: BranchSnapshot,
	) -> Result<Self, DispatchError> {
		let deposit = match Deposits::<T>::get((collateral_id, stable_id, owner)) {
			Some(deposit) => deposit,
			None => {
				let current = Pallet::<T>::sums_at(
					collateral_id,
					stable_id,
					Leg::Active,
					&pool.state.coords,
				)?;
				Deposit::fresh(pool.state.snapshot(Leg::Active, &current))
			},
		};
		Self { collateral_id, stable_id, owner, pool, deposit }.refresh(branch)
	}

	/// Settles the row at `branch.now`. Due cohorts activate first so the settlement uses the
	/// current risk classification.
	fn refresh(mut self, branch: BranchSnapshot) -> Result<Self, DispatchError> {
		Pallet::<T>::advance_cohorts(self.collateral_id, self.stable_id, &mut self.pool, branch)?;
		Pallet::<T>::realize_deposit(
			self.collateral_id,
			self.stable_id,
			&self.pool,
			&mut self.deposit,
		)?;
		Pallet::<T>::settle_pending(
			self.collateral_id,
			self.stable_id,
			self.owner,
			&mut self.pool,
			&mut self.deposit,
		)?;
		Ok(self)
	}

	/// Adds funded stablecoin to pending capital.
	/// A merge restarts the entry delay for all pending capital of the depositor.
	fn queue_pending(&mut self, pending_amount: BalanceOf<T>, now: Millis) -> DispatchResult {
		debug_assert!(!pending_amount.is_zero());
		if self.pool.config.entry_delay == 0 {
			// There is no wait to enforce: the capital is active from the start. The refresh
			// already reset the row snapshot to the live active coordinates.
			self.deposit.active_deposit.ensure_add_assign(pending_amount)?;
			self.pool.state.total_active_deposits.ensure_add_assign(pending_amount)?;
			return Ok(());
		}

		// A merge pulls the old tranche out of its cohort first. Its snapshot stands at the live
		// coordinates, so the revalued aggregate carries exactly `pending.amount` for it.
		let live = self.pool.state.pending_coords;
		let merged = match self.deposit.pending_deposit.take() {
			Some(pending) => {
				let cohort = self.pool.state.revalued_cohort(pending.cohort, &live)?;
				cohort.amount.ensure_sub_assign(pending.amount)?;
				self.pool.state.drop_member(pending.cohort);
				pending.amount.ensure_add(pending_amount)?
			},
			None => pending_amount,
		};

		let deadline = math::cohort_deadline(now, self.pool.config.entry_delay);
		let snapshot = {
			let current =
				Pallet::<T>::sums_at(self.collateral_id, self.stable_id, Leg::Pending, &live)?;
			self.pool.state.snapshot(Leg::Pending, &current)
		};
		let cohort = Pallet::<T>::join_cohort(&mut self.pool.state, deadline, live)?;
		cohort.revalue(&live);
		cohort.members.ensure_add_assign(1)?;
		cohort.amount.ensure_add_assign(merged)?;
		self.deposit.pending_deposit =
			Some(PendingDeposit { amount: merged, cohort: cohort.id, snapshot });
		self.pool.state.total_pending_deposits.ensure_add_assign(pending_amount)?;
		Ok(())
	}

	/// Debits the active stablecoin that `mode` authorizes, up to `amount`, and returns it.
	///
	/// The debit is bounded by the realized active deposit, which rounding keeps at or below the
	/// pool total.
	fn take_withdrawal(
		&mut self,
		mode: UnfrozenMode,
		now: Millis,
		amount: BalanceOf<T>,
	) -> Result<BalanceOf<T>, DispatchError> {
		let take = match mode {
			UnfrozenMode::Normal => amount.min(self.deposit.active_deposit),
			UnfrozenMode::Safety => {
				let request = self
					.deposit
					.withdrawal_request
					.as_mut()
					.ok_or(Error::<T>::WithdrawalRequestMissing)?;
				ensure!(now >= request.executable_at, Error::<T>::SafetyWithdrawalDelayActive);
				ensure!(
					!request.lapsed(now, self.pool.config.safety_withdrawal_delay),
					Error::<T>::WithdrawalRequestExpired
				);
				let take = amount.min(request.amount).min(self.deposit.active_deposit);
				request.amount.saturating_reduce(take);
				if request.amount.is_zero() {
					self.deposit.withdrawal_request = None;
				}
				take
			},
		};
		self.deposit.active_deposit.ensure_sub_assign(take)?;
		self.pool.state.total_active_deposits.ensure_sub_assign(take)?;
		Ok(take)
	}

	/// Removes the claimable balance of `kind` and the same amount from its pool obligation.
	///
	/// An underflow indicates that the user claim exceeds the recorded pool obligation.
	fn take_claim(&mut self, kind: ClaimKind) -> Result<BalanceOf<T>, DispatchError> {
		let (claimable, unclaimed_total, empty_error) = match kind {
			ClaimKind::Collateral => (
				&mut self.deposit.claimable_collateral,
				&mut self.pool.state.total_collateral_gains_unclaimed,
				Error::<T>::NoClaimableCollateral,
			),
			ClaimKind::Yield => (
				&mut self.deposit.claimable_yield,
				&mut self.pool.state.total_yield_unclaimed,
				Error::<T>::NoClaimableYield,
			),
		};
		ensure!(!claimable.is_zero(), empty_error);
		let amount = core::mem::take(claimable);
		unclaimed_total.ensure_sub_assign(amount)?;
		Ok(amount)
	}

	/// Moves claimable yield into the active deposit, up to `amount`, and returns it.
	///
	/// The row snapshot is current, so the moved amount receives no gains from before
	/// compounding.
	fn compound_yield(&mut self, amount: BalanceOf<T>) -> Result<BalanceOf<T>, DispatchError> {
		let take = amount.min(self.deposit.claimable_yield);
		self.deposit.claimable_yield.ensure_sub_assign(take)?;
		self.deposit.active_deposit.ensure_add_assign(take)?;
		self.pool.state.total_active_deposits.ensure_add_assign(take)?;
		// An underflow would mean a row claiming more than the pool tracks.
		self.pool.state.total_yield_unclaimed.ensure_sub_assign(take)?;
		Ok(take)
	}

	/// Writes both drafts. An empty row is removed rather than stored so storage only holds
	/// depositors with value.
	fn commit(self) {
		let key = (self.collateral_id, self.stable_id, self.owner);
		if self.deposit.is_empty() {
			Deposits::<T>::remove(key);
		} else {
			Deposits::<T>::insert(key, self.deposit);
		}
		Pools::<T>::insert(self.collateral_id, self.stable_id, self.pool);
	}
}

/// What settling active pool stablecoin against the `FinalRecovery` queue head did.
///
/// Only [`Self::Applied`] moves value.
pub(crate) enum RecoveryOffsetOutcome {
	/// Head debt was cancelled and its collateral credited to the active depositors.
	Applied,
	/// The pool holds no stablecoin it may burn.
	NoCapacity,
	/// No head is queued, or the queued one offers nothing to offset.
	NoTarget,
	/// Only redemptions permit settlement below par.
	BelowPar,
}

/// Operating mode of a branch that admits value movement.
///
/// [`Pallet::ensure_not_frozen`] returns it, so code past that check matches only the modes it can
/// meet.
#[derive(Clone, Copy)]
pub(crate) enum UnfrozenMode {
	Normal,
	Safety,
}

impl From<UnfrozenMode> for BranchMode {
	fn from(mode: UnfrozenMode) -> Self {
		match mode {
			UnfrozenMode::Normal => Self::Normal,
			UnfrozenMode::Safety => Self::Safety,
		}
	}
}

/// Frozen-branch check of [`Pallet::open_op`].
#[derive(Clone, Copy)]
enum FrozenCheck {
	/// Rejects a frozen branch before the row loads.
	Reject,
	/// Settles the row in any mode.
	Skip,
}

/// Result of resolving one due cohort against the current pending leg.
pub(crate) struct CohortRoll<Balance> {
	id: CohortId,
	deadline: Millis,
	/// Capital moved from the pending total to the active total.
	survived: Balance,
	members: u32,
}

/// Due cohorts of one activation, in deadline order.
pub(crate) type CohortRolls<Balance> = BoundedVec<CohortRoll<Balance>, MaxOpenCohorts>;

impl<T: Config> Pallet<T> {
	/// Returns the registered pool of a branch.
	pub(crate) fn load_pool(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<StabilityPoolOf<T>, DispatchError> {
		Pools::<T>::get(collateral_id, stable_id)
			.ok_or_else(|| Error::<T>::PoolNotRegistered.into())
	}

	/// Applies all cohort deadlines at or before `now` to the supplied pool state.
	///
	/// The function changes no storage. Thus, read-only capacity inspection can use the same result
	/// that [`Pallet::advance_cohorts`] commits in settlement.
	pub(crate) fn roll_due_cohorts(
		pool: &mut StabilityPoolOf<T>,
		now: Millis,
	) -> Result<CohortRolls<BalanceOf<T>>, DispatchError> {
		let state = &mut pool.state;
		let mut rolls = CohortRolls::default();
		// Pending offsets keep every reserve funded, and try-state checks the same property.
		if !state.cohorts_covered(&state.pending_coords, state.total_pending_deposits) {
			defensive!("open cohort reserves exceed the pending total");
			return Err(DispatchError::Corruption);
		}
		// Deadlines are ordered, so the due cohorts are a prefix of the open set.
		while let Some(cohort) = state.open_cohorts.first() {
			if cohort.deadline > now {
				break;
			}
			// Pending offsets keep the sum of these reserves inside the pending total, so
			// moving one cohort leaves the other cohort's claims fully funded.
			let survived = math::compound_ceil::<BalanceOf<T>>(
				cohort.amount,
				&cohort.coords,
				&state.pending_coords,
			);
			let cohort = state.open_cohorts.remove(0);
			state.total_pending_deposits.ensure_sub_assign(survived)?;
			state.total_active_deposits.ensure_add_assign(survived)?;
			rolls
				.try_push(CohortRoll {
					id: cohort.id,
					deadline: cohort.deadline,
					survived,
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
	pub(crate) fn advance_cohorts(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		pool: &mut StabilityPoolOf<T>,
		branch: BranchSnapshot,
	) -> Result<bool, DispatchError> {
		if branch.mode.is_frozen() {
			return Ok(false);
		}
		let rolls = Self::roll_due_cohorts(pool, branch.now)?;
		if rolls.is_empty() {
			return Ok(false);
		}
		// Every cohort due now materializes at this same instant, so one snapshot pair serves
		// them all: the pending leg ends here, and the active leg starts here.
		let state = &pool.state;
		let pending_end = {
			let sums =
				Self::sums_at(collateral_id, stable_id, Leg::Pending, &state.pending_coords)?;
			state.snapshot(Leg::Pending, &sums)
		};
		let active_start = {
			let sums = Self::sums_at(collateral_id, stable_id, Leg::Active, &state.coords)?;
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
						activated: roll.survived,
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

	/// Supplies stablecoin to the stability pool of a branch.
	pub(crate) fn do_deposit(
		who: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		let minimum_deposit = Self::load_pool(&collateral_id, &stable_id)?.config.minimum_deposit;
		ensure!(amount >= minimum_deposit, Error::<T>::DepositTooSmall);
		T::BranchInterface::accrue_interest(&collateral_id, &stable_id)?;

		let mode = Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let now = T::TimeProvider::now();
		let pool = Self::load_pool(&collateral_id, &stable_id)?;
		let branch = BranchSnapshot { mode: mode.into(), now };
		let mut op = DepositOp::<T>::load_or_fresh(&collateral_id, &stable_id, &who, pool, branch)?;

		// `Expendable` only on a full drain, so the withdrawal itself rejects an amount that would
		// leave the depositor below the minimum balance.
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
		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		Self::resolve_into_pool(&stable_id, &pool_account, payment)?;
		op.queue_pending(amount, now)?;

		op.commit();
		Self::deposit_event(Event::DepositReceived {
			collateral_id,
			stable_id,
			depositor: who,
			amount,
		});
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
			let cohort =
				state.open_cohorts.last_mut().defensive_ok_or(DispatchError::Corruption)?;
			cohort.deadline = required;
			return Ok(cohort);
		}

		let id = state.next_cohort_id;
		state.next_cohort_id = CohortId(id.0.ensure_add(1)?);
		state
			.open_cohorts
			.try_push(OpenCohort::fresh(id, required, coords))
			.map_err(|_| DispatchError::Corruption)
			.defensive_proof("capacity was checked above")?;
		state.open_cohorts.last_mut().defensive_ok_or(DispatchError::Corruption)
	}

	pub(crate) fn do_offset_recovery(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		max_stable_in: BalanceOf<T>,
	) -> DispatchResult {
		let pool = Self::load_pool(&collateral_id, &stable_id)?;
		// Settling recovery debt reduces risk, so Safety Mode allows it. Only a freeze stops it.
		let mode = Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		match Self::try_offset_recovery(collateral_id, stable_id, pool, mode.into(), max_stable_in)?
		{
			RecoveryOffsetOutcome::Applied => Ok(()),
			RecoveryOffsetOutcome::NoCapacity => Err(Error::<T>::NoRecoveryOffsetPerformed.into()),
			RecoveryOffsetOutcome::NoTarget => Err(Error::<T>::RecoveryVaultNotFound.into()),
			RecoveryOffsetOutcome::BelowPar => Err(Error::<T>::RecoveryOffsetBelowPar.into()),
		}
	}

	/// Shared offset rules give depositors consistent accounting for liquidations and recovery.
	///
	/// Errors can occur after settlement, so the caller must roll back the transaction on error.
	pub(crate) fn try_offset_recovery(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		mut pool: StabilityPoolOf<T>,
		mode: BranchMode,
		max_stable_in: BalanceOf<T>,
	) -> Result<RecoveryOffsetOutcome, DispatchError> {
		// Matured capital must stand in the active total before the offset is sized against it.
		// The activation is stored as a transition of its own, because the settlement below
		// re-enters this pallet through the vault's yield hook and reads the stored row.
		let branch = BranchSnapshot { mode, now: T::TimeProvider::now() };
		if Self::advance_cohorts(&collateral_id, &stable_id, &mut pool, branch)? {
			Pools::<T>::insert(&collateral_id, &stable_id, &pool);
		}

		// Size the burn before touching anything.
		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		let custody = PoolCustody::<T>::read(&stable_id, pool_account.clone());
		let Some(funded) =
			Self::fund_offset(&pool, &custody, Leg::Active, max_stable_in, BalanceOf::<T>::zero())
		else {
			return Ok(RecoveryOffsetOutcome::NoCapacity);
		};

		let payment = T::StableAssets::withdraw(
			stable_id.clone(),
			&pool_account,
			funded,
			Precision::Exact,
			Preservation::Preserve,
			Fortitude::Polite,
		)?;
		let max_partial_debt = pool
			.state
			.total_active_deposits
			.saturating_sub(pool.config.minimum_active_pool_balance);
		let (result, change) = T::RecoveryOffsets::execute_recovery_offset(
			&collateral_id,
			payment,
			max_partial_debt,
			&pool_account,
		)?;
		let collateral_out = match result {
			// Unused credit must return to the pool to prevent a loss of deposits.
			RecoveryOffsetResult::NoTarget | RecoveryOffsetResult::NotOffsettable => {
				Self::resolve_into_pool(&stable_id, &pool_account, change)?;
				return Ok(RecoveryOffsetOutcome::NoTarget);
			},
			RecoveryOffsetResult::BelowPar => {
				Self::resolve_into_pool(&stable_id, &pool_account, change)?;
				return Ok(RecoveryOffsetOutcome::BelowPar);
			},
			RecoveryOffsetResult::Applied { collateral_out } => collateral_out,
		};
		// The settlement can only spend value the credit carried, so the difference is what it
		// spent.
		let debt_cancelled = funded.defensive_saturating_sub(change.peek());
		ensure!(!debt_cancelled.is_zero(), Error::<T>::NoRecoveryOffsetPerformed);
		// Return unspent credit above the pool's custody float.
		Self::resolve_into_pool(&stable_id, &pool_account, change)?;

		// The recovery interface already moved the value, so only the accounting remains. It
		// applies to the stored row, which now carries any yield the settlement routed here.
		let mut pool = Self::load_pool(&collateral_id, &stable_id)?;
		// Funding the whole pool does not imply depletion: the head can return change. Apply
		// the floor to the actual burn, rolling back the settlement if it would leave dust.
		ensure!(
			math::clamp_offset_debt(
				debt_cancelled,
				pool.state.total_active_deposits,
				pool.config.minimum_active_pool_balance,
			) == debt_cancelled,
			Error::<T>::NoRecoveryOffsetPerformed
		);
		Self::apply_offset(
			&collateral_id,
			&stable_id,
			Leg::Active,
			&mut pool,
			debt_cancelled,
			collateral_out,
		)?;
		Pools::<T>::insert(&collateral_id, &stable_id, pool);
		Self::deposit_event(Event::RecoveryOffsetApplied { collateral_id, stable_id });
		Ok(RecoveryOffsetOutcome::Applied)
	}

	/// Records a Safety-mode withdrawal request.
	///
	/// A new request replaces the current request. Normal mode requires no request.
	pub(crate) fn do_request_withdraw(
		who: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		ensure!(!amount.is_zero(), Error::<T>::ZeroAmount);
		let pool = Self::load_pool(&collateral_id, &stable_id)?;
		let mode = Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		match mode {
			UnfrozenMode::Normal => return Err(Error::<T>::NotInSafetyMode.into()),
			UnfrozenMode::Safety => {},
		}

		let now = T::TimeProvider::now();
		let branch = BranchSnapshot { mode: mode.into(), now };
		let mut op = DepositOp::<T>::load(&collateral_id, &stable_id, &who, pool, branch)?;

		let executable_at = now.saturating_add(op.pool.config.safety_withdrawal_delay);
		op.deposit.withdrawal_request = Some(WithdrawalRequest { amount, executable_at });

		op.commit();
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
	) -> DispatchResultWithPostInfo {
		ensure!(!amount.is_zero(), Error::<T>::ZeroAmount);
		ensure!(
			Pools::<T>::contains_key(&collateral_id, &stable_id),
			Error::<T>::PoolNotRegistered
		);
		ensure!(
			Deposits::<T>::contains_key((&collateral_id, &stable_id, &who)),
			Error::<T>::DepositNotFound
		);
		T::BranchInterface::accrue_interest(&collateral_id, &stable_id)?;

		let pool = Self::load_pool(&collateral_id, &stable_id)?;
		let mode = Self::ensure_not_frozen(&collateral_id, &stable_id)?;
		let now = T::TimeProvider::now();
		let branch = BranchSnapshot { mode: mode.into(), now };
		let mut op = DepositOp::<T>::load(&collateral_id, &stable_id, &who, pool, branch)?;

		let take = op.take_withdrawal(mode, now, amount)?;
		ensure!(
			!take.is_zero(),
			Error::<T>::NoActiveDeposit.with_weight(T::WeightInfo::withdraw_exhausted())
		);

		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		Self::pay_claim::<T::StableAssets>(stable_id.clone(), &pool_account, &recipient, take)?;

		op.commit();
		Self::deposit_event(Event::WithdrawalExecuted {
			collateral_id,
			stable_id,
			depositor: who,
			recipient,
			amount: take,
		});
		Ok(Some(T::WeightInfo::withdraw()).into())
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
		ensure!(
			Pools::<T>::contains_key(&collateral_id, &stable_id),
			Error::<T>::PoolNotRegistered
		);
		ensure!(
			Deposits::<T>::contains_key((&collateral_id, &stable_id, &who)),
			Error::<T>::DepositNotFound
		);
		if matches!(kind, ClaimKind::Yield) {
			T::BranchInterface::accrue_interest(&collateral_id, &stable_id)?;
		}
		let mut op = Self::open_op(&who, &collateral_id, &stable_id, FrozenCheck::Reject)?;

		let amount = op.take_claim(kind)?;
		let pool_account = Self::pool_account(&collateral_id, &stable_id);
		match kind {
			ClaimKind::Collateral => Self::pay_claim::<T::CollateralAssets>(
				collateral_id.clone(),
				&pool_account,
				&recipient,
				amount,
			)?,
			ClaimKind::Yield => Self::pay_claim::<T::StableAssets>(
				stable_id.clone(),
				&pool_account,
				&recipient,
				amount,
			)?,
		}

		op.commit();
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

	/// Returns the debt that `leg` can cancel.
	///
	/// The limits of [`Pallet::fund_offset`] apply, and the supported precision range of `P` also
	/// applies. Capacity includes the precision limit so a caller does not allocate collateral to
	/// an invalid burn. The active leg must settle before a reserved pending leg.
	///
	/// `minimum_active_pool_balance` also protects pending `P` because both legs use the same
	/// precision parameters.
	pub(crate) fn size_offset(
		pool: &StabilityPoolOf<T>,
		custody: &PoolCustody<T>,
		leg: Leg,
		max_debt: BalanceOf<T>,
		reserved: BalanceOf<T>,
	) -> Option<BalanceOf<T>> {
		let debt = Self::fund_offset(pool, custody, leg, max_debt, reserved)?;
		Self::offset_p_update(pool, leg, debt)?;
		Some(debt)
	}

	/// Reserves enough pending capital for each cohort's upward-rounded aggregate.
	///
	/// With two cohorts, their ceilings can exceed the surviving total by one unit. Only in
	/// that case, `P` uses a survivor numerator one unit smaller; the actual burn stays unchanged.
	/// If the old reserves sum to at most the leg total, their new ceilings sum to at most the
	/// surviving total (leg total minus `debt`) when `P` scales by
	/// `(surviving total - 1) / leg total`. Quoting and execution use this same precision check.
	fn offset_p_update(pool: &StabilityPoolOf<T>, leg: Leg, debt: BalanceOf<T>) -> Option<PUpdate> {
		let state = &pool.state;
		let coords = *state.coords(leg);
		let total = state.total(leg);
		let update = math::update_p_after_offset(coords.p, total, debt)?;
		if !leg.holds_cohorts() {
			return Some(update);
		}
		match update {
			PUpdate::Depleted => return Some(update),
			PUpdate::Updated { .. } => {},
		}
		if !state.cohorts_covered(&coords, total) {
			return None;
		}
		let covered = |update: &PUpdate| match update {
			PUpdate::Depleted => false,
			PUpdate::Updated { new_p, scales_crossed } => {
				let scale = match coords.scale.checked_add(*scales_crossed) {
					Some(scale) => scale,
					None => return false,
				};
				let live = Accumulators { p: *new_p, epoch: coords.epoch, scale };
				state.cohorts_covered(&live, total.saturating_sub(debt))
			},
		};
		if covered(&update) {
			return Some(update);
		}
		let conservative_debt = debt.checked_add(&One::one())?;
		let update = math::update_p_after_offset(coords.p, total, conservative_debt)?;
		covered(&update).then_some(update)
	}

	/// Returns the nonzero debt that `leg` can fund from the pool's `custody`.
	///
	/// Three limits apply:
	///
	/// - The capital available in the leg.
	/// - The required balance after a partial offset.
	/// - The reducible pool-account balance. Donations can add surplus, but the leg's tracked
	///   capital still caps the debit. A shortfall rounds it down to what custody can pay.
	///
	/// `reserved` is debt that another leg will burn from the same account first. This input keeps
	/// both reservations valid against one custody balance.
	fn fund_offset(
		pool: &StabilityPoolOf<T>,
		custody: &PoolCustody<T>,
		leg: Leg,
		max_debt: BalanceOf<T>,
		reserved: BalanceOf<T>,
	) -> Option<BalanceOf<T>> {
		let minimum = {
			let base = pool.config.minimum_active_pool_balance;
			if leg.holds_cohorts() {
				// Every nonzero cohort needs at least one unit for its rounded-up reserve.
				let cohorts: BalanceOf<T> = pool
					.state
					.open_cohorts
					.iter()
					.filter(|cohort| !cohort.amount.is_zero())
					.count()
					.saturated_into();
				base.max(cohorts)
			} else {
				base
			}
		};
		let accounting_cap = math::clamp_offset_debt(max_debt, pool.state.total(leg), minimum);
		if accounting_cap.is_zero() {
			return None;
		}
		let headroom = custody.debit(accounting_cap.saturating_add(reserved));
		let debt = headroom.saturating_sub(reserved).min(accounting_cap);
		if debt.is_zero() {
			return None;
		}
		Some(debt)
	}

	/// Pays an exact claim from the pool account.
	///
	/// The claim is tracked value, and the pool account holds its custody float on top of the
	/// tracked totals, so `Preserve` always has the minimum balance to leave behind. A full drain
	/// under `Expendable` would fold any remainder into the payout, and the remainder is not the
	/// claimant's.
	fn pay_claim<Assets>(
		asset_id: Assets::AssetId,
		pool_account: &T::AccountId,
		recipient: &T::AccountId,
		amount: BalanceOf<T>,
	) -> DispatchResult
	where
		Assets: frame::traits::fungibles::Mutate<T::AccountId, Balance = BalanceOf<T>>,
	{
		Assets::transfer(asset_id, pool_account, recipient, amount, Preservation::Preserve)?;
		Ok(())
	}

	/// Resolves a nonzero stablecoin credit into pool custody. A zero credit is dropped.
	///
	/// The deposit check runs first, so a refusal fails the call instead of losing the credit.
	fn resolve_into_pool(
		stable_id: &StableIdOf<T>,
		pool_account: &T::AccountId,
		credit: StableCreditOf<T>,
	) -> DispatchResult {
		if let Err(credit) = credit.drop_zero() {
			T::StableAssets::can_deposit(
				stable_id.clone(),
				pool_account,
				credit.peek(),
				Provenance::Extant,
			)
			.into_result()?;
			T::StableAssets::resolve(pool_account, credit)
				.map_err(|_| DispatchError::Corruption)
				.defensive_proof("`can_deposit` just passed; qed")?;
		}
		Ok(())
	}

	/// Records one exact debt and collateral reservation on `leg` without moving value.
	///
	/// A leg with no reservation carries no collateral, which [`Self::offset`] already checked, so
	/// its credit is dropped. Otherwise the reservation and its credit come back for the caller to
	/// settle, since both legs move as one. The caller reports both legs in one event.
	pub(crate) fn record_offset(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		pool: &mut StabilityPoolOf<T>,
		reservation: Option<OffsetReservation<BalanceOf<T>>>,
		collateral: CollateralCreditOf<T>,
	) -> Result<Option<(OffsetReservation<BalanceOf<T>>, CollateralCreditOf<T>)>, DispatchError>
	{
		let Some(reservation) = reservation else {
			debug_assert!(collateral.peek().is_zero());
			drop(collateral);
			return Ok(None);
		};
		ensure!(collateral.asset() == *collateral_id, Error::<T>::OffsetSettlementFailed);
		// The accounting reads no custody balance, so it can precede the value movement. The
		// caller's transaction rolls it back when the movement fails.
		Self::apply_offset(
			collateral_id,
			stable_id,
			leg,
			pool,
			reservation.debt,
			collateral.peek(),
		)?;
		Ok(Some((reservation, collateral)))
	}

	/// Folds the recorded legs into the one movement that settles both.
	///
	/// The pending leg was sized with the active debt reserved ahead of it, so the summed
	/// withdrawal fits the custody both legs read. An offset with no leg never reaches here: the
	/// caller returns before reserving anything.
	pub(crate) fn merge_offset_movement(
		active: Option<(OffsetReservation<BalanceOf<T>>, CollateralCreditOf<T>)>,
		pending: Option<(OffsetReservation<BalanceOf<T>>, CollateralCreditOf<T>)>,
	) -> Result<(OffsetReservation<BalanceOf<T>>, CollateralCreditOf<T>), DispatchError> {
		match (active, pending) {
			(Some(active), None) => Ok(active),
			(None, Some(pending)) => Ok(pending),
			(Some((active, mut active_collateral)), Some((pending, pending_collateral))) => {
				let debt = active.debt.ensure_add(pending.debt)?;
				// A refusal still aborts rather than losing a credit.
				active_collateral
					.subsume(pending_collateral)
					.map_err(|pending_collateral| {
						drop(pending_collateral);
						DispatchError::Corruption
					})
					.defensive_proof("both credits passed the asset check in `record_offset`")?;
				Ok((OffsetReservation { debt }, active_collateral))
			},
			(None, None) => {
				defensive!("an offset with no leg reached settlement");
				Err(DispatchError::Corruption)
			},
		}
	}

	/// Withdraws the reserved stablecoin and resolves all collateral into pool custody.
	///
	/// Exact movement keeps debt cancellation equal to the quoted amount. The withdrawn credit
	/// comes back for the caller to burn.
	pub(crate) fn settle_reservation_exact(
		stable_id: &StableIdOf<T>,
		pool_account: &T::AccountId,
		reservation: OffsetReservation<BalanceOf<T>>,
		collateral: CollateralCreditOf<T>,
	) -> Result<StableCreditOf<T>, DispatchError> {
		let stable_credit = T::StableAssets::withdraw(
			stable_id.clone(),
			pool_account,
			reservation.debt,
			Precision::Exact,
			Preservation::Preserve,
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
		Ok(stable_credit)
	}

	/// Records an offset of `debt` on `leg` that pays `collateral` to its depositors, and creates
	/// the accumulator rows it requires.
	///
	/// `S` uses the deposit total before the offset. `P` then records the principal loss. This
	/// order gives collateral to the capital that paid the debt.
	///
	/// All fallible arithmetic completes before the first write. The caller must run inside a
	/// storage transaction and persist `pool`.
	fn apply_offset(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		pool: &mut StabilityPoolOf<T>,
		debt: BalanceOf<T>,
		collateral: BalanceOf<T>,
	) -> DispatchResult {
		let update =
			Self::offset_p_update(pool, leg, debt).ok_or(Error::<T>::UnsupportedOffsetPrecision)?;
		let state = &mut pool.state;
		let coords = *state.coords(leg);
		let total = state.total(leg);
		debug_assert!(!debt.is_zero());
		debug_assert!(debt <= total);

		let mut sums = Self::sums_at(collateral_id, stable_id, leg, &coords)?;
		let delta_s =
			math::delta_sum(collateral, coords.p, total).ok_or(ArithmeticError::Overflow)?;
		sums.s_collateral.ensure_add_assign(delta_s)?;
		let new_unclaimed = state.total_collateral_gains_unclaimed.ensure_add(collateral)?;

		let (new_coords, new_total) = match update {
			PUpdate::Depleted => (
				Accumulators { p: FixedU128::one(), epoch: coords.epoch.ensure_add(1)?, scale: 0 },
				BalanceOf::<T>::zero(),
			),
			PUpdate::Updated { new_p, scales_crossed } => (
				Accumulators {
					p: new_p,
					epoch: coords.epoch,
					scale: coords.scale.ensure_add(scales_crossed)?,
				},
				total.ensure_sub(debt)?,
			),
		};

		PoolSumsStore::<T>::insert(
			(collateral_id, stable_id, leg, coords.epoch, coords.scale),
			sums,
		);
		// A new epoch starts with its scale-zero row. Bounded by `math::MAX_SCALE_CROSSINGS`.
		let first_new_scale =
			if new_coords.epoch == coords.epoch { coords.scale.saturating_add(1) } else { 0 };
		for scale in first_new_scale..=new_coords.scale {
			PoolSumsStore::<T>::insert(
				(collateral_id, stable_id, leg, new_coords.epoch, scale),
				PoolSums::default(),
			);
		}
		*state.coords_mut(leg) = new_coords;
		*state.total_mut(leg) = new_total;
		state.total_collateral_gains_unclaimed = new_unclaimed;
		if leg.holds_cohorts() {
			// All cohort reserves share the live coordinates. Row touches and later joins
			// therefore cannot accumulate additional ceiling rounding between offsets.
			for cohort in &mut state.open_cohorts {
				cohort.revalue(&new_coords);
			}
		}
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
		branch: BranchSnapshot,
		mut pool: StabilityPoolOf<T>,
		credit: StableCreditOf<T>,
	) -> StableCreditOf<T> {
		if credit.peek().is_zero() {
			return credit;
		}
		// A frozen branch takes no yield.
		if branch.mode.is_frozen() {
			return credit;
		}

		// Matured capital enters the denominator before the new yield is shared out, so a
		// deposit earns everything distributed after its deadline and nothing before it.
		// Advancement is a complete transition of its own: it persists even when the
		// distribution behind it bails out.
		let Ok(advanced) = Self::advance_cohorts(collateral_id, stable_id, &mut pool, branch)
		else {
			return credit;
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
		let Ok(mut sums) = Self::sums_at(collateral_id, stable_id, Leg::Active, &pool.state.coords)
		else {
			return Err(credit);
		};
		let Some(new_g) = sums.g_yield.checked_add(&delta_g) else {
			return Err(credit);
		};
		let Some(new_total_yield) = pool.state.total_yield_unclaimed.checked_add(&amount) else {
			return Err(credit);
		};

		let pool_account = Self::pool_account(collateral_id, stable_id);
		T::StableAssets::resolve(&pool_account, credit)?;

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
		Ok(StableCreditOf::<T>::zero(stable_id.clone()))
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
		ensure!(
			Pools::<T>::contains_key(&collateral_id, &stable_id),
			Error::<T>::PoolNotRegistered
		);
		ensure!(
			Deposits::<T>::contains_key((&collateral_id, &stable_id, &who)),
			Error::<T>::DepositNotFound
		);
		ensure!(!amount.is_zero(), Error::<T>::NoYieldToCompound);
		T::BranchInterface::accrue_interest(&collateral_id, &stable_id)?;
		let mut op = Self::open_op(&who, &collateral_id, &stable_id, FrozenCheck::Reject)?;

		let take = op.compound_yield(amount)?;
		ensure!(!take.is_zero(), Error::<T>::NoYieldToCompound);

		op.commit();
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
	/// The function moves no value. A frozen branch prevents new activation because activation
	/// changes risk. Settlement of prior losses and gains remains available.
	pub(crate) fn do_settle_deposit(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> DispatchResult {
		Self::open_op(&owner, &collateral_id, &stable_id, FrozenCheck::Skip)?.commit();
		Ok(())
	}

	/// Loads the pool and the stored row of `owner`, settles the row at the current time, and
	/// leaves the commit to the caller.
	///
	/// Cohort activation and checkpoint release write storage before the commit. A caller that
	/// must change nothing has to roll those writes back.
	fn open_op<'a>(
		owner: &'a T::AccountId,
		collateral_id: &'a CollateralIdOf<T>,
		stable_id: &'a StableIdOf<T>,
		frozen: FrozenCheck,
	) -> Result<DepositOp<'a, T>, DispatchError> {
		let pool = Self::load_pool(collateral_id, stable_id)?;
		let mode = match frozen {
			FrozenCheck::Reject => Self::ensure_not_frozen(collateral_id, stable_id)?.into(),
			FrozenCheck::Skip => Self::branch_mode_or_frozen(collateral_id, stable_id),
		};
		let branch = BranchSnapshot { mode, now: T::TimeProvider::now() };
		DepositOp::<T>::load(collateral_id, stable_id, owner, pool, branch)
	}

	/// Returns the row that [`Pallet::do_settle_deposit`] would store, without storing it.
	///
	/// The real settlement runs in a transaction that always rolls back, so the result cannot
	/// differ from execution. The rollback also discards the deposited events.
	pub(crate) fn settled_deposit(
		owner: &T::AccountId,
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<DepositOf<T>, DispatchError> {
		with_transaction(|| {
			TransactionOutcome::Rollback(
				Self::open_op(owner, collateral_id, stable_id, FrozenCheck::Skip)
					.map(|op| op.deposit),
			)
		})
	}

	/// Returns the pool state with each due cohort activated, without storing it.
	///
	/// A frozen or unavailable mode prevents activation, as in [`Pallet::advance_cohorts`].
	pub(crate) fn activated_pool_state(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<PoolStateOf<T>, DispatchError> {
		let mut pool = Self::load_pool(collateral_id, stable_id)?;
		if Self::ensure_not_frozen(collateral_id, stable_id).is_ok() {
			Self::roll_due_cohorts(&mut pool, T::TimeProvider::now())?;
		}
		Ok(pool.state)
	}

	/// Returns the current mode, counting a branch whose mode cannot be read as frozen.
	///
	/// Paths that must not fail on an unavailable mode use this: unknown risk stops activation and
	/// offsets, as a freeze does.
	pub(crate) fn branch_mode_or_frozen(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> BranchMode {
		T::BranchInterface::branch_mode(collateral_id, stable_id).unwrap_or(BranchMode::Frozen)
	}

	/// Returns the current mode or rejects a frozen branch.
	///
	/// An unavailable mode also causes rejection. This behavior prevents value movement without
	/// current market-risk information.
	pub(crate) fn ensure_not_frozen(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<UnfrozenMode, DispatchError> {
		match T::BranchInterface::branch_mode(collateral_id, stable_id)? {
			BranchMode::Normal => Ok(UnfrozenMode::Normal),
			BranchMode::Safety => Ok(UnfrozenMode::Safety),
			BranchMode::Frozen => Err(Error::<T>::BranchFrozen.into()),
		}
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
		)?;
		deposit.active_deposit = realized.remaining_principal;
		deposit.claimable_collateral.ensure_add_assign(realized.collateral_gain)?;
		deposit.claimable_yield.ensure_add_assign(realized.yield_gain)?;
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
		let phase_one = Self::realize_between(
			collateral_id,
			stable_id,
			Leg::Pending,
			pending.amount,
			&pending.snapshot,
			&checkpoint.pending_end,
		)?;
		debug_assert!(phase_one.yield_gain.is_zero());
		debug_assert!(phase_one.remaining_principal <= checkpoint.activated);

		let (phase_two, live) = Self::realize_leg(
			collateral_id,
			stable_id,
			Leg::Active,
			pool,
			phase_one.remaining_principal,
			&checkpoint.active_start,
		)?;
		debug_assert!(deposit.snapshot.coords.p == live.coords.p);
		deposit.active_deposit.ensure_add_assign(phase_two.remaining_principal)?;
		let gains = phase_one.collateral_gain.ensure_add(phase_two.collateral_gain)?;
		deposit.claimable_collateral.ensure_add_assign(gains)?;
		deposit.claimable_yield.ensure_add_assign(phase_two.yield_gain)?;

		Self::release_checkpoint(collateral_id, stable_id, pending.cohort, checkpoint);
		Self::deposit_event(Event::PendingDepositActivated {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			depositor: who.clone(),
			amount: phase_one.remaining_principal,
		});
		Ok(())
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
		pending: PendingDeposit<BalanceOf<T>>,
	) -> Result<Option<PendingDeposit<BalanceOf<T>>>, DispatchError> {
		let live = pool.state.pending_coords;
		let joined = pending.snapshot.coords;
		if joined == live {
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
		)?;
		debug_assert!(realized.yield_gain.is_zero());
		deposit.claimable_collateral.ensure_add_assign(realized.collateral_gain)?;

		pool.state.revalued_cohort(pending.cohort, &live)?;
		if realized.remaining_principal.is_zero() {
			pool.state.drop_member(pending.cohort);
			return Ok(None);
		}
		Ok(Some(PendingDeposit {
			amount: realized.remaining_principal,
			cohort: pending.cohort,
			snapshot,
		}))
	}

	/// Removes one member reference and deletes an unused checkpoint.
	fn release_checkpoint(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		id: CohortId,
		mut checkpoint: CohortCheckpointOf<T>,
	) {
		// A checkpoint is removed with its last member, so a stored one counts at least one.
		checkpoint.members.defensive_saturating_dec();
		let remaining = (checkpoint.members > 0).then_some(checkpoint);
		CohortCheckpoints::<T>::set((collateral_id, stable_id, id), remaining);
	}

	/// Settles `amount` on one leg and returns its current principal, gains, and snapshot.
	pub(crate) fn realize_leg(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		pool: &StabilityPoolOf<T>,
		amount: BalanceOf<T>,
		snapshot: &DepositSnapshot,
	) -> Result<(Realized<BalanceOf<T>>, DepositSnapshot), DispatchError> {
		let live = {
			let sums = Self::sums_at(collateral_id, stable_id, leg, pool.state.coords(leg))?;
			pool.state.snapshot(leg, &sums)
		};
		let realized =
			Self::realize_between(collateral_id, stable_id, leg, amount, snapshot, &live)?;
		Ok((realized, live))
	}

	/// Settles `amount` on one leg from `snapshot` to the later `end` snapshot of the same leg.
	pub(crate) fn realize_between(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		amount: BalanceOf<T>,
		snapshot: &DepositSnapshot,
		end: &DepositSnapshot,
	) -> Result<Realized<BalanceOf<T>>, DispatchError> {
		let window = Self::window_until(collateral_id, stable_id, leg, snapshot, end)?;
		let realized = math::realize(amount, snapshot, &end.coords, &window);
		debug_assert!(realized.remaining_principal <= amount);
		Ok(realized)
	}

	/// Returns the sums window that settles `snapshot` up to the `end` snapshot.
	///
	/// The `end` snapshot provides the endpoint sums, because the leg can continue after `end`.
	/// Stored rows provide the sums before the endpoint. Rows after the endpoint belong to later
	/// capital and contribute zero.
	///
	/// An `end` in a later epoch means the leg was depleted after `snapshot`. Every row of the
	/// snapshot epoch froze at that depletion, so plain storage reads cover the window.
	fn window_until(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		snapshot: &DepositSnapshot,
		end: &DepositSnapshot,
	) -> Result<SumsWindow, DispatchError> {
		let epoch = snapshot.coords.epoch;
		if epoch != end.coords.epoch {
			return Self::sums_window(collateral_id, stable_id, leg, snapshot);
		}
		let row = |scale: u32| {
			if scale == end.coords.scale {
				Ok(end.sums)
			} else {
				Self::sums_row(collateral_id, stable_id, leg, epoch, scale)
			}
		};
		let snap = row(snapshot.coords.scale)?;
		let mut ahead = [PoolSums::default(); math::SCALE_SPAN as usize];
		let mut scale = snapshot.coords.scale;
		for slot in &mut ahead {
			scale = scale.saturating_add(1);
			if scale > end.coords.scale {
				break;
			}
			*slot = row(scale)?;
		}
		Ok(SumsWindow { snap, ahead })
	}

	/// Validates and stores replacement pool parameters.
	pub(crate) fn do_set_stability_pool_config(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		config: StabilityPoolConfigOf<T>,
	) -> DispatchResult {
		let old = Pools::<T>::try_mutate(
			&collateral_id,
			&stable_id,
			|maybe_pool| -> Result<_, Error<T>> {
				let pool = maybe_pool.as_mut().ok_or(Error::<T>::PoolNotRegistered)?;
				ensure!(config.is_valid(), Error::<T>::InvalidStabilityPoolConfig);
				let old = pool.config.clone();
				pool.config = config.clone();
				Ok(old)
			},
		)?;
		Self::deposit_event(Event::StabilityPoolConfigUpdated {
			collateral_id,
			stable_id,
			old,
			new: config,
		});
		Ok(())
	}

	/// Removes up to `limit` (capped at [`MAX_PRUNE_SUMS`]) historical `PoolSumsStore` rows of an
	/// empty pool, leaving both live `(epoch, scale)` rows intact.
	pub(crate) fn do_prune_pool_sums(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		limit: u32,
	) -> DispatchResult {
		let limit = limit.min(MAX_PRUNE_SUMS);
		let pool = Self::load_pool(&collateral_id, &stable_id)?;
		ensure!(
			!Deposits::<T>::contains_prefix((collateral_id.clone(), stable_id.clone())),
			Error::<T>::PoolNotEmpty
		);
		if limit == 0 {
			return Ok(());
		}

		let take = usize::try_from(limit).unwrap_or(usize::MAX);
		let active_live = pool.state.coords.position();
		let pending_live = pool.state.pending_coords.position();
		// At most two keys match the live positions, so the scan visits at most `limit + 2` keys.
		let to_remove: alloc::vec::Vec<_> =
			PoolSumsStore::<T>::iter_key_prefix((&collateral_id, &stable_id))
				.filter(|(leg, epoch, scale)| {
					let pos = LegCoords { epoch: *epoch, scale: *scale };
					match leg {
						Leg::Active => pos != active_live,
						Leg::Pending => pos != pending_live,
					}
				})
				.take(take)
				.collect();
		for (leg, epoch, scale) in to_remove {
			PoolSumsStore::<T>::remove((&collateral_id, &stable_id, leg, epoch, scale));
		}
		Ok(())
	}

	/// Returns the sums row of `leg` at `coords`.
	///
	/// Every live or snapshot coordinate has a row: offsets create rows as they advance and no
	/// path removes one. An absent row is corruption, and the read fails rather than substitute
	/// zero. Only the look-ahead scans in the window readers may hit absent rows.
	pub(crate) fn sums_at(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		coords: &Accumulators,
	) -> Result<PoolSums, DispatchError> {
		Self::sums_row(collateral_id, stable_id, leg, coords.epoch, coords.scale)
	}

	/// Returns a sums row by raw `(epoch, scale)` coordinates. See [`Pallet::sums_at`].
	fn sums_row(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		epoch: u32,
		scale: u32,
	) -> Result<PoolSums, DispatchError> {
		PoolSumsStore::<T>::get((collateral_id, stable_id, leg, epoch, scale))
			.defensive_ok_or(Error::<T>::AccumulatorRowMissing.into())
	}

	/// Returns the bounded sums window required to settle a snapshot.
	///
	/// Rows have no gaps within an epoch. Therefore, the first absent row ends the window.
	fn sums_window(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		snapshot: &DepositSnapshot,
	) -> Result<SumsWindow, DispatchError> {
		let snap = Self::sums_at(collateral_id, stable_id, leg, &snapshot.coords)?;
		let mut ahead = [PoolSums::default(); math::SCALE_SPAN as usize];
		let mut scale = snapshot.coords.scale;
		for slot in &mut ahead {
			scale = scale.saturating_add(1);
			let Some(sums) = PoolSumsStore::<T>::get((
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
		Ok(SumsWindow { snap, ahead })
	}
}
