//! Implementations of vault interfaces used by other pallets.

use crate::{
	context::{BranchOp, ResidualSettlement},
	pallet::{
		BalanceOf, Branches, CollateralCreditOf, CollateralIdOf, Config, Error, Event, HoldReason,
		Pallet, StableCreditOf, StableIdOf,
	},
	types::{Position, VaultListId, VaultStatus},
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
use linked_list_interface::SortedListInterface;
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
		let liquidation = op.liquidation_snapshot();
		let post_touch_debt = liquidation.debt;
		let held = op.vault().collateral;

		// Turn this vault's exact collateral into one credit for settlement.
		let (collateral, shortfall) = T::CollateralAssets::slash(
			op.collateral_id().clone(),
			&HoldReason::VaultCollateral.into(),
			op.owner(),
			held,
		);
		ensure!(shortfall.is_zero(), Error::<T>::InvalidLiquidationSettlement);
		let settlement = build_settlement(liquidation, collateral)?;
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

		// `debt_offset <= post_touch_debt` was checked above, so the
		// subtraction is exact.
		op.apply_liquidation(Position {
			debt: post_touch_debt.saturating_sub(debt_offset),
			collateral: redistribution_amount,
		})?;

		if redistribution_amount.is_zero() {
			drop(redistribution_collateral);
		} else {
			let redistribution_account =
				Pallet::<T>::redistribution_account(op.collateral_id(), op.stable_id());
			resolve_collateral_credit::<T>(&redistribution_account, redistribution_collateral)?;
			T::CollateralAssets::hold(
				op.collateral_id().clone(),
				&HoldReason::VaultCollateral.into(),
				&redistribution_account,
				redistribution_amount,
			)?;
		}

		// Return unused collateral to the vault owner.
		if owner_surplus_amount.is_zero() {
			drop(owner_surplus);
		} else {
			resolve_collateral_credit::<T>(op.owner(), owner_surplus)?;
		}

		// The ratio check above applies; market mode rules do not.
		op.remove_exempt()
	}

	/// Returns the next redemption target.
	///
	/// Final recovery comes first, then the dormant target, then the lowest-rate vault.
	fn next_redemption_target(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		after: Option<&T::AccountId>,
	) -> Option<(T::AccountId, VaultStatus)> {
		// A previous step may create a priority target; it preempts any cursor.
		Self::priority_redemption_target(collateral_id, stable_id).or_else(|| {
			Self::ordinary_redemption_target(collateral_id, stable_id, after)
				.map(|owner| (owner, VaultStatus::Active))
		})
	}

	fn redemption_quote_targets(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> impl Iterator<Item = T::AccountId> {
		let priority =
			Self::priority_redemption_target(collateral_id, stable_id).map(|(owner, _)| owner);
		let rate = T::VaultLists::iter_from_tail(VaultListId::Rate(
			collateral_id.clone(),
			stable_id.clone(),
		));
		priority.into_iter().chain(rate)
	}

	fn project_redemption_snapshot(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<RedemptionStepSnapshot<BalanceOf<T>>, DispatchError> {
		let now = T::TimeProvider::now();
		let branch = Self::branch_of(collateral_id, stable_id)?;
		ensure!(!branch.state.is_frozen(), Error::<T>::BranchFrozen);

		let vault = Self::vault_of(collateral_id, stable_id, owner)?;
		let pending = Self::pending_touch_for(&vault, &branch.state, now);
		Ok(RedemptionStepSnapshot {
			status: Self::vault_status_of(collateral_id, stable_id, owner),
			debt: pending.total_debt(&vault.debt),
			collateral: vault.collateral.saturating_add(pending.collateral),
			redistribution_penalty: branch.config.redistribution_penalty,
		})
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
			// Keep interest applied by the touch even when this target is skipped.
			return op.commit_exempt();
		};
		let RedemptionSettlement { debt_payment, collateral_to_recipient } = settlement;

		// Only the credit amount sets how much debt is cancelled. Use `None` to skip the target.
		ensure!(debt_payment.asset() == *op.stable_id(), Error::<T>::InvalidRedemptionSettlement);
		let debt_to_cancel = debt_payment.peek();
		ensure!(
			!debt_to_cancel.is_zero() &&
				debt_to_cancel <= post_touch_debt &&
				collateral_to_recipient <= held,
			Error::<T>::InvalidRedemptionSettlement
		);

		// Burn the stable asset used for payment.
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
		// Move the remaining vault debt to market bad debt. The caller heals it with insurance
		// funds.
		let ResidualSettlement { residual_debt, collateral_dust, swept_orphan_debt } =
			op.settle_recovery_residual()?;

		// Release only this market's dust. The same hold may back other markets.
		if !collateral_dust.is_zero() {
			T::CollateralAssets::release(
				op.collateral_id().clone(),
				&HoldReason::VaultCollateral.into(),
				op.owner(),
				collateral_dust,
				Precision::Exact,
			)?;
		}

		if !swept_orphan_debt.is_zero() {
			Pallet::<T>::deposit_event(Event::BadDebtRecorded {
				collateral_id: op.collateral_id().clone(),
				stable_id: op.stable_id().clone(),
				amount: swept_orphan_debt,
			});
		}
		if !residual_debt.is_zero() {
			Pallet::<T>::deposit_event(Event::BadDebtRecorded {
				collateral_id: op.collateral_id().clone(),
				stable_id: op.stable_id().clone(),
				amount: residual_debt,
			});
		}
		// Emit a closure event because the removed vault cannot carry a status change.
		Pallet::<T>::deposit_event(Event::VaultClosed {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner: op.owner().clone(),
			recipient: op.owner().clone(),
			collateral: collateral_dust,
		});
		op.remove_exempt()?;
		Ok(residual_debt)
	}

	fn redistribution_penalty(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Option<Permill> {
		Branches::<T>::get(collateral_id, stable_id).map(|b| b.config.redistribution_penalty)
	}

	fn stablecoin_debt(stable_id: &StableIdOf<T>) -> BalanceOf<T> {
		Self::accrued_stablecoin_debt(stable_id)
	}

	#[transactional]
	fn heal(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		credit: StableCreditOf<T>,
	) -> Result<StableCreditOf<T>, DispatchError> {
		// Return credit for a different stable asset unchanged.
		if credit.asset() != *stable_id {
			return Ok(credit);
		}
		let (surplus, healable) =
			Self::try_mutate_branch_state(collateral_id, stable_id, move |_, state, _| {
				let healable = credit.peek().min(state.debt.bad_debt);
				if healable.is_zero() {
					return Ok((credit, healable));
				}
				let (to_burn, surplus) = credit.split(healable);
				// Burn the stable asset used to heal the debt.
				drop(to_burn);
				state.heal_bad_debt(healable);
				Ok((surplus, healable))
			})?;
		if healable.is_zero() {
			return Ok(surplus);
		}
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
		// The liquidation transaction restores the hold and issuance on failure.
		drop(credit);
		TokenError::CannotCreate.into()
	})
}
