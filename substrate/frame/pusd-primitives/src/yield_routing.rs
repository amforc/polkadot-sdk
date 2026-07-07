//! Per-market yield hook between the vault engine and the Stability Pool.

/// Called with every stable-coin credit the vault engine mints for a market
/// (branch interest and upfront fees). The implementation takes the
/// Stability-Pool share and returns the remainder, which the vault engine
/// hands to its fee destination. Runtimes without a pool use `()`.
///
/// Must be infallible: yield minting happens on commit paths that cannot
/// roll back user operations over a routing failure. An implementation that
/// cannot distribute (no pool row, empty active pool, frozen branch) returns
/// the credit untouched.
pub trait OnBranchYield<CollateralId, StableId, Credit> {
	fn distribute_yield(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		credit: Credit,
	) -> Credit;
}

impl<CollateralId, StableId, Credit> OnBranchYield<CollateralId, StableId, Credit> for () {
	fn distribute_yield(_: &CollateralId, _: &StableId, credit: Credit) -> Credit {
		credit
	}
}
