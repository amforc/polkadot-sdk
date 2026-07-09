//! pUSD primitive trait implementations.

use crate::{
	context::{OpContext, TcrGate, VaultOp},
	pallet::{BalanceOf, Branches, Config, Error, Event, HoldReason, Pallet, StableCreditOf},
	recovery,
	types::VaultStatus,
};
use frame::{
	deps::frame_support::transactional,
	prelude::*,
	traits::{
		fungibles::{Balanced as FungiblesBalanced, MutateHold as FungiblesMutateHold},
		tokens::Restriction,
		SameOrOther, Time,
	},
};
use pallet_linked_list::{ListError, SortedListInterface};
use pusd_primitives::{
	BranchMode, BranchModeProvider, LiquidationAllocation, LiquidationSnapshot,
	RedemptionAllocation, RedemptionStepSnapshot, VaultInterface,
};

impl<T: Config> BranchModeProvider<T::CollateralAssetId, T::StableAssetId> for Pallet<T> {
	fn branch_mode(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Result<BranchMode, DispatchError> {
		Self::current_mode(collateral_id, stable_id)
	}
}

impl<T: Config> VaultInterface for Pallet<T> {
	type CollateralId = T::CollateralAssetId;
	type StableId = T::StableAssetId;
	type AccountId = T::AccountId;
	type Balance = BalanceOf<T>;
	type Credit = StableCreditOf<T>;

	#[transactional]
	fn execute_liquidation(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		owner: &T::AccountId,
		build_allocation: impl FnOnce(
			LiquidationSnapshot<BalanceOf<T>>,
		) -> Result<
			LiquidationAllocation<T::AccountId, BalanceOf<T>>,
			DispatchError,
		>,
	) -> DispatchResult {
		let op = OpContext::<T>::load(collateral_id.clone(), stable_id.clone())?;
		op.ensure_not_frozen()?;
		let price = op.price()?;
		let mut op = op.touch(owner)?;
		ensure!(!op.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);

		let post_touch_debt = op.vault.debt.total();
		let held = op.vault.collateral;
		let config = op.ctx.config();
		let cr =
			pusd_primitives::collateralization_ratio::<BalanceOf<T>>(held, post_touch_debt, price)
				.ok_or(Error::<T>::VaultNotLiquidatable)?;
		ensure!(cr < config.minimum_collateralization_ratio, Error::<T>::VaultNotLiquidatable);

		ensure!(
			op.ctx.branch.state.stakes.total != op.vault.redistribution_stake,
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
		op.ctx.branch.state.detach_vault(&op.vault);
		op.ctx.branch.state.release_dormant_target(owner);
		// Redistributed collateral stays branch-owned until recipient touch.
		let non_redistribution_out = held.saturating_sub(allocation.redistribution_collateral);
		op.ctx.branch.state.remove_collateral(non_redistribution_out);
		if !redistributed_debt.is_zero() || !allocation.redistribution_collateral.is_zero() {
			op.ctx
				.branch
				.state
				.record_redistribution(
					redistributed_debt,
					allocation.redistribution_collateral,
					op.ctx.now,
				)
				.ok_or(Error::<T>::RedistributionWouldOverflow)?;
		}

		match T::VaultLists::remove(&op.ctx.rate_list(), owner) {
			Ok(()) | Err(ListError::ItemNotFound) => {},
			Err(_) => return Err(Error::<T>::RateIndexInvariantBroken.into()),
		}

		if !allocation.redistribution_collateral.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				op.ctx.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				owner,
				&Pallet::<T>::redistribution_account(&op.ctx.collateral_id, &op.ctx.stable_id),
				allocation.redistribution_collateral,
				Precision::Exact,
				Restriction::OnHold,
				Fortitude::Polite,
			)?;
		}

