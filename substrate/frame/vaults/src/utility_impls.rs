//! Internal (non-dispatchable) `Pallet` helpers: storage accessors, safety
//! gates, interest/fee accounting, mode rules, view-function backends, and the
//! `on_idle` refresh walk.

use crate::{
	context::OpContext,
	math,
	pallet::{
		BalanceOf, BranchAdmin, BranchConfigs, BranchStates, Config, Error, Millis, Pallet, Vaults,
	},
	recovery,
	types::{AdminLevel, BranchConfig, BranchMode, BranchState, Vault, VaultListId, VaultStatus},
	weights::WeightInfo,
};
use frame::{
	deps::frame_support::{defensive_assert, storage::with_storage_layer},
	prelude::*,
	traits::{fungibles::Balanced as FungiblesBalanced, OriginTrait, Time},
};
use pallet_linked_list::{ListError, SortedListInterface};
use pusd_primitives::ProvidePrice;

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

	/// Read the branch state, returning `UnknownCollateral` when missing.
	pub(crate) fn branch_state_of(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Result<BranchState<T::AccountId, BalanceOf<T>>, DispatchError> {
		BranchStates::<T>::get(collateral_id, stable_id)
			.ok_or_else(|| Error::<T>::UnknownCollateral.into())
	}

	/// Read the branch config, returning `UnknownCollateral` when missing.
	pub(crate) fn branch_config_of(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Result<BranchConfig<BalanceOf<T>>, DispatchError> {
		BranchConfigs::<T>::get((collateral_id, stable_id))
			.ok_or_else(|| Error::<T>::UnknownCollateral.into())
	}

	/// Read a vault row, returning `VaultNotFound` when missing.
	pub(crate) fn vault_of(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
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

	/// Ensure a vault's fully-accrued collateralization ratio is strictly below the
	/// branch MCR. Used by the liquidation-eligibility and enter-final-recovery
	/// gates.
	pub(crate) fn ensure_below_mcr(
		collateral: BalanceOf<T>,
		debt: BalanceOf<T>,
		price: FixedU128,
		config: &BranchConfig<BalanceOf<T>>,
	) -> Result<(), DispatchError> {
		let cr = Self::ratio(collateral, debt, price)?;
		ensure!(
			cr < config.minimum_collateralization_ratio,
			Error::<T>::UnsafeCollateralizationRatio
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
		rate_list: &VaultListId<T::CollateralAssetId, T::StableAssetId>,
		recovery_list: &VaultListId<T::CollateralAssetId, T::StableAssetId>,
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
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
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
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Result<BranchMode, DispatchError> {
		let state = Self::branch_state_of(collateral_id, stable_id)?;
		if state.is_frozen() {
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
		let config = Self::branch_config_of(collateral_id, stable_id)?;
		let now = T::TimeProvider::now();
		let tcr = Self::compute_tcr(&state, price, now)?;
		if tcr < config.safety_collateralization_ratio {
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
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
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
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		required: AdminLevel,
	) -> Result<AdminLevel, DispatchError> {
		let info = BranchAdmin::<T>::get((collateral_id, stable_id))
			.ok_or(Error::<T>::UnknownCollateral)?;
		let caller = origin.into_caller();
		if caller == info.admins.full_admin {
			return Ok(AdminLevel::Full);
		}
		if matches!(required, AdminLevel::Emergency) && caller == info.admins.emergency_admin {
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

	/// Issue `amount` of the market's coin (branch interest or an upfront fee) and
	/// hand it to `T::FeeHandler`. The Stability-Pool yield-share split returns
	/// with `pallet-stability-pool`; until then the whole minted amount routes to
	/// the fee destination.
	pub(crate) fn mint_and_route_yield(stable_id: &T::StableAssetId, amount: BalanceOf<T>) {
		let credit = T::StableAssets::issue(stable_id.clone(), amount);
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

	/// Upfront fee for opening a vault at the post-open average branch rate.
	pub(crate) fn open_upfront_fee(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		config: &BranchConfig<BalanceOf<T>>,
		new_debt: BalanceOf<T>,
		new_rate: FixedU128,
	) -> BalanceOf<T> {
		let total_ib = state
			.debt
			.principal
			.saturating_add(state.debt.pending_redistribution_principal)
			.saturating_add(new_debt);
		let weighted = state
			.debt
			.weighted_principal_sum
			.saturating_add(new_rate.saturating_mul_int(new_debt));
		let avg = math::average_branch_rate(weighted, total_ib);
		math::simple_interest_ceil(new_debt, avg, config.upfront_fee_period)
	}

	fn avg_rate(state: &BranchState<T::AccountId, BalanceOf<T>>) -> FixedU128 {
		math::average_branch_rate(
			state.debt.weighted_principal_sum,
			state.debt.principal.saturating_add(state.debt.pending_redistribution_principal),
		)
	}

	/// Apply a borrow's branch-side accounting to `state` and return the
	/// upfront fee. `vault` is the pre-borrow row; the caller stamps it (the
	/// live path) or discards it (the `predict_*` views, which apply to a
	/// scratch copy of the state).
	///
	/// `rate_change_fee_base` is the existing principal that the rate-change
	/// component of the upfront fee is charged against (zero when the call is a
	/// pure debt increase or the cooldown has elapsed).
	pub(crate) fn apply_borrow(
		state: &mut BranchState<T::AccountId, BalanceOf<T>>,
		config: &BranchConfig<BalanceOf<T>>,
		vault: &Vault<BalanceOf<T>>,
		debt_increase: BalanceOf<T>,
		new_rate: FixedU128,
		rate_change_fee_base: BalanceOf<T>,
	) -> BalanceOf<T> {
		// Swap the vault's full aggregate contribution: detach the pre-borrow row
		// and attach the post-borrow one, so `attach_vault`/`detach_vault` stay
		// the only writers of the weighted sums. The fee is not stamped on
		// `vault_after` — attach would then double-count it against the explicit
		// `minted_interest` add below (the caller stamps the vault row).
		let mut vault_after = vault.clone();
		vault_after.debt.principal = vault.debt.principal.saturating_add(debt_increase);
		vault_after.annual_rate = new_rate;
		state.detach_vault(vault);
		state.attach_vault(&vault_after);
		let avg = Self::avg_rate(state);
		let fee = math::simple_interest_ceil(
			debt_increase.saturating_add(rate_change_fee_base),
			avg,
			config.upfront_fee_period,
		);
		state.debt.minted_interest = state.debt.minted_interest.saturating_add(fee);
		fee
	}

	/// Apply a rate change's branch-side accounting to `state` and return the
	/// upfront fee; see [`Self::apply_borrow`] for the caller contract.
	pub(crate) fn apply_rate_change(
		state: &mut BranchState<T::AccountId, BalanceOf<T>>,
		config: &BranchConfig<BalanceOf<T>>,
		vault: &Vault<BalanceOf<T>>,
		new_rate: FixedU128,
		cooldown_elapsed: bool,
	) -> BalanceOf<T> {
		state.change_vault_rate(
			vault.annual_rate,
			new_rate,
			vault.debt.principal,
			vault.redistribution_stake,
		);
		let fee = if cooldown_elapsed {
			BalanceOf::<T>::zero()
		} else {
			let avg = Self::avg_rate(state);
			math::simple_interest_ceil(vault.debt.principal, avg, config.upfront_fee_period)
		};
		state.debt.minted_interest = state.debt.minted_interest.saturating_add(fee);
		fee
	}

	/// Fully-accrued total branch debt (principal + minted interest + pending
	/// aggregate interest + pending redistribution + bad debt + ownerless debt).
	/// Mirrors the numerator-side of [`Self::compute_tcr`]; used to size the
	/// redemption fee's redeemed fraction. Zero for an unregistered branch.
	pub(crate) fn view_branch_debt(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		now: Millis,
	) -> BalanceOf<T> {
		let Some(bs) = BranchStates::<T>::get(collateral_id, stable_id) else {
			return BalanceOf::<T>::zero();
		};
		Self::accrued_branch_debt(&bs, now)
	}

	pub(crate) fn view_vault_cr(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		owner: &T::AccountId,
	) -> Option<FixedU128> {
		let vault = Vaults::<T>::get((collateral_id, stable_id, owner))?;
		let state = BranchStates::<T>::get(collateral_id, stable_id)?;
		let now = T::TimeProvider::now();
		let price = T::Oracle::provide_price(collateral_id).ok()?;
		let pending = Self::pending_touch_for(&vault, &state, now);
		let total_coll = vault.collateral.saturating_add(pending.collateral);
		let total_debt = vault
			.debt
			.total()
			.saturating_add(pending.principal)
			.saturating_add(pending.interest);
		pusd_primitives::collateralization_ratio::<BalanceOf<T>>(total_coll, total_debt, price)
	}

	pub(crate) fn view_branch_tcr(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Option<FixedU128> {
		let state = BranchStates::<T>::get(collateral_id, stable_id)?;
		let price = T::Oracle::provide_price(collateral_id).ok()?;
		let now = T::TimeProvider::now();
		Self::compute_tcr(&state, price, now).ok()
	}

	/// Lazily walk a vault list from its tail, following `prev` pointers — the same
	/// order as [`SortedListInterface::iter_from_tail`], but every storage read is
	/// deferred until the iterator advances, so a caller taking only the head pays
	/// for only the tail read.
	fn list_from_tail(
		list: VaultListId<T::CollateralAssetId, T::StableAssetId>,
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
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> impl Iterator<Item = (T::AccountId, VaultStatus)> {
		let priority = recovery::next_target::<T>(collateral_id, stable_id)
			.map(|owner| (owner, VaultStatus::FinalRecovery))
			.or_else(|| {
				BranchStates::<T>::get(collateral_id, stable_id)
					.and_then(|bs| bs.dormant_redemption_target)
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
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		owner: &T::AccountId,
	) -> Option<T::AccountId> {
		let rate_list = VaultListId::Rate(collateral_id.clone(), stable_id.clone());
		defensive_assert!(
			T::VaultLists::contains(&rate_list, owner),
			"redemption after-cursor must be a current rate-index member"
		);
		T::VaultLists::neighbors(&rate_list, owner).and_then(|p| p.prev)
	}

	/// Walk the rate index tail-first, summing active-vault principal while the
	/// stored priority is strictly below `rate`, visiting at most `max_steps`
	/// vaults. Returns the partial sum when the cap stops the walk early.
	pub(crate) fn view_debt_in_front(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		rate: FixedU128,
		max_steps: u32,
	) -> BalanceOf<T> {
		let mut total = BalanceOf::<T>::zero();
		let rate_list = VaultListId::Rate(collateral_id.clone(), stable_id.clone());
		let mut cursor = T::VaultLists::tail(&rate_list);
		for _ in 0..max_steps {
			let Some(o) = cursor else { break };
			let Some((priority, neighbors)) = T::VaultLists::node(&rate_list, &o) else { break };
			if priority >= rate {
				break;
			}
			if let Some(v) = Vaults::<T>::get((collateral_id, stable_id, &o)) {
				total = total.saturating_add(v.debt.principal);
			}
			cursor = neighbors.prev;
		}
		total
	}

	pub(crate) fn predict_upfront_fee_open(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		initial_debt: BalanceOf<T>,
		annual_rate: FixedU128,
	) -> BalanceOf<T> {
		match (
			BranchConfigs::<T>::get((collateral_id, stable_id)),
			BranchStates::<T>::get(collateral_id, stable_id),
		) {
			(Some(config), Some(state)) => {
				Self::open_upfront_fee(&state, &config, initial_debt, annual_rate)
			},
			_ => BalanceOf::<T>::zero(),
		}
	}

	pub(crate) fn predict_upfront_fee_borrow(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		owner: &T::AccountId,
		debt_increase: BalanceOf<T>,
		maybe_new_rate: Option<FixedU128>,
	) -> BalanceOf<T> {
		let Some((config, mut state, vault)) =
			Self::predict_inputs(collateral_id, stable_id, owner)
		else {
			return BalanceOf::<T>::zero();
		};
		let new_rate = maybe_new_rate.unwrap_or(vault.annual_rate);
		let now = T::TimeProvider::now();
		let cooldown_elapsed = vault.cooldown_elapsed(&config, now);
		let rate_change_fee_base = vault.rate_change_base(maybe_new_rate, cooldown_elapsed);
		Self::apply_borrow(
			&mut state,
			&config,
			&vault,
			debt_increase,
			new_rate,
			rate_change_fee_base,
		)
	}

	pub(crate) fn predict_upfront_fee_rate_change(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		owner: &T::AccountId,
		new_rate: FixedU128,
	) -> BalanceOf<T> {
		let Some((config, mut state, vault)) =
			Self::predict_inputs(collateral_id, stable_id, owner)
		else {
			return BalanceOf::<T>::zero();
		};
		let now = T::TimeProvider::now();
		let cooldown_elapsed = vault.cooldown_elapsed(&config, now);
		Self::apply_rate_change(&mut state, &config, &vault, new_rate, cooldown_elapsed)
	}

	/// Read the `(config, branch state, vault)` triple for a `predict_*` view.
	/// Returns `None` if any row is missing — the predict APIs treat that as
	/// "no fee" rather than an error.
	fn predict_inputs(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		owner: &T::AccountId,
	) -> Option<(
		BranchConfig<BalanceOf<T>>,
		BranchState<T::AccountId, BalanceOf<T>>,
		Vault<BalanceOf<T>>,
	)> {
		Some((
			BranchConfigs::<T>::get((collateral_id, stable_id))?,
			BranchStates::<T>::get(collateral_id, stable_id)?,
			Vaults::<T>::get((collateral_id, stable_id, owner))?,
		))
	}

	/// Refresh the next handful of vaults across each branch using the cursor.
	pub(crate) fn on_idle_walk(remaining: Weight) -> Weight {
		let per_call = T::WeightInfo::on_idle_one_vault();
		if remaining.any_lt(per_call) {
			return Weight::zero();
		}
		let mut consumed = Weight::zero();
		let mut budget = T::MaxOnIdleVaultRefresh::get();
		let touch_one = |collateral_id: &T::CollateralAssetId,
		                 stable_id: &T::StableAssetId,
		                 owner: &T::AccountId| {
			if !Vaults::<T>::contains_key((collateral_id, stable_id, owner)) {
				return;
			}
			let _ = with_storage_layer::<(), DispatchError, _>(|| {
				OpContext::<T>::refresh(collateral_id.clone(), stable_id.clone(), owner)
			});
		};
		for (collateral_id, stable_id) in BranchConfigs::<T>::iter_keys() {
			let collateral_id = &collateral_id;
			let stable_id = &stable_id;
			if budget == 0 || (remaining.saturating_sub(consumed)).any_lt(per_call) {
				break;
			}
			let _ = with_storage_layer::<(), DispatchError, _>(|| {
				Self::do_refresh_branch(collateral_id, stable_id)
			});
			let Some(branch) = BranchStates::<T>::get(collateral_id, stable_id) else { continue };
			let rate_list = VaultListId::Rate(collateral_id.clone(), stable_id.clone());
			let initial_cursor = branch.idle_cursor.or_else(|| T::VaultLists::head(&rate_list));
			let mut cursor = initial_cursor.clone();
			let final_recovery_head = recovery::next_target::<T>(collateral_id, stable_id);
			let dormant_target = branch.dormant_redemption_target;

			while budget > 0 {
				let Some(owner) = cursor.clone() else { break };
				touch_one(collateral_id, stable_id, &owner);
				cursor = T::VaultLists::neighbors(&rate_list, &owner).and_then(|p| p.next);
				budget = budget.saturating_sub(1);
				consumed = consumed.saturating_add(per_call);
				if (remaining.saturating_sub(consumed)).any_lt(per_call) {
					break;
				}
			}

			let try_extra = |owner: T::AccountId, budget: &mut u32, consumed: &mut Weight| {
				if *budget == 0 || (remaining.saturating_sub(*consumed)).any_lt(per_call) {
					return;
				}
				touch_one(collateral_id, stable_id, &owner);
				*budget = budget.saturating_sub(1);
				*consumed = consumed.saturating_add(per_call);
			};
			if let Some(owner) = final_recovery_head {
				try_extra(owner, &mut budget, &mut consumed);
			}
			if let Some(owner) = dormant_target {
				try_extra(owner, &mut budget, &mut consumed);
			}

			if cursor != initial_cursor {
				BranchStates::<T>::mutate(collateral_id, stable_id, |maybe| {
					if let Some(state) = maybe {
						state.idle_cursor = cursor.take();
					}
				});
			}
		}
		consumed
	}
}
