//! pUSD primitive trait implementations.

use crate::{
	context::{OpContext, TcrGate, VaultOp},
	pallet::{
		BalanceOf, Branches, CollateralCreditOf, CollateralIdOf, Config, Error, Event, HoldReason,
		Pallet, StableCreditOf, StableIdOf,
	},
	recovery,
	types::VaultStatus,
};
use frame::{
	deps::frame_support::transactional,
	prelude::*,
	traits::{
		fungibles::{
			Balanced as FungiblesBalanced, BalancedHold as FungiblesBalancedHold,
			MutateHold as FungiblesMutateHold,
		},
		tokens::Restriction,
		Time,
	},
};
use pallet_linked_list::{ListError, SortedListInterface};
use pusd_primitives::{
	BranchMode, BranchModeProvider, LiquidationSettlement, LiquidationSnapshot,
	RedemptionSettlement, RedemptionStepSnapshot, VaultInterface,
};

impl<T: Config> BranchModeProvider<CollateralIdOf<T>, StableIdOf<T>> for Pallet<T> {
	fn branch_mode(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<BranchMode, DispatchError> {
		Self::current_mode(collateral_id, stable_id)
	}
}

impl<T: Config> VaultInterface for Pallet<T> {
	type CollateralId = CollateralIdOf<T>;
	type StableId = StableIdOf<T>;
	type AccountId = T::AccountId;
	type Balance = BalanceOf<T>;
	type StableCredit = StableCreditOf<T>;
	type CollateralCredit = CollateralCreditOf<T>;

	#[transactional]
	fn execute_liquidation(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
		build_settlement: impl FnOnce(
			LiquidationSnapshot<BalanceOf<T>>,
			CollateralCreditOf<T>,
		) -> Result<
			LiquidationSettlement<CollateralCreditOf<T>, BalanceOf<T>>,
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

		// Turn the exact vault hold into the one collateral budget external
		// liquidation paths partition.
		let (collateral, shortfall) = T::CollateralAssets::slash(
			op.ctx.collateral_id.clone(),
			&HoldReason::VaultCollateral.into(),
			owner,
			held,
		);
		ensure!(shortfall.is_zero(), Error::<T>::InvalidLiquidationSettlement);
		let settlement =
			build_settlement(LiquidationSnapshot { debt: post_touch_debt }, collateral)?;
		let LiquidationSettlement { debt_offset, redistribution_collateral, owner_surplus } =
			settlement;
		ensure!(debt_offset <= post_touch_debt, Error::<T>::InvalidLiquidationSettlement);
		ensure!(
			owner_surplus.asset() == *collateral_id &&
				redistribution_collateral.asset() == *collateral_id,
			Error::<T>::InvalidLiquidationSettlement
		);
		let owner_surplus_amount = owner_surplus.peek();
		let redistribution_amount = redistribution_collateral.peek();
		let returned = owner_surplus_amount
			.checked_add(&redistribution_amount)
			.ok_or(ArithmeticError::Overflow)?;
		ensure!(returned <= held, Error::<T>::InvalidLiquidationSettlement);

		let redistributed_debt = post_touch_debt.saturating_sub(debt_offset);
		op.ctx.branch.state.detach_vault(&op.vault);
		op.ctx.branch.state.release_dormant_target(owner);
		// Redistributed collateral stays branch-owned until recipient touch.
		let non_redistribution_out = held.saturating_sub(redistribution_amount);
		op.ctx.branch.state.remove_collateral(non_redistribution_out);
		if !redistributed_debt.is_zero() || !redistribution_amount.is_zero() {
			op.ctx
				.branch
				.state
				.record_redistribution(redistributed_debt, redistribution_amount, op.ctx.now)
				.ok_or(Error::<T>::RedistributionWouldOverflow)?;
		}

		match T::VaultLists::remove(&op.ctx.rate_list(), owner) {
			Ok(()) | Err(ListError::ItemNotFound) => {},
			Err(_) => return Err(Error::<T>::RateIndexInvariantBroken.into()),
		}

		if !redistribution_amount.is_zero() {
			let redistribution_account =
				Pallet::<T>::redistribution_account(&op.ctx.collateral_id, &op.ctx.stable_id);
			resolve_collateral_credit::<T>(&redistribution_account, redistribution_collateral)?;
			T::CollateralAssets::hold(
				op.ctx.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				&redistribution_account,
				redistribution_amount,
			)?;
		} else {
			drop(redistribution_collateral);
		}

		// Whatever external offsets, JIT, and the keeper did not consume is the
		// liquidated owner's surplus.
		if !owner_surplus_amount.is_zero() {
			resolve_collateral_credit::<T>(owner, owner_surplus)?;
		} else {
			drop(owner_surplus);
		}

		// Liquidation eligibility is MCR-gated above; the mode rules do not apply.
		op.commit_removing_vault(TcrGate::Exempt)
	}

