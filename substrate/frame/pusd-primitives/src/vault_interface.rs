//! The vault-side surface external orchestrator pallets drive — redemption
//! sweeps, liquidation execution, and bad-debt healing — keyed by the
//! `(collateral_id, stable_id)` market.

use crate::VaultStatus;
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
}

/// Everything an orchestrator drives on the vault pallet. Reads are
/// authoritative current state; writes re-shape the priority queue.
///
/// The builder closures make each step atomic: the orchestrator sizes its
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

	/// One redemption step against `owner`'s vault: touch it, hand the caller a
	/// fully-accrued snapshot, and apply the returned settlement atomically —
	/// cancel exactly the debt `debt_payment` covers,
	/// burn the payment, and pay `collateral_to_recipient` to `recipient`.
	/// `Ok(None)` from the closure skips the target but persists the touch, so
	/// build the payment only on the settlement path. Charging the redemption
	/// fee stays with the caller.
	fn redeem_step(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		owner: &Self::AccountId,
		recipient: &Self::AccountId,
		build_settlement: impl FnOnce(
			RedemptionStepSnapshot<Self::Balance>,
		) -> Result<
			Option<RedemptionSettlement<Self::StableCredit, Self::Balance>>,
			DispatchError,
		>,
	) -> DispatchResult;

	/// Move a `FinalRecovery` vault's fully-accrued residual debt off the row
	/// and into the branch bad-debt ledger, returning the amount. The caller
	/// settles it (Insurance Fund burn) atomically.
	fn settle_recovery_residual(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		owner: &Self::AccountId,
	) -> Result<Self::Balance, DispatchError>;

	/// Fully-accrued total debt of the market, used to size the dynamic
	/// redemption fee's redeemed fraction.
	fn branch_debt(collateral_id: &Self::CollateralId, stable_id: &Self::StableId)
		-> Self::Balance;

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
