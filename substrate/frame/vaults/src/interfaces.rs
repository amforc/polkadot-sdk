//! Implementations of vault interfaces used by other pallets.

use crate::{
	context::VaultOp,
	pallet::{
		BalanceOf, CollateralIdOf, Config, Error, Event, HoldReason, Pallet, StableCreditOf,
		StableIdOf,
	},
	types::{VaultListId, VaultStatus},
};
use frame::{
	deps::frame_support::transactional,
	prelude::*,
	traits::{fungibles::MutateHold as FungiblesMutateHold, tokens::Restriction},
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
			redistribution_penalty: draft.config.liquidation.redistribution_penalty,
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
