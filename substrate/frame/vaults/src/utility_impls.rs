//! Internal (non-dispatchable) `Pallet` helpers: storage accessors, safety
//! gates, interest/fee accounting, mode rules, and the `on_idle` refresh walk.

use crate::{
	context::VaultOp,
	math,
	pallet::{
		BalanceOf, BranchIdleCursor, BranchOf, Branches, CollateralIdOf, Config, Error, IdleCursor,
		Millis, Pallet, StableCreditOf, StableIdOf, StablecoinDebt, VaultRecordOf, Vaults,
	},
	recovery,
	types::{
		AdminLevel, BranchConfig, BranchMode, BranchState, DebtBreakdown, DebtCollateral,
		InterestWeight, PendingInterest, RedistributionAttribution, StablecoinDebtState, Vault,
		VaultListId, VaultStatus,
	},
	weights::WeightInfo,
};
use frame::{
	arithmetic::ArithmeticError,
	deps::frame_support::{
		require_transactional, storage::with_storage_layer, weights::WeightMeter,
	},
	prelude::*,
	traits::{
		fungibles::{Balanced as FungiblesBalanced, Inspect as FungiblesInspect, InspectHold as _},
		tokens::{Fortitude, Precision, Preservation, Provenance},
		AccountTouch, AssetFootprint, DefensiveOption, Footprint, Time,
	},
};
use linked_list_interface::{ListError, SortedListInterface};
use pusd_primitives::{collateralization_ratio, OnBranchYield, ProvidePrice};

/// Exact changes the next vault touch would apply.
pub(crate) struct PendingTouch<Balance> {
	/// Principal and collateral moved from the redistribution pools into the vault.
	pub redistribution: DebtCollateral<Balance>,
	/// Pending weight and its time anchor removed from the virtual pool atomically.
	pub attribution: RedistributionAttribution<Balance>,
	/// Whole interest folded into the vault and the sub-unit remainder retained on it.
	pub interest: PendingInterest<Balance>,
}

/// One fully touched vault and its isolated branch draft, accrued to `now`.
pub(crate) struct TouchedVaultDraft<AccountId, Balance> {
	pub(crate) config: BranchConfig<Balance>,
	pub(crate) state: BranchState<AccountId, Balance>,
	pub(crate) vault: Vault<Balance>,
	pub(crate) status: VaultStatus,
	pub(crate) now: Millis,
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

/// The part of one branch that contributes to derived debt aggregates.
struct BranchContribution<Balance> {
	outstanding: Balance,
	pending_interest: PendingInterest<Balance>,
	active_weight: InterestWeight<Balance>,
}

impl<T: Config> Pallet<T> {
	/// Translate a rate-index insert/re-insert failure. A stale user-supplied
	/// hint surfaces as [`Error::InvalidPositionHints`]; every other kind —
	/// index/vault disagreement or the list's internal transactional limit
	/// ([`ListError::Internal`]).
	pub(crate) const fn map_error(e: ListError) -> Error<T> {
		match e {
			ListError::InvalidPositionHints => Error::<T>::InvalidPositionHints,
			ListError::ItemNotFound |
			ListError::ItemAlreadyExists |
			ListError::ListTooLong |
			ListError::CorruptList |
			ListError::Internal => Error::<T>::RateIndexInvariantBroken,
		}
	}

	/// Apply one vault touch to in-memory branch and vault drafts.
	///
	/// The branch's aggregate interest must already be accrued to `now`.
	/// Returns the touch and the interest units that require new issuance.
	pub(crate) fn apply_vault_touch(
		state: &mut BranchState<T::AccountId, BalanceOf<T>>,
		vault: &mut Vault<BalanceOf<T>>,
		status: VaultStatus,
		now: Millis,
	) -> Result<(PendingTouch<BalanceOf<T>>, BalanceOf<T>), DispatchError> {
		debug_assert_eq!(state.debt.last_interest_time, state.interest_time(now));
		let pending = Self::pending_touch_for(vault, state, now)?;
		let interest_to_mint = state
			.debt
			.attribute_interest(pending.interest.interest)
			.ok_or(Error::<T>::ArithmeticOverflow)?;

		if !pending.interest.interest.is_zero() {
			vault.debt.interest = vault
				.debt
				.interest
				.checked_add(&pending.interest.interest)
				.ok_or(Error::<T>::ArithmeticOverflow)?;
		}
		vault.interest_remainder = pending.interest.remainder;
		let accounted_before = vault.clone();
		if !pending.redistribution.debt.is_zero() ||
			!pending.redistribution.collateral.is_zero() ||
			!pending.attribution.is_zero()
		{
			state.consume_redistribution(pending.redistribution, pending.attribution)?;
			vault.debt.principal = vault
				.debt
				.principal
				.checked_add(&pending.redistribution.debt)
				.ok_or(Error::<T>::ArithmeticOverflow)?;
			vault.collateral = vault
				.collateral
				.checked_add(&pending.redistribution.collateral)
				.ok_or(Error::<T>::ArithmeticOverflow)?;
		}
		vault.redistribution_checkpoint = state.redistribution;
		vault.last_interest_time = state.interest_time(now);
		vault.redistribution_stake = if status.is_final_recovery() {
			Zero::zero()
		} else {
			state.stake_for(vault.collateral).ok_or(Error::<T>::ArithmeticOverflow)?
		};

		state.replace_vault(Some(&accounted_before), Some(vault))?;
		Ok((pending, interest_to_mint))
	}

