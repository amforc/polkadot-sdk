//! # pUSD Primitives
//!
//! Shared types and traits for the pUSD protocol pallets (vaults, redemptions,
//! stability pool, ...). Carries no pallet-specific assumptions:
//! every type is parameterised over the consumer's `AccountId`, `AssetId`,
//! `Balance`, and credit/debt imbalance shapes.

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, DecodeWithMemTracking, Encode};
use core::cmp::Ordering;
use frame::{
	arithmetic::{helpers_128bit::multiply_by_rational_with_rounding, Rounding, Zero},
	deps::{
		frame_support::PalletId,
		sp_io::hashing::blake2_256,
		sp_runtime::{
			traits::AccountIdConversion, ArithmeticError, FixedPointNumber, FixedPointOperand,
			FixedU128,
		},
	},
};
use scale_info::TypeInfo;

pub mod branch_mode;
pub mod debit;
pub mod oracle;
pub mod recovery_offset;
pub mod recovery_pricing;
pub mod registration;
pub mod stability_pool;
pub mod vault_interface;
pub mod yield_routing;

pub use branch_mode::{BranchMode, BranchModeProvider};
pub use debit::{debit_preservation, reducible_debit};
pub use oracle::{OraclePriceConversion, ProvidePrice, ZERO_ORACLE_PRICE};
pub use recovery_offset::{RecoveryOffsetInterface, RecoveryOffsetResult};
pub use recovery_pricing::InsuranceAdjusted;
pub use registration::OnBranchLifecycle;
pub use stability_pool::{OffsetLegs, StabilityPoolInspect, StabilityPoolOffset};
pub use vault_interface::{RedemptionSettlement, RedemptionStepSnapshot, VaultInterface};
pub use yield_routing::OnBranchYield;

/// TODO: Check if this is the best way to handle the "time"
pub type Millis = u64;

pub const MILLIS_PER_YEAR: Millis = 31_557_600_000;

/// Lifecycle status of a vault. The single classification shared by the vault
/// pallet's storage/events and the redemption surface: queue membership
/// derives it, target selection returns it, and step pricing keys off it
/// (`Active` and `Dormant` redeem at face value, `FinalRecovery` at
/// recovery-settlement pricing).
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
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

/// Debt and its associated collateral.
///
/// The pair is used both for live positions and for amounts assigned by a
/// settlement path.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub struct DebtCollateral<Balance> {
	/// Debt side of the pair.
	pub debt: Balance,
	/// Collateral side of the pair.
	pub collateral: Balance,
}

/// A position's collateralization ratio.
///
/// A debt-free position has no ratio to compute, yet it is safer than any
/// position that does: the variant order makes `DebtFree` compare greater
/// than every `Ratio`, and the [`FixedU128`] comparisons below let a threshold
/// gate read `cr >= threshold` without special-casing it.
#[derive(Encode, TypeInfo, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum CollateralRatio {
	/// `floor(price * collateral / debt)` for a debt-bearing position.
	Ratio(FixedU128),
	/// The position carries no debt.
	DebtFree,
}

impl PartialEq<FixedU128> for CollateralRatio {
	fn eq(&self, threshold: &FixedU128) -> bool {
		match self {
			Self::Ratio(ratio) => ratio == threshold,
			Self::DebtFree => false,
		}
	}
}

impl PartialOrd<FixedU128> for CollateralRatio {
	fn partial_cmp(&self, threshold: &FixedU128) -> Option<Ordering> {
		match self {
			Self::Ratio(ratio) => ratio.partial_cmp(threshold),
			Self::DebtFree => Some(Ordering::Greater),
		}
	}
}

/// The collateralization ratio of `position` at `price`.
///
/// `DebtFree` when `debt == 0`; `Overflow` when the value or the ratio does
/// not fit.
pub fn collateralization_ratio<Balance: FixedPointOperand>(
	position: &DebtCollateral<Balance>,
	price: FixedU128,
) -> Result<CollateralRatio, ArithmeticError> {
	if position.debt.is_zero() {
		return Ok(CollateralRatio::DebtFree);
	}
	let value = price.checked_mul_int(position.collateral).ok_or(ArithmeticError::Overflow)?;
	FixedU128::checked_from_rational(value, position.debt)
		.map(CollateralRatio::Ratio)
		.ok_or(ArithmeticError::Overflow)
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

/// Returns `floor(value * rate / denominator)` as a `FixedU128` per-unit delta.
///
/// Returns `Some(0)` for a zero value or rate. Returns `None` for a zero denominator or overflow.
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

/// Returns a pallet sub-account for one `(collateral, stable)` market.
///
/// The function hashes the complete encoded asset pair with Blake2-256. Thus, long asset
/// identifiers do not lose bytes before the function calculates the digest.
///
/// The digest has a fixed-size byte encoding. [`AccountIdConversion::into_sub_account_truncating`]
/// truncates the encoded sub-account value only to fit `AccountId`. Different `PalletId` values
/// keep the sub-accounts of sibling pallets separate.
pub fn market_sub_account<AccountId, CollateralId, StableId>(
	pallet_id: PalletId,
	collateral_id: &CollateralId,
	stable_id: &StableId,
) -> AccountId
where
	AccountId: Encode + Decode,
	CollateralId: Encode,
	StableId: Encode,
{
	let seed = blake2_256(&(collateral_id, stable_id).encode());
	pallet_id.into_sub_account_truncating(seed)
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame::arithmetic::{FixedPointNumber, One, Saturating};

	#[test]
	fn mul_div_rate_floor_round_trips_small_inputs() {
		let got = mul_div_rate_floor::<u128>(100, FixedU128::one(), 1_000).expect("fits");
		assert_eq!(got, FixedU128::from_rational(1u128, 10u128));
	}

	#[test]
	fn mul_div_rate_zero_value_returns_zero() {
		assert_eq!(mul_div_rate_floor::<u128>(0, FixedU128::one(), 1_000), Some(FixedU128::zero()));
	}

	#[test]
	fn mul_div_rate_floor_overflow_returns_none() {
		let got = mul_div_rate_floor(u128::MAX / 2, FixedU128::one(), 1);
		assert!(got.is_none());
		// Confirm that the function does not reject values below the overflow limit.
		let safe = mul_div_rate_floor(u128::MAX / (FixedU128::DIV * 2), FixedU128::one(), 1);
		assert!(safe.is_some());
	}

	#[test]
	fn mul_div_rate_floor_matches_two_step_when_safe() {
		let rate = FixedU128::from_rational(5u128, 100u128);
		let got = mul_div_rate_floor::<u128>(10_000, rate, 100).expect("fits");
		let two_step = FixedU128::from_rational(10_000u128, 100u128).saturating_mul(rate);
		assert!(got.into_inner().abs_diff(two_step.into_inner()) <= 1);
	}

	#[test]
	fn mul_div_rate_floor_avoids_two_step_overflow() {
		// The complete formula fits although `value / denominator` does not fit in `FixedU128`.
		let rate = FixedU128::from_inner(1);
		let got = mul_div_rate_floor(u128::MAX / 4, rate, 1);
		assert_eq!(got, Some(FixedU128::from_inner(u128::MAX / 4)));
	}
}
