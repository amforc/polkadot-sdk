//! # pUSD Primitives
//!
//! Shared types and traits for the pUSD protocol pallets (vaults, redemptions,
//! liquidation, stability pool, ...). Carries no pallet-specific assumptions:
//! every type is parameterised over the consumer's `AccountId`, `AssetId`,
//! `Balance`, and credit/debt imbalance shapes.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::deps::sp_runtime::{FixedPointNumber, FixedPointOperand, FixedU128};
use scale_info::TypeInfo;

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
pub use redemption::{RedemptionAllocation, RedemptionStepSnapshot, VaultRedemptionInterface};
pub use registration::OnBranchLifecycle;

/// TODO: Check if this is the best way to handle the "time"
pub type Millis = u64;

pub const MILLIS_PER_YEAR: Millis = 31_557_600_000;

/// Lifecycle status of a vault. The single classification shared by the vault
/// pallet's storage/events and the redemption surface: queue membership
/// derives it, target selection returns it, and step pricing keys off it
/// (`Active` and `Dormant` redeem at face value, `FinalRecovery` at
/// recovery-settlement pricing).
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
)]
pub enum VaultStatus {
	/// Debt-bearing vault with `Debt >= MinimumDebt`. In the rate index.
	Active,
	/// Below `MinimumDebt` (possibly zero) after redemption. Out of the rate
	/// index, may be revived to `Active`.
	Dormant,
	/// Below-MCR last-eligible vault parked in the FIFO and resolved by
	/// recovery redemptions / offsets.
	FinalRecovery,
}

impl VaultStatus {
	/// Debt-bearing vault, present in the rate index.
	pub fn is_active(&self) -> bool {
		matches!(self, Self::Active)
	}

	/// Drained below `minimum_debt`, out of the rate index.
	pub fn is_dormant(&self) -> bool {
		matches!(self, Self::Dormant)
	}

	/// Parked in the FIFO awaiting recovery settlement.
	pub fn is_final_recovery(&self) -> bool {
		matches!(self, Self::FinalRecovery)
	}
}

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
