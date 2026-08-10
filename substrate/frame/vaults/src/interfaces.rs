//! Implementations of vault interfaces used by other pallets.

use crate::{
	context::VaultOp,
	pallet::{
		BalanceOf, CollateralCreditOf, CollateralIdOf, Config, Error, Event, HoldReason, Pallet,
		StableCreditOf, StableIdOf,
	},
	types::{DebtCollateral, LiquidationSettlement, LiquidationSnapshot, VaultListId, VaultStatus},
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
	},
};
use linked_list_interface::SortedListInterface;
use pusd_primitives::{
	BranchMode, BranchModeProvider, RedemptionSettlement, RedemptionStepSnapshot, VaultInterface,
};

impl<T: Config> BranchModeProvider<CollateralIdOf<T>, StableIdOf<T>> for Pallet<T> {
	fn branch_mode(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<BranchMode, DispatchError> {
		Self::current_mode(collateral_id, stable_id)
	}
}

impl<T: Config> Pallet<T> {
	/// Liquidates `owner`'s eligible vault when its collateralization ratio is below MCR.
	///
	/// The pallet first applies accrued interest. It then gives `build_settlement` a post-touch
	/// [`LiquidationSnapshot`] and one credit for all held collateral.
	///
	/// The builder uses the credit for offset payments and keeper compensation. It returns the
	/// offset debt and the collateral credits for redistribution and owner surplus.
	///
	/// The function rejects a settlement that offsets too much debt or returns too much collateral.
	/// The storage transaction rolls back all changes after an error.
	#[transactional]
	pub fn execute_liquidation(
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
		let mut op = VaultOp::<T>::load_priced(collateral_id.clone(), stable_id.clone(), owner)?;
		op.ensure_liquidatable()?;
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
		op.apply_liquidation(DebtCollateral {
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
		op.commit_exempt()
	}
}

impl<T: Config> VaultInterface for Pallet<T> {
	type CollateralId = CollateralIdOf<T>;
	type StableId = StableIdOf<T>;
	type AccountId = T::AccountId;
	type Balance = BalanceOf<T>;
	type StableCredit = StableCreditOf<T>;

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
		let draft = Self::touched_vault_draft(collateral_id, stable_id, owner)?;
		ensure!(!draft.state.is_frozen(), Error::<T>::BranchFrozen);
		Ok(RedemptionStepSnapshot {
			status: draft.status,
			debt: draft.vault.debt.total(),
			collateral: draft.vault.collateral,
			redistribution_penalty: draft.config.redistribution_penalty,
		})
	}

	#[transactional]
	fn redeem_step(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
		recipient: &T::AccountId,
		settlement: RedemptionSettlement<StableCreditOf<T>, BalanceOf<T>>,
	) -> Result<(), DispatchError> {
		let RedemptionSettlement { debt_payment, collateral_to_recipient } = settlement;
		let mut op = VaultOp::<T>::load(collateral_id.clone(), stable_id.clone(), owner)?;
		let snapshot = op.redemption_snapshot();

		// Only the credit amount sets how much debt is cancelled. The caller
		// sized it against a projection of the same touch; fail closed on any
		// divergence.
		ensure!(debt_payment.asset() == *op.stable_id(), Error::<T>::InvalidRedemptionSettlement);
		let debt_to_cancel = debt_payment.peek();
		ensure!(!debt_to_cancel.is_zero(), Error::<T>::InvalidRedemptionSettlement);
		ensure!(debt_to_cancel <= snapshot.debt, Error::<T>::InvalidRedemptionSettlement);
		ensure!(
			collateral_to_recipient <= snapshot.collateral,
			Error::<T>::InvalidRedemptionSettlement
		);

		// Burn the stable asset used for payment.
		drop(debt_payment);

		let payment = op.redeem(debt_to_cancel)?;
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

	fn stablecoin_debt(stable_id: &StableIdOf<T>) -> BalanceOf<T> {
		Self::accrued_stablecoin_debt(stable_id)
	}

	fn heal(collateral_id: &CollateralIdOf<T>, credit: StableCreditOf<T>) -> StableCreditOf<T> {
		// The credit's own coin selects the market's stable axis.
		let stable_id = credit.asset();
		let available = credit.peek();
		let mutated = Self::try_mutate_branch_state(collateral_id, &stable_id, |_, state, _| {
			let healable = available.min(state.debt.bad_debt);
			state.heal_bad_debt(healable);
			Ok(healable)
		});
		let healable = match mutated {
			Ok(healable) => healable,
			// Unknown market (or defensive aggregate failure): the helper
			// errors before its first storage write, so nothing happened.
			Err(_) => return credit,
		};
		if healable.is_zero() {
			return credit;
		}
		let (to_burn, surplus) = credit.split(healable);
		debug_assert_eq!(to_burn.peek(), healable);
		// Burn the stable asset used to heal the debt.
		drop(to_burn);
		Pallet::<T>::deposit_event(Event::BadDebtHealed {
			collateral_id: collateral_id.clone(),
			stable_id,
			amount: healable,
		});
		surplus
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
