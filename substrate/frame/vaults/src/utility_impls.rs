//! Internal (non-dispatchable) `Pallet` helpers: storage accessors, safety
//! gates, interest/fee accounting, mode rules, and the `on_idle` refresh walk.

use crate::{
	context::BranchOp,
	math,
	pallet::{
		BalanceOf, BranchIdleCursor, BranchOf, Branches, CollateralIdOf, CollateralRisks, Config,
		Error, IdleCursor, Millis, Pallet, StableIdOf, Vaults,
	},
	recovery,
	types::{
		AdminLevel, BranchConfig, BranchMode, BranchState, CollateralRisk, Vault, VaultDebt,
		VaultListId, VaultStatus,
	},
	weights::WeightInfo,
};
use frame::{
	deps::frame_support::{storage::with_storage_layer, weights::WeightMeter},
	prelude::*,
	traits::{
		fungibles::Balanced as FungiblesBalanced, Defensive, DefensiveOption, OriginTrait, Time,
	},
};
use pallet_linked_list::{ListError, SortedListInterface};
use pusd_primitives::{OnBranchYield, ProvidePrice};

/// The two numbers a branch TCR depends on. [`Pallet::compute_tcr`] derives
/// them from live state; the operation context captures them once at load as
/// the structurally immutable "pre" side of its TCR gate.
pub(crate) struct TcrInputs<Balance> {
	pub collateral: Balance,
	pub debt: Balance,
}

/// Deltas the next vault touch would apply.
pub(crate) struct PendingTouch<Balance> {
	/// Capped redistributed principal moved into `vault.debt.principal`
	/// (and out of `state.debt.pending_redistribution_principal`).
	pub principal: Balance,
	/// Redistributed collateral released to the owner's hold.
	pub collateral: Balance,
	/// Stored-principal pending interest plus redistribution interest, both
	/// folded into `vault.debt.interest`.
	pub interest: Balance,
}

impl<Balance: Ord + Saturating + Copy> PendingTouch<Balance> {
	/// Entire debt the vault would have after a touch.
	pub fn total_debt(&self, debt: &VaultDebt<Balance>) -> Balance {
		debt.total().saturating_add(self.principal).saturating_add(self.interest)
	}
}

/// How one idle-walk pass ended, deciding the cursor write.
enum WalkExit<K> {
	/// Not even one step fit the meter — leave the stored cursor untouched.
	Untouched,
	/// The map drained — clear any stored cursor so the next pass wraps to
	/// the front.
	Drained,
	/// The meter ran dry mid-map — park the cursor after the last charged
	/// step.
	Parked(K),
}

impl<T: Config> Pallet<T> {
	/// Translate a rate-index insert/re-insert failure. A stale user-supplied
	/// hint surfaces as [`Error::InvalidPositionHints`]; every other kind —
	/// index/vault disagreement or the list's internal transactional limit
	/// ([`ListError::Internal`]).
	pub(crate) fn map_error(e: ListError) -> Error<T> {
		match e {
			ListError::InvalidPositionHints => Error::<T>::InvalidPositionHints,
			ListError::ItemNotFound |
			ListError::ItemAlreadyExists |
			ListError::ListTooLong |
			ListError::CorruptList |
			ListError::Internal => Error::<T>::RateIndexInvariantBroken,
		}
	}

