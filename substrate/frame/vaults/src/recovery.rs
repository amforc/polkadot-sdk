//! `FinalRecovery` FIFO operations.
//!
//! Settlement pricing is intentionally not
//! implemented here — the redemption orchestrator pallet owns recovery-pricing
//! math and passes the resulting `RedemptionSettlement` to `redeem_step`.

use crate::{
	pallet::{CollateralIdOf, Config, Error, StableIdOf, VaultListIdOf},
	types::VaultListId,
};
use frame::prelude::*;
use linked_list_interface::{fifo_append, ListError, SortedListInterface};

/// The per-branch FIFO list id.
pub fn list_id<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
) -> VaultListIdOf<T> {
	VaultListId::FinalRecovery(collateral_id.clone(), stable_id.clone())
}

/// Append `owner` to the per-branch FIFO (see [`fifo_append`] for the
/// ordering discipline). Errors when already present.
pub fn append<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
	owner: T::AccountId,
) -> DispatchResult {
	fifo_append::<_, _, T::VaultLists>(list_id::<T>(collateral_id, stable_id), owner)
		.map_err(map_error::<T>)?;
	Ok(())
}

/// Remove `owner` from the per-branch FIFO. Errors if not present.
pub fn remove<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
	owner: &T::AccountId,
) -> DispatchResult {
	T::VaultLists::remove(&list_id::<T>(collateral_id, stable_id), owner)
		.map_err(map_error::<T>)?;
	Ok(())
}

/// Translates a FIFO failure into [`Error::FinalRecoveryInvariantBroken`].
///
/// The pallet derives every FIFO position itself, so any failure other than the length
/// counter or the transactional limit means the queue and the vault records disagree: storage
/// corruption, reported defensively.
fn map_error<T: Config>(e: ListError) -> Error<T> {
	match e {
		ListError::ItemNotFound |
		ListError::ItemAlreadyExists |
		ListError::CorruptList |
		ListError::InvalidPositionHints => {
			defensive!("final recovery queue disagrees with the vault records", e);
		},
		ListError::ListTooLong | ListError::Internal => {},
	}
	Error::<T>::FinalRecoveryInvariantBroken
}

/// Peek the head of the FIFO, if any.
pub fn next_target<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
) -> Option<T::AccountId> {
	T::VaultLists::tail(&list_id::<T>(collateral_id, stable_id))
}
