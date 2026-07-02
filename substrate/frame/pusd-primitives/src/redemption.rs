//! Redemption handoff types and trait.

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::deps::{
	frame_support::pallet_prelude::{DispatchError, DispatchResult},
	sp_runtime::Permill,
};
use scale_info::TypeInfo;

/// Per-vault allocation produced by the redemption orchestrator and applied by
/// [`VaultRedemptionInterface::apply_redemption`].
///
/// `fee_collateral_retained` stays in the vault as a branch-local fee.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct RedemptionAllocation<Balance> {
	pub debt_to_cancel: Balance,
	pub collateral_to_redeemer: Balance,
	pub fee_collateral_retained: Balance,
}

/// Pricing regime of a redemption target. Returned alongside the target so the
/// orchestrator selects the regime without a second classifying call.
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
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct RedemptionTarget<AccountId> {
	pub owner: AccountId,
	pub kind: RedemptionTargetKind,
}

/// Fully-accrued, post-touch snapshot of a redemption target. These are the
/// numbers the orchestrator sizes and prices the step against: `kind` selects
/// the regime and `redistribution_penalty` bounds the recovery bonus.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct RedemptionStepSnapshot<AccountId, Balance> {
	pub owner: AccountId,
	pub kind: RedemptionTargetKind,
	/// Post-touch total debt; the cap on `debt_to_cancel`.
	pub debt: Balance,
	/// Collateral currently held against the vault.
	pub collateral: Balance,
	/// Branch redistribution penalty: the upper bound on the recovery bonus.
	pub redistribution_penalty: Permill,
}

/// Vault-side surface the redemption orchestrator drives, keyed by the
/// `(collateral_id, stable_id)` market. Reads are authoritative current state;
/// writes re-shape the priority queue.
pub trait VaultRedemptionInterface<AccountId, CollateralId, StableId, Balance> {
	/// Redemption target on the `(collateral_id, stable_id)` market, tagged with
	/// its pricing regime, or `None` when there is none.
	///
	/// Both forms re-apply the `FinalRecovery`/dormant barrier first, so slot
	/// clearance, creation, liquidation, activation, or close since the previous
	/// step is reflected before any ordinary target is selected; targets behind an
	/// occupied dormant slot are never exposed.
	///
	/// `after == None` returns the highest-priority target: `FinalRecovery` FIFO
	/// head, then the dormant redemption target, then the rate index tail-first.
	///
	/// `after == Some(owner)`, when no barrier gates, returns the next ordinary
	/// target after `owner` in the rate index (its head-ward neighbor). This lets
	/// the orchestrator carry a cursor across steps and skip an underwater ordinary
	/// prefix once rather than re-walking it after every redeem. Because a skipped
	/// vault stays live, its head-ward neighbor advances past vaults removed by
	/// intervening redeems. `owner` must be a current rate-index member.
	fn next_redemption_target(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		after: Option<&AccountId>,
	) -> Option<RedemptionTarget<AccountId>>;

	/// Touch the target, fully accrue interest and redistribution, validate its
	/// status, and return a post-touch snapshot safe to price against.
	fn prepare_redemption_step(
		collateral_id: CollateralId,
		stable_id: StableId,
		owner: AccountId,
	) -> Result<RedemptionStepSnapshot<AccountId, Balance>, DispatchError>;

	/// Apply the orchestrator's per-vault allocation: cancel `debt_to_cancel`
	/// from the vault, move `collateral_to_redeemer` to `redeemer`, retain
	/// `fee_collateral_retained`, and re-shape the priority queue. Verifies
	/// conservation against the post-touch debt and held collateral.
	fn apply_redemption(
		collateral_id: CollateralId,
		stable_id: StableId,
		owner: AccountId,
		redeemer: AccountId,
		allocation: RedemptionAllocation<Balance>,
	) -> DispatchResult;

	/// Terminal `FinalRecovery` settlement, called once the redeemer has
	/// consumed all market-cancellable debt and the vault's collateral is
	/// exhausted. Moves the vault's residual debt to branch bad debt, releases
	/// any collateral dust to the owner, and removes the vault. Returns the
	/// residual recorded, which the orchestrator burns from the Insurance Fund
	/// via [`super::VaultBadDebtInterface::heal`].
	fn settle_recovery_residual(
		collateral_id: CollateralId,
		stable_id: StableId,
		owner: AccountId,
	) -> Result<Balance, DispatchError>;

	/// Fully-accrued total debt of the market, used to size the dynamic
	/// redemption fee's redeemed fraction.
	fn branch_debt(collateral_id: &CollateralId, stable_id: &StableId) -> Balance;
}
