//! # pUSD Primitives
//!
//! Shared types and traits for the pUSD protocol pallets (vaults, redemptions,
//! liquidation, stability pool, ...). Carries no pallet-specific assumptions:
//! every type is parameterised over the consumer's `AccountId`, `AssetId`,
//! `Balance`, and credit/debt imbalance shapes.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::{
	arithmetic::{helpers_128bit::multiply_by_rational_with_rounding, Rounding, Zero},
	deps::sp_runtime::{FixedPointNumber, FixedPointOperand, FixedU128},
};
use scale_info::TypeInfo;

pub mod branch_mode;
pub mod debit;
pub mod list_id;
pub mod oracle;
pub mod recovery_offset;
pub mod recovery_pricing;
pub mod registration;
pub mod stability_pool;
pub mod vault_interface;
pub mod yield_routing;

pub use branch_mode::{BranchMode, BranchModeProvider};
pub use debit::{debit_preservation, reducible_debit};
pub use list_id::StableListId;
pub use oracle::ProvidePrice;
pub use recovery_offset::{RecoveryOffsetInterface, RecoveryOffsetResult};
pub use recovery_pricing::InsuranceAdjusted;
pub use registration::OnBranchLifecycle;
pub use stability_pool::StabilityPoolOffsetApi;
pub use vault_interface::{
	LiquidationSettlement, LiquidationSnapshot, RedemptionSettlement, RedemptionStepSnapshot,
	VaultInterface,
};
pub use yield_routing::OnBranchYield;

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

/// `floor(value * numerator / denominator)` at `Balance` precision — the
/// shared pro-rata building block of the pUSD math modules. `None` when
/// `denominator` is zero, the product overflows, or the result exceeds
/// `Balance`; callers pick their own defensive fallback.
pub fn mul_div_floor<Balance: FixedPointOperand>(
	value: Balance,
	numerator: Balance,
	denominator: Balance,
) -> Option<Balance> {
	if denominator.is_zero() {
		return None;
	}
	multiply_by_rational_with_rounding(
		value.unique_saturated_into(),
		numerator.unique_saturated_into(),
		denominator.unique_saturated_into(),
		Rounding::Down,
	)
	.and_then(|raw| Balance::try_from(raw).ok())
}

/// `floor(value * rate / denominator)` reinterpreted as a `FixedU128` per-unit
/// delta — the shared per-stake accumulator step for vault redistribution
/// weights. `Some(0)` when there is nothing to distribute, `None` when the
/// stake denominator is zero or the product overflows `u128`.
pub fn mul_div_rate_floor<Balance: FixedPointOperand>(
	value: Balance,
	rate: FixedU128,
	denominator: Balance,
) -> Option<FixedU128> {
	if value.is_zero() || rate.is_zero() {
		return Some(FixedU128::zero());
	}
	if denominator.is_zero() {
		return None;
	}
	multiply_by_rational_with_rounding(
		value.unique_saturated_into(),
		rate.into_inner(),
		denominator.unique_saturated_into(),
		Rounding::Down,
	)
	.map(FixedU128::from_inner)
}
