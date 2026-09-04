//! Implementations of vault interfaces used by other pallets.

use crate::{
	context::{Commit, VaultOp},
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
		Ok(draft.vault.redemption_snapshot(draft.status, &draft.config))
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
		let mut op = VaultOp::<T>::load_unfrozen(collateral_id.clone(), stable_id.clone(), owner)?;
		let snapshot = op.redemption_snapshot();

		// The owned payment determines settlement size and prevents an unsupported close request.
		ensure!(debt_payment.asset() == *op.stable_id(), Error::<T>::InvalidRedemptionSettlement);
		let debt_to_cancel = debt_payment.peek();
		ensure!(!debt_to_cancel.is_zero(), Error::<T>::InvalidRedemptionSettlement);
		ensure!(
			collateral_to_recipient <= snapshot.collateral,
			Error::<T>::InvalidRedemptionSettlement
		);

		// Burn the stable asset used for payment.
		drop(debt_payment);

		let payment = op.cancel_debt(debt_to_cancel)?;
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
			op.remove_collateral(collateral_to_recipient)?;
		}
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
		// A settlement that leaves neither debt nor collateral has nothing to keep the row
		// for: the vault closes and its deposit returns to the owner. Residual collateral
		// keeps a Dormant husk that only the owner may close.
		if op.vault().debt.total().is_zero() && op.vault().collateral.is_zero() {
			return op.finish_close(owner, Commit::Exempt);
		}
		op.commit(Commit::Exempt)
	}

	fn stablecoin_debt(stable_id: &StableIdOf<T>) -> BalanceOf<T> {
		Self::accrued_stablecoin_debt(stable_id)
	}
}
