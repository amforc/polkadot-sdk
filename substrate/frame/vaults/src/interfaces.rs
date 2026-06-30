//! pUSD primitive trait implementations.

use crate::{
	helpers::{self, OpContext, TouchedVault},
	math,
	pallet::{
		BalanceOf, BranchStates, Config, Error, Event, HoldReason, Millis, Pallet, StableCreditOf,
	},
	recovery,
	types::{BranchState, VaultStatus},
};
use frame::{
	deps::frame_support::transactional,
	prelude::*,
	traits::{
		fungible::Balanced as FungibleBalanced,
		fungibles::{InspectHold as FungiblesInspectHold, MutateHold as FungiblesMutateHold},
		tokens::{Imbalance, Restriction},
		SameOrOther,
	},
};
use pallet_linked_list::{ListError, SortedListInterface};
use pusd_primitives::{
	AllocationResult, LiquidationSnapshot, RedemptionAllocation, VaultBadDebtInterface,
	VaultLiquidationInterface, VaultRedemptionInterface,
};

impl<T: Config> VaultLiquidationInterface<T::AccountId, T::AssetId, BalanceOf<T>> for Pallet<T> {
	#[transactional]
	fn execute_liquidation(
		collateral_id: T::AssetId,
		owner: T::AccountId,
		build_allocation: impl FnOnce(
			LiquidationSnapshot<BalanceOf<T>>,
		) -> AllocationResult<T::AccountId, BalanceOf<T>>,
	) -> DispatchResult {
		let mut context = OpContext::<T>::load(collateral_id)?;
		context.ensure_not_frozen()?;
		let price = context.price()?;
		let TouchedVault { vault, status } = context.touch(&owner)?;
		ensure!(!status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);

		let post_touch_debt = vault.debt.total();
		let held = vault.redistribution_stake;
		let config = context.config()?;
		let cr = math::collateralization_ratio::<BalanceOf<T>>(held, post_touch_debt, price)
			.ok_or(Error::<T>::VaultNotLiquidatable)?;
		ensure!(cr < config.minimum_collateralization_ratio, Error::<T>::VaultNotLiquidatable);

		ensure!(
			context.state.stakes.total != vault.redistribution_stake,
			Error::<T>::LastVaultCannotBeLiquidated
		);

		let allocation =
			build_allocation(LiquidationSnapshot { debt: post_touch_debt, collateral: held })?;
		ensure!(
			allocation.offset.debt <= post_touch_debt,
			Error::<T>::InvalidLiquidationAllocation
		);
		let total_paid_out = allocation
			.offset
			.collateral
			.saturating_add(allocation.redistribution_collateral)
			.saturating_add(allocation.keeper.collateral);
		ensure!(total_paid_out <= held, Error::<T>::InvalidLiquidationAllocation);

		let redistributed_debt = post_touch_debt.saturating_sub(allocation.offset.debt);
		context.state.detach_vault(&vault);
		context.state.release_dormant_target(&owner);
		// Redistributed collateral stays branch-owned until recipient touch.
		let non_redistribution_out = held.saturating_sub(allocation.redistribution_collateral);
		context.state.total_collateral =
			context.state.total_collateral.saturating_sub(non_redistribution_out);
		if !redistributed_debt.is_zero() || !allocation.redistribution_collateral.is_zero() {
			apply_redistribution_accumulators::<T>(
				&mut context.state,
				redistributed_debt,
				allocation.redistribution_collateral,
				context.now,
			)?;
		}

		match T::VaultLists::remove(&context.rate_list(), &owner) {
			Ok(()) | Err(ListError::ItemNotFound) => {},
			Err(_) => return Err(Error::<T>::RateIndexInvariantBroken.into()),
		}

		if !allocation.redistribution_collateral.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				context.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				&owner,
				&Pallet::<T>::redistribution_account(),
				allocation.redistribution_collateral,
				Precision::Exact,
				Restriction::OnHold,
				Fortitude::Polite,
			)?;
		}

		if !allocation.offset.collateral.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				context.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				&owner,
				&allocation.offset.recipient,
				allocation.offset.collateral,
				Precision::Exact,
				Restriction::Free,
				Fortitude::Polite,
			)?;
		}

		if !allocation.keeper.collateral.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				context.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				&owner,
				&allocation.keeper.recipient,
				allocation.keeper.collateral,
				Precision::Exact,
				Restriction::Free,
				Fortitude::Polite,
			)?;
		}

		let after_outflow = T::CollateralAssets::balance_on_hold(
			context.collateral_id.clone(),
			&HoldReason::VaultCollateral.into(),
			&owner,
		);
		if !after_outflow.is_zero() {
			T::CollateralAssets::release(
				context.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				&owner,
				after_outflow,
				Precision::Exact,
			)?;
		}

		context.commit_removing_vault(&owner);
		Ok(())
	}
}

