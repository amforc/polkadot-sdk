use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::deps::sp_runtime::{
	traits::{Saturating, Zero},
	FixedPointOperand, FixedU128, Permill,
};
use scale_info::TypeInfo;

use pusd_primitives::RedemptionStepSnapshot;

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

impl<Balance: Zero> Default for RedemptionQuote<Balance> {
	fn default() -> Self {
		Self {
			stable_in: Balance::zero(),
			collateral_out: Balance::zero(),
			fee: Balance::zero(),
			steps: 0,
			truncated: false,
		}
	}
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
		snap: &RedemptionStepSnapshot<Balance>,
		price: FixedU128,
		budget: Balance,
	) -> Self {
		match self {
			Self::RecoveryBonus { bonus, .. } => {
				let debt = snap.debt.min(budget);
				let collateral_out =
					pusd_primitives::recovery_pricing::recovery_bonus_collateral_out(
						debt, bonus, price,
					)
					.min(snap.collateral);
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
					.min(snap.collateral);
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

/// One classified-and-priced step, shared by execution and quoting so the
/// classify→price ladder cannot drift between them.
pub(crate) enum PricedStep<Balance> {
	/// Redeem an ordinary (or dormant-target) vault at face value.
	Redeem(OrdinaryStep<Balance>),
	/// Settle the `FinalRecovery` head with this priced plan.
	Recovery(RecoveryPricing<Balance>),
	/// Unredeemable target; the walk may skip past it.
	Skip,
	/// A barrier, an unpriceable target, or a zero-sized step with no
	/// residual to settle ends the walk.
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

/// The one `FinalRecovery` settlement a walk may perform: the walk breaks
/// after its first recovery step, so at most one record exists.
pub(crate) struct RecoveryOutcome<Balance, AccountId> {
	pub(crate) owner: AccountId,
	pub(crate) regime: RecoveryRegime,
	/// Redeemer-funded stablecoin burned against the vault debt.
	pub(crate) burned: Balance,
	/// Collateral paid to the recipient.
	pub(crate) collateral_out: Balance,
	/// Insurance-Fund-settled debt obtained after the step committed; it
	/// settles debt without consuming redeemer stablecoin.
	pub(crate) residual: Balance,
}

pub(crate) struct Accumulators<Balance, AccountId> {
	pub(crate) ordinary_debt: Balance,
	pub(crate) ordinary_collateral: Balance,
	pub(crate) recovery: Option<RecoveryOutcome<Balance, AccountId>>,
}

impl<Balance: Zero, AccountId> Default for Accumulators<Balance, AccountId> {
	fn default() -> Self {
		Self {
			ordinary_debt: Balance::zero(),
			ordinary_collateral: Balance::zero(),
			recovery: None,
		}
	}
}

impl<Balance, AccountId> Accumulators<Balance, AccountId>
where
	Balance: Zero + Saturating + Copy,
{
	/// All debt the walk settled: ordinary cancels plus the recovery burn and
	/// its Insurance-Fund residual.
	pub(crate) fn debt_settled(&self) -> Balance {
		let recovery = self
			.recovery
			.as_ref()
			.map_or_else(Balance::zero, |r| r.burned.saturating_add(r.residual));
		self.ordinary_debt.saturating_add(recovery)
	}

	pub(crate) fn collateral_out(&self) -> Balance {
		let recovery = self.recovery.as_ref().map_or_else(Balance::zero, |r| r.collateral_out);
		self.ordinary_collateral.saturating_add(recovery)
	}

	pub(crate) fn apply_ordinary(&mut self, step: &OrdinaryStep<Balance>) {
		self.ordinary_debt = self.ordinary_debt.saturating_add(step.debt);
		self.ordinary_collateral = self.ordinary_collateral.saturating_add(step.collateral_out);
	}

	/// `residual` is the Insurance-Fund-settled debt the loop obtained after
	/// the step committed.
	pub(crate) fn apply_recovery(
		&mut self,
		owner: AccountId,
		step: &RecoveryStep<Balance>,
		residual: Balance,
	) {
		debug_assert!(self.recovery.is_none(), "the walk breaks after one recovery step");
		self.recovery = Some(RecoveryOutcome {
			owner,
			regime: step.regime,
			burned: step.burned,
			collateral_out: step.collateral_out,
			residual,
		});
	}
}

/// Shared validation and fee-rate setup for execution and quoting.
/// The fee rate is deliberately absent: it depends on how much debt the walk
/// actually cancels, so it is derived from `decayed` once the walk is done.
pub(crate) struct RedemptionPreamble<Balance> {
	pub(crate) config: RedemptionConfig<Balance>,
	pub(crate) state: RedemptionState,
	pub(crate) price: FixedU128,
	pub(crate) now: Millis,
	pub(crate) decayed: FixedU128,
	/// Fully-accrued stablecoin-wide debt before this redemption; the
	/// denominator of the dynamic-fee raise.
	pub(crate) stablecoin_debt: Balance,
}
