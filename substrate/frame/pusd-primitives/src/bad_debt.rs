//! Bad-debt healing trait.

use frame::deps::sp_runtime::DispatchError;

/// Branch-level bad-debt healing surface.
///
/// Bad debt is only ever *recorded* inside the vault pallet (recovery settlement and
/// orphan-debt sweeps), so this trait carries just the inverse side: the orchestrator withdraws
/// cover from the Insurance Fund as a `fungible::Credit` and hands it here; the implementation
/// rescinds the underlying pUSD and decrements the branch's recorded bad debt by the same
/// amount.
pub trait VaultBadDebtInterface<CollateralId, StableId, Credit> {
	/// Burn up to the recorded bad debt of the `(collateral_id, stable_id)`
	/// market from `credit` and return the unconsumed surplus (zero when the
	/// credit was fully used). The coin to rescind comes from `credit.asset()`.
	fn heal(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		credit: Credit,
	) -> Result<Credit, DispatchError>;
}
