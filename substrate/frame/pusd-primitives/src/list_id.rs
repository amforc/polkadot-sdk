//! Shared list-id namespace for the runtime's single `pallet-linked-list`
//! instance.
//!
//! Every pUSD pallet that keeps a sorted list or FIFO adds a variant here and
//! constructs its own ids; the runtime wires one linked-list instance whose
//! `PriorityProvider` (implemented by `pallet-vaults`) matches on the variant.
//! Variants must only ever be appended — the enum is a storage key, and the
//! SCALE index of existing variants must not move.

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::TypeInfo;

/// One list per `(collateral, stable)` market and per use case.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Debug,
)]
pub enum StableListId<CollateralId, StableId> {
	/// Vaults' borrow-rate index, ordered by annual rate.
	Rate(CollateralId, StableId),
	/// Vaults' `FinalRecovery` FIFO (strictly increasing priorities; the
	/// oldest member is the list tail).
	FinalRecovery(CollateralId, StableId),
	/// The stability pool's pending-deposit FIFO (same FIFO convention as
	/// `FinalRecovery`).
	StabilityPending(CollateralId, StableId),
}

impl<CollateralId: Default, StableId: Default> Default for StableListId<CollateralId, StableId> {
	fn default() -> Self {
		Self::Rate(CollateralId::default(), StableId::default())
	}
}
