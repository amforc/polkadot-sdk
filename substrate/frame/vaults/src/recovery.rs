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
use linked_list_interface::{ListError, Position, SortedListInterface};

/// The per-branch FIFO list id.
pub fn list_id<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
) -> VaultListIdOf<T> {
	VaultListId::FinalRecovery(collateral_id.clone(), stable_id.clone())
}

/// Append `owner` to the per-branch FIFO.
///
/// Assigns one raw unit above the current head's priority (starting at zero when empty), placing
/// the new member at the head so tail-first iteration is FIFO across arbitrary join/leave
/// interleavings and the total order remains encoded in the stored priorities. Errors when already
/// present.
pub fn append<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
	owner: T::AccountId,
) -> DispatchResult {
	let list = list_id::<T>(collateral_id, stable_id);
	let head = T::VaultLists::head(&list);
	let priority = match &head {
		Some(head_item) => T::VaultLists::priority(&list, head_item)
			.and_then(|p| p.checked_add(&FixedU128::from_inner(1)))
			.ok_or(ListError::CorruptList)
			.map_err(map_error::<T>)?,
		None => FixedU128::zero(),
	};

	let hint = Position { prev: None, next: head };
	T::VaultLists::insert(list, owner, priority, hint).map_err(map_error::<T>)?;
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
/// The pallet derives every FIFO position itself, so any failure other than the transactional
/// limit means the queue and the vault records disagree: storage corruption, reported defensively.
fn map_error<T: Config>(e: ListError) -> Error<T> {
	match e {
		ListError::ItemNotFound |
		ListError::ItemAlreadyExists |
		ListError::CorruptList |
		ListError::InvalidPositionHints => {
			defensive!("final recovery queue disagrees with the vault records", e);
		},
		ListError::Internal => {},
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
