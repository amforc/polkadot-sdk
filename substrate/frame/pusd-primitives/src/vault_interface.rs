//! The vault-side surface external orchestrator pallets drive — redemption
//! sweeps, liquidation execution, and bad-debt healing — keyed by the
//! `(collateral_id, stable_id)` market.

use crate::VaultStatus;
use frame::deps::{
	frame_support::pallet_prelude::{DispatchError, DispatchResult},
	sp_runtime::Permill,
};

/// Per-vault allocation produced by the redemption orchestrator and applied by
/// [`VaultInterface::redeem_step`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RedemptionAllocation<Balance> {
	pub debt_to_cancel: Balance,
	pub collateral_to_recipient: Balance,
}

/// Fully-accrued, post-touch snapshot of a redemption target. These are the
/// numbers the orchestrator sizes and prices the step against; `status`
/// selects the pricing rules: `Active` and `Dormant` redeem at face value,
/// `FinalRecovery` by recovery-settlement rules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RedemptionStepSnapshot<Balance> {
	pub status: VaultStatus,
	/// Post-touch total debt; the cap on `debt_to_cancel`.
	pub debt: Balance,
	/// Collateral currently held against the vault.
	pub collateral: Balance,
	/// Branch redistribution penalty, bounding the recovery bonus. Only
	/// consulted by `FinalRecovery` pricing.
	pub redistribution_penalty: Permill,
}

/// Debt cancelled by external pUSD (Stability Pool + JIT combined) and the
/// matching vault-held collateral paid to the external offset path.
///
/// `collateral_recipient` is the account that receives `collateral` from the
/// vault pallet during [`VaultInterface::execute_liquidation`]. For Stability
/// Pool offsets this must correspond to collateral already consumed through
/// `StabilityPoolOffsetApi` as a real `CollateralCredit`; for JIT it is the
/// JIT settlement recipient.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OffsetAllocation<AccountId, Balance> {
	pub collateral_recipient: AccountId,
	pub debt: Balance,
	pub collateral: Balance,
}

/// Compensation paid to the liquidation keeper.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeeperCompensation<AccountId, Balance> {
	pub recipient: AccountId,
	pub collateral: Balance,
}

/// Allocation produced by the liquidation orchestrator and applied by
/// [`VaultInterface::execute_liquidation`].
///
/// Redistributed debt is derived inside `execute_liquidation` as
/// `snapshot.debt - offset.debt`, so the orchestrator only needs to specify
/// the collateral split.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LiquidationAllocation<AccountId, Balance> {
	pub offset: OffsetAllocation<AccountId, Balance>,
	pub redistribution_collateral: Balance,
	pub keeper: KeeperCompensation<AccountId, Balance>,
}

/// Fully-accrued vault figures handed to the allocation builder. These are the
/// post-touch numbers the orchestrator must size its allocation against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LiquidationSnapshot<Balance> {
	/// Post-touch total debt to settle.
	pub debt: Balance,
	/// Collateral currently held against the vault.
	pub collateral: Balance,
}

/// Everything an orchestrator drives on the vault pallet. Reads are
/// authoritative current state; writes re-shape the priority queue.
///
/// The `build_allocation` closures make each step atomic: the orchestrator
/// sizes its allocation against a post-touch snapshot inside the same call
/// that applies it, and returning `Err` (or producing an invalid allocation)
/// rolls the whole step back, so a rejected step never leaves partial state.
///
/// Bad debt is only ever *recorded* inside the vault pallet (recovery
/// settlement and orphan-debt sweeps); [`Self::heal`] carries the inverse
/// side: the orchestrator withdraws cover from the Insurance Fund as a credit
/// and hands it here to be rescinded against the recorded amount.
pub trait VaultInterface {
	type CollateralId;
	type StableId;
	type AccountId;
	type Balance;
	/// Stable-coin credit shape consumed by [`Self::heal`].
	type Credit;

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
	/// fully-accrued snapshot, and apply the returned allocation atomically
	/// (cancel debt, pay `collateral_to_recipient` to `recipient`). `Ok(None)`
	/// from the closure skips the target but persists the touch. Burning the
	/// redeemer's stablecoin and charging the fee stay with the caller.
	fn redeem_step(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		owner: &Self::AccountId,
		recipient: &Self::AccountId,
		build_allocation: impl FnOnce(
			RedemptionStepSnapshot<Self::Balance>,
		) -> Result<
			Option<RedemptionAllocation<Self::Balance>>,
			DispatchError,
		>,
	) -> Result<Option<RedemptionAllocation<Self::Balance>>, DispatchError>;

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

	/// Liquidate `owner`'s below-MCR vault with the collateral/debt split the
	/// orchestrator computes from the post-touch snapshot.
	fn execute_liquidation(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		owner: &Self::AccountId,
		build_allocation: impl FnOnce(
			LiquidationSnapshot<Self::Balance>,
		) -> Result<
			LiquidationAllocation<Self::AccountId, Self::Balance>,
			DispatchError,
		>,
	) -> DispatchResult;

	/// Burn up to the recorded bad debt of the `(collateral_id, stable_id)`
	/// market from `credit` and return the unconsumed surplus (zero when the
	/// credit was fully used). The coin to rescind comes from `credit.asset()`;
	/// `stable_id` is the intended market and must match it.
	fn heal(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		credit: Self::Credit,
	) -> Result<Self::Credit, DispatchError>;
}
