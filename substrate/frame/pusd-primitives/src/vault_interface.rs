//! The vault-side surface external redemption and bad-debt flows drive, keyed
//! by the `(collateral_id, stable_id)` market.

use crate::{DebtCollateral, VaultStatus};
use frame::deps::{
	frame_support::pallet_prelude::{DispatchError, DispatchResult},
	sp_runtime::Permill,
};

/// Settlement produced by the redemption orchestrator and consumed by
/// [`VaultInterface::redeem_step`]. Owning `debt_payment` ties the burn to the
/// cancellation by construction: the vault pallet cannot cancel ledger debt
/// without consuming the matching coin, and the orchestrator cannot misreport
/// the amount it paid.
#[must_use = "the settlement must be returned to VaultInterface::redeem_step"]
pub struct RedemptionSettlement<Credit, Balance> {
	pub debt_payment: Credit,
	pub collateral_to_recipient: Balance,
}

/// Fully-accrued, post-touch snapshot of a redemption target. These are the
/// numbers the orchestrator sizes and prices the step against; `status`
/// selects the pricing rules: `Active` and `Dormant` redeem at face value,
/// `FinalRecovery` by recovery-settlement rules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RedemptionStepSnapshot<Balance> {
	pub status: VaultStatus,
	/// Post-touch total debt; the maximum debt the payment may cover.
	pub debt: Balance,
	/// Collateral currently held against the vault.
	pub collateral: Balance,
	/// Branch redistribution penalty, bounding the recovery bonus. Only
	/// consulted by `FinalRecovery` pricing.
	pub redistribution_penalty: Permill,
}

impl<Balance: Copy> RedemptionStepSnapshot<Balance> {
	/// The debt/collateral pair CR math reads.
	pub fn position(&self) -> DebtCollateral<Balance> {
		DebtCollateral { debt: self.debt, collateral: self.collateral }
	}
}

/// Result of the external liquidation work consumed by
/// [`VaultInterface::execute_liquidation`]. External offset paths burn their
/// stablecoin internally, so `debt_offset` remains a balance. Both collateral
/// remainders return as credits because Vaults owns their final destinations:
/// its redistribution account and the liquidated owner.
#[must_use = "the settlement must be returned to VaultInterface::execute_liquidation"]
pub struct LiquidationSettlement<CollateralCredit, Balance> {
	pub debt_offset: Balance,
	pub redistribution_collateral: CollateralCredit,
	pub owner_surplus: CollateralCredit,
}

/// Fully-accrued vault figures handed to the settlement builder. These are the
/// post-touch numbers the orchestrator must size its settlement against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LiquidationSnapshot<Balance> {
	/// Post-touch total debt to settle.
	pub debt: Balance,
	/// Branch redistribution penalty: the premium debt pushed onto other vaults
	/// carries, above the liquidation penalty an offset pays. Vaults owns it,
	/// so the orchestrator prices redistribution against this rather than
	/// against a parameter of its own.
	pub redistribution_penalty: Permill,
}

/// The vault-side API used by external redemption and bad-debt flows. Reads
/// are authoritative current state; writes re-shape the priority queue.
///
/// The settlement builder makes each redemption step atomic: the caller sizes its
/// settlement against a post-touch snapshot inside the same
/// call that applies it, and returning `Err` (or producing an invalid one)
/// rolls the whole step back, so a rejected step never leaves partial state.
///
/// Bad debt is only ever *recorded* inside the vault pallet (recovery
/// settlement and orphan-debt sweeps); [`Self::heal`] carries the inverse
/// side: the orchestrator withdraws cover from the Insurance Fund as a credit
/// and hands it here to be burned against the recorded amount.
pub trait VaultInterface {
	type CollateralId;
	type StableId;
	type AccountId;
	type Balance;
	type StableCredit;
	type CollateralCredit;

	/// The highest-priority redemption target and its lifecycle status:
	/// `FinalRecovery` FIFO head first, then the dormant redemption target,
	/// then the rate-index tail (`Active`). `after` resumes the rate-index
	/// walk behind a carried cursor; a priority target preempts any cursor.
	fn next_redemption_target(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		after: Option<&Self::AccountId>,
	) -> Option<(Self::AccountId, VaultStatus)>;

