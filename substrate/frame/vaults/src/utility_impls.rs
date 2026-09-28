//! Internal (non-dispatchable) `Pallet` helpers: storage accessors,
//! interest/fee accounting, and branch modes.

use crate::{
	math,
	pallet::{
		BalanceOf, BranchOf, Branches, CollateralIdOf, Config, Error, Event, Millis, Pallet,
		StableCreditOf, StableIdOf, StablecoinDebt, VaultRecordOf, Vaults,
	},
	recovery,
	types::{
		AdminLevel, BranchConfig, BranchMode, BranchState, DebtBreakdown, DebtCollateral,
		StablecoinDebtState, Vault, VaultListId, VaultStatus, Wide,
	},
};
use frame::{
	arithmetic::{ArithmeticError, Rounding},
	deps::sp_core::U256,
	prelude::*,
	traits::{
		fungibles::{Balanced as FungiblesBalanced, Inspect as FungiblesInspect, InspectHold as _},
		tokens::{Fortitude, Precision, Preservation, Provenance},
		AccountTouch, AssetFootprint, DefensiveOption, Footprint, Time,
	},
};
use linked_list_interface::{ListError, SortedListInterface};
use pusd_primitives::{
	collateralization_ratio, BranchSnapshot, CollateralRatio, OnBranchYield, ProvidePrice,
};

/// Changes the next vault touch would apply.
pub(crate) struct PendingTouch<Balance> {
	/// Principal and collateral moved from the redistribution pools into the vault.
	pub redistribution: DebtCollateral<Balance>,
	/// Accrual rate moved from the pending pool into the vault's principal.
	pub accrual_rate: U256,
	/// Interest folded into the vault, rounded up.
	pub interest: Balance,
	pub interest_prepaid: u128,
}

/// One fully touched vault and its isolated branch draft, accrued to `now`.
pub(crate) struct TouchedVaultDraft<AccountId, Balance> {
	pub(crate) config: BranchConfig<Balance>,
	pub(crate) state: BranchState<AccountId, Balance>,
	pub(crate) vault: Vault<Balance>,
	pub(crate) status: VaultStatus,
	pub(crate) now: Millis,
}

/// Stablecoin one commit issues, drawn first from stablecoin the same operation burns.
///
/// Issuing out of a burned credit nets the two supply changes into one; whatever is left of the
/// credit burns when this drops.
pub(crate) struct Issuance<T: Config> {
	stable_id: StableIdOf<T>,
	burned: Option<StableCreditOf<T>>,
}

impl<T: Config> Issuance<T> {
	/// Issues only freshly minted stablecoin.
	pub(crate) const fn minted(stable_id: StableIdOf<T>) -> Self {
		Self { stable_id, burned: None }
	}

	/// Issues out of `burned` first, then mints the shortfall.
	pub(crate) fn netted(stable_id: StableIdOf<T>, burned: Option<StableCreditOf<T>>) -> Self {
		if let Some(burned) = burned.as_ref() {
			debug_assert!(burned.asset() == stable_id, "burned credit of another stablecoin");
		}
		Self { stable_id, burned }
	}

	fn issue(&mut self, amount: BalanceOf<T>) -> Result<StableCreditOf<T>, DispatchError> {
		let Some(burned) = self.burned.take() else {
			return Ok(T::StableAssets::issue(self.stable_id.clone(), amount));
		};
		let (mut taken, rest) = burned.split(amount);
		self.burned = Some(rest);
		let shortfall = amount.saturating_sub(taken.peek());
		if !shortfall.is_zero() {
			taken
				.subsume(T::StableAssets::issue(self.stable_id.clone(), shortfall))
				.map_err(|minted| {
					// Both credits are of `stable_id`, so the merge cannot fail.
					drop(minted);
					DispatchError::Corruption
				})?;
		}
		debug_assert!(taken.peek() == amount);
		Ok(taken)
	}
}

/// The part of one branch that contributes to derived debt aggregates.
pub(crate) struct BranchContribution<Balance> {
	outstanding: Balance,
	pending_interest: U256,
	active_accrual_rate: U256,
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
			.attribute_interest(pending.interest)
			.ok_or(Error::<T>::ArithmeticOverflow)?;

