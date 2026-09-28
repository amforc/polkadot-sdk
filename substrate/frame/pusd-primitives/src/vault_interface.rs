//! Vault operations for redemption flows, keyed by `(collateral_id, stable_id)`.

use crate::{DebtCollateral, VaultStatus};
use frame::deps::{
	frame_support::pallet_prelude::DispatchError,
	sp_runtime::{FixedU128, Permill},
};

/// Settlement consumed by [`VaultInterface::redeem_step`].
///
/// Owning `debt_payment` binds the burn to the cancellation: the vault cannot cancel debt without
/// consuming the coin, and the caller cannot misreport what it paid.
#[must_use = "dropping the settlement burns the payment without cancelling any debt"]
pub struct RedemptionSettlement<Credit, Balance> {
	/// Stablecoin burned to cancel debt.
	pub debt_payment: Credit,
	/// Collateral sent to the recipient.
	pub collateral_to_recipient: Balance,
}

/// Fully accrued snapshot of a redemption target, used to size and price a step.
///
/// `status` selects the pricing: face value for `Active` and `Dormant`, recovery settlement for
/// `FinalRecovery`. The last three fields are read only by recovery pricing.
pub struct RedemptionStepSnapshot<Balance> {
	/// Lifecycle status.
	pub status: VaultStatus,
	/// Debt, which a payment of this amount settles in full.
	pub debt: Balance,
	/// Collateral held by the vault.
	pub collateral: Balance,
	/// Branch redistribution penalty; caps the recovery bonus.
	pub redistribution_penalty: Permill,
	/// Branch ICR, the upper bound of recovery settlement; a vault above it must exit instead.
	pub initial_collateralization_ratio: FixedU128,
	/// Branch minimum debt. Below it an occupied Dormant slot can block the exit, so the vault
	/// stays settleable.
	pub minimum_debt: Balance,
}

impl<Balance: Copy> RedemptionStepSnapshot<Balance> {
	/// Returns the debt/collateral pair for CR math.
	pub fn position(&self) -> DebtCollateral<Balance> {
		DebtCollateral { debt: self.debt, collateral: self.collateral }
	}
}

impl<Balance: Copy + Ord> RedemptionStepSnapshot<Balance> {
	/// Returns the largest payment within `budget`.
	pub fn size_within(&self, budget: Balance) -> Balance {
		self.debt.min(budget)
	}
}

/// Authoritative vault state and atomic settlement for redemption flows.
///
/// [`Self::redeem_step`] revalidates against a fresh projection; a mismatch fails the step.
pub trait VaultInterface {
	type CollateralId;
	type StableId;
	type AccountId;
	type Balance;
	type StableCredit;

	/// Returns the highest-priority redemption target and its status: the `FinalRecovery` FIFO
	/// head, then the dormant target, then the rate-index tail (`Active`). `after` resumes the
	/// rate-index walk past a cursor; priority targets preempt it.
	fn next_redemption_target(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		after: Option<&Self::AccountId>,
	) -> Option<(Self::AccountId, VaultStatus)>;

	/// Projects the values [`Self::redeem_step`] would settle against, without writing storage or
	/// moving assets.
	fn project_redemption_snapshot(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		owner: &Self::AccountId,
	) -> Result<RedemptionStepSnapshot<Self::Balance>, DispatchError>;

	/// Applies one atomic redemption to `owner`'s vault.
	///
	/// The payment must be in the market stablecoin and at most the debt; the collateral at most
	/// the vault's. A full payment clears the debt. The caller charges the redemption fee.
	///
	/// An error consumes `settlement.debt_payment` and writes roll back only with the caller's
	/// transaction, so callers must propagate it and abort the dispatch.
	fn redeem_step(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		owner: &Self::AccountId,
		recipient: &Self::AccountId,
		settlement: RedemptionSettlement<Self::StableCredit, Self::Balance>,
	) -> Result<(), DispatchError>;

	/// Returns the total debt of `stable_id` across its markets, including interest accrued since
	/// each market's last touch.
	///
	/// The denominator of the dynamic redemption fee, which is stablecoin-wide because it targets
	/// how much of the coin is redeemed, whatever backs it.
	fn stablecoin_debt(stable_id: &Self::StableId) -> Self::Balance;
}
