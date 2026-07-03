//! Redemption handoff types and trait.

use crate::VaultStatus;
use frame::deps::{frame_support::pallet_prelude::DispatchError, sp_runtime::Permill};

/// Per-vault allocation produced by the redemption orchestrator and applied by
/// [`VaultRedemptionInterface::redeem_step`].
///
/// `redeemer` receives `collateral_to_redeemer`; `fee_collateral_retained`
/// stays in the vault as a branch-local fee.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RedemptionAllocation<AccountId, Balance> {
	pub redeemer: AccountId,
	pub debt_to_cancel: Balance,
	pub collateral_to_redeemer: Balance,
	pub fee_collateral_retained: Balance,
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

/// Vault-side surface the redemption orchestrator drives, keyed by the
/// `(collateral_id, stable_id)` market. Reads are authoritative current state;
/// writes re-shape the priority queue.
pub trait VaultRedemptionInterface<CollateralId, StableId, AccountId, Balance> {
	/// The highest-priority redemption target and its lifecycle status:
	/// `FinalRecovery` FIFO head first, then the dormant redemption target,
	/// then the rate-index tail (`Active`). `after` resumes the rate-index
	/// walk behind a carried cursor; a priority target preempts any cursor.
	fn next_redemption_target(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		after: Option<&AccountId>,
	) -> Option<(AccountId, VaultStatus)>;

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
