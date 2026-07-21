//! pUSD primitive trait implementations.

use crate::{
	context::BranchOp,
	pallet::{
		BalanceOf, Branches, CollateralCreditOf, CollateralIdOf, Config, Error, Event, HoldReason,
		Pallet, StableCreditOf, StableIdOf,
	},
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
		let op = BranchOp::<T>::load_unfrozen(collateral_id.clone(), stable_id.clone())?;
		let price = op.price()?;
		let mut op = op.touch(owner)?;
		op.ensure_liquidatable(price)?;
		let post_touch_debt = op.vault().debt.total();
		let held = op.vault().collateral;

		// Turn the exact vault hold into the one collateral budget external
		// liquidation paths partition.
		let (collateral, shortfall) = T::CollateralAssets::slash(
			op.collateral_id().clone(),
			&HoldReason::VaultCollateral.into(),
			op.owner(),
			held,
		);
		ensure!(shortfall.is_zero(), Error::<T>::InvalidLiquidationSettlement);
		let settlement =
			build_settlement(LiquidationSnapshot { debt: post_touch_debt }, collateral)?;
		let LiquidationSettlement { debt_offset, redistribution_collateral, owner_surplus } =
			settlement;
		ensure!(debt_offset <= post_touch_debt, Error::<T>::InvalidLiquidationSettlement);
		ensure!(
			owner_surplus.asset() == *op.collateral_id() &&
				redistribution_collateral.asset() == *op.collateral_id(),
			Error::<T>::InvalidLiquidationSettlement
		);
		let owner_surplus_amount = owner_surplus.peek();
		let redistribution_amount = redistribution_collateral.peek();
		let returned = owner_surplus_amount
			.checked_add(&redistribution_amount)
			.ok_or(ArithmeticError::Overflow)?;
		ensure!(returned <= held, Error::<T>::InvalidLiquidationSettlement);

		op.apply_liquidation(debt_offset, redistribution_amount)?;

		if !redistribution_amount.is_zero() {
			let redistribution_account =
				Pallet::<T>::redistribution_account(op.collateral_id(), op.stable_id());
			resolve_collateral_credit::<T>(&redistribution_account, redistribution_collateral)?;
			T::CollateralAssets::hold(
				op.collateral_id().clone(),
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
			resolve_collateral_credit::<T>(op.owner(), owner_surplus)?;
		} else {
			drop(owner_surplus);
		}

		// Liquidation eligibility is MCR-gated above; the mode rules do not apply.
		op.remove_exempt()
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
		let op = BranchOp::<T>::load_unfrozen(collateral_id.clone(), stable_id.clone())?;
		let mut op = op.touch(owner)?;
		let snapshot = op.redemption_snapshot();
		let post_touch_debt = snapshot.debt;
		let held = snapshot.collateral;

		let Some(settlement) = build_settlement(snapshot)? else {
			// Skipped target: persist the touch so the accrual it caused is paid for.
			return op.commit_exempt();
		};
		let RedemptionSettlement { debt_payment, collateral_to_recipient } = settlement;

		// The payment credit is the sole authority on how much debt this step
		// cancels. It must carry real value: `Ok(None)` is the touch-only form,
		// so a zero payment cannot release collateral.
		ensure!(debt_payment.asset() == *op.stable_id(), Error::<T>::InvalidRedemptionSettlement);
		let debt_to_cancel = debt_payment.peek();
		ensure!(
			!debt_to_cancel.is_zero() &&
				debt_to_cancel <= post_touch_debt &&
				collateral_to_recipient <= held,
			Error::<T>::InvalidRedemptionSettlement
		);

		// Dropping the credit burns the withdrawn stablecoin.
		drop(debt_payment);

		let payment = op.redeem(debt_to_cancel);
		debug_assert_eq!(payment.total(), debt_to_cancel);

		if !collateral_to_recipient.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				op.collateral_id().clone(),
				&HoldReason::VaultCollateral.into(),
				op.owner(),
				recipient,
				collateral_to_recipient,
				Precision::Exact,
				Restriction::Free,
				Fortitude::Polite,
			)?;
		}

		op.remove_collateral(collateral_to_recipient)?;
		op.reconcile_after_debt_reduction()?;

		Pallet::<T>::deposit_event(Event::VaultRedeemed {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner: op.owner().clone(),
			recipient: recipient.clone(),
			debt_cancelled: debt_to_cancel,
			collateral_to_recipient,
			vault_annual_rate: op.vault().annual_rate,
		});
		op.commit_exempt()
	}

	#[transactional]
	fn settle_recovery_residual(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<BalanceOf<T>, DispatchError> {
		let op = BranchOp::<T>::load_unfrozen(collateral_id.clone(), stable_id.clone())?;
		let mut op = op.touch(owner)?;
		// Move the fully-accrued residual debt off the vault and onto the branch
		// bad-debt ledger; the orchestrator burns it from the Insurance Fund via
		// `heal`.
		let (residual, dust, swept) = op.settle_recovery_residual()?;

		// Release only this market's residual dust from the shared hold before the
		// vault row vanishes; the owner's hold may still back other markets.
		if !dust.is_zero() {
			T::CollateralAssets::release(
				op.collateral_id().clone(),
				&HoldReason::VaultCollateral.into(),
				op.owner(),
				dust,
				Precision::Exact,
			)?;
		}

		if !swept.is_zero() {
			Pallet::<T>::deposit_event(Event::BadDebtRecorded {
				collateral_id: op.collateral_id().clone(),
				stable_id: op.stable_id().clone(),
				amount: swept,
			});
		}
		if !residual.is_zero() {
			Pallet::<T>::deposit_event(Event::BadDebtRecorded {
				collateral_id: op.collateral_id().clone(),
				stable_id: op.stable_id().clone(),
				amount: residual,
			});
		}
		// The owner-naming record of this settlement: the row is removed, so
		// no status-change event can carry it. The dust went to the owner.
		Pallet::<T>::deposit_event(Event::VaultClosed {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner: op.owner().clone(),
			recipient: op.owner().clone(),
		});
		op.remove_exempt()?;
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
