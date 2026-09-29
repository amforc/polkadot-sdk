//! # pUSD Primitives
//!
//! Types and traits shared by the pUSD pallets (vaults, redemptions, stability pool).
//! Everything is generic over the runtime's `AccountId`, `AssetId`, `Balance` and imbalances.

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, DecodeWithMemTracking, Encode};
use core::cmp::Ordering;
use frame::deps::{
	frame_support::PalletId,
	sp_io::hashing::blake2_256,
	sp_runtime::{traits::AccountIdConversion, FixedU128},
};
use scale_info::TypeInfo;

pub mod branch_interface;
pub mod debit;
pub mod math;
pub mod oracle;
pub mod origin;
pub mod recovery_offset;
pub mod recovery_pricing;
pub mod registration;
pub mod stability_pool;
pub mod vault_interface;
pub mod yield_routing;

pub use branch_interface::{BranchInterface, BranchMode, BranchSnapshot};
pub use debit::{debit_preservation, reducible_debit, refine_debit};
pub use math::{collateralization_ratio, mul_div_floor, mul_div_rate_floor};
pub use oracle::{OraclePriceConversion, ProvidePrice};
pub use origin::EnsureStableOwnerOrRoot;
pub use recovery_offset::{RecoveryOffsetInterface, RecoveryOffsetResult};
pub use registration::OnBranchLifecycle;
pub use stability_pool::{OffsetLegs, StabilityPoolInspect, StabilityPoolOffset};
pub use vault_interface::{RedemptionSettlement, RedemptionStepSnapshot, VaultInterface};
pub use yield_routing::OnBranchYield;

/// TODO: Check if this is the best way to handle the "time"
pub type Millis = u64;

/// Milliseconds in a Julian year (365.25 days).
pub const MILLIS_PER_YEAR: Millis = 31_557_600_000;

/// Vault lifecycle status, shared by the vaults and redemptions pallets.
///
/// Redemption pricing keys off it: `Active` and `Dormant` redeem at face value,
/// `FinalRecovery` at recovery-settlement pricing.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub enum VaultStatus {
	/// Debt at or above `MinimumDebt`; in the rate index.
	Active,
	/// Debt below `MinimumDebt` (possibly zero) after redemption; out of the rate index.
	/// May be revived to `Active`.
	Dormant,
	/// Below-MCR last eligible vault, queued in the FIFO for recovery redemptions and offsets.
	FinalRecovery,
}

impl VaultStatus {
	/// Returns `true` for [`Self::Active`].
	pub fn is_active(&self) -> bool {
		matches!(self, Self::Active)
	}

	/// Returns `true` for [`Self::Dormant`].
	pub fn is_dormant(&self) -> bool {
		matches!(self, Self::Dormant)
	}

	/// Returns `true` for [`Self::FinalRecovery`].
	pub fn is_final_recovery(&self) -> bool {
		matches!(self, Self::FinalRecovery)
	}
}

/// A debt amount and its collateral, for live positions or settlement amounts.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub struct DebtCollateral<Balance> {
	pub debt: Balance,
	pub collateral: Balance,
}

/// A position's collateralization ratio.
///
/// `DebtFree` orders above every `Ratio` and compares greater than any [`FixedU128`], so a
/// threshold check reads `cr >= threshold` without a special case.
#[derive(Encode, TypeInfo, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum CollateralRatio {
	/// `floor(price * collateral / debt)`.
	Ratio(FixedU128),
	/// No debt.
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

/// Returns the pallet sub-account for a `(collateral, stable)` market.
///
/// The seed is the Blake2-256 hash of the encoded pair, so long asset IDs are never truncated;
/// distinct `PalletId`s keep sibling pallets' sub-accounts apart.
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