	/// Read the whole branch record, returning `BranchNotFound` when missing.
	pub(crate) fn branch_of(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<BranchOf<T>, DispatchError> {
		Branches::<T>::get(collateral_id, stable_id)
			.ok_or_else(|| Error::<T>::BranchNotFound.into())
	}

	/// Replace the stored branch and update derived aggregates from its
	/// authoritative stored preimage.
	#[require_transactional]
	pub(crate) fn commit_branch(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		now: Millis,
		state: BranchState<T::AccountId, BalanceOf<T>>,
	) -> DispatchResult {
		Branches::<T>::try_mutate_exists(collateral_id, stable_id, move |stored| {
			let before = Self::branch_contribution(
				&stored.as_ref().ok_or(Error::<T>::BranchNotFound)?.state,
				now,
			)?;
			Self::update_branch_aggregates(stable_id, now, &before, &state)?;
			stored.as_mut().ok_or(Error::<T>::BranchNotFound)?.state = state;
			Ok(())
		})
	}

	/// Mutate one branch's runtime state through its FRAME storage entry while
	/// keeping every derived debt aggregate in step.
	pub(crate) fn try_mutate_branch_state<R>(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		mutate: impl FnOnce(
			&BranchConfig<BalanceOf<T>>,
			&mut BranchState<T::AccountId, BalanceOf<T>>,
			Millis,
		) -> Result<R, DispatchError>,
	) -> Result<R, DispatchError> {
		let now = T::TimeProvider::now();
		Branches::<T>::try_mutate_exists(collateral_id, stable_id, |maybe| {
			let branch = maybe.as_mut().ok_or(Error::<T>::BranchNotFound)?;
			let before = Self::branch_contribution(&branch.state, now)?;
			let result = mutate(&branch.config, &mut branch.state, now)?;
			Self::update_branch_aggregates(stable_id, now, &before, &branch.state)?;
			Ok(result)
		})
	}

	fn update_branch_aggregates(
		stable_id: &StableIdOf<T>,
		now: Millis,
		before: &BranchContribution<BalanceOf<T>>,
		after_state: &BranchState<T::AccountId, BalanceOf<T>>,
	) -> DispatchResult {
		let after = Self::branch_contribution(after_state, now)?;
		let stablecoin_debt = Self::updated_stablecoin_debt(stable_id, before, &after, now)?;
		if stablecoin_debt.is_empty() {
			StablecoinDebt::<T>::remove(stable_id);
		} else {
			StablecoinDebt::<T>::insert(stable_id, stablecoin_debt);
		}
		Ok(())
	}

	/// Advance the stablecoin-wide debt projection to `now`, then replace one
	/// market's realized debt, pending interest, and active weight.
	fn updated_stablecoin_debt(
		stable_id: &StableIdOf<T>,
		before: &BranchContribution<BalanceOf<T>>,
		after: &BranchContribution<BalanceOf<T>>,
		now: Millis,
	) -> Result<StablecoinDebtState<BalanceOf<T>>, DispatchError> {
		let mut total = StablecoinDebt::<T>::get(stable_id);
		let elapsed = now.saturating_sub(total.last_update);
		let elapsed_interest =
			PendingInterest::from_interest_weight(total.active_weighted_principal, elapsed)
				.ok_or(Error::<T>::ArithmeticOverflow)?;
		total.pending_interest = total
			.pending_interest
			.checked_add(&elapsed_interest)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		total.last_update = now;

		total.pending_interest = total
			.pending_interest
			.checked_sub(&before.pending_interest)
			.defensive_ok_or(DispatchError::Corruption)?
			.checked_add(&after.pending_interest)
			.ok_or(Error::<T>::ArithmeticOverflow)?;

		total.active_weighted_principal = total
			.active_weighted_principal
			.shifted(&before.active_weight, &after.active_weight)
			.ok_or(DispatchError::Corruption)?;
		total.outstanding =
			Self::shifted_total(total.outstanding, before.outstanding, after.outstanding)?;
		Ok(total)
	}

	/// Fully accrued stablecoin debt if one market were replaced by `after_state`.
	///
	/// This is the exact state [`Self::commit_branch`] would derive, including sibling-market
	/// interest and the current market's own rounding, without writing it.
	pub(crate) fn projected_stablecoin_debt(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		after_state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> Result<BalanceOf<T>, DispatchError> {
		let before_state = Self::branch_of(collateral_id, stable_id)?.state;
		let before = Self::branch_contribution(&before_state, now)?;
		let after = Self::branch_contribution(after_state, now)?;
		let projected = Self::updated_stablecoin_debt(stable_id, &before, &after, now)?;
		let pending = projected.pending_interest.ceil().ok_or(Error::<T>::ArithmeticOverflow)?;
		projected
			.outstanding
			.checked_add(&pending)
			.ok_or_else(|| Error::<T>::ArithmeticOverflow.into())
	}

	/// Derive the complete aggregate contribution of one branch at `now`.
	fn branch_contribution(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> Result<BranchContribution<BalanceOf<T>>, DispatchError> {
		Ok(BranchContribution {
			outstanding: state.debt.outstanding(),
			pending_interest: Self::branch_pending_interest(state, now)?,
			active_weight: if state.is_frozen() {
				InterestWeight::zero()
			} else {
				state.debt.weighted_principal
			},
		})
	}

	/// Returns the market's exact pending-interest contribution in split form.
	pub(crate) fn branch_pending_interest(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> Result<PendingInterest<BalanceOf<T>>, DispatchError> {
		let tau = state.interest_time(now);
		let elapsed = tau.saturating_sub(state.debt.last_interest_time);
		Self::attributed_window_with_carry(state, elapsed)
	}

	/// Returns the attributed interest window with its carried subunit residue.
	fn attributed_window_with_carry(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		elapsed: Millis,
	) -> Result<PendingInterest<BalanceOf<T>>, DispatchError> {
		let carry = PendingInterest::from_remainder(state.debt.aggregate_interest_remainder);
		if elapsed == 0 {
			return Ok(carry);
		}
		PendingInterest::from_interest_weight(state.debt.weighted_principal, elapsed)
			.and_then(|window| window.checked_add(&carry))
			.ok_or_else(|| Error::<T>::ArithmeticOverflow.into())
	}

	/// Fully accrued debt across every market issuing `stable_id`.
	///
	/// NOTE: This projection rounds pending interest once after aggregating
	/// the exact branch numerators. Across `N` interest-bearing branches, it can
	/// therefore be up to `N - 1` base units below the sum obtained by rounding
	/// every branch separately. Computing that literal sum requires walking or
	/// hard-bounding the stablecoin's sibling branches.
	pub(crate) fn accrued_stablecoin_debt(stable_id: &StableIdOf<T>) -> BalanceOf<T> {
		let debt = StablecoinDebt::<T>::get(stable_id);
		let elapsed = T::TimeProvider::now().saturating_sub(debt.last_update);
		let Some(elapsed_interest) =
			PendingInterest::from_interest_weight(debt.active_weighted_principal, elapsed)
		else {
			return BalanceOf::<T>::max_value();
		};
		let Some(pending) = debt.pending_interest.checked_add(&elapsed_interest) else {
			return BalanceOf::<T>::max_value();
		};
		let Some(accrued) = pending.ceil() else {
			return BalanceOf::<T>::max_value();
		};
		debt.outstanding.saturating_add(accrued)
	}

	/// Move an aggregate from `before` to `after`. Underflow means the aggregate
	/// had drifted from the markets it sums, so it is corruption rather than a
	/// user-reachable error.
	fn shifted_total(
		total: BalanceOf<T>,
		before: BalanceOf<T>,
		after: BalanceOf<T>,
	) -> Result<BalanceOf<T>, DispatchError> {
		total
			.checked_sub(&before)
			.defensive_ok_or(DispatchError::Corruption)?
			.checked_add(&after)
			.ok_or_else(|| Error::<T>::ArithmeticOverflow.into())
	}

	/// Returns a vault, or `VaultNotFound` if it does not exist.
	pub fn vault_of(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<Vault<BalanceOf<T>>, DispatchError> {
		Self::record_of(collateral_id, stable_id, owner).map(|record| record.vault)
	}

	/// Returns a vault record, or `VaultNotFound` if it does not exist.
	pub(crate) fn record_of(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<VaultRecordOf<T>, DispatchError> {
		Vaults::<T>::get((collateral_id, stable_id, owner))
			.ok_or_else(|| Error::<T>::VaultNotFound.into())
	}

	/// Returns the per-vault storage footprint, quoted in the collateral asset.
	///
	/// It includes the `Vaults` key, the record, the rate-list node, and the node key. The ticket
	/// uses the collateral ID's encoded size. This
	/// avoids the large maximum length of location-based IDs. The data is one blob because
	/// [`LinearStoragePrice`](frame::traits::LinearStoragePrice) multiplies the count by the size.
	///
	/// It excludes shared rate-list metadata, which the branch deposit covers. FRAME charges no
	/// deposit for account hold data.
	pub fn vault_footprint(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
	) -> AssetFootprint<CollateralIdOf<T>> {
		let key = Vaults::<T>::hashed_key_for((collateral_id, stable_id, owner)).len();
		let ticket = collateral_id.encoded_size().saturating_add(BalanceOf::<T>::max_encoded_len());
		let row = Vault::<BalanceOf<T>>::max_encoded_len().saturating_add(ticket);
		let rate_list = VaultListId::Rate(collateral_id.clone(), stable_id.clone());
		let node = T::VaultLists::node_footprint(&rate_list, owner);
		let local = Footprint::from_parts(1, key.saturating_add(row));
		let size = local.size.saturating_add(node.size);
		AssetFootprint::new(collateral_id.clone(), Footprint { count: 1, size })
	}

	/// Ensure a vault's collateralization ratio is at or above the branch ICR.
	/// Used by the open/borrow/withdraw safety gates. A `None` ratio (zero debt)
	/// and a below-ICR ratio both surface as `UnsafeCollateralizationRatio`.
	pub(crate) fn ensure_above_icr(
		position: &DebtCollateral<BalanceOf<T>>,
		price: FixedU128,
		config: &BranchConfig<BalanceOf<T>>,
	) -> DispatchResult {
		let cr = collateralization_ratio(position, price)
			.ok_or(Error::<T>::UnsafeCollateralizationRatio)?;
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
		position: &DebtCollateral<BalanceOf<T>>,
		price: FixedU128,
		config: &BranchConfig<BalanceOf<T>>,
	) -> DispatchResult {
		let cr = collateralization_ratio(position, price)
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
		position: &DebtCollateral<BalanceOf<T>>,
		price: FixedU128,
		config: &BranchConfig<BalanceOf<T>>,
	) -> DispatchResult {
		let cr = collateralization_ratio(position, price)
			.ok_or(Error::<T>::CollateralizationRatioTooLow)?;
		ensure!(
			cr >= config.minimum_collateralization_ratio,
			Error::<T>::CollateralizationRatioTooLow
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
		Self::mode_of(&branch.state, &branch.config, collateral_id, T::TimeProvider::now())
	}

	/// Derive a branch's current mode from its runtime state and risk config.
	pub(crate) fn mode_of(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		config: &BranchConfig<BalanceOf<T>>,
		collateral_id: &CollateralIdOf<T>,
		now: Millis,
	) -> Result<BranchMode, DispatchError> {
		if state.is_frozen() {
			return Ok(BranchMode::Frozen);
		}
		// A failing oracle is what `do_refresh_branch` would persist as
		// `Frozen { OracleFailure }`; report `Frozen` to observers even before
		// that poke lands, rather than defaulting to the most permissive mode
		// while prices are unknowable.
		let Ok(price) = T::Oracle::provide_price(collateral_id) else {
			return Ok(BranchMode::Frozen);
		};
		let tcr = Self::compute_tcr(state, price, now)?;
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
	) -> DispatchResult {
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
	) -> DispatchResult {
		if state.is_frozen() {
			return Err(Error::<T>::BranchFrozen.into());
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

	/// Authorizes [`Config::ForceOrigin`] as a full administrator.
	///
	/// This authority lets governance recover a market when its administrators are unavailable.
	pub(crate) fn ensure_force_or_branch_admin(
		origin: OriginFor<T>,
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		required: AdminLevel,
	) -> Result<AdminLevel, DispatchError> {
		let Err(origin) = T::ForceOrigin::try_origin(origin) else { return Ok(AdminLevel::Full) };
		let who = ensure_signed(origin)?;
		Self::ensure_branch_admin(&who, collateral_id, stable_id, required)
	}

	/// Authorize a per-market admin account, returning its [`AdminLevel`].
	/// `full_admin` satisfies any `required`; `emergency_admin` satisfies only
	/// `Emergency`.
	pub(crate) fn ensure_branch_admin(
		who: &T::AccountId,
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		required: AdminLevel,
	) -> Result<AdminLevel, DispatchError> {
		let admins = Self::branch_of(collateral_id, stable_id)?.admins;
		if who == &admins.full_admin {
			return Ok(AdminLevel::Full);
		}
		if matches!(required, AdminLevel::Emergency) && who == &admins.emergency_admin {
			return Ok(AdminLevel::Emergency);
		}
		Err(Error::<T>::NotBranchAdmin.into())
	}

	/// Fully-accrued total branch debt (the TCR numerator): principal + minted
	/// interest plus the rounded-up pending numerator.
	pub(crate) fn accrued_branch_debt(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> BalanceOf<T> {
		let pending_aggregate = Self::branch_pending_interest(state, now)
			.ok()
			.and_then(|pending| pending.ceil())
			.unwrap_or_else(BalanceOf::<T>::max_value);
		state.debt.outstanding().saturating_add(pending_aggregate)
	}

	/// Compute TCR including aggregate interest accrued since the last update.
	pub(crate) fn compute_tcr(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		price: FixedU128,
		now: Millis,
	) -> Result<FixedU128, DispatchError> {
		let inputs = DebtCollateral {
			collateral: state.total_collateral,
			debt: Self::accrued_branch_debt(state, now),
		};
		Self::tcr_from_inputs(&inputs, price)
	}

	/// The single TCR formula, shared by [`Self::compute_tcr`] (live state) and
	/// the operation gate's load-time baseline so the pre and post sides of a
	/// gate cannot diverge.
	pub(crate) fn tcr_from_inputs(
		inputs: &DebtCollateral<BalanceOf<T>>,
		price: FixedU128,
	) -> Result<FixedU128, DispatchError> {
		if inputs.debt.is_zero() {
			// Branch with no debt is treated as "infinitely well-collateralized".
			return Ok(FixedU128::max_value());
		}
		collateralization_ratio(inputs, price).ok_or_else(|| Error::<T>::ArithmeticOverflow.into())
	}

	/// Accrue aggregate branch interest in memory and return the new amount.
	///
	/// Returns an error without advancing the aggregate when the realized
	/// interest does not fit.
	pub(crate) fn accrue_aggregate_interest(
		state: &mut BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> Result<BalanceOf<T>, DispatchError> {
		let tau = state.interest_time(now);
		let elapsed = tau.saturating_sub(state.debt.last_interest_time);
		if elapsed == 0 {
			return Ok(BalanceOf::<T>::zero());
		}
		let total = Self::attributed_window_with_carry(state, elapsed)?;
		let new_interest = total.interest;
		let minted_interest = state
			.debt
			.minted_interest
			.checked_add(&new_interest)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		state.debt.minted_interest = minted_interest;
		state.debt.pending_interest_attribution = state
			.debt
			.pending_interest_attribution
			.checked_add(&new_interest)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		state.debt.aggregate_interest_remainder = total.remainder;
		state.debt.last_interest_time = tau;
		Ok(new_interest)
	}

	/// Issues market yield and routes the remainder from `T::YieldHook` to `T::FeeAccount`.
	pub(crate) fn mint_and_route_yield(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		let credit = T::StableAssets::issue(stable_id.clone(), amount);
		let credit = T::YieldHook::distribute_yield(collateral_id, credit);
		Self::resolve_fee_credit(stable_id, credit)
	}

	/// Issues uncovered terminal rounding revenue to the fee account.
	pub(crate) fn mint_rounding_fee(
		stable_id: &StableIdOf<T>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		Self::resolve_fee_credit(stable_id, T::StableAssets::issue(stable_id.clone(), amount))
	}

	/// Makes the fee account able to receive a one-unit credit for `stable_id`.
	///
	/// The fee account pays its deposit because it can outlive one market.
	pub(crate) fn ensure_fee_account_receivable(stable_id: &StableIdOf<T>) -> DispatchResult {
		let fee_account = T::FeeAccount::convert(stable_id.clone());
		let can_receive_unit = || {
			<T::StableAssets as FungiblesInspect<T::AccountId>>::can_deposit(
				stable_id.clone(),
				&fee_account,
				BalanceOf::<T>::one(),
				Provenance::Extant,
			)
			.into_result()
			.is_ok()
		};
		if can_receive_unit() {
			return Ok(());
		}
		<T::StableAssets as AccountTouch<_, _>>::touch(
			stable_id.clone(),
			&fee_account,
			&fee_account,
		)?;
		ensure!(can_receive_unit(), Error::<T>::FeeAccountNotReceivable);
		Ok(())
	}

	/// Returns the account that pays and is refunded a market's custody seed.
	///
	/// A market that charged a creation deposit charges the same account. A privileged creation
	/// has no depositor, so the market's own full administrator funds it. Removal recomputes this
	/// from the stored record: rotating that admin of a privileged market therefore refunds the
	/// new one.
	pub(crate) fn custody_funder(
		depositor: Option<&T::AccountId>,
		admins: &crate::types::BranchAdmins<T::AccountId>,
	) -> T::AccountId {
		depositor.unwrap_or(&admins.full_admin).clone()
	}

	/// Parks one minimum balance of collateral in the market's redistribution account.
	///
	/// A hold must leave the asset's minimum balance free, so custody carries that float for as
	/// long as the market exists. The provider reference alone does not cover this: `pallet-assets`
	/// keeps the minimum balance untouchable whatever the account's existence reason, while
	/// `pallet-balances` waives it for an account another provider keeps alive. Seeding both makes
	/// every later seizure hold exactly what it deposited.
	pub(crate) fn seed_redistribution_custody(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		funder: &T::AccountId,
	) -> DispatchResult {
		let seed = T::CollateralAssets::minimum_balance(collateral_id.clone());
		if seed.is_zero() {
			return Ok(());
		}
		let custody = Pallet::<T>::redistribution_account(collateral_id, stable_id);
		// A registration fee must not dust the account that pays it.
		let credit = <T::CollateralAssets as FungiblesBalanced<T::AccountId>>::withdraw(
			collateral_id.clone(),
			funder,
			seed,
			Precision::Exact,
			Preservation::Preserve,
			Fortitude::Polite,
		)
		.map_err(|_| Error::<T>::CustodySeedUnavailable)?;
		T::CollateralAssets::resolve(&custody, credit).map_err(|credit| {
			drop(credit);
			Error::<T>::CustodySeedUnavailable
		})?;
		Ok(())
	}

	/// Returns the custody float to the account that funded it.
	///
	/// Removal happens on an empty market, so nothing is on hold and the whole free balance is
	/// swept: the seed, plus anything donated to the address, which the funder keeps. Emptying the
	/// account also lets it die, releasing the consumer references that would otherwise block the
	/// market's provider reference.
	pub(crate) fn refund_redistribution_custody(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		funder: &T::AccountId,
	) -> DispatchResult {
		let custody = Pallet::<T>::redistribution_account(collateral_id, stable_id);
		defensive_assert!(
			T::CollateralAssets::balance_on_hold(
				collateral_id.clone(),
				&crate::pallet::HoldReason::VaultCollateral.into(),
				&custody,
			)
			.is_zero(),
			"custody still holds collateral at market removal"
		);
		let seed = <T::CollateralAssets as FungiblesInspect<T::AccountId>>::reducible_balance(
			collateral_id.clone(),
			&custody,
			Preservation::Expendable,
			Fortitude::Polite,
		);
		if seed.is_zero() {
			return Ok(());
		}
		let credit = <T::CollateralAssets as FungiblesBalanced<T::AccountId>>::withdraw(
			collateral_id.clone(),
			&custody,
			seed,
			Precision::Exact,
			Preservation::Expendable,
			Fortitude::Polite,
		)?;
		T::CollateralAssets::resolve(funder, credit).map_err(|credit| {
			drop(credit);
			Error::<T>::CollateralPayoutFailed
		})?;
		Ok(())
	}

	/// Resolves a fee credit to the configured fee account.
	///
	/// A failure aborts the transaction because the credit backs a recorded liability.
	fn resolve_fee_credit(stable_id: &StableIdOf<T>, credit: StableCreditOf<T>) -> DispatchResult {
		if credit.peek().is_zero() {
			return Ok(());
		}
		let fee_account = T::FeeAccount::convert(stable_id.clone());
		T::StableAssets::resolve(&fee_account, credit).map_err(|credit| {
			drop(credit);
			Error::<T>::FeeResolutionFailed
		})?;
		Ok(())
	}

	/// Project a vault touch without mutating storage.
	pub(crate) fn pending_touch_for(
		vault: &Vault<BalanceOf<T>>,
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> Result<PendingTouch<BalanceOf<T>>, ArithmeticError> {
		let tau = state.interest_time(now);
		let elapsed = tau.saturating_sub(vault.last_interest_time);
		let carry = PendingInterest::from_remainder(vault.interest_remainder);
		let principal_interest = PendingInterest::from_principal_rate_millis(
			vault.debt.principal,
			vault.annual_rate,
			elapsed,
		)
		.and_then(|pending| pending.checked_add(&carry))
		.ok_or(ArithmeticError::Overflow)?;

		let redistribution = state.redistribution;
		let snap = vault.redistribution_checkpoint;
		let only_recipient = state.stakes.total == vault.redistribution_stake;
		let pending_complement = !state.debt.pending_redistribution_principal.is_zero() ||
			!state.pending_redistribution_collateral.is_zero() ||
			!state.debt.pending_redistribution_weight.is_zero();
		if (redistribution == snap && !(only_recipient && pending_complement)) ||
			vault.redistribution_stake.is_zero()
		{
			return Ok(PendingTouch {
				redistribution: DebtCollateral {
					debt: BalanceOf::<T>::zero(),
					collateral: BalanceOf::<T>::zero(),
				},
				attribution: RedistributionAttribution::zero(),
				interest: principal_interest,
			});
		}

		let delta_principal =
			redistribution.principal_per_stake.saturating_sub(snap.principal_per_stake);
		let delta_collateral =
			redistribution.collateral_per_stake.saturating_sub(snap.collateral_per_stake);
		let delta_weight = redistribution
			.weight_per_weighted_stake
			.saturating_sub(snap.weight_per_weighted_stake);
		let delta_weight_time = redistribution
			.weight_time_per_weighted_stake
			.checked_sub(snap.weight_time_per_weighted_stake)
			.ok_or(ArithmeticError::Underflow)?;
		let principal = if only_recipient {
			state.debt.pending_redistribution_principal
		} else {
			delta_principal
				.saturating_mul_int(vault.redistribution_stake)
				.min(state.debt.pending_redistribution_principal)
		};
		let collateral = if only_recipient {
			state.pending_redistribution_collateral
		} else {
			delta_collateral
				.saturating_mul_int(vault.redistribution_stake)
				.min(state.pending_redistribution_collateral)
		};
		let weighted_stake =
			InterestWeight::from_principal_rate(vault.redistribution_stake, vault.annual_rate);
		let attribution = if only_recipient {
			RedistributionAttribution::claim(
				state.debt.pending_redistribution_weight,
				state.pending_redistribution_weight_time,
				state.debt.pending_redistribution_weight,
				state.pending_redistribution_weight_time,
				tau,
			)?
		} else if let Some(weighted_stake) = weighted_stake {
			let candidate = weighted_stake
				.apply_redistribution_ratio(delta_weight)
				.ok_or(ArithmeticError::Overflow)?;
			let desired_weight_time = weighted_stake
				.apply_weight_time_ratio(delta_weight_time)
				.ok_or(ArithmeticError::Overflow)?;
			RedistributionAttribution::claim(
				candidate,
				desired_weight_time,
				state.debt.pending_redistribution_weight,
				state.pending_redistribution_weight_time,
				tau,
			)?
		} else {
			RedistributionAttribution::zero()
		};
		let pending_redistribution_interest = attribution.pending_interest(tau)?;
		let redistribution_interest = principal_interest
			.checked_add(&pending_redistribution_interest)
			.ok_or(ArithmeticError::Overflow)?;

		Ok(PendingTouch {
			redistribution: DebtCollateral { debt: principal, collateral },
			attribution,
			interest: redistribution_interest,
		})
	}

	/// Projects a vault's fully accrued debt from branch state accrued to `now`.
	///
	/// Returns `None` when the vault is missing. Each projection is independent of iteration order.
	pub(crate) fn projected_vault_debt(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
		accrued_state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> Option<BalanceOf<T>> {
		let mut state = accrued_state.clone();
		let mut vault = Self::vault_of(collateral_id, stable_id, owner).ok()?;
		let status = Self::vault_status_of(collateral_id, stable_id, owner);
		Self::apply_vault_touch(&mut state, &mut vault, status, now).ok()?;
		Some(vault.debt.total())
	}

	/// A zero-debt, zero-stake vault row: the pre-borrow shape an open feeds
	/// to [`Self::apply_borrow_unchecked`], so the open fee is priced by the same
	/// code path as every borrow. The stake MUST be zero here — the borrow update
	/// swaps the row's full aggregate contribution, and the open's stake
	/// enters the aggregates when the operation synchronizes the stake after
	/// the borrow is applied.
	pub(crate) fn open_scratch_row(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		annual_rate: FixedU128,
		collateral: BalanceOf<T>,
		now: Millis,
	) -> Vault<BalanceOf<T>> {
		Vault {
			collateral,
			debt: DebtBreakdown { principal: Zero::zero(), interest: Zero::zero() },
			annual_rate,
			last_interest_time: state.interest_time(now),
			interest_remainder: 0,
			last_rate_update: now,
			redistribution_stake: Zero::zero(),
			redistribution_checkpoint: state.redistribution,
		}
	}

	fn avg_rate(state: &BranchState<T::AccountId, BalanceOf<T>>) -> FixedU128 {
		math::average_branch_rate(
			state.debt.weighted_principal.whole,
			state.debt.principal.saturating_add(state.debt.pending_redistribution_principal),
		)
	}

	/// Apply a borrow to a branch/vault draft pair and return the upfront fee.
	///
	/// A borrow that also changes the rate inside the cooldown charges the
	/// upfront fee over both the debt increase and the existing principal. A
	/// zero increase is a pure rate change, priced on the same rule.
	pub(crate) fn apply_borrow_unchecked(
		state: &mut BranchState<T::AccountId, BalanceOf<T>>,
		config: &BranchConfig<BalanceOf<T>>,
		vault: &mut Vault<BalanceOf<T>>,
		debt_increase: BalanceOf<T>,
		new_rate: FixedU128,
		now: Millis,
	) -> Result<BalanceOf<T>, DispatchError> {
		let old_rate = vault.annual_rate;
		let rate_changed = new_rate != old_rate;
		let rate_change_fee_base = if rate_changed && !vault.cooldown_elapsed(config, now) {
			vault.debt.principal
		} else {
			BalanceOf::<T>::zero()
		};
		let before = vault.clone();
		vault.debt.principal = vault
			.debt
			.principal
			.checked_add(&debt_increase)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		vault.annual_rate = new_rate;
		if rate_changed {
			vault.last_rate_update = now;
		}
		state.replace_vault(Some(&before), Some(vault))?;
		let avg = Self::avg_rate(state);
		let fee = math::simple_interest_ceil(
			debt_increase.saturating_add(rate_change_fee_base),
			avg,
			config.upfront_fee_period,
		);
		if !fee.is_zero() {
			let before_fee = vault.clone();
			vault.debt.interest =
				vault.debt.interest.checked_add(&fee).ok_or(Error::<T>::ArithmeticOverflow)?;
			state.replace_vault(Some(&before_fee), Some(vault))?;
		}
		Ok(fee)
	}

	/// The single target that preempts the rate index, with its status: the
	/// `FinalRecovery` FIFO head, else the parked dormant redemption target.
	pub(crate) fn priority_redemption_target(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Option<(T::AccountId, VaultStatus)> {
		recovery::next_target::<T>(collateral_id, stable_id)
			.map(|owner| (owner, VaultStatus::FinalRecovery))
			.or_else(|| {
				Branches::<T>::get(collateral_id, stable_id)
					.and_then(|branch| branch.state.dormant_redemption_target)
					.map(|owner| (owner, VaultStatus::Dormant))
			})
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

	/// The lowest-rate active vault, or the active vault immediately after
	/// `after`, without consulting the priority tiers.
	pub(crate) fn ordinary_redemption_target(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		after: Option<&T::AccountId>,
	) -> Option<T::AccountId> {
		match after {
			Some(owner) => Self::ordinary_target_after(collateral_id, stable_id, owner),
			None => {
				T::VaultLists::tail(&VaultListId::Rate(collateral_id.clone(), stable_id.clone()))
			},
		}
	}

	/// Returns branch configuration and accrued state for a read-only projection.
	///
	/// The projection does not issue aggregate interest.
	pub(crate) fn accrued_branch_view(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<
		(BranchConfig<BalanceOf<T>>, BranchState<T::AccountId, BalanceOf<T>>, Millis),
		DispatchError,
	> {
		let now = T::TimeProvider::now();
		let branch = Self::branch_of(collateral_id, stable_id)?;
		let mut state = branch.state;
		Self::accrue_aggregate_interest(&mut state, now)?;
		Ok((branch.config, state, now))
	}

	/// Read and fully touch one vault into an isolated branch draft.
	///
	/// Views use the same transition kernel as execution, but the returned
	/// drafts are never persisted and no collateral hold is moved.
	pub(crate) fn touched_vault_draft(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<TouchedVaultDraft<T::AccountId, BalanceOf<T>>, DispatchError> {
		let (config, mut state, now) = Self::accrued_branch_view(collateral_id, stable_id)?;
		let mut vault = Self::vault_of(collateral_id, stable_id, owner)?;
		let status = Self::vault_status_of(collateral_id, stable_id, owner);
		Self::apply_vault_touch(&mut state, &mut vault, status, now)?;
		Ok(TouchedVaultDraft { config, state, vault, status, now })
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
		let _ = with_storage_layer(|| Self::do_refresh_branch(collateral_id, stable_id));
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
				Self::idle_vault_step(collateral_id, stable_id, owner);
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
		let _ = with_storage_layer(|| {
			VaultOp::<T>::refresh(collateral_id.clone(), stable_id.clone(), owner)
		});
	}

	/// Drive `step` over `iter`, charging `per_step` from `meter` for every
	/// attempted step — failures included, since a failed attempt costs the
	/// same execution. The terminal probe (one uncharged `iter.next()` after
	/// the meter runs dry) only distinguishes [`WalkExit::Drained`] from
	/// [`WalkExit::Parked`]; the probed key is re-read as the next pass's
	/// first charged step.
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
