use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::deps::sp_runtime::{
	traits::{Saturating, Zero},
	FixedPointOperand, FixedU128, Permill,
};
use scale_info::TypeInfo;

pub use pusd_primitives::{Millis, VaultStatus};

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
	/// Divides the redeemed branch-debt fraction before it raises the
	/// dynamic fee after an ordinary redemption.
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
	/// Stable asset consumed, including the redemption fee.
	pub stable_in: Balance,
	/// Collateral paid to the recipient.
	pub collateral_out: Balance,
	/// Portion of `stable_in` routed as the redemption fee.
	pub fee: Balance,
	/// Targets inspected, including skipped targets and barriers.
	pub steps: u32,
	/// The step cap stopped the quote while budget remained.
	pub truncated: bool,
}

impl<Balance: Saturating + Copy> RedemptionQuote<Balance> {
	/// Stable asset burned against vault debt.
	pub fn debt_cancelled(&self) -> Balance {
		self.stable_in.saturating_sub(self.fee)
	}
}

pub(crate) struct OrdinaryStep<Balance> {
	pub(crate) debt: Balance,
	pub(crate) collateral_out: Balance,
	pub(crate) fee: Balance,
}

pub(crate) struct RecoveryStep<Balance> {
	pub(crate) burned: Balance,
	pub(crate) collateral_out: Balance,
	pub(crate) regime: RecoveryRegime,
}

/// Authoritative priced plan for one `FinalRecovery` head. Execution, quoting,
/// and recovery offsets all consume this type so regime and residual policy
/// cannot be re-derived independently.
pub(crate) enum RecoveryPricing<Balance> {
	/// `CR >= 100%`: face value plus a collateral bonus.
	RecoveryBonus { debt: Balance, collateral_out: Balance, bonus: FixedU128 },
	/// `CR < 100%`: the Insurance Fund cover splits the debt.
	InsuranceAdjusted {
		debt: Balance,
		collateral_out: Balance,
		split: pusd_primitives::InsuranceAdjusted<Balance>,
	},
}

impl<Balance: Copy> RecoveryPricing<Balance> {
	pub(crate) fn debt(&self) -> Balance {
		match self {
			Self::RecoveryBonus { debt, .. } | Self::InsuranceAdjusted { debt, .. } => *debt,
		}
	}

	pub(crate) fn collateral_out(&self) -> Balance {
		match self {
			Self::RecoveryBonus { collateral_out, .. } |
			Self::InsuranceAdjusted { collateral_out, .. } => *collateral_out,
		}
	}

	pub(crate) fn regime(&self) -> RecoveryRegime {
		match self {
			Self::RecoveryBonus { .. } => RecoveryRegime::RecoveryBonus,
			Self::InsuranceAdjusted { .. } => RecoveryRegime::InsuranceAdjusted,
		}
	}
}

impl<Balance: FixedPointOperand + Ord> RecoveryPricing<Balance> {
	/// Resize a priced plan without selecting its regime or Insurance Fund
	/// split again.
	pub(crate) fn rebudget(
		self,
		vault_debt: Balance,
		vault_collateral: Balance,
		price: FixedU128,
		budget: Balance,
	) -> Self {
		match self {
			Self::RecoveryBonus { bonus, .. } => {
				let debt = vault_debt.min(budget);
				let collateral_out =
					pusd_primitives::recovery_pricing::recovery_bonus_collateral_out(
						debt, bonus, price,
					)
					.min(vault_collateral);
				Self::RecoveryBonus { debt, collateral_out, bonus }
			},
			Self::InsuranceAdjusted { split, .. } => {
				let debt = split.market_cancel_debt.min(budget);
				let collateral_out =
					pusd_primitives::recovery_pricing::recovery_rate_collateral_out(
						debt,
						split.recovery_rate,
						price,
					)
					.min(vault_collateral);
				Self::InsuranceAdjusted { debt, collateral_out, split }
			},
		}
	}

	/// Whether committing this priced step exhausts the externally cancellable
	/// debt and therefore unlocks the Insurance Fund residual settlement.
	pub(crate) fn settles_residual(&self) -> bool {
		match self {
			Self::RecoveryBonus { .. } => false,
			Self::InsuranceAdjusted { debt, split, .. } => {
				*debt == split.market_cancel_debt && !split.effective_cover.is_zero()
			},
		}
	}
}

