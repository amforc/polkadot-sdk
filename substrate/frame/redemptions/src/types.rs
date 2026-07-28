use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::deps::sp_runtime::{
	traits::{Saturating, Zero},
	FixedU128, Permill,
};
use scale_info::TypeInfo;

pub use pusd_primitives::Millis;

/// Head-of-FIFO quote at shared settlement pricing
/// ([`crate::Pallet::preview_recovery_offset`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecoveryOffsetQuote<Balance> {
	/// No `FinalRecovery` vault is queued: deposits proceed as ordinary
	/// pending deposits, and active-pool recovery offsets have no target.
	NoTarget,
	/// The head is below par (`CR < 100%`). Recovery offsets are
	/// unavailable and new pool deposits must be rejected — settlement at
	/// a discount stays exclusive to the explicit redemption pathway.
	BelowPar,
	/// Up to `debt` is cancellable against the head. The collateral the
	/// settlement pays out arrives with the execution's
	/// [`pusd_primitives::RecoveryOffsetResult::Applied`]; a quote only sizes the
	/// burn.
	Available { debt: Balance },
}

/// The ordinary-redemption fee rate is
/// `min(base_fee + decayed dynamic fee, fee_ceiling)`: a constant base every
/// redemption pays plus a decaying component that redemption volume raises.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct RedemptionConfig<Balance> {
	pub minimum_redemption_amount: Balance,
	/// Half-life of the decaying dynamic fee.
	pub dynamic_fee_decay_period: Millis,
	pub dynamic_fee_floor: FixedU128,
	pub dynamic_fee_ceiling: FixedU128,
	/// Constant fee component every ordinary redemption pays (e.g. 0.5%).
	pub base_fee: Permill,
	/// Cap on the total fee rate, base and dynamic components combined.
	pub fee_ceiling: Permill,
	/// Divides the redeemed stablecoin-wide debt fraction before it raises
	/// the dynamic fee after an ordinary redemption.
	pub dynamic_fee_increase_divisor: FixedU128,
	/// Prevents the recovery bonus from worsening a `CR >= 100%` recovery vault.
	pub final_recovery_bonus_buffer: Permill,
}

impl<Balance: Zero> RedemptionConfig<Balance> {
	/// Zero thresholds/divisors and inverted ranges break fee and loop semantics.
	pub fn is_valid(&self) -> bool {
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

#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RedemptionState {
	pub dynamic_fee: FixedU128,
	pub last_fee_operation: Millis,
}

#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecoveryRegime {
	RecoveryBonus,
	InsuranceAdjusted,
}

#[derive(Encode, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub struct RedemptionQuote<Balance> {
	/// Debt cancelled using the redeemer's stable assets. This excludes both
	/// the redemption fee and any Insurance Fund residual settlement.
	pub debt_cancelled: Balance,
	/// Collateral paid to the recipient.
	pub collateral_out: Balance,
	/// Stable assets routed as the redemption fee.
	pub fee: Balance,
	/// Targets inspected, including skipped targets and barriers.
	pub steps: u32,
	/// The step cap stopped the quote while budget remained.
	pub truncated: bool,
}

impl<Balance: Zero> Default for RedemptionQuote<Balance> {
	fn default() -> Self {
		Self {
			debt_cancelled: Balance::zero(),
			collateral_out: Balance::zero(),
			fee: Balance::zero(),
			steps: 0,
			truncated: false,
		}
	}
}

impl<Balance: Saturating + Copy> RedemptionQuote<Balance> {
	/// Total stable assets the redeemer must supply.
	pub fn stable_in(&self) -> Balance {
		self.debt_cancelled.saturating_add(self.fee)
	}
}
