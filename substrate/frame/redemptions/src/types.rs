use alloc::vec::Vec;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::deps::sp_runtime::{traits::Zero, FixedU128, Permill};
use scale_info::TypeInfo;

pub use pusd_primitives::VaultStatus;

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
	/// [`pusd_primitives::RecoveryOffsetOutcome`]; a quote only sizes the
	/// burn.
	Available { debt: Balance },
}

/// The ordinary-redemption fee rate is
/// `min(base_fee + decayed dynamic fee, fee_ceiling)`: a constant base every
/// redemption pays plus a decaying component that redemption volume raises.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct RedemptionConfig<Balance, Moment> {
	pub minimum_redemption_amount: Balance,
	/// Half-life of the decaying dynamic fee.
	pub dynamic_fee_decay_period: Moment,
	pub dynamic_fee_floor: FixedU128,
	pub dynamic_fee_ceiling: FixedU128,
	/// Constant fee component every ordinary redemption pays (e.g. 0.5%).
	pub base_fee: Permill,
	/// Cap on the total fee rate, base and dynamic components combined.
	pub fee_ceiling: Permill,
	/// Divides the redeemed branch-debt fraction before it raises the
	/// dynamic fee after an ordinary redemption.
	pub dynamic_fee_increase_divisor: FixedU128,
	/// Prevents the recovery bonus from worsening a `CR >= 100%` recovery vault.
	pub final_recovery_bonus_buffer: Permill,
}

impl<Balance: Zero, Moment: Zero> RedemptionConfig<Balance, Moment> {
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
	Default,
)]
pub struct RedemptionState<Moment> {
	pub dynamic_fee: FixedU128,
	pub last_fee_operation: Moment,
}

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
pub enum RecoveryRegime {
	RecoveryBonus,
	InsuranceAdjusted,
}

#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct RedemptionPreviewStep<AccountId, Balance> {
	pub target: AccountId,
	pub status: VaultStatus,
	pub debt_cancellable: Balance,
	pub collateral_out: Balance,
	pub fee_pusd: Balance,
	pub pusd_in: Balance,
}

#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct RedemptionPreview<AccountId, Balance> {
	pub steps_detail: Vec<RedemptionPreviewStep<AccountId, Balance>>,
	pub total_pusd_in: Balance,
	pub total_collateral_out: Balance,
	pub total_fee_pusd: Balance,
	pub steps: u32,
	pub truncated: bool,
}