/// Per-target decision shared by execution and read-only quoting.
pub(crate) enum StepAction {
	Recovery,
	Redeem,
	Skip,
	Stop,
}

/// Offset classification of a priced `FinalRecovery` head, shared by the
/// quote and execution surfaces so the two cannot diverge.
pub(crate) enum OffsetDecision<Balance> {
	NoTarget,
	BelowPar,
	Cancellable { debt: Balance, collateral_out: Balance },
}

/// What one vault-side `redeem_step` did, escaped from the pricing closure by
/// `&mut` capture and consumed by `run_loop` to steer the walk.
pub(crate) enum StepOutcome<Balance> {
	/// An ordinary (or dormant-target) vault was redeemed at face value.
	Redeemed(OrdinaryStep<Balance>),
	/// A `FinalRecovery` vault was (partially) settled. `settle_residual`
	/// defers the Insurance-Fund settlement to the loop: it re-enters the
	/// vault pallet, which the in-flight step must not do.
	Recovery { step: RecoveryStep<Balance>, settle_residual: bool },
	/// Unredeemable target skipped; the cursor advances past it.
	Skipped,
	/// A barrier or an unpriceable target ended the walk.
	Stopped,
}

pub(crate) struct WalkResult<Balance> {
	pub(crate) remaining: Balance,
	pub(crate) steps: u32,
}

pub(crate) struct Accumulators<Balance, AccountId> {
	pub(crate) debt_settled: Balance,
	pub(crate) ordinary_debt: Balance,
	pub(crate) ordinary_collateral: Balance,
	pub(crate) ordinary_fee: Balance,
	pub(crate) recovery_burned: Balance,
	pub(crate) recovery_collateral: Balance,
	// Recovery stops after one FIFO head, so one owner/regime is enough.
	pub(crate) recovery_owner: Option<(AccountId, RecoveryRegime)>,
}

impl<Balance: Zero, AccountId> Default for Accumulators<Balance, AccountId> {
	fn default() -> Self {
		Self {
			debt_settled: Balance::zero(),
			ordinary_debt: Balance::zero(),
			ordinary_collateral: Balance::zero(),
			ordinary_fee: Balance::zero(),
			recovery_burned: Balance::zero(),
			recovery_collateral: Balance::zero(),
			recovery_owner: None,
		}
	}
}

impl<Balance, AccountId> Accumulators<Balance, AccountId>
where
	Balance: Zero + Saturating + Copy,
{
	pub(crate) fn collateral_out(&self) -> Balance {
		self.ordinary_collateral.saturating_add(self.recovery_collateral)
	}

	pub(crate) fn apply_ordinary(&mut self, step: &OrdinaryStep<Balance>) {
		self.debt_settled = self.debt_settled.saturating_add(step.debt);
		self.ordinary_debt = self.ordinary_debt.saturating_add(step.debt);
		self.ordinary_collateral = self.ordinary_collateral.saturating_add(step.collateral_out);
		self.ordinary_fee = self.ordinary_fee.saturating_add(step.fee);
	}

	/// `residual` is the Insurance-Fund-settled debt the loop obtained after
	/// the step committed; it settles debt without consuming redeemer pUSD.
	pub(crate) fn apply_recovery(
		&mut self,
		owner: AccountId,
		step: &RecoveryStep<Balance>,
		residual: Balance,
	) {
		self.debt_settled = self.debt_settled.saturating_add(step.burned).saturating_add(residual);
		self.recovery_burned = self.recovery_burned.saturating_add(step.burned);
		self.recovery_collateral = self.recovery_collateral.saturating_add(step.collateral_out);
		self.recovery_owner = Some((owner, step.regime));
	}
}

/// Shared validation and fee-rate setup for execution and quoting.
pub(crate) struct RedemptionPreamble<Balance> {
	pub(crate) config: RedemptionConfig<Balance>,
	pub(crate) state: RedemptionState,
	pub(crate) price: FixedU128,
	pub(crate) now: Millis,
	pub(crate) decayed: FixedU128,
	pub(crate) fee_rate: FixedU128,
}