	/// The traversal a read-only redemption quote folds over, read lazily
	/// from live state: the `FinalRecovery` FIFO head — else the parked
	/// dormant target — first, then active vaults from the lowest rate
	/// upward.
	///
	/// A quote projection ONLY, not the executable queue: projection never
	/// reshapes the queue, so a skipped and a drained target both continue at
	/// the next element. Execution must instead re-read
	/// [`Self::next_redemption_target`] each step, because settling a step
	/// can reshape the queue (new priority targets, index departures).
	fn redemption_quote_targets(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
	) -> impl Iterator<Item = Self::AccountId>;

	/// Project the fully-accrued values that [`Self::redeem_step`] would hand
	/// to its settlement builder, without touching storage or moving assets.
	fn project_redemption_snapshot(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		owner: &Self::AccountId,
	) -> Result<RedemptionStepSnapshot<Self::Balance>, DispatchError>;

	/// One redemption step against `owner`'s vault: touch it, hand the caller a
	/// fully-accrued snapshot, and apply the returned settlement atomically —
	/// cancel exactly the debt `debt_payment` covers,
	/// burn the payment, and pay `collateral_to_recipient` to `recipient`.
	/// A `None` settlement skips the target but persists the touch, so build the
	/// payment only on the settlement path. The tuple's `Outcome` is returned
	/// unchanged after commit, letting the caller carry loop or pricing state
	/// without a mutable side channel. Charging the redemption fee stays with
	/// the caller.
	fn redeem_step<Outcome>(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		owner: &Self::AccountId,
		recipient: &Self::AccountId,
		build_settlement: impl FnOnce(
			RedemptionStepSnapshot<Self::Balance>,
		) -> Result<
			(Option<RedemptionSettlement<Self::StableCredit, Self::Balance>>, Outcome),
			DispatchError,
		>,
	) -> Result<Outcome, DispatchError>;

	/// Move a `FinalRecovery` vault's fully-accrued residual debt off the row
	/// and into the branch bad-debt ledger, returning the amount. The caller
	/// settles it (Insurance Fund burn) atomically.
	fn settle_recovery_residual(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		owner: &Self::AccountId,
	) -> Result<Self::Balance, DispatchError>;

	/// The market's redistribution penalty, or `None` when it is not
	/// registered. The same value [`LiquidationSnapshot`] carries, exposed for
	/// callers that must validate their own penalty against it before a
	/// liquidation is in flight.
	fn redistribution_penalty(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
	) -> Option<Permill>;

	/// Total debt issued in `stable_id` across every one of its collateral
	/// markets, the denominator of the dynamic redemption fee's redeemed
	/// fraction. Stablecoin-wide rather than per-market because the fee nudges
	/// how much of the coin is redeemed, whichever collateral backs it.
	///
	/// Includes aggregate interest accrued since each market was last touched.
	fn stablecoin_debt(stable_id: &Self::StableId) -> Self::Balance;

	/// Liquidate `owner`'s below-MCR vault. Vaults turns the fully-accrued
	/// collateral hold into one credit and transfers ownership to the
	/// orchestrator. After consuming external offsets and keeper compensation,
	/// the orchestrator returns the redistribution and owner-surplus credits in
	/// the settlement. Vaults owns both final destinations.
	fn execute_liquidation(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		owner: &Self::AccountId,
		build_settlement: impl FnOnce(
			LiquidationSnapshot<Self::Balance>,
			Self::CollateralCredit,
		) -> Result<
			LiquidationSettlement<Self::CollateralCredit, Self::Balance>,
			DispatchError,
		>,
	) -> DispatchResult;

	/// Burn up to the recorded bad debt of the `(collateral_id, stable_id)`
	/// market from `credit` and return the unconsumed surplus (zero when the
	/// credit was fully used). The coin to burn comes from `credit.asset()`;
	/// `stable_id` is the intended market and must match it.
	fn heal(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		credit: Self::StableCredit,
	) -> Result<Self::StableCredit, DispatchError>;
}
