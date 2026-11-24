//! Per-branch yield hook from the vault engine to the Stability Pool.

use crate::BranchSnapshot;

/// Receives every stablecoin credit the vault engine mints for a branch (interest and upfront
/// fees), keeps the Stability Pool share, and returns the rest for the fee destination.
///
/// The branch is `collateral_id` plus the credit's asset. `branch` is the engine's view at mint
/// time, so the pool need not reload it. Runtimes without a pool use `()`.
///
/// Infallible, since minting runs on commit paths that cannot roll back. When it cannot
/// distribute (no pool row, empty active pool, frozen branch), it returns the credit untouched.
pub trait OnBranchYield<CollateralId, Credit> {
	/// Takes the Stability Pool share of `credit` and returns the remainder, which is the whole
	/// `credit` when the pool cannot accept yield.
	fn distribute_yield(
		collateral_id: &CollateralId,
		branch: BranchSnapshot,
		credit: Credit,
	) -> Credit;
}

impl<CollateralId, Credit> OnBranchYield<CollateralId, Credit> for () {
	fn distribute_yield(_: &CollateralId, _: BranchSnapshot, credit: Credit) -> Credit {
		credit
	}
}
