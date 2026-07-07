//! Pending-deposit FIFO operations over the runtime's shared linked-list
//! instance (`StableListId::StabilityPending`); ordering discipline as per
//! [`fifo_append`], the same convention as vaults' `FinalRecovery` FIFO.

use crate::pallet::{Config, Error};
use frame::prelude::*;
use pallet_linked_list::{fifo_append, SortedListInterface};
use pusd_primitives::StableListId;

/// The per-branch FIFO list id.
pub(crate) fn list_id<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> StableListId<T::CollateralAssetId, T::StableAssetId> {
	StableListId::StabilityPending(collateral_id.clone(), stable_id.clone())
}

/// Append `depositor` to the per-branch FIFO. Errors if already present.
pub(crate) fn append<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	depositor: T::AccountId,
) -> Result<(), DispatchError> {
	fifo_append::<_, _, T::PendingLists>(list_id::<T>(collateral_id, stable_id), depositor)
		.map_err(|_| Error::<T>::PendingFifoInvariantBroken)?;
	Ok(())
}

/// Remove `depositor` from the per-branch FIFO. Errors if not present.
pub(crate) fn remove<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	depositor: &T::AccountId,
) -> Result<(), DispatchError> {
	T::PendingLists::remove(&list_id::<T>(collateral_id, stable_id), depositor)
		.map_err(|_| Error::<T>::PendingFifoInvariantBroken)?;
	Ok(())
}