/// Fold one liquidation's redistribution into branch accumulators.
fn apply_redistribution_accumulators<T: Config>(
	state: &mut BranchState<T::AccountId, BalanceOf<T>>,
	redistributed_debt: BalanceOf<T>,
	redistribution_collateral: BalanceOf<T>,
	now: Millis,
) -> DispatchResult {
	let avg_rate = math::average_branch_rate(state.stakes.weighted_sum, state.stakes.total);
	let debt_per_stake = math::redistribution_per_stake(redistributed_debt, state.stakes.total)
		.ok_or(Error::<T>::RedistributionWouldOverflow)?;
	let collateral_per_stake =
		math::redistribution_per_stake(redistribution_collateral, state.stakes.total)
			.ok_or(Error::<T>::RedistributionWouldOverflow)?;
	let weight_per_stake =
		math::redistribution_weight_per_stake(redistributed_debt, avg_rate, state.stakes.total)
			.ok_or(Error::<T>::RedistributionWouldOverflow)?;
	// Must match `pending_touch_for`'s interest-time origin.
	let now_fp = FixedU128::saturating_from_integer(state.interest_time(now));
	let debt_time_increment = now_fp
		.checked_mul(&debt_per_stake)
		.ok_or(Error::<T>::RedistributionWouldOverflow)?;
	state.redistribution.debt_per_stake = state
		.redistribution
		.debt_per_stake
		.checked_add(&debt_per_stake)
		.ok_or(Error::<T>::RedistributionWouldOverflow)?;
	state.redistribution.collateral_per_stake = state
		.redistribution
		.collateral_per_stake
		.checked_add(&collateral_per_stake)
		.ok_or(Error::<T>::RedistributionWouldOverflow)?;
	state.redistribution.debt_time_per_stake = state
		.redistribution
		.debt_time_per_stake
		.checked_add(&debt_time_increment)
		.ok_or(Error::<T>::RedistributionWouldOverflow)?;
	state.redistribution.weight_per_stake = state
		.redistribution
		.weight_per_stake
		.checked_add(&weight_per_stake)
		.ok_or(Error::<T>::RedistributionWouldOverflow)?;
	let distributed_debt = debt_per_stake.saturating_mul_int(state.stakes.total);
	let debt_dust = redistributed_debt.saturating_sub(distributed_debt);
	state.debt.pending_redistribution_principal =
		state.debt.pending_redistribution_principal.saturating_add(distributed_debt);
	state.debt.weighted_principal_sum = state
		.debt
		.weighted_principal_sum
		.saturating_add(avg_rate.saturating_mul_int(redistributed_debt));
	if !debt_dust.is_zero() {
		state.add_ownerless_pusd_debt(debt_dust);
	}
	let distributed_coll = collateral_per_stake.saturating_mul_int(state.stakes.total);
	let collateral_dust = redistribution_collateral.saturating_sub(distributed_coll);
	if !collateral_dust.is_zero() {
		state.add_ownerless_collateral_surplus(collateral_dust);
	}
	Ok(())
}

impl<T: Config> VaultRedemptionInterface<T::AccountId, T::AssetId, BalanceOf<T>> for Pallet<T> {
	/// Priority order: `FinalRecovery` FIFO head, then `dormant_redemption_target`,
	/// then the rate-index tail.
	fn next_redemption_target(collateral_id: T::AssetId) -> Option<T::AccountId> {
		helpers::redemption_targets::<T>(&collateral_id).next()
	}

	#[transactional]
	fn touch_for_redemption(
		collateral_id: T::AssetId,
		owner: T::AccountId,
	) -> Result<BalanceOf<T>, DispatchError> {
		let mut context = OpContext::<T>::load(collateral_id)?;
		context.ensure_not_frozen()?;
		let touched = context.touch(&owner)?;
		let post_touch_debt = touched.vault.debt.total();
		context.commit_with_vault(&owner, &touched.vault);
		Ok(post_touch_debt)
	}

