//! Branch operating mode and branch-level vault engine operations.

use crate::Millis;
use codec::{Decode, DecodeWithMemTracking, Encode};
use frame::deps::sp_runtime::{DispatchError, DispatchResult};
use scale_info::TypeInfo;

/// Branch operating mode, derived by the vault engine.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BranchMode {
	/// Live TCR at or above the branch's safety collateralization ratio.
	Normal,
	/// Live TCR below the branch's safety collateralization ratio. The vault engine rejects
	/// operations that would lower the TCR further, and Stability Pool withdrawals wait out a
	/// delay after a request.
	Safety,
	/// Frozen by an administrator, or without a usable oracle price so the TCR is unknown. Admits
	/// no capital or yield changes.
	Frozen,
}

impl BranchMode {
	/// Returns `true` for [`Self::Frozen`], the mode that admits no capital or yield changes.
	pub const fn is_frozen(self) -> bool {
		match self {
			Self::Normal | Self::Safety => false,
			Self::Frozen => true,
		}
	}
}

/// Branch state a Stability Pool operation runs under, supplied by the vault engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BranchSnapshot {
	/// Branch mode, as derived by the vault engine.
	pub mode: BranchMode,
	/// Current time, read by the vault engine from the shared stablecoin clock (see [`Millis`]).
	/// Pool cohorts activate and capital is classified as of this instant.
	pub now: Millis,
}

/// Branch-level operations of the vault engine, the owner of branch and vault state.
pub trait BranchInterface<CollateralId, StableId> {
	/// Returns the branch's mode: `Frozen` without a usable oracle price, `Err` if unregistered.
	fn branch_mode(
		collateral_id: &CollateralId,
		stable_id: &StableId,
	) -> Result<BranchMode, DispatchError>;

	/// Issues the branch's pending interest through its yield route.
	///
	/// Fails if the branch is unregistered. A frozen branch issues nothing.
	fn accrue_interest(collateral_id: &CollateralId, stable_id: &StableId) -> DispatchResult;
}
