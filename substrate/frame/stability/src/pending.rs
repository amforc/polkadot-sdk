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

/// Append `depositor` to the `fifo` list. Errors if already present.
pub(crate) fn append<T: Config>(
	fifo: &StableListId<T::CollateralAssetId, T::StableAssetId>,
	depositor: T::AccountId,
) -> Result<(), DispatchError> {
	fifo_append::<_, _, T::PendingLists>(fifo.clone(), depositor)
		.map_err(|_| Error::<T>::PendingFifoInvariantBroken)?;
	Ok(())
}

/// Remove `depositor` from the `fifo` list. Errors if not present. Both ops
/// take the prebuilt list id ([`list_id`]) so loops do not reclone the asset
/// ids per call.
pub(crate) fn remove<T: Config>(
	fifo: &StableListId<T::CollateralAssetId, T::StableAssetId>,
	depositor: &T::AccountId,
) -> Result<(), DispatchError> {
	T::PendingLists::remove(fifo, depositor).map_err(|_| Error::<T>::PendingFifoInvariantBroken)?;
	Ok(())
}