		if !pending.interest.is_zero() {
			vault.debt.interest = vault
				.debt
				.interest
				.checked_add(&pending.interest)
				.ok_or(Error::<T>::ArithmeticOverflow)?;
		}
		vault.interest_prepaid = pending.interest_prepaid;
		let accounted_before = vault.clone();
		if !pending.redistribution.debt.is_zero() ||
			!pending.redistribution.collateral.is_zero() ||
			!pending.accrual_rate.is_zero()
		{
			state.consume_redistribution(pending.redistribution, pending.accrual_rate)?;
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

	/// Replace the stored branch and update derived aggregates.
	///
	/// `before` is the contribution of the row as the caller loaded it. The caller holds the
	/// whole row, so storing it needs no second read; nothing else may write the row between
	/// that load and this call.
	pub(crate) fn commit_branch(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		now: Millis,
		before: &BranchContribution<BalanceOf<T>>,
		branch: BranchOf<T>,
	) -> DispatchResult {
		#[cfg(debug_assertions)]
		{
			let stored = Self::branch_of(collateral_id, stable_id)?;
			let stored = Self::branch_contribution(&stored.state, now)?;
			debug_assert!(stored.outstanding == before.outstanding, "branch row moved under an op");
			debug_assert!(stored.pending_interest == before.pending_interest);
			debug_assert!(stored.active_accrual_rate == before.active_accrual_rate);
		}
		Self::update_branch_aggregates(stable_id, now, before, &branch.state)?;
		Branches::<T>::insert(collateral_id, stable_id, branch);
		Ok(())
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
	/// market's realized debt, pending interest, and active accrual rate.
	fn updated_stablecoin_debt(
		stable_id: &StableIdOf<T>,
		before: &BranchContribution<BalanceOf<T>>,
		after: &BranchContribution<BalanceOf<T>>,
		now: Millis,
	) -> Result<StablecoinDebtState<BalanceOf<T>>, DispatchError> {
		let mut total = StablecoinDebt::<T>::get(stable_id);
		let elapsed = now.saturating_sub(total.last_update);
		let pending_interest = total
			.pending_interest
			.to_wide()
			.checked_add(
				math::interest_numerator(total.active_accrual_rate.to_wide(), elapsed)
					.ok_or(Error::<T>::ArithmeticOverflow)?,
			)
			.ok_or(Error::<T>::ArithmeticOverflow)?
			.checked_sub(before.pending_interest)
			.defensive_ok_or(DispatchError::Corruption)?
			.checked_add(after.pending_interest)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		total.pending_interest = Wide::from_wide(pending_interest);
		total.last_update = now;
		total.active_accrual_rate = total
			.active_accrual_rate
			.checked_sub(before.active_accrual_rate)
			.defensive_ok_or(DispatchError::Corruption)?
			.checked_add(after.active_accrual_rate)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		total.outstanding =
			Self::shifted_total(total.outstanding, before.outstanding, after.outstanding)?;
		Ok(total)
	}

	/// Fully accrued stablecoin debt if one market were replaced by `after_state`.
	///
	/// This is the exact state [`Self::commit_branch`] would derive from `before`, the market's
	/// stored contribution, including sibling-market interest and the current market's own
	/// rounding, without writing it.
	pub(crate) fn projected_stablecoin_debt(
		stable_id: &StableIdOf<T>,
		before: &BranchContribution<BalanceOf<T>>,
		after_state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> Result<BalanceOf<T>, DispatchError> {
		let after = Self::branch_contribution(after_state, now)?;
		let projected = Self::updated_stablecoin_debt(stable_id, before, &after, now)?;
		let pending = math::interest_units_ceil(projected.pending_interest.to_wide())
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		projected
			.outstanding
			.checked_add(&pending)
			.ok_or_else(|| Error::<T>::ArithmeticOverflow.into())
	}

	/// Derive the complete aggregate contribution of one branch at `now`.
	pub(crate) fn branch_contribution(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> Result<BranchContribution<BalanceOf<T>>, DispatchError> {
		Ok(BranchContribution {
			outstanding: state.debt.outstanding(),
			pending_interest: Self::branch_pending_interest(state, now)?,
			active_accrual_rate: if state.is_frozen() {
				U256::zero()
			} else {
				state.debt.accrual_rate.to_wide()
			},
		})
	}

	/// Returns the market's unminted interest, over [`math::INTEREST_DENOMINATOR`].
	pub(crate) fn branch_pending_interest(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> Result<U256, DispatchError> {
		let tau = state.interest_time(now);
		let elapsed = tau.saturating_sub(state.debt.last_interest_time);
		math::interest_numerator(state.debt.accrual_rate.to_wide(), elapsed)
			.and_then(|window| {
				window.checked_add(U256::from(state.debt.aggregate_interest_remainder))
			})
			.ok_or_else(|| Error::<T>::ArithmeticOverflow.into())
	}

	/// Fully accrued debt across every market issuing `stable_id`.
	///
	/// The projection rounds the markets' combined unminted interest up once and does not net
	/// interest vaults were charged ahead, so it bounds the debt from above by a few units.
	pub(crate) fn accrued_stablecoin_debt(stable_id: &StableIdOf<T>) -> BalanceOf<T> {
		let debt = StablecoinDebt::<T>::get(stable_id);
		let elapsed = T::TimeProvider::now().saturating_sub(debt.last_update);
		math::interest_numerator(debt.active_accrual_rate.to_wide(), elapsed)
			.and_then(|window| debt.pending_interest.to_wide().checked_add(window))
			.and_then(math::interest_units_ceil)
			.map_or_else(BalanceOf::<T>::max_value, |accrued| {
				debt.outstanding.saturating_add(accrued)
			})
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

	/// Returns the vault's status, or `None` when the vault does not exist.
	///
	/// Not a view: [`Pallet::vault_after_touch`] reports the same status.
	pub fn vault_status(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: T::AccountId,
	) -> Option<VaultStatus> {
		Vaults::<T>::contains_key((&collateral_id, &stable_id, &owner))
			.then(|| Self::vault_status_of(&collateral_id, &stable_id, &owner))
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
		Self::mode_at_price(state, config, price, now)
	}

	/// Derive a branch's current mode from its runtime state and a price already in hand.
	///
	/// This is [`Pallet::mode_of`] without the oracle read, for callers that loaded the price
	/// earlier in the same operation.
	pub(crate) fn mode_at_price(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		config: &BranchConfig<BalanceOf<T>>,
		price: FixedU128,
		now: Millis,
	) -> Result<BranchMode, DispatchError> {
		if state.is_frozen() {
			return Ok(BranchMode::Frozen);
		}
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
	/// interest plus the rounded-up pending numerator, less the part of it vaults were
	/// already charged ahead.
	pub(crate) fn accrued_branch_debt(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> BalanceOf<T> {
		let pending_aggregate = Self::branch_pending_interest(state, now)
			.ok()
			.and_then(math::interest_units_ceil)
			.unwrap_or_else(BalanceOf::<T>::max_value)
			.saturating_sub(state.debt.interest_minted_ahead);
		state.debt.outstanding().saturating_add(pending_aggregate)
	}

	/// Compute TCR including aggregate interest accrued since the last update.
	///
	/// The operation gate's load-time baseline feeds the same
	/// `collateralization_ratio` directly, so the pre and post sides of a gate
	/// cannot diverge.
	pub(crate) fn compute_tcr(
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		price: FixedU128,
		now: Millis,
	) -> Result<CollateralRatio, DispatchError> {
		let inputs = DebtCollateral {
			collateral: state.total_collateral,
			debt: Self::accrued_branch_debt(state, now),
		};
		Ok(collateralization_ratio(&inputs, price)?)
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
		let (accrued, remainder) = math::split_interest(Self::branch_pending_interest(state, now)?)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		let new_interest =
			state.debt.accrue_aggregate(accrued).ok_or(Error::<T>::ArithmeticOverflow)?;
		state.debt.aggregate_interest_remainder = remainder;
		state.debt.last_interest_time = tau;
		Ok(new_interest)
	}

	/// Issues market yield and routes the remainder from `T::YieldHook` to `T::FeeAccount`.
	///
	/// `branch` is the market as stored at this point: the caller derives it from the state it
	/// just committed, so the hook does not load the branch again.
	pub(crate) fn mint_and_route_yield(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		branch: BranchSnapshot,
		amount: BalanceOf<T>,
		issuance: &mut Issuance<T>,
	) -> DispatchResult {
		let credit = issuance.issue(amount)?;
		let credit = T::YieldHook::distribute_yield(collateral_id, branch, credit);
		Self::resolve_fee_credit(stable_id, credit)
	}

	/// Issues aggregate interest as market yield and reports it.
	pub(crate) fn issue_interest(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		branch: BranchSnapshot,
		amount: BalanceOf<T>,
		issuance: &mut Issuance<T>,
	) -> DispatchResult {
		if amount.is_zero() {
			return Ok(());
		}
		Self::mint_and_route_yield(collateral_id, stable_id, branch, amount, issuance)?;
		Self::deposit_event(Event::InterestIssued {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			amount,
		});
		Ok(())
	}

	/// Persists the market's pending aggregate interest and issues it as yield.
	///
	/// A frozen market has no elapsed interest time, so it issues nothing.
	pub(crate) fn accrue_branch_interest(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> DispatchResult {
		let (minted, branch) =
			Self::try_mutate_branch_state(collateral_id, stable_id, |config, state, now| {
				let minted = Self::accrue_aggregate_interest(state, now)?;
				if minted.is_zero() {
					return Ok((minted, None));
				}
				// The yield route must not fail the accrual: a mode it cannot derive counts as
				// frozen, which sends the whole mint to the fee account.
				let mode =
					Self::mode_of(state, config, collateral_id, now).unwrap_or(BranchMode::Frozen);
				Ok((minted, Some(BranchSnapshot { mode, now })))
			})?;
		// Mint only after storing the updated market.
		if let Some(branch) = branch {
			let mut issuance = Issuance::minted(stable_id.clone());
			Self::issue_interest(collateral_id, stable_id, branch, minted, &mut issuance)?;
		}
		Ok(())
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
		let principal_interest = math::interest_numerator(
			math::accrual_rate(vault.debt.principal, vault.annual_rate),
			elapsed,
		)
		.ok_or(ArithmeticError::Overflow)?;
		let charge = |accrued: U256| {
			math::charge_interest(accrued, vault.interest_prepaid).ok_or(ArithmeticError::Overflow)
		};

		let redistribution = state.redistribution;
		let snap = vault.redistribution_checkpoint;
		let only_recipient = state.stakes.total == vault.redistribution_stake;
		let pending_complement = !state.debt.pending_redistribution_principal.is_zero() ||
			!state.pending_redistribution_collateral.is_zero() ||
			!state.debt.pending_redistribution_accrual_rate.is_zero();
		if (redistribution == snap && !(only_recipient && pending_complement)) ||
			vault.redistribution_stake.is_zero()
		{
			let (interest, interest_prepaid) = charge(principal_interest)?;
			return Ok(PendingTouch {
				redistribution: DebtCollateral {
					debt: BalanceOf::<T>::zero(),
					collateral: BalanceOf::<T>::zero(),
				},
				accrual_rate: U256::zero(),
				interest,
				interest_prepaid,
			});
		}

		let delta_principal =
			redistribution.principal_per_stake.saturating_sub(snap.principal_per_stake);
		let delta_collateral =
			redistribution.collateral_per_stake.saturating_sub(snap.collateral_per_stake);
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

		let pool_accrual_rate = state.debt.pending_redistribution_accrual_rate.to_wide();
		let stake_accrual_rate = math::accrual_rate(vault.redistribution_stake, vault.annual_rate);
		let accrual_rate = if only_recipient {
			pool_accrual_rate
		} else {
			math::claimable_accrual_rate(stake_accrual_rate, delta_principal)
				.ok_or(ArithmeticError::Overflow)?
				.min(pool_accrual_rate)
		};

		// `τ · ΔA − ΔB` is `Σ d_k · (τ − τ_k)` over the redistributions since the checkpoint.
		let principal_time = U256::from(delta_principal.into_inner())
			.checked_mul(U256::from(tau))
			.ok_or(ArithmeticError::Overflow)?
			.checked_sub(
				redistribution
					.principal_time_per_stake
					.to_wide()
					.checked_sub(snap.principal_time_per_stake.to_wide())
					.ok_or(ArithmeticError::Underflow)?,
			)
			.ok_or(ArithmeticError::Underflow)?;
		let (interest, interest_prepaid) =
			math::scale_wide(stake_accrual_rate, principal_time, Rounding::Up)
				.and_then(|share| share.checked_add(principal_interest))
				.ok_or(ArithmeticError::Overflow)
				.and_then(charge)?;

		Ok(PendingTouch {
			redistribution: DebtCollateral { debt: principal, collateral },
			accrual_rate,
			interest,
			interest_prepaid,
		})
	}

	/// Projects a vault's fully accrued debt from branch state accrued to `now`.
	///
	/// Each projection is independent of iteration order.
	pub(crate) fn projected_vault_debt(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
		accrued_state: &BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> Result<BalanceOf<T>, DispatchError> {
		let mut state = accrued_state.clone();
		let (vault, _) = Self::touched_row(collateral_id, stable_id, owner, &mut state, now)?;
		Ok(vault.debt.total())
	}

	/// Reads one vault row with its status and touches it against `state`, accrued to `now`.
	fn touched_row(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
		state: &mut BranchState<T::AccountId, BalanceOf<T>>,
		now: Millis,
	) -> Result<(Vault<BalanceOf<T>>, VaultStatus), DispatchError> {
		let mut vault = Self::vault_of(collateral_id, stable_id, owner)?;
		let status = Self::vault_status_of(collateral_id, stable_id, owner);
		Self::apply_vault_touch(state, &mut vault, status, now)?;
		Ok((vault, status))
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
			interest_prepaid: 0,
			last_rate_update: now,
			redistribution_stake: Zero::zero(),
			redistribution_checkpoint: state.redistribution,
		}
	}

	fn avg_rate(state: &BranchState<T::AccountId, BalanceOf<T>>) -> FixedU128 {
		let accrual_rate: BalanceOf<T> = state.debt.accrual_rate.whole();
		math::average_branch_rate(
			accrual_rate,
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
		let (vault, status) = Self::touched_row(collateral_id, stable_id, owner, &mut state, now)?;
		Ok(TouchedVaultDraft { config, state, vault, status, now })
	}
}
