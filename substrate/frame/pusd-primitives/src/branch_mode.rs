//! Branch operating-mode surface shared across pUSD pallets.

use codec::{Decode, DecodeWithMemTracking, Encode};
use frame::deps::sp_runtime::DispatchError;
use scale_info::TypeInfo;

/// Branch operating mode. `Normal` and `Safety` are derived from live TCR;
/// `Frozen` is the only persisted mode.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BranchMode {
	Normal,
	Safety,
	Frozen,
}

/// Read-only access to a market's operating mode (implemented by the vault
/// pallet, the source of truth for branch state).
///
/// Implementations MUST report `Frozen` when no usable oracle price exists —
/// fail closed: price-dependent consumers halt rather than default to the
/// most permissive mode — and `Err` when the market is not registered.
pub trait BranchModeProvider<CollateralId, StableId> {
	fn branch_mode(
		collateral_id: &CollateralId,
		stable_id: &StableId,
	) -> Result<BranchMode, DispatchError>;
}
