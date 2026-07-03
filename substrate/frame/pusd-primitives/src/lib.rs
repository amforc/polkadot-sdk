//! # pUSD Primitives
//!
//! Shared types and traits for the pUSD protocol pallets (vaults, redemptions,
//! liquidation, stability pool, ...). Carries no pallet-specific assumptions:
//! every type is parameterised over the consumer's `AccountId`, `AssetId`,
//! `Balance`, and credit/debt imbalance shapes.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use frame::deps::sp_runtime::{FixedPointNumber, FixedPointOperand, FixedU128};

pub mod bad_debt;
pub mod liquidation;
pub mod oracle;
pub mod recovery_pricing;
pub mod redemption;
pub mod registration;

pub use bad_debt::VaultBadDebtInterface;
pub use liquidation::{
	AllocationResult, KeeperCompensation, LiquidationAllocation, LiquidationSnapshot,
	OffsetAllocation, VaultLiquidationInterface,
};
pub use oracle::ProvidePrice;
pub use recovery_pricing::InsuranceAdjusted;
pub use redemption::{
	RedemptionAllocation, RedemptionRegime, RedemptionStepSnapshot, RedemptionTarget,
	RedemptionTargetKind, VaultRedemptionInterface,
};
pub use registration::OnBranchLifecycle;

/// TODO: Check if this is the best way to handle the "time"
pub type Millis = u64;

pub const MILLIS_PER_YEAR: Millis = 31_557_600_000;

/// `floor(price * collateral / debt)` as a collateralization ratio. `None` when
/// `debt == 0` (CR undefined) or either step overflows.
pub fn collateralization_ratio<Balance: FixedPointOperand>(
	collateral: Balance,
	debt: Balance,
	price: FixedU128,
) -> Option<FixedU128> {
	let value = price.checked_mul_int(collateral)?;
	FixedU128::checked_from_rational(value, debt)
}