	/// Read the whole market record, returning `UnknownCollateral` when
	/// missing.
	pub(crate) fn branch_of(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<BranchOf<T>, DispatchError> {
		Branches::<T>::get(collateral_id, stable_id)
			.ok_or_else(|| Error::<T>::UnknownCollateral.into())
	}

	/// TODO: DOC
	pub(crate) fn commit_branch(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		branch: BranchOf<T>,
	) -> DispatchResult {
		let outstanding_before =
			Self::branch_of(collateral_id, stable_id)?.state.debt.outstanding();
		Self::apply_debt_delta(collateral_id, outstanding_before, branch.state.debt.outstanding())?;
		Branches::<T>::insert(collateral_id, stable_id, branch);
		Ok(())
	}

	fn apply_debt_delta(
		collateral_id: &CollateralIdOf<T>,
		outstanding_before: BalanceOf<T>,
		outstanding_after: BalanceOf<T>,
	) -> Result<(), DispatchError> {
		if outstanding_before == outstanding_after {
			return Ok(());
		}
		CollateralRisks::<T>::try_mutate_exists(collateral_id, |maybe| {
			let mut risk = maybe.take().unwrap_or_default();
			risk.outstanding = risk
				.outstanding
				.checked_sub(&outstanding_before)
				.defensive_ok_or(DispatchError::Corruption)?
				.checked_add(&outstanding_after)
				.ok_or(Error::<T>::ArithmeticOverflow)?;
			*maybe = (risk != CollateralRisk::default()).then_some(risk);
			Ok(())
		})
	}

	/// Read a vault row, returning `VaultNotFound` when missing.
	pub(crate) fn vault_of(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<Vault<BalanceOf<T>>, DispatchError> {
		Vaults::<T>::get((collateral_id, stable_id, owner))
			.ok_or_else(|| Error::<T>::VaultNotFound.into())
	}

	fn ratio(
		collateral: BalanceOf<T>,
		debt: BalanceOf<T>,
		price: FixedU128,
	) -> Result<FixedU128, Error<T>> {
		pusd_primitives::collateralization_ratio::<BalanceOf<T>>(collateral, debt, price)
			.ok_or(Error::<T>::UnsafeCollateralizationRatio)
	}

	/// Ensure a vault's collateralization ratio is at or above the branch ICR.
	/// Used by the open/borrow/withdraw safety gates. A `None` ratio (zero debt)
	/// and a below-ICR ratio both surface as `UnsafeCollateralizationRatio`.
	pub(crate) fn ensure_above_icr(
		collateral: BalanceOf<T>,
		debt: BalanceOf<T>,
		price: FixedU128,
		config: &BranchConfig<BalanceOf<T>>,
	) -> Result<(), DispatchError> {
		let cr = Self::ratio(collateral, debt, price)?;
		ensure!(
			cr >= config.initial_collateralization_ratio,
			Error::<T>::UnsafeCollateralizationRatio
		);
		Ok(())
	}

	/// Ensure a vault's fully-accrued collateralization ratio is strictly below
	/// the branch MCR. Used by the enter-final-recovery gate. A `None` ratio
	/// (zero debt) counts as too healthy.
	pub(crate) fn ensure_below_mcr(
		collateral: BalanceOf<T>,
		debt: BalanceOf<T>,
		price: FixedU128,
		config: &BranchConfig<BalanceOf<T>>,
	) -> Result<(), DispatchError> {
		let cr = pusd_primitives::collateralization_ratio::<BalanceOf<T>>(collateral, debt, price)
			.ok_or(Error::<T>::CollateralizationRatioTooHealthy)?;
		ensure!(
			cr < config.minimum_collateralization_ratio,
			Error::<T>::CollateralizationRatioTooHealthy
		);
		Ok(())
	}

	/// Ensure a vault's fully-accrued collateralization ratio is at or above the
	/// branch MCR. Used by the exit-final-recovery gate.
	pub(crate) fn ensure_at_or_above_mcr(
		collateral: BalanceOf<T>,
		debt: BalanceOf<T>,
		price: FixedU128,
		config: &BranchConfig<BalanceOf<T>>,
	) -> Result<(), DispatchError> {
		let cr = Self::ratio(collateral, debt, price)?;
		ensure!(
			cr >= config.minimum_collateralization_ratio,
			Error::<T>::UnsafeCollateralizationRatio
		);
		Ok(())
	}

	/// Derive a vault's lifecycle status from queue/index membership.
	pub(crate) fn vault_status_in(
		rate_list: &VaultListId<CollateralIdOf<T>, StableIdOf<T>>,
		recovery_list: &VaultListId<CollateralIdOf<T>, StableIdOf<T>>,
		owner: &T::AccountId,
	) -> VaultStatus {
		debug_assert!(matches!(rate_list, VaultListId::Rate(..)));
		debug_assert!(matches!(recovery_list, VaultListId::FinalRecovery(..)));
		if T::VaultLists::contains(rate_list, owner) {
			return VaultStatus::Active;
		}
		if T::VaultLists::contains(recovery_list, owner) {
			return VaultStatus::FinalRecovery;
		}
		VaultStatus::Dormant
	}

	/// Derive the lifecycle status of an existing vault row from queue/index
	/// membership. Status is not stored on the row, and the keys must be
	/// re-supplied because the row does not carry them.
	pub(crate) fn vault_status_of(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
	) -> VaultStatus {
		Self::vault_status_in(
			&VaultListId::Rate(collateral_id.clone(), stable_id.clone()),
			&recovery::list_id::<T>(collateral_id, stable_id),
			owner,
		)
	}

	/// Mode is `Frozen` if persisted, otherwise derived from live TCR.
	pub(crate) fn current_mode(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<BranchMode, DispatchError> {
		let branch = Self::branch_of(collateral_id, stable_id)?;
		Self::mode_of(&branch, collateral_id, T::TimeProvider::now())
	}

	/// TODO: DOC
	pub(crate) fn mode_of(
		branch: &BranchOf<T>,
		collateral_id: &CollateralIdOf<T>,
		now: Millis,
	) -> Result<BranchMode, DispatchError> {
		if branch.state.is_frozen() {
			return Ok(BranchMode::Frozen);
		}
		// A failing oracle is what `do_refresh_branch` would persist as
		// `Frozen { OracleFailure }`; report `Frozen` to observers even before
		// that poke lands, rather than defaulting to the most permissive mode
		// while prices are unknowable.
		let price = match T::Oracle::provide_price(collateral_id) {
			Ok(price) => price,
			Err(_) => return Ok(BranchMode::Frozen),
		};
		let tcr = Self::compute_tcr(&branch.state, price, now)?;
		if tcr < branch.config.safety_collateralization_ratio {
			Ok(BranchMode::Safety)
		} else {
			Ok(BranchMode::Normal)
		}
	}

	/// Validate the rate is within branch bounds.
	pub(crate) fn validate_rate(
		config: &BranchConfig<BalanceOf<T>>,
		rate: FixedU128,
	) -> Result<(), DispatchError> {
		if rate < config.minimum_borrow_rate || rate > config.maximum_borrow_rate {
			return Err(Error::<T>::RateOutOfBounds.into());
		}
		Ok(())
	}

	/// Apply Normal/Safety mode TCR rules. `state` is the operation's post
	/// state; the frozen flag is operation-invariant, so it stands in for the
	/// pre state too.
	pub(crate) fn enforce_mode_rules(
		config: &BranchConfig<BalanceOf<T>>,
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		pre_tcr: FixedU128,
		post_tcr: FixedU128,
		is_settlement: bool,
	) -> Result<(), DispatchError> {
		if state.is_frozen() {
			return Err(Error::<T>::BranchFrozen.into());
		}
		if is_settlement {
			return Ok(());
		}
		if pre_tcr < config.safety_collateralization_ratio {
			ensure!(post_tcr >= pre_tcr, Error::<T>::SafetyModeTcrWorsening);
		} else {
			ensure!(
				post_tcr >= config.safety_collateralization_ratio,
				Error::<T>::WouldEnterSafetyMode
			);
		}
		Ok(())
	}

	/// Advance the autoline `effective_ceiling` toward `min(branch_debt + gap,
	/// debt_ceiling)`. Increases are gated by `ceiling_ttl`; decreases apply
	/// immediately. A frozen market pins or lowers the ceiling but never raises it.
	/// Returns whether `state` was changed; a no-op (autoline disabled via
	/// `ceiling_gap == 0`, or already at target) returns `false`.
	pub(crate) fn ratchet_ceiling(
		state: &mut BranchState<T::AccountId, BalanceOf<T>>,
		config: &BranchConfig<BalanceOf<T>>,
		now: Millis,
	) -> bool {
		if config.ceiling_gap.is_zero() {
			return false;
		}
		let target =
			state.debt.principal.saturating_add(config.ceiling_gap).min(config.debt_ceiling);
		if target < state.effective_ceiling {
			state.effective_ceiling = target;
			true
		} else if target > state.effective_ceiling &&
			!state.is_frozen() &&
			now >= state.ceiling_last_inc.saturating_add(config.ceiling_ttl)
		{
			state.effective_ceiling = target;
			state.ceiling_last_inc = now;
			true
		} else {
			false
		}
	}

	/// Authorize a call [`Config::GlobalManagerOrigin`] may force and a market
	/// admin of `required` tier may issue: governance can always do what a
	/// market admin can do here. Governance is checked first; `try_origin`
	/// hands the origin back on failure, so the admin fallback is lossless.
	pub(crate) fn ensure_branch_admin_or_manager(
		origin: OriginFor<T>,
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		required: AdminLevel,
	) -> Result<(), DispatchError> {
		let origin = match T::GlobalManagerOrigin::try_origin(origin) {
			Ok(_) => return Ok(()),
			Err(origin) => origin,
		};
		Self::ensure_branch_admin(origin, collateral_id, stable_id, required).map(|_| ())
	}

	/// Authorize a per-market admin call, returning the caller's [`AdminLevel`].
	/// `full_admin` satisfies any `required`; `emergency_admin` satisfies only
	/// `Emergency`.
	pub(crate) fn ensure_branch_admin(
		origin: OriginFor<T>,
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		required: AdminLevel,
	) -> Result<AdminLevel, DispatchError> {
		let admins = Self::branch_of(collateral_id, stable_id)?.admins;
		let caller = origin.into_caller();
		if caller == admins.full_admin {
			return Ok(AdminLevel::Full);
		}
		if matches!(required, AdminLevel::Emergency) && caller == admins.emergency_admin {
			return Ok(AdminLevel::Emergency);
		}
		Err(Error::<T>::NotBranchAdmin.into())
	}

	/// Fully-accrued total branch debt (the TCR numerator): principal + minted
	/// interest + pending aggregate interest + pending redistribution principal +
	/// bad debt + ownerless debt. Single definition shared by [`Self::compute_tcr`]
	/// and the `branch_debt` redemption-fee accessor so the two cannot diverge.
	pub(crate) fn accrued_branch_debt(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> BalanceOf<T> {
		let elapsed = state.interest_time(now).saturating_sub(state.debt.last_interest_time);
		let pending_aggregate = math::simple_interest_ceil(
			state.debt.weighted_principal_sum,
			FixedU128::one(),
			elapsed,
		);
		state
			.debt
			.principal
			.saturating_add(state.debt.minted_interest)
			.saturating_add(pending_aggregate)
			.saturating_add(state.debt.pending_redistribution_principal)
			.saturating_add(state.debt.bad_debt)
			.saturating_add(state.ownerless_debt)
	}

	/// Compute TCR including aggregate interest accrued since the last update.
	pub(crate) fn compute_tcr(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		price: FixedU128,
		now: Millis,
	) -> Result<FixedU128, DispatchError> {
		let inputs = TcrInputs {
			collateral: state.total_collateral,
			debt: Self::accrued_branch_debt(state, now),
		};
		Self::tcr_from_inputs(&inputs, price)
	}

	/// The single TCR formula, shared by [`Self::compute_tcr`] (live state) and
	/// the operation gate's load-time baseline so the pre and post sides of a
	/// gate cannot diverge.
	pub(crate) fn tcr_from_inputs(
		inputs: &TcrInputs<BalanceOf<T>>,
		price: FixedU128,
	) -> Result<FixedU128, DispatchError> {
		if inputs.debt.is_zero() {
			// Branch with no debt is treated as "infinitely well-collateralized".
			return Ok(FixedU128::max_value());
		}
		let value =
			price.checked_mul_int(inputs.collateral).ok_or(Error::<T>::ArithmeticOverflow)?;
		FixedU128::checked_from_rational(value, inputs.debt)
			.ok_or_else(|| Error::<T>::ArithmeticOverflow.into())
	}

	/// Accrue aggregate branch interest in memory and return the new amount.
	pub(crate) fn accrue_aggregate_interest(
		state: &mut BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> BalanceOf<T> {
		let tau = state.interest_time(now);
		let elapsed = tau.saturating_sub(state.debt.last_interest_time);
		if elapsed == 0 {
			return BalanceOf::<T>::zero();
		}
		let new_interest = math::simple_interest_ceil(
			state.debt.weighted_principal_sum,
			FixedU128::one(),
			elapsed,
		);
		state.debt.last_interest_time = tau;
		if new_interest.is_zero() {
			return BalanceOf::<T>::zero();
		}
		state.debt.minted_interest = state.debt.minted_interest.saturating_add(new_interest);
		new_interest
	}

	/// Issue `amount` of the market's coin (branch interest or an upfront
	/// fee), let `T::YieldHook` take the Stability-Pool share, and hand the
	/// remainder to `T::FeeHandler`.
	pub(crate) fn mint_and_route_yield(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		amount: BalanceOf<T>,
	) {
		let credit = T::StableAssets::issue(stable_id.clone(), amount);
		let credit = T::YieldHook::distribute_yield(collateral_id, stable_id, credit);
		T::FeeHandler::on_unbalanced(credit);
	}

	/// Project a vault touch without mutating storage.
	pub(crate) fn pending_touch_for(
		vault: &Vault<BalanceOf<T>>,
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> PendingTouch<BalanceOf<T>> {
		let tau = state.interest_time(now);
		let elapsed = tau.saturating_sub(vault.last_interest_time);
		let principal_interest =
			math::simple_interest_floor(vault.debt.principal, vault.annual_rate, elapsed);

		let redistribution = state.redistribution;
		let snap = vault.redistribution_snapshot;
		if snap == redistribution {
			return PendingTouch {
				principal: BalanceOf::<T>::zero(),
				collateral: BalanceOf::<T>::zero(),
				interest: principal_interest,
			};
		}

		let delta_debt_per_stake =
			redistribution.debt_per_stake.saturating_sub(snap.debt_per_stake);
		let delta_collat_per_stake =
			redistribution.collateral_per_stake.saturating_sub(snap.collateral_per_stake);
		let delta_dt_per_stake =
			redistribution.debt_time_per_stake.saturating_sub(snap.debt_time_per_stake);
		// Cap against the branch counter; rounding dust stays in branch aggregates.
		let raw_principal = delta_debt_per_stake.saturating_mul_int(vault.redistribution_stake);
		let principal = core::cmp::min(raw_principal, state.debt.pending_redistribution_principal);
		let collateral = delta_collat_per_stake.saturating_mul_int(vault.redistribution_stake);
		// Keep this in branch interest time, matching the liquidation writer.
		let now_fp = FixedU128::saturating_from_integer(tau);
		let extra_per_stake =
			now_fp.saturating_mul(delta_debt_per_stake).saturating_sub(delta_dt_per_stake);
		let rate_factor = vault
			.annual_rate
			.checked_div(&FixedU128::saturating_from_integer(pusd_primitives::MILLIS_PER_YEAR))
			.defensive_unwrap_or_else(FixedU128::zero);
		let redistribution_interest = extra_per_stake
			.saturating_mul(rate_factor)
			.saturating_mul_int(vault.redistribution_stake);

		PendingTouch {
			principal,
			collateral,
			interest: principal_interest.saturating_add(redistribution_interest),
		}
	}

	/// A zero-debt, zero-stake vault row: the pre-borrow shape an open feeds
	/// to [`Self::apply_borrow`], so the open fee is priced by the same code
	/// path as every borrow. The stake MUST be zero here — `apply_borrow`
	/// swaps the row's full aggregate contribution, and the open's stake
	/// enters the aggregates via `set_vault_stake` after the borrow is
	/// applied. The caller stamps the returned fee onto the row's debt.
	pub(crate) fn open_scratch_row(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		annual_rate: FixedU128,
		collateral: BalanceOf<T>,
		now: Millis,
	) -> Vault<BalanceOf<T>> {
		Vault {
			collateral,
			debt: VaultDebt { principal: Zero::zero(), interest: Zero::zero() },
			annual_rate,
			last_interest_time: state.interest_time(now),
			last_rate_update: now,
			redistribution_stake: Zero::zero(),
			redistribution_snapshot: state.redistribution,
		}
	}

	fn avg_rate(state: &BranchState<T::AccountId, BalanceOf<T>>) -> FixedU128 {
		math::average_branch_rate(
			state.debt.weighted_principal_sum,
			state.debt.principal.saturating_add(state.debt.pending_redistribution_principal),
		)
	}

	/// Apply a borrow to a branch/vault draft pair and return the upfront fee.
	///
	/// A borrow that also changes the rate inside the cooldown charges the
	/// upfront fee over both the debt increase and the existing principal.
	pub(crate) fn apply_borrow(
		state: &mut BranchState<T::AccountId, BalanceOf<T>>,
		config: &BranchConfig<BalanceOf<T>>,
		vault: &mut Vault<BalanceOf<T>>,
		debt_increase: BalanceOf<T>,
		new_rate: FixedU128,
		now: Millis,
	) -> BalanceOf<T> {
		let old_rate = vault.annual_rate;
		let rate_change_fee_base = if new_rate != old_rate && !vault.cooldown_elapsed(config, now) {
			vault.debt.principal
		} else {
			BalanceOf::<T>::zero()
		};
		state.detach_vault(vault);
		vault.debt.principal = vault.debt.principal.saturating_add(debt_increase);
		vault.annual_rate = new_rate;
		if new_rate != old_rate {
			vault.last_rate_update = now;
		}
		state.attach_vault(vault);
		let avg = Self::avg_rate(state);
		let fee = math::simple_interest_ceil(
			debt_increase.saturating_add(rate_change_fee_base),
			avg,
			config.upfront_fee_period,
		);
		state.debt.minted_interest = state.debt.minted_interest.saturating_add(fee);
		vault.debt.interest = vault.debt.interest.saturating_add(fee);
		fee
	}

	/// Apply a rate change's branch-side accounting to `state` and return the
	/// upfront fee; see [`Self::apply_borrow`] for the caller contract.
	pub(crate) fn apply_rate_change(
		state: &mut BranchState<T::AccountId, BalanceOf<T>>,
		config: &BranchConfig<BalanceOf<T>>,
		vault: &mut Vault<BalanceOf<T>>,
		new_rate: FixedU128,
		now: Millis,
	) -> BalanceOf<T> {
		let old_rate = vault.annual_rate;
		if new_rate == old_rate {
			return BalanceOf::<T>::zero();
		}
		state.change_vault_rate(
			old_rate,
			new_rate,
			vault.debt.principal,
			vault.redistribution_stake,
		);
		let fee = if vault.cooldown_elapsed(config, now) {
			BalanceOf::<T>::zero()
		} else {
			let avg = Self::avg_rate(state);
			math::simple_interest_ceil(vault.debt.principal, avg, config.upfront_fee_period)
		};
		state.debt.minted_interest = state.debt.minted_interest.saturating_add(fee);
		vault.annual_rate = new_rate;
		vault.last_rate_update = now;
		vault.debt.interest = vault.debt.interest.saturating_add(fee);
		fee
	}

	/// Lazily walk a vault list from its tail, following `prev` pointers — the same
	/// order as [`SortedListInterface::iter_from_tail`], but every storage read is
	/// deferred until the iterator advances, so a caller taking only the head pays
	/// for only the tail read.
	fn list_from_tail(
		list: VaultListId<CollateralIdOf<T>, StableIdOf<T>>,
	) -> impl Iterator<Item = T::AccountId> {
		let mut started = false;
		let mut cursor: Option<T::AccountId> = None;
		core::iter::from_fn(move || {
			if !started {
				started = true;
				cursor = T::VaultLists::tail(&list);
			} else if let Some(current) = &cursor {
				cursor = T::VaultLists::neighbors(&list, current).and_then(|p| p.prev);
			}
			cursor.clone()
		})
	}

	/// A branch's redemption targets, each tagged with its lifecycle status, in
	/// priority order: if the `FinalRecovery` FIFO is
	/// non-empty, yield only its head; else if `dormant_redemption_target` is set,
	/// yield only that; otherwise yield the rate index tail-first (all `Active`).
	/// Lazy and allocation-free: `.next()` gives the next target and `take(n)` the
	/// queue view, reading only the tiers they reach.
	pub(crate) fn redemption_targets(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> impl Iterator<Item = (T::AccountId, VaultStatus)> {
		let priority = recovery::next_target::<T>(collateral_id, stable_id)
			.map(|owner| (owner, VaultStatus::FinalRecovery))
			.or_else(|| {
				Branches::<T>::get(collateral_id, stable_id)
					.and_then(|branch| branch.state.dormant_redemption_target)
					.map(|owner| (owner, VaultStatus::Dormant))
			});
		// The rate index is walked only when no FinalRecovery/Dormant target gates it.
		let rate = priority
			.is_none()
			.then(|| {
				Self::list_from_tail(VaultListId::Rate(collateral_id.clone(), stable_id.clone()))
			})
			.into_iter()
			.flatten()
			.map(|owner| (owner, VaultStatus::Active));
		priority.into_iter().chain(rate)
	}

	/// The next ordinary redemption target after `owner` in the rate index: its
	/// head-ward (`prev`) neighbor. Lets the orchestrator skip an underwater
	/// ordinary head tail-first without mutating the index. `None` when `owner` is
	/// the head (highest-rate) vault or is not a rate-index member — the latter is
	/// an orchestrator contract violation, logged-but-tolerated in release so a
	/// broken cursor reads as an exhausted queue rather than corrupting the walk.
	pub(crate) fn ordinary_target_after(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
	) -> Option<T::AccountId> {
		let rate_list = VaultListId::Rate(collateral_id.clone(), stable_id.clone());
		defensive_assert!(
			T::VaultLists::contains(&rate_list, owner),
			"redemption after-cursor must be a current rate-index member"
		);
		T::VaultLists::neighbors(&rate_list, owner).and_then(|p| p.prev)
	}

	/// Read the `(config, branch state, vault)` triple for a `predict_*` view.
	/// Returns `None` if any row is missing — the predict APIs treat that as
	/// "no fee" rather than an error.
	pub(crate) fn predict_inputs(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
	) -> Option<(
		BranchConfig<BalanceOf<T>>,
		BranchState<T::AccountId, BalanceOf<T>>,
		Vault<BalanceOf<T>>,
	)> {
		let branch = Branches::<T>::get(collateral_id, stable_id)?;
		let vault = Vaults::<T>::get((collateral_id, stable_id, owner))?;
		Some((branch.config, branch.state, vault))
	}

	/// Refresh markets and vaults with the block's leftover weight, clamped to
	/// [`Config::IdleMaxRefreshWeight`]: charge the cursor bookkeeping, resume
	/// the [`BranchIdleCursor`] walk reconciling oracle-frozen state, then
	/// resume the flat [`IdleCursor`] walk over the [`Vaults`] map until the
	/// meter drains. Every attempted step — failed transactional attempts
	/// included — is charged in the returned weight; [`Self::idle_walk_pass`]
	/// states the exact per-step and terminal-probe accounting.
	pub(crate) fn on_idle_walk(remaining: Weight) -> Weight {
		let Some(limit) = T::IdleMaxRefreshWeight::get() else { return Weight::zero() };
		let mut meter = WeightMeter::with_limit(remaining.min(limit));

		// The walk's flat cost — the cursor reads/writes, modeled by
		// `on_idle_base`. If even that does not fit, nothing is read or
		// written — report zero.
		if meter.try_consume(T::WeightInfo::on_idle_base()).is_err() {
			return Weight::zero();
		}

		// The registry is unbounded, so the branch walk gets at most half the
		// remaining budget — permissionless branch registration can never
		// starve vault maintenance — and the vault walk inherits whatever the
		// branch walk leaves unused.
		let mut branch_budget = WeightMeter::with_limit(meter.remaining().saturating_div(2));
		Self::idle_branch_walk(&mut branch_budget);
		meter.consume(branch_budget.consumed());

		Self::idle_vault_walk(&mut meter);
		meter.consumed()
	}

	/// Resume the branch-refresh walk at [`BranchIdleCursor`], reconciling
	/// oracle-frozen state until `meter` drains.
	fn idle_branch_walk(meter: &mut WeightMeter) {
		let cursor = BranchIdleCursor::<T>::get();
		let iter = match &cursor {
			Some((collateral_id, stable_id)) => Branches::<T>::iter_keys_from(
				Branches::<T>::hashed_key_for(collateral_id, stable_id),
			),
			None => Branches::<T>::iter_keys(),
		};
		let pass = Self::idle_walk_pass(
			meter,
			T::WeightInfo::on_idle_one_branch(),
			iter,
			|(collateral_id, stable_id)| Self::idle_branch_step(collateral_id, stable_id),
		);
		match pass {
			WalkExit::Untouched => {},
			// Skip the write when no cursor was stored: the steady state of a
			// registry that drains within budget every block.
			WalkExit::Drained => {
				if cursor.is_some() {
					BranchIdleCursor::<T>::set(None);
				}
			},
			WalkExit::Parked(key) => BranchIdleCursor::<T>::set(Some(key)),
		}
	}

	/// One transactional branch-refresh attempt: the per-key unit of
	/// [`Self::idle_branch_walk`], and exactly what `on_idle_one_branch`
	/// measures. Failures roll back and are swallowed — the walk charges
	/// attempts, not outcomes.
	pub(crate) fn idle_branch_step(collateral_id: &CollateralIdOf<T>, stable_id: &StableIdOf<T>) {
		let _ = with_storage_layer::<(), DispatchError, _>(|| {
			Self::do_refresh_branch(collateral_id, stable_id)
		});
	}

	/// Resume the flat vault-refresh walk at [`IdleCursor`] until the meter
	/// drains. Map order visits every row eventually — dormant husks and
	/// mid-FIFO `FinalRecovery` vaults included, which a per-branch rate-index
	/// cursor never reached.
	fn idle_vault_walk(meter: &mut WeightMeter) {
		let cursor = IdleCursor::<T>::get();
		let iter = match &cursor {
			Some(key) => Vaults::<T>::iter_keys_from(Vaults::<T>::hashed_key_for(key.clone())),
			None => Vaults::<T>::iter_keys(),
		};
		let pass = Self::idle_walk_pass(
			meter,
			T::WeightInfo::on_idle_one_vault(),
			iter,
			|(collateral_id, stable_id, owner)| {
				Self::idle_vault_step(collateral_id, stable_id, owner)
			},
		);
		match pass {
			WalkExit::Untouched => {},
			// Skip the write when no cursor was stored: the steady state of a
			// map that drains within budget every block.
			WalkExit::Drained => {
				if cursor.is_some() {
					IdleCursor::<T>::set(None);
				}
			},
			WalkExit::Parked(key) => IdleCursor::<T>::set(Some(key)),
		}
	}

	/// One transactional vault-refresh attempt: the per-key unit of
	/// [`Self::idle_vault_walk`], and exactly what `on_idle_one_vault`
	/// measures.
	pub(crate) fn idle_vault_step(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
	) {
		let _ = with_storage_layer::<(), DispatchError, _>(|| {
			BranchOp::<T>::refresh(collateral_id.clone(), stable_id.clone(), owner)
		});
	}

	/// TODO: DOC
	fn idle_walk_pass<K>(
		meter: &mut WeightMeter,
		per_step: Weight,
		mut iter: impl Iterator<Item = K>,
		mut step: impl FnMut(&K),
	) -> WalkExit<K> {
		defensive_assert!(
			per_step != Weight::zero(),
			"zero per-step weight disables the idle walk"
		);
		if per_step == Weight::zero() {
			return WalkExit::Untouched;
		}
		if !meter.can_consume(per_step) {
			return WalkExit::Untouched;
		}
		// Bounded: every iteration consumes the non-zero `per_step` from the
		// finite meter, or breaks when the map drains.
		loop {
			let Some(key) = iter.next() else { break WalkExit::Drained };
			step(&key);
			meter.consume(per_step);
			if !meter.can_consume(per_step) {
				// Drained rather than Parked when the meter ran dry exactly
				// at the map's end, so the next pass starts at the front
				// instead of burning a block discovering the drain.
				break if iter.next().is_some() {
					WalkExit::Parked(key)
				} else {
					WalkExit::Drained
				};
			}
		}
	}
}
