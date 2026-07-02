//! Storage-touching helpers: vault lifecycle, branch mode, interest,
//! redistribution, fees, governance, on-idle.
//!
//! Most extrinsics in `lib.rs` are thin wrappers over these.

use crate::{
	math,
	pallet::{
		BalanceOf, BranchAdmin, BranchConfigs, BranchStates, Config, Error, Event,
		GlobalDebtCeiling, HoldReason, Millis, Pallet, PalletsOriginOf, Vaults,
	},
	recovery,
	types::{
		AdminLevel, BranchAdminInfo, BranchAdmins, BranchConfig, BranchDebt, BranchMode,
		BranchStakes, BranchState, DebtPayment, FrozenReason, FrozenState, InterestClock,
		RedistributionSnapshot, Vault, VaultDebt, VaultListId, VaultStatus,
	},
	weights::WeightInfo,
};
use frame::{
	deps::frame_support::{defensive_assert, storage::with_storage_layer},
	prelude::*,
	traits::{
		fungibles::{
			Balanced as FungiblesBalanced, Mutate as FungiblesMutate,
			MutateHold as FungiblesMutateHold,
		},
		tokens::Restriction,
		Consideration, Footprint, OriginTrait, Time,
	},
};
use pallet_linked_list::{ListError, Position, SortedListInterface};
use pusd_primitives::{OnBranchLifecycle, ProvidePrice};

/// Translate a rate-index insert/re-insert failure. A stale user-supplied
/// hint surfaces as [`Error::InvalidPositionHints`]; every other kind means
/// the index and the vault rows disagree.
pub(crate) fn map_error<T: Config>(e: ListError) -> Error<T> {
	match e {
		ListError::InvalidPositionHints => Error::<T>::InvalidPositionHints,
		ListError::ItemNotFound |
		ListError::ItemAlreadyExists |
		ListError::ListTooLong |
		ListError::CorruptList => Error::<T>::RateIndexInvariantBroken,
	}
}

/// Read the branch state, returning `UnknownCollateral` when missing.
pub(crate) fn branch_state_of<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> Result<BranchState<T::AccountId, BalanceOf<T>>, DispatchError> {
	BranchStates::<T>::get(collateral_id, stable_id)
		.ok_or_else(|| Error::<T>::UnknownCollateral.into())
}

/// Read the branch config, returning `UnknownCollateral` when missing.
pub(crate) fn branch_config_of<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> Result<BranchConfig<BalanceOf<T>>, DispatchError> {
	BranchConfigs::<T>::get((collateral_id, stable_id))
		.ok_or_else(|| Error::<T>::UnknownCollateral.into())
}

fn ratio<T: Config>(
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
pub(crate) fn ensure_above_icr<T: Config>(
	collateral: BalanceOf<T>,
	debt: BalanceOf<T>,
	price: FixedU128,
	config: &BranchConfig<BalanceOf<T>>,
) -> Result<(), DispatchError> {
	let cr = ratio::<T>(collateral, debt, price)?;
	ensure!(cr >= config.initial_collateralization_ratio, Error::<T>::UnsafeCollateralizationRatio);
	Ok(())
}

/// Ensure a vault's fully-accrued collateralization ratio is strictly below the
/// branch MCR. Used by the liquidation-eligibility and enter-final-recovery
/// gates.
pub(crate) fn ensure_below_mcr<T: Config>(
	collateral: BalanceOf<T>,
	debt: BalanceOf<T>,
	price: FixedU128,
	config: &BranchConfig<BalanceOf<T>>,
) -> Result<(), DispatchError> {
	let cr = ratio::<T>(collateral, debt, price)?;
	ensure!(cr < config.minimum_collateralization_ratio, Error::<T>::UnsafeCollateralizationRatio);
	Ok(())
}

/// Ensure a vault's fully-accrued collateralization ratio is at or above the
/// branch MCR. Used by the exit-final-recovery gate.
pub(crate) fn ensure_at_or_above_mcr<T: Config>(
	collateral: BalanceOf<T>,
	debt: BalanceOf<T>,
	price: FixedU128,
	config: &BranchConfig<BalanceOf<T>>,
) -> Result<(), DispatchError> {
	let cr = ratio::<T>(collateral, debt, price)?;
	ensure!(cr >= config.minimum_collateralization_ratio, Error::<T>::UnsafeCollateralizationRatio);
	Ok(())
}

/// Read a vault row, returning `VaultNotFound` when missing.
pub(crate) fn vault_of<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	owner: &T::AccountId,
) -> Result<Vault<BalanceOf<T>>, DispatchError> {
	Vaults::<T>::get((collateral_id, stable_id, owner))
		.ok_or_else(|| Error::<T>::VaultNotFound.into())
}

