//! Branch-aware yield sink trait.
//!
//! The vault pallet mints pUSD interest as a `fungible::Credit`, splits it per
//! `SpYieldShare`, and hands the SP-bound share to a sink that resolves the
//! credit into the branch pool account in one call.

use frame::deps::frame_support::pallet_prelude::DispatchResult;

/// Sink for the SP share of branch-tagged yield. `Credit` is intended to be a
/// `fungible::Credit<AccountId, StableAsset>`; making it a generic parameter
/// avoids depending on the consumer's stable-asset configuration here.
pub trait OnBranchYield<CollateralId, StableId, Credit> {
	/// Consume `credit` against the `(collateral_id, stable_id)` market,
	/// decreasing the credit to zero by depositing into the market pool account
	/// or otherwise settling it. Implementations must drop or net the credit before
	/// returning to satisfy `OnDropCredit` accounting.
	fn on_branch_yield(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		credit: Credit,
	) -> DispatchResult;
}

/// Convenience no-op implementation: drops the credit on the floor. Useful in
/// runtimes that route 100% of yield via `FeeHandler` instead.
impl<CollateralId, StableId, Credit> OnBranchYield<CollateralId, StableId, Credit> for () {
	fn on_branch_yield(
		_collateral_id: &CollateralId,
		_stable_id: &StableId,
		_credit: Credit,
	) -> DispatchResult {
		Ok(())
	}
}