	/// Priority order: `FinalRecovery` FIFO head, then `dormant_redemption_target`,
	/// then the rate-index tail.
	fn next_redemption_target(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
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
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
		recipient: &T::AccountId,
		build_settlement: impl FnOnce(
			RedemptionStepSnapshot<BalanceOf<T>>,
		) -> Result<
			Option<RedemptionSettlement<StableCreditOf<T>, BalanceOf<T>>>,
			DispatchError,
		>,
	) -> DispatchResult {
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

		let Some(settlement) = build_settlement(snapshot)? else {
			// Skipped target: persist the touch so the accrual it caused is paid for.
			return op.commit(TcrGate::Exempt);
		};
		let RedemptionSettlement { debt_payment, collateral_to_recipient } = settlement;

		// The payment credit is the sole authority on how much debt this step
		// cancels. It must carry real value: `Ok(None)` is the touch-only form,
		// so a zero payment cannot release collateral.
		ensure!(debt_payment.asset() == *stable_id, Error::<T>::InvalidRedemptionSettlement);
		let debt_to_cancel = debt_payment.peek();
		ensure!(
			!debt_to_cancel.is_zero() &&
				debt_to_cancel <= post_touch_debt &&
				collateral_to_recipient <= held,
			Error::<T>::InvalidRedemptionSettlement
		);

		// Dropping the credit burns the withdrawn stablecoin.
		drop(debt_payment);

		let payment = op.vault.debt.cancel(debt_to_cancel);
		debug_assert_eq!(payment.total(), debt_to_cancel);

		if !collateral_to_recipient.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				op.ctx.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				owner,
				recipient,
				collateral_to_recipient,
				Precision::Exact,
				Restriction::Free,
				Fortitude::Polite,
			)?;
		}

		let new_total = op.vault.debt.total();
		let stake_changes = matches!(op.status, VaultStatus::Active | VaultStatus::Dormant) &&
			!collateral_to_recipient.is_zero();
		op.ctx.branch.state.apply_debt_payment(
			payment,
			op.vault.annual_rate,
			op.vault.debt.principal,
		);
		op.ctx.branch.state.remove_collateral(collateral_to_recipient);
		op.vault.collateral = op.vault.collateral.saturating_sub(collateral_to_recipient);
		if stake_changes {
			let new_stake = op.vault.redistribution_stake.saturating_sub(collateral_to_recipient);
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
			debt_cancelled: debt_to_cancel,
			collateral_to_recipient,
			vault_annual_rate: op.vault.annual_rate,
		});
		op.commit(TcrGate::Exempt)
	}

	#[transactional]
	fn settle_recovery_residual(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
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

	fn branch_debt(collateral_id: &CollateralIdOf<T>, stable_id: &StableIdOf<T>) -> BalanceOf<T> {
		// Zero for an unregistered branch: this sizes the dynamic redemption
		// fee, it is not an error surface.
		let Some(branch) = Branches::<T>::get(collateral_id, stable_id) else {
			return BalanceOf::<T>::zero();
		};
		Self::accrued_branch_debt(&branch.state, T::TimeProvider::now())
	}

	#[transactional]
	fn heal(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		credit: StableCreditOf<T>,
	) -> Result<StableCreditOf<T>, DispatchError> {
		// A credit denominated in another coin cannot heal this market's bad
		// debt — hand it straight back.
		if credit.asset() != *stable_id {
			return Ok(credit);
		}
		let mut branch = Self::branch_of(collateral_id, stable_id)?;
		let healable = credit.peek().min(branch.state.debt.bad_debt);
		if healable.is_zero() {
			// Nothing recorded (or an empty credit) — hand everything back.
			return Ok(credit);
		}
		let (to_burn, surplus) = credit.split(healable);
		// Dropping the credit burns the withdrawn stablecoin.
		drop(to_burn);
		branch.state.heal_bad_debt(healable);
		Pallet::<T>::commit_branch(collateral_id, stable_id, branch)?;
		Pallet::<T>::deposit_event(Event::BadDebtHealed {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			amount: healable,
		});
		Ok(surplus)
	}
}

fn resolve_collateral_credit<T: Config>(
	recipient: &T::AccountId,
	credit: CollateralCreditOf<T>,
) -> DispatchResult {
	T::CollateralAssets::resolve(recipient, credit).map_err(|credit| {
		// The surrounding liquidation transaction restores both the original
		// hold and issuance after this credit is dropped.
		drop(credit);
		TokenError::CannotCreate.into()
	})
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
