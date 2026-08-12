//! Vault operations for external redemption flows, keyed by `(collateral_id, stable_id)`.

use crate::{DebtCollateral, VaultStatus};
use frame::{
	arithmetic::{One, Saturating, Zero},
	deps::{frame_support::pallet_prelude::DispatchError, sp_runtime::Permill},
};

/// Settlement consumed by [`VaultInterface::redeem_step`]. Owning
/// `debt_payment` ties the burn to the cancellation by construction: the vault
/// pallet cannot cancel ledger debt without consuming the matching coin, and
/// the orchestrator cannot misreport the amount it paid.
#[must_use = "dropping the settlement burns the payment without cancelling any debt"]
pub struct RedemptionSettlement<Credit, Balance> {
	pub debt_payment: Credit,
	pub collateral_to_recipient: Balance,
}

/// Fully-accrued, post-touch snapshot of a redemption target. These are the
/// numbers the orchestrator sizes and prices a step against; `status`
/// selects the pricing rules: `Active` and `Dormant` redeem at face value,
/// `FinalRecovery` by recovery-settlement rules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RedemptionStepSnapshot<Balance> {
	pub status: VaultStatus,
	/// Post-touch base debt, excluding the conditional terminal charge.
	pub debt: Balance,
	/// One-unit charge applied only when this step settles the vault in full.
	pub terminal_interest_charge: Balance,
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

impl<Balance: Copy + Ord + Zero + One + Saturating> RedemptionStepSnapshot<Balance> {
	/// The payment that settles the vault in full: base debt plus the terminal charge.
	pub fn full_payoff(&self) -> Balance {
		self.debt.saturating_add(self.terminal_interest_charge)
	}

	/// Reserves one base-debt unit for the terminal charge on a partial payment.
	pub fn partial_cap(&self, limit: Balance) -> Balance {
		if self.terminal_interest_charge.is_zero() {
			limit
		} else {
			limit.saturating_sub(Balance::one())
		}
	}

	/// Returns the largest debt payment within `budget`.
	///
	/// Returns the full payoff when possible. Otherwise, returns a payment limited by
	/// [`Self::partial_cap`].
	pub fn size_within(&self, budget: Balance) -> Balance {
		let full_payoff = self.full_payoff();
		if budget >= full_payoff {
			full_payoff
		} else {
			self.partial_cap(self.debt).min(budget)
		}
	}
}

/// Provides authoritative vault state and atomic settlement for external redemption flows.
///
/// [`Self::redeem_step`] validates the settlement against a new projection. A mismatch aborts the
/// complete step.
pub trait VaultInterface {
	type CollateralId;
	type StableId;
	type AccountId;
	type Balance;
	type StableCredit;

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

	/// Project the fully-accrued values [`Self::redeem_step`] would settle
	/// against, without touching storage or moving assets.
	fn project_redemption_snapshot(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		owner: &Self::AccountId,
	) -> Result<RedemptionStepSnapshot<Self::Balance>, DispatchError>;

	/// Applies one atomic redemption settlement to `owner`'s vault.
	///
	/// The payment must use the market stablecoin and must not exceed the full payoff. The
	/// collateral payment must not exceed the vault collateral. A full payment closes the vault.
	/// A partial payment must leave base debt when a terminal charge applies. The caller charges
	/// the redemption fee.
	///
	/// An error consumes `settlement.debt_payment` inside the rolled-back
	/// step, so callers MUST propagate the error and abort the dispatch —
	/// swallowing it would strand the payment's issuance accounting.
	fn redeem_step(
		collateral_id: &Self::CollateralId,
		stable_id: &Self::StableId,
		owner: &Self::AccountId,
		recipient: &Self::AccountId,
		settlement: RedemptionSettlement<Self::StableCredit, Self::Balance>,
	) -> Result<(), DispatchError>;

	/// Total debt issued in `stable_id` across every one of its collateral
	/// markets, the denominator of the dynamic redemption fee's redeemed
	/// fraction. Stablecoin-wide rather than per-market because the fee nudges
	/// how much of the coin is redeemed, whichever collateral backs it.
	///
	/// Includes aggregate interest accrued since each market was last touched.
	fn stablecoin_debt(stable_id: &Self::StableId) -> Self::Balance;
}
