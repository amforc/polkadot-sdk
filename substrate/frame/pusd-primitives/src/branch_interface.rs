//! Branch operating mode and branch-level vault engine operations.

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
