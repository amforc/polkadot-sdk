//! `FinalRecovery` FIFO operations.
//!
//! Settlement pricing is intentionally not
//! implemented here — the redemption orchestrator pallet owns recovery-pricing
//! math and passes the resulting `RedemptionAllocation` to `redeem_step`.

use crate::{
	pallet::{Config, Error},
	types::VaultListId,
};
use alloc::vec::Vec;
use frame::prelude::*;
use pallet_linked_list::{Position, SortedListInterface};

/// The per-branch FIFO list id.
pub(crate) fn list_id<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> VaultListId<T::CollateralAssetId, T::StableAssetId> {
	VaultListId::FinalRecovery(collateral_id.clone(), stable_id.clone())
}

/// Append `owner` to the per-branch FIFO.
///
/// The insertion priority is derived from the current head: one above it, so
/// it is strictly greater than every present priority — FIFO order holds
/// across arbitrary enter/exit interleavings, the stored priorities stay
/// distinct (the linked list's permissionless re-anchoring can never legally
/// relocate a member), and the sequence resets whenever the list empties.
pub fn append<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	owner: T::AccountId,
) -> Result<(), DispatchError> {
	let list_id = list_id::<T>(collateral_id, stable_id);
	ensure!(!T::VaultLists::contains(&list_id, &owner), Error::<T>::FinalRecoveryInvariantBroken,);

	let head = T::VaultLists::head(&list_id);
	let priority = match &head {
		Some(head_owner) => {
			let head_priority = T::VaultLists::priority(&list_id, head_owner)
				.ok_or(Error::<T>::FinalRecoveryInvariantBroken)?;
			// Unreachable in practice (u128::MAX consecutive occupied appends);
			// checked so an overflow can only surface, never wrap.
			let inner = head_priority
				.into_inner()
				.checked_add(1)
				.ok_or(Error::<T>::FinalRecoveryInvariantBroken)?;
			FixedU128::from_inner(inner)
		},
		None => FixedU128::zero(),
	};

	let hint = Position { prev: None, next: head };
	T::VaultLists::insert(list_id, owner, priority, hint)
		.map_err(|_| Error::<T>::FinalRecoveryInvariantBroken)?;
	Ok(())
}

/// Remove `owner` from the per-branch FIFO. Errors if not present.
pub fn remove<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	owner: &T::AccountId,
) -> Result<(), DispatchError> {
	T::VaultLists::remove(&list_id::<T>(collateral_id, stable_id), owner)
		.map_err(|_| Error::<T>::FinalRecoveryInvariantBroken)?;
	Ok(())
}

/// Peek the head of the FIFO, if any.
pub fn next_target<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> Option<T::AccountId> {
	T::VaultLists::tail(&list_id::<T>(collateral_id, stable_id))
}

/// First `n` FIFO owners, oldest first.
pub fn queue_head<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	n: u32,
) -> Vec<T::AccountId> {
	T::VaultLists::iter_from_tail(&list_id::<T>(collateral_id, stable_id), n)
}
