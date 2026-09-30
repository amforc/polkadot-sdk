//! Branch operating mode and branch-level vault engine operations.

use crate::Millis;
use codec::{Decode, DecodeWithMemTracking, Encode};
use frame::deps::sp_runtime::{DispatchError, DispatchResult};
use scale_info::TypeInfo;

/// Branch operating mode. `Normal` and `Safety` are derived from live TCR.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BranchMode {
	Normal,
	Safety,
	Frozen,
}

/// Market state a Stability Pool operation runs under, supplied by the vault engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BranchSnapshot {
	/// Market mode, as derived by the vault engine.
	pub mode: BranchMode,
	/// Current time.
	pub now: Millis,
}

/// Branch-level vault engine operations, implemented by the vaults pallet.
pub trait BranchInterface<CollateralId, StableId> {
	/// Returns the market's mode: `Frozen` without a usable oracle price, `Err` if unregistered.
	fn branch_mode(
		collateral_id: &CollateralId,
		stable_id: &StableId,
	) -> Result<BranchMode, DispatchError>;

	/// Issues the market's pending interest through its yield route.
	///
	/// Fails if the market is unregistered. A frozen market issues nothing.
	fn accrue_interest(collateral_id: &CollateralId, stable_id: &StableId) -> DispatchResult;
}
