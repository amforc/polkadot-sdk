//! `FinalRecovery` FIFO operations.
//!
//! Settlement pricing is intentionally not
//! implemented here — the redemption orchestrator pallet owns recovery-pricing
//! math and passes the resulting `RedemptionSettlement` to `redeem_step`.

use crate::{
	pallet::{CollateralIdOf, Config, Error, StableIdOf},
	types::VaultListId,
};
use alloc::vec::Vec;
use frame::prelude::*;
use linked_list_interface::{fifo_append, SortedListInterface};

/// The per-branch FIFO list id.
pub(crate) fn list_id<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
) -> VaultListId<CollateralIdOf<T>, StableIdOf<T>> {
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
		.map_err(|_| Error::<T>::FinalRecoveryInvariantBroken)?;
	Ok(())
}

/// Remove `owner` from the per-branch FIFO. Errors if not present.
pub fn remove<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
	owner: &T::AccountId,
) -> DispatchResult {
	T::VaultLists::remove(&list_id::<T>(collateral_id, stable_id), owner)
		.map_err(|_| Error::<T>::FinalRecoveryInvariantBroken)?;
	Ok(())
}

/// Peek the head of the FIFO, if any.
pub fn next_target<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
) -> Option<T::AccountId> {
	T::VaultLists::tail(&list_id::<T>(collateral_id, stable_id))
}

/// First `n` FIFO owners, oldest first.
pub fn queue<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
	n: u32,
) -> Vec<T::AccountId> {
	T::VaultLists::iter_from_tail(list_id::<T>(collateral_id, stable_id))
		.take(n as usize)
		.collect()
}