	#[transactional]
	fn apply_redemption(
		collateral_id: T::AssetId,
		owner: T::AccountId,
		redeemer: T::AccountId,
		allocation: RedemptionAllocation<BalanceOf<T>>,
	) -> DispatchResult {
		let mut context = OpContext::<T>::load(collateral_id)?;
		context.ensure_not_frozen()?;
		let TouchedVault { mut vault, status } = context.touch(&owner)?;
		let post_touch_debt = vault.debt.total();
		let held = T::CollateralAssets::balance_on_hold(
			context.collateral_id.clone(),
			&HoldReason::VaultCollateral.into(),
			&owner,
		);
		ensure!(
			allocation.debt_to_cancel <= post_touch_debt,
			Error::<T>::InvalidRedemptionAllocation
		);
		ensure!(
			allocation
				.collateral_to_redeemer
				.saturating_add(allocation.fee_collateral_retained) <=
				held,
			Error::<T>::InvalidRedemptionAllocation
		);

		let payment = vault.debt.cancel(allocation.debt_to_cancel);
		debug_assert_eq!(payment.total(), allocation.debt_to_cancel);

		if !allocation.collateral_to_redeemer.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				context.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				&owner,
				&redeemer,
				allocation.collateral_to_redeemer,
				Precision::Exact,
				Restriction::Free,
				Fortitude::Polite,
			)?;
		}

		let config = context.config()?;
		let new_total = vault.debt.total();
		let stake_changes = matches!(status, VaultStatus::Active | VaultStatus::Dormant) &&
			!allocation.collateral_to_redeemer.is_zero();
		let old_stake = vault.redistribution_stake;
		let new_stake = old_stake.saturating_sub(allocation.collateral_to_redeemer);
		context
			.state
			.apply_debt_payment(payment, vault.annual_rate, vault.debt.principal);
		context.state.remove_collateral(allocation.collateral_to_redeemer);
		if stake_changes {
			context.state.refresh_vault_stake(vault.annual_rate, old_stake, new_stake);
		}
		if matches!(status, VaultStatus::Active | VaultStatus::Dormant) {
			if new_total.is_zero() {
				context.state.release_dormant_target(&owner);
			} else if new_total < config.minimum_debt &&
				!context.state.try_park_dormant_target(owner.clone())
			{
				return Err(Error::<T>::DormantTargetOccupied.into());
			}
		}
		if stake_changes {
			vault.redistribution_stake = new_stake;
		}

		settle_redemption_status::<T>(
			&mut context,
			&owner,
			&mut vault,
			status,
			new_total,
			&config,
		)?;

		Pallet::<T>::deposit_event(Event::VaultRedeemed {
			collateral_id: context.collateral_id.clone(),
			owner: owner.clone(),
			redeemer,
			debt_cancelled: allocation.debt_to_cancel,
			collateral_to_redeemer: allocation.collateral_to_redeemer,
			fee_collateral_retained: allocation.fee_collateral_retained,
			vault_annual_rate: vault.annual_rate,
		});
		context.commit_with_vault(&owner, &vault);
		Ok(())
	}
}

/// Update rate/FIFO membership after redemption.
fn settle_redemption_status<T: Config>(
	context: &mut OpContext<T>,
	owner: &T::AccountId,
	vault: &mut crate::types::Vault<BalanceOf<T>>,
	status: VaultStatus,
	new_total: BalanceOf<T>,
	config: &crate::types::BranchConfig<BalanceOf<T>>,
) -> DispatchResult {
	match status {
		VaultStatus::Active if new_total < config.minimum_debt => {
			T::VaultLists::remove(&context.rate_list(), owner)
				.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
		},
		VaultStatus::FinalRecovery if new_total.is_zero() => {
			recovery::remove::<T>(&context.collateral_id, owner)?;
			let held_now = T::CollateralAssets::balance_on_hold(
				context.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				owner,
			);
			context
				.state
				.refresh_vault_stake(vault.annual_rate, BalanceOf::<T>::zero(), held_now);
			vault.redistribution_snapshot = context.state.redistribution;
			vault.redistribution_stake = held_now;
		},
		_ => {},
	}
	Ok(())
}

impl<T: Config> VaultBadDebtInterface<T::AssetId, BalanceOf<T>, StableCreditOf<T>> for Pallet<T> {
	#[transactional]
	fn record_bad_debt(collateral_id: T::AssetId, amount: BalanceOf<T>) -> DispatchResult {
		if amount.is_zero() {
			return Ok(());
		}
		BranchStates::<T>::try_mutate(&collateral_id, |maybe| -> Result<_, DispatchError> {
			let state = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
			state.debt.bad_debt = state.debt.bad_debt.saturating_add(amount);
			Ok(())
		})?;
		Pallet::<T>::deposit_event(Event::BadDebtRecorded { collateral_id, amount });
		Ok(())
	}

	#[transactional]
	fn heal(
		collateral_id: T::AssetId,
		credit: StableCreditOf<T>,
	) -> Result<StableCreditOf<T>, DispatchError> {
		let state = helpers::branch_state_of::<T>(&collateral_id)?;
		let healable = credit.peek().min(state.debt.bad_debt);
		if healable.is_zero() {
			// Nothing recorded (or an empty credit) — hand everything back.
			return Ok(credit);
		}
		let (to_burn, surplus) = credit.split(healable);
		// Rescind matching pUSD to net the imbalance to zero.
		let debt = T::StableAsset::rescind(healable);
		// `offset` returns `SameOrOther<credit-side, debt-side>`. With
		// matching peeks the result is `None`, which is perfect netting.
		match to_burn.offset(debt) {
			SameOrOther::None => {},
			SameOrOther::Same(remaining_credit) => {
				// Defensive: `peek == healable` rescind should fully net.
				drop(remaining_credit);
				return Err(Error::<T>::ArithmeticOverflow.into());
			},
			SameOrOther::Other(remaining_debt) => {
				drop(remaining_debt);
				return Err(Error::<T>::ArithmeticOverflow.into());
			},
		}
		BranchStates::<T>::try_mutate(&collateral_id, |maybe| -> Result<_, DispatchError> {
			let state = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
			state.debt.bad_debt = state.debt.bad_debt.saturating_sub(healable);
			Ok(())
		})?;
		Pallet::<T>::deposit_event(Event::BadDebtHealed { collateral_id, amount: healable });
		Ok(surplus)
	}
}