		if !allocation.offset.collateral.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				op.ctx.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				owner,
				&allocation.offset.collateral_recipient,
				allocation.offset.collateral,
				Precision::Exact,
				Restriction::Free,
				Fortitude::Polite,
			)?;
		}

		if !allocation.keeper.collateral.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				op.ctx.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				owner,
				&allocation.keeper.recipient,
				allocation.keeper.collateral,
				Precision::Exact,
				Restriction::Free,
				Fortitude::Polite,
			)?;
		}

		// Release only this market's residual. The owner's hold may also back
		// other markets' collateral on the same asset, which must stay locked.
		let leftover = held.saturating_sub(total_paid_out);
		if !leftover.is_zero() {
			T::CollateralAssets::release(
				op.ctx.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				owner,
				leftover,
				Precision::Exact,
			)?;
		}

		// Liquidation eligibility is MCR-gated above; the mode rules do not apply.
		op.commit_removing_vault(TcrGate::Exempt)
	}

	/// Priority order: `FinalRecovery` FIFO head, then `dormant_redemption_target`,
	/// then the rate-index tail.
	fn next_redemption_target(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		after: Option<&T::AccountId>,
	) -> Option<(T::AccountId, VaultStatus)> {
		// The priority head (FinalRecovery FIFO, then the dormant slot) is a
		// barrier that preempts any carried rate-index cursor: the previous step
		// may have created one. Re-check it regardless of `after`; the cursor
		// only resolves the rate-index tail position when no barrier gates.
		match Self::redemption_targets(collateral_id, stable_id).next() {
			Some((owner, status)) if !status.is_active() => Some((owner, status)),
			head => match after {
				None => head,
				Some(owner) => Self::ordinary_target_after(collateral_id, stable_id, owner)
					.map(|owner| (owner, VaultStatus::Active)),
			},
		}
	}

	#[transactional]
	fn redeem_step(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		owner: &T::AccountId,
		recipient: &T::AccountId,
		build_allocation: impl FnOnce(
			RedemptionStepSnapshot<BalanceOf<T>>,
		) -> Result<
			Option<RedemptionAllocation<BalanceOf<T>>>,
			DispatchError,
		>,
	) -> Result<Option<RedemptionAllocation<BalanceOf<T>>>, DispatchError> {
		let op = OpContext::<T>::load(collateral_id.clone(), stable_id.clone())?;
		op.ensure_not_frozen()?;
		let mut op = op.touch(owner)?;
		let config = op.ctx.config();
		let post_touch_debt = op.vault.debt.total();
		// Collateral lives on the vault row (a shared on-hold balance may back
		// several markets on the same collateral asset), so read it directly.
		let held = op.vault.collateral;
		let snapshot = RedemptionStepSnapshot {
			status: op.status,
			debt: post_touch_debt,
			collateral: held,
			redistribution_penalty: config.redistribution_penalty,
		};

		let Some(allocation) = build_allocation(snapshot)? else {
			// Skipped target: persist the touch so the accrual it caused is paid for.
			op.commit(TcrGate::Exempt)?;
			return Ok(None);
		};

		ensure!(
			allocation.debt_to_cancel <= post_touch_debt,
			Error::<T>::InvalidRedemptionAllocation
		);
		ensure!(
			allocation.collateral_to_recipient <= held,
			Error::<T>::InvalidRedemptionAllocation
		);

		let payment = op.vault.debt.cancel(allocation.debt_to_cancel);
		debug_assert_eq!(payment.total(), allocation.debt_to_cancel);

		if !allocation.collateral_to_recipient.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				op.ctx.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				owner,
				recipient,
				allocation.collateral_to_recipient,
				Precision::Exact,
				Restriction::Free,
				Fortitude::Polite,
			)?;
		}

		let new_total = op.vault.debt.total();
		let stake_changes = matches!(op.status, VaultStatus::Active | VaultStatus::Dormant) &&
			!allocation.collateral_to_recipient.is_zero();
		op.ctx.branch.state.apply_debt_payment(
			payment,
			op.vault.annual_rate,
			op.vault.debt.principal,
		);
		op.ctx.branch.state.remove_collateral(allocation.collateral_to_recipient);
		op.vault.collateral =
			op.vault.collateral.saturating_sub(allocation.collateral_to_recipient);
		if stake_changes {
			let new_stake =
				op.vault.redistribution_stake.saturating_sub(allocation.collateral_to_recipient);
			op.ctx.branch.state.set_vault_stake(&mut op.vault, new_stake);
		}
		if matches!(op.status, VaultStatus::Active | VaultStatus::Dormant) {
			if new_total.is_zero() {
				op.ctx.branch.state.release_dormant_target(owner);
			} else if new_total < config.minimum_debt &&
				!op.ctx.branch.state.try_park_dormant_target(owner.clone())
			{
				return Err(Error::<T>::DormantTargetOccupied.into());
			}
		}

		settle_redemption_status::<T>(&mut op, &config)?;

		Pallet::<T>::deposit_event(Event::VaultRedeemed {
			collateral_id: op.ctx.collateral_id.clone(),
			stable_id: op.ctx.stable_id.clone(),
			owner: owner.clone(),
			recipient: recipient.clone(),
			debt_cancelled: allocation.debt_to_cancel,
			collateral_to_recipient: allocation.collateral_to_recipient,
			vault_annual_rate: op.vault.annual_rate,
		});
		op.commit(TcrGate::Exempt)?;
		Ok(Some(allocation))
	}

	#[transactional]
	fn settle_recovery_residual(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		owner: &T::AccountId,
	) -> Result<BalanceOf<T>, DispatchError> {
		let op = OpContext::<T>::load(collateral_id.clone(), stable_id.clone())?;
		op.ensure_not_frozen()?;
		let mut op = op.touch(owner)?;
		ensure!(op.status.is_final_recovery(), Error::<T>::InvalidVaultStatus);
		// Move the fully-accrued residual debt off the vault and onto the branch
		// bad-debt ledger; the orchestrator burns it from the Insurance Fund via
		// `heal`.
		let residual = op.vault.debt.total();
		// Collateral lives on the vault row; the leftover here is sub-atom dust.
		let dust = op.vault.collateral;
		op.ctx.branch.state.detach_vault(&op.vault);
		op.ctx.branch.state.record_bad_debt(residual);
		op.ctx.branch.state.remove_collateral(dust);
		let swept = if op.ctx.branch.state.is_empty_of_liability() {
			op.ctx.branch.state.sweep_orphan_debt()
		} else {
			BalanceOf::<T>::zero()
		};
		recovery::remove::<T>(&op.ctx.collateral_id, &op.ctx.stable_id, owner)?;

		// Release only this market's residual dust from the shared hold before the
		// vault row vanishes; the owner's hold may still back other markets.
		if !dust.is_zero() {
			T::CollateralAssets::release(
				op.ctx.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				owner,
				dust,
				Precision::Exact,
			)?;
		}

		if !swept.is_zero() {
			Pallet::<T>::deposit_event(Event::BadDebtRecorded {
				collateral_id: op.ctx.collateral_id.clone(),
				stable_id: op.ctx.stable_id.clone(),
				amount: swept,
			});
		}
		if !residual.is_zero() {
			Pallet::<T>::deposit_event(Event::BadDebtRecorded {
				collateral_id: op.ctx.collateral_id.clone(),
				stable_id: op.ctx.stable_id.clone(),
				amount: residual,
			});
		}
		// The owner-naming record of this settlement: the row is removed, so
		// no status-change event can carry it. The dust went to the owner.
		Pallet::<T>::deposit_event(Event::VaultClosed {
			collateral_id: op.ctx.collateral_id.clone(),
			stable_id: op.ctx.stable_id.clone(),
			owner: owner.clone(),
			recipient: owner.clone(),
		});
		op.commit_removing_vault(TcrGate::Exempt)?;
		Ok(residual)
	}

	fn branch_debt(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> BalanceOf<T> {
		// Zero for an unregistered branch: this sizes the dynamic redemption
		// fee, it is not an error surface.
		let Some(branch) = Branches::<T>::get((collateral_id, stable_id)) else {
			return BalanceOf::<T>::zero();
		};
		Self::accrued_branch_debt(&branch.state, T::TimeProvider::now())
	}

	#[transactional]
	fn heal(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		credit: StableCreditOf<T>,
	) -> Result<StableCreditOf<T>, DispatchError> {
		// A credit denominated in another coin cannot heal this market's bad
		// debt — hand it straight back. The `offset` below would reject it
		// anyway (mismatched asset), but checking up front keeps the imbalance
		// intact for the caller.
		if credit.asset() != *stable_id {
			return Ok(credit);
		}
		let state = Self::branch_of(collateral_id, stable_id)?.state;
		let healable = credit.peek().min(state.debt.bad_debt);
		if healable.is_zero() {
			// Nothing recorded (or an empty credit) — hand everything back.
			return Ok(credit);
		}
		let (to_burn, surplus) = credit.split(healable);
		// Rescind the market's own coin to net the burn to zero. The credit's
		// asset equals `stable_id` (checked above) and its size equals
		// `healable`, so `offset` always nets fully; anything else is
		// corruption, and rolling back keeps the ledgers coherent.
		let debt = T::StableAssets::rescind(stable_id.clone(), healable);
		if !matches!(to_burn.offset(debt), Ok(SameOrOther::None)) {
			defensive!("healed credit must net to zero against its own rescind");
			return Err(Error::<T>::ArithmeticOverflow.into());
		}
		Branches::<T>::try_mutate(
			(collateral_id, stable_id),
			|maybe| -> Result<_, DispatchError> {
				let state = &mut maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?.state;
				state.heal_bad_debt(healable);
				Ok(())
			},
		)?;
		Pallet::<T>::deposit_event(Event::BadDebtHealed {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			amount: healable,
		});
		Ok(surplus)
	}
}

/// Update rate/FIFO membership after redemption.
fn settle_redemption_status<T: Config>(
	op: &mut VaultOp<T>,
	config: &crate::types::BranchConfig<BalanceOf<T>>,
) -> DispatchResult {
	let new_total = op.vault.debt.total();
	match op.status {
		VaultStatus::Active if new_total < config.minimum_debt => {
			T::VaultLists::remove(&op.ctx.rate_list(), &op.owner)
				.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
		},
		VaultStatus::FinalRecovery if new_total.is_zero() => {
			recovery::remove::<T>(&op.ctx.collateral_id, &op.ctx.stable_id, &op.owner)?;
			let new_stake = op.vault.collateral;
			op.ctx.branch.state.set_vault_stake(&mut op.vault, new_stake);
			op.vault.redistribution_snapshot = op.ctx.branch.state.redistribution;
			Pallet::<T>::deposit_event(Event::VaultStatusChanged {
				collateral_id: op.ctx.collateral_id.clone(),
				stable_id: op.ctx.stable_id.clone(),
				owner: op.owner.clone(),
				old_status: VaultStatus::FinalRecovery,
				new_status: VaultStatus::Dormant,
			});
		},
		_ => {},
	}
	Ok(())
}