/// Derive a vault's lifecycle status from queue/index membership.
pub(crate) fn vault_status_in<T: Config>(
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

impl<Balance> Vault<Balance> {
	/// Derive this vault's lifecycle status from queue/index membership.
	///
	/// Status is not stored on the row, and the keys must be re-supplied
	/// because the row does not carry them. The `&self` receiver is a proof
	/// of existence.
	pub(crate) fn status<T: Config>(
		&self,
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		owner: &T::AccountId,
	) -> VaultStatus {
		vault_status_in::<T>(
			&VaultListId::Rate(collateral_id.clone(), stable_id.clone()),
			&recovery::list_id::<T>(collateral_id, stable_id),
			owner,
		)
	}

	/// Whether the rate-adjustment cooldown has elapsed. A rate change is free of
	/// the upfront fee once `rate_adjustment_cooldown` has passed since the last
	/// one.
	pub(crate) fn cooldown_elapsed(&self, config: &BranchConfig<Balance>, now: Millis) -> bool {
		now.saturating_sub(self.last_rate_update) >= config.rate_adjustment_cooldown
	}

	/// Existing principal the rate-change part of the borrow upfront fee is
	/// charged against: the current principal when `borrow` also moves the rate
	/// within the cooldown window, zero otherwise (a pure debt increase, or the
	/// cooldown has elapsed).
	pub(crate) fn rate_change_base(
		&self,
		maybe_new_rate: Option<FixedU128>,
		cooldown_elapsed: bool,
	) -> Balance
	where
		Balance: Zero + Copy,
	{
		if maybe_new_rate.is_some_and(|rate| rate != self.annual_rate) && !cooldown_elapsed {
			self.debt.principal
		} else {
			Balance::zero()
		}
	}
}

mod accounting;
mod branch;
mod context;
mod ops;
mod views;

pub(crate) use accounting::{
	accrued_branch_debt, compute_tcr, open_upfront_fee, pending_touch_for,
};
use accounting::{simulate_borrow, simulate_change_rate};
pub(crate) use branch::{
	clear_governance_frozen_mode, create_branch, enable_frozen_mode, enforce_mode_rules,
	ensure_branch_admin, poke_ceiling, ratchet_ceiling, refresh_branch, remove_branch, set_param,
	validate_rate,
};
// Only the test mock reads the derived mode from outside this module.
#[cfg(test)]
pub(crate) use branch::current_mode;
pub(crate) use context::{OpContext, TouchedVault};
pub(crate) use ops::{
	activate_dormant, borrow, change_rate, close_vault, deposit_collateral_for,
	enter_final_recovery, exit_final_recovery, on_idle_walk, open_vault, poke, repay_for,
	withdraw_collateral,
};
pub(crate) use views::{
	ordinary_target_after, predict_upfront_fee_borrow, predict_upfront_fee_open,
	predict_upfront_fee_rate_change, redemption_targets, view_branch_debt, view_branch_tcr,
	view_debt_in_front, view_vault_cr, view_vault_status,
};
