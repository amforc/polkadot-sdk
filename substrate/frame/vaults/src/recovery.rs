//! `FinalRecovery` FIFO operations.
//!
//! Settlement pricing is intentionally not
//! implemented here — the redemption orchestrator pallet owns recovery-pricing
//! math and passes the resulting `RedemptionAllocation` to `apply_redemption`.

use crate::{
	pallet::{BalanceOf, Config, Error, Event, Pallet},
	types::{BranchState, VaultListId},
};
use alloc::vec::Vec;
use frame::prelude::*;
use pallet_linked_list::{Position, SortedListInterface};

/// Append `owner` to the per-branch FIFO.
pub fn append<T: Config>(
	state: &mut BranchState<T::AccountId, BalanceOf<T>>,
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	owner: T::AccountId,
) -> Result<(), DispatchError> {
	let list_id = VaultListId::FinalRecovery(collateral_id.clone(), stable_id.clone());
	ensure!(!T::VaultLists::contains(&list_id, &owner), Error::<T>::FinalRecoveryInvariantBroken,);

	let nonce = state.next_final_recovery_nonce;
	state.next_final_recovery_nonce =
		nonce.checked_add(1).ok_or(Error::<T>::FinalRecoverySequenceOverflow)?;
	let priority = FixedU128::from_inner(nonce);

	let hint = Position { prev: None, next: T::VaultLists::head(&list_id) };
	T::VaultLists::insert(list_id, owner.clone(), priority, hint)
		.map_err(|_| Error::<T>::FinalRecoveryInvariantBroken)?;

	Pallet::<T>::deposit_event(Event::FinalRecoveryEntered {
		collateral_id: collateral_id.clone(),
		stable_id: stable_id.clone(),
		owner,
	});
	Ok(())
}

/// Remove `owner` from the per-branch FIFO. Errors if not present.
pub fn remove<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	owner: &T::AccountId,
) -> Result<(), DispatchError> {
	let list_id = VaultListId::FinalRecovery(collateral_id.clone(), stable_id.clone());
	T::VaultLists::remove(&list_id, owner).map_err(|_| Error::<T>::FinalRecoveryInvariantBroken)?;
	Pallet::<T>::deposit_event(Event::FinalRecoveryExited {
		collateral_id: collateral_id.clone(),
		stable_id: stable_id.clone(),
		owner: owner.clone(),
	});
	Ok(())
}

/// Peek the head of the FIFO, if any.
pub fn next_target<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> Option<T::AccountId> {
	T::VaultLists::tail(&VaultListId::FinalRecovery(collateral_id.clone(), stable_id.clone()))
}

/// First `n` FIFO owners, oldest first.
pub fn queue_head<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	n: u32,
) -> Vec<T::AccountId> {
	T::VaultLists::iter_from_tail(
		&VaultListId::FinalRecovery(collateral_id.clone(), stable_id.clone()),
		n,
	)
}
