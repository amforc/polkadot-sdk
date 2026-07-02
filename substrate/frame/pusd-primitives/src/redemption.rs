//! Redemption handoff types and trait.

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::deps::{frame_support::pallet_prelude::DispatchError, sp_runtime::Permill};
use scale_info::TypeInfo;

/// Per-vault allocation produced by the redemption orchestrator and applied by
/// [`VaultRedemptionInterface::redeem_step`].
///
/// `redeemer` receives `collateral_to_redeemer`; `fee_collateral_retained`
/// stays in the vault as a branch-local fee.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct RedemptionAllocation<AccountId, Balance> {
	pub redeemer: AccountId,
	pub debt_to_cancel: Balance,
	pub collateral_to_redeemer: Balance,
	pub fee_collateral_retained: Balance,
}

/// Pricing regime of a redemption target. Returned alongside the target so the
/// orchestrator selects the regime without a second classifying call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RedemptionTargetKind {
	/// Active rate-index vault, redeemed at face value (ordinary redemption).
	Ordinary,
	/// The branch's single dormant redemption-target slot occupant. Priced like
	/// an ordinary redemption, but it gates the rate-index tail behind it.
	Dormant,
	/// `FinalRecovery` FIFO head, priced by recovery-settlement rules.
	FinalRecovery,
}

impl RedemptionTargetKind {
	/// True for the `FinalRecovery` regime.
	pub fn is_final_recovery(&self) -> bool {
		matches!(self, Self::FinalRecovery)
	}

	/// True for the dormant redemption target (a hard ordering barrier).
	pub fn is_dormant(&self) -> bool {
		matches!(self, Self::Dormant)
	}
}

/// The current highest-priority redemption target on a market.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RedemptionTarget<AccountId> {
	pub owner: AccountId,
	pub kind: RedemptionTargetKind,
}

/// Post-touch pricing regime of a redemption step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RedemptionRegime {
	/// Active rate-index vault, redeemed at face value.
	Ordinary,
	/// Dormant redemption-target slot occupant, redeemed at face value.
	Dormant,
	/// `FinalRecovery` FIFO head, priced by recovery-settlement rules. The
	/// branch redistribution penalty bounds the recovery bonus.
	FinalRecovery { redistribution_penalty: Permill },
}

impl RedemptionRegime {
	/// True for the `FinalRecovery` regime.
	pub fn is_final_recovery(&self) -> bool {
		matches!(self, Self::FinalRecovery { .. })
	}

	/// True for the dormant redemption target.
	pub fn is_dormant(&self) -> bool {
		matches!(self, Self::Dormant)
	}
}

/// Fully-accrued, post-touch snapshot of a redemption target. These are the
/// numbers the orchestrator sizes and prices the step against; `regime`
/// selects the pricing rules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RedemptionStepSnapshot<Balance> {
	pub regime: RedemptionRegime,
	/// Post-touch total debt; the cap on `debt_to_cancel`.
	pub debt: Balance,
	/// Collateral currently held against the vault.
	pub collateral: Balance,
}

/// Vault-side surface the redemption orchestrator drives, keyed by the
/// `(collateral_id, stable_id)` market. Reads are authoritative current state;
/// writes re-shape the priority queue.
pub trait VaultRedemptionInterface<CollateralId, StableId, AccountId, Balance> {
/// TODO: Doc
	fn next_redemption_target(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		after: Option<&AccountId>,
	) -> Option<RedemptionTarget<AccountId>>;

	/// TODO: Doc
	fn redeem_step(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		owner: &AccountId,
		build_allocation: impl FnOnce(
			RedemptionStepSnapshot<Balance>,
		) -> Result<
			Option<RedemptionAllocation<AccountId, Balance>>,
			DispatchError,
		>,
	) -> Result<Option<RedemptionAllocation<AccountId, Balance>>, DispatchError>;

	/// TODO: Doc
	fn settle_recovery_residual(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		owner: &AccountId,
	) -> Result<Balance, DispatchError>;

	/// Fully-accrued total debt of the market, used to size the dynamic
	/// redemption fee's redeemed fraction.
	fn branch_debt(collateral_id: &CollateralId, stable_id: &StableId) -> Balance;
}
