//! Redemption policy, per-stablecoin fee state, and the types calls and events carry.

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::deps::sp_runtime::{traits::Zero, FixedU128, Permill};
use scale_info::TypeInfo;

pub use stablecoin_primitives::Millis;

/// Defines the ordinary-redemption fee policy.
///
/// The fee rate is `min(base_fee + dynamic fee, fee_ceiling)`. Each redemption pays the constant
/// base fee and a dynamic component.
///
/// Time decreases the dynamic component, and redemption volume increases it. The fee integrates
/// this component over the debt that the redemption cancels.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct RedemptionConfig<Balance> {
	/// Smallest redemption, in stablecoin units. Any redemption whose `max_stable_to_spend` is
	/// below it (`<`) is refused, and so is an ordinary one whose budget buys less debt than it
	/// once the fee is paid. Must be nonzero.
	pub minimum_redemption_amount: Balance,
	/// Half-life of the decaying dynamic fee.
	pub dynamic_fee_decay_period: Millis,
	/// Lower bound (inclusive) of the dynamic fee component, as a rate (`0.01` is 1%). Decay
	/// never takes the component below it. Must not exceed `dynamic_fee_ceiling`.
	pub dynamic_fee_floor: FixedU128,
	/// Upper bound (inclusive) of the dynamic fee component, as a rate. Redemption volume never
	/// raises the component above it; `fee_ceiling` still caps the total rate.
	pub dynamic_fee_ceiling: FixedU128,
	/// Constant fee component every ordinary redemption pays (e.g. 0.5%).
	pub base_fee: Permill,
	/// Cap on the total fee rate, base and dynamic components combined.
	pub fee_ceiling: Permill,
	/// Divisor applied to the redeemed share of the current stablecoin-wide debt.
	///
	/// Thus, redemption of debt fraction `u` increases the dynamic fee by `u / divisor`. The
	/// redemption pays the mean of the arrival and terminal dynamic fees.
	pub dynamic_fee_increase_divisor: FixedU128,
	/// Prevents the recovery bonus from worsening a `CR >= 100%` recovery vault.
	pub final_recovery_bonus_buffer: Permill,
}

impl<Balance: Zero> RedemptionConfig<Balance> {
	/// Zero thresholds/divisors and inverted ranges break fee and loop semantics.
	pub(crate) fn is_valid(&self) -> bool {
		if self.minimum_redemption_amount.is_zero() {
			return false;
		}
		if self.dynamic_fee_decay_period.is_zero() {
			return false;
		}
		if self.dynamic_fee_floor > self.dynamic_fee_ceiling {
			return false;
		}
		if self.base_fee > self.fee_ceiling {
			return false;
		}
		!self.dynamic_fee_increase_divisor.is_zero()
	}
}

/// Redeemer-supplied execution terms.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub struct RedemptionTerms<Balance> {
	/// Maximum stable-asset amount that the redeemer spends, including the redemption fee.
	pub max_stable_to_spend: Balance,
	/// Collateral floor when the whole `max_stable_to_spend` is spent.
	///
	/// Partial fills scale this floor pro rata to the stable amount spent.
	pub min_collateral_out: Balance,
}

/// A stablecoin's redemption policy and the fee state priced against it.
///
/// Both share one lifecycle: the coin's first branch seeds them and its last branch clears them.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct RedemptionInfo<Balance> {
	/// Policy set by the first branch and replaced by governance.
	pub config: RedemptionConfig<Balance>,
	/// Dynamic fee as of the last ordinary redemption; zero until the first one.
	pub fee_state: RedemptionState,
}

/// Dynamic fee of a stablecoin as of its last ordinary redemption.
///
/// The fee decays over time without a write, so the stored value is not the current fee.
/// [`crate::Pallet::current_fee_rate`] returns the current rate, decayed and with the base fee.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RedemptionState {
	/// Dynamic fee right after the last ordinary redemption, before any decay since.
	pub dynamic_fee: FixedU128,
	/// Time of the last ordinary redemption, from which the decay runs.
	pub last_fee_operation: Millis,
}

/// How a `FinalRecovery` settlement was priced, as reported by its event.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecoveryRegime {
	/// The vault was at or above par: the redeemer bought collateral at face value plus a bonus,
	/// capped by the vault's excess collateral less a buffer and by its redistribution penalty,
	/// and zero past ICR.
	RecoveryBonus,
	/// The vault was below par: pricing used debt less Insurance Fund cover, capped at the
	/// shortfall and the fund's available balance. The redeemer bears any uncovered shortfall.
	/// Only a full fill draws the cover; partial fills receive collateral pro rata at that price.
	InsuranceAdjusted,
}
