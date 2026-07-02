use alloc::vec::Vec;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::deps::sp_runtime::{traits::Zero, FixedU128};
use scale_info::TypeInfo;

pub use pusd_primitives::RedemptionTargetKind;

#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct RedemptionConfig<Balance, Moment> {
	pub minimum_redemption_amount: Balance,
	pub base_rate_decay_period: Moment,
	pub base_rate_floor: FixedU128,
	pub base_rate_ceiling: FixedU128,
	pub redemption_fee_floor: FixedU128,
	pub redemption_fee_ceiling: FixedU128,
	pub base_rate_increase_divisor: FixedU128,
	/// Prevents the recovery bonus from worsening a `CR >= 100%` recovery vault.
	pub final_recovery_bonus_buffer: FixedU128,
}

impl<Balance: Zero, Moment: Zero> RedemptionConfig<Balance, Moment> {
	/// Zero thresholds/divisors and inverted ranges break fee and loop semantics.
	pub fn is_valid(&self) -> bool {
		if self.minimum_redemption_amount.is_zero() {
			return false;
		}
		if self.base_rate_decay_period.is_zero() {
			return false;
		}
		if self.base_rate_floor > self.base_rate_ceiling {
			return false;
		}
		if self.redemption_fee_floor > self.redemption_fee_ceiling {
			return false;
		}
		!self.base_rate_increase_divisor.is_zero()
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
	pub base_rate: FixedU128,
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
	pub kind: RedemptionTargetKind,
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
