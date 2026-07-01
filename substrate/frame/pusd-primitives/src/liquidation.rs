//! Liquidation handoff types and trait

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::deps::frame_support::pallet_prelude::{DispatchError, DispatchResult};
use scale_info::TypeInfo;

/// Debt cancelled by external pUSD (Stability Pool + JIT combined) and the
/// matching collateral credited to the offset path. The orchestrator may
/// internally split the collateral across standing depositors and JIT.
///
/// `recipient` is the account that receives `collateral` — the vault pallet
/// moves it inline during `finalize_liquidation`, so the orchestrator never
/// needs to take possession of liquidated collateral itself.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct OffsetAllocation<AccountId, Balance> {
	pub recipient: AccountId,
	pub debt: Balance,
	pub collateral: Balance,
}

/// Compensation paid to the liquidation keeper.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct KeeperCompensation<AccountId, Balance> {
	pub recipient: AccountId,
	pub collateral: Balance,
}

/// Allocation produced by the liquidation orchestrator and applied by
/// [`VaultLiquidationInterface::execute_liquidation`].
///
/// Redistributed debt is derived inside `execute_liquidation` as
/// `snapshot.debt - offset.debt`, so the orchestrator only needs to specify
/// the collateral split.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct LiquidationAllocation<AccountId, Balance> {
	pub offset: OffsetAllocation<AccountId, Balance>,
	pub redistribution_collateral: Balance,
	pub keeper: KeeperCompensation<AccountId, Balance>,
}

/// Fully-accrued vault figures handed to the allocation builder. These are the
/// post-touch numbers the orchestrator must size its allocation against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LiquidationSnapshot<Balance> {
	/// Post-touch total debt to settle.
	pub debt: Balance,
	/// Collateral currently held against the vault.
	pub collateral: Balance,
}

/// What a `build_allocation` closure returns: the orchestrator's collateral and
/// debt split, or a `DispatchError` that rolls the whole liquidation back.
pub type AllocationResult<AccountId, Balance> =
	Result<LiquidationAllocation<AccountId, Balance>, DispatchError>;

/// Returning `Err` from `build_allocation`, or producing an invalid allocation,
/// rolls the whole call back, so a rejected liquidation never leaves partial
/// state behind.
pub trait VaultLiquidationInterface<AccountId, CollateralId, StableId, Balance> {
	fn execute_liquidation(
		collateral_id: CollateralId,
		stable_id: StableId,
		owner: AccountId,
		build_allocation: impl FnOnce(
			LiquidationSnapshot<Balance>,
		) -> AllocationResult<AccountId, Balance>,
	) -> DispatchResult;
}
