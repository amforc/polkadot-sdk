//! Branch operating mode and the branch-level interface of the vault engine.

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

/// Branch-level operations of the vault engine, implemented by the vault pallet.
pub trait BranchInterface<CollateralId, StableId> {
	/// Returns the market's operating mode.
	///
	/// Reports `Frozen` when no usable oracle price exists, and `Err` when the
	/// market is not registered.
	fn branch_mode(
		collateral_id: &CollateralId,
		stable_id: &StableId,
	) -> Result<BranchMode, DispatchError>;

	/// Issues the market's pending aggregate interest through its yield route.
	///
	/// Fails when the market is not registered. A frozen market issues nothing.
	fn accrue_interest(collateral_id: &CollateralId, stable_id: &StableId) -> DispatchResult;
}
