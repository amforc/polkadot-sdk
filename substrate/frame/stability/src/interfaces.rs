//! pUSD primitive trait implementations: the surfaces sibling pallets drive
//! on the pool.

use crate::pallet::{BalanceOf, CollateralCreditOf, Config, Pallet, Pools, StableCreditOf};
use frame::prelude::*;
use pusd_primitives::{
	OnBranchYield, PendingOffsetResult, PoolOffsetResult, StabilityPoolOffsetApi,
};

/// The vault engine hands every minted branch credit through here; the pool
/// takes `floor(yield_share * credit)` and returns the rest for the fee
/// destination. Infallible: whatever cannot be distributed (no pool row, a
/// zero share, an empty or frozen pool) comes back with the remainder. The
/// pool row is loaded once and handed down to the distribution engine.
impl<T: Config> OnBranchYield<T::CollateralAssetId, T::StableAssetId, StableCreditOf<T>>
	for Pallet<T>
{
	fn distribute_yield(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		credit: StableCreditOf<T>,
	) -> StableCreditOf<T> {
		if credit.asset() != *stable_id {
			return credit;
		}
		let Some(pool) = Pools::<T>::get(collateral_id, stable_id) else {
			return credit;
		};
		let take = pool.config.yield_share.mul_floor(credit.peek());
		if take.is_zero() {
			return credit;
		}
		let (taken, mut remainder) = credit.split(take);
		let leftover = Self::do_distribute_yield(collateral_id, stable_id, pool, taken);
		if let Err(leftover) = remainder.subsume(leftover) {
			// Both halves came from one credit, so a mismatch cannot
			// happen; burning the leftover keeps issuance conservative.
			debug_assert!(false, "yield credit halves diverged");
			drop(leftover);
		}
		remainder
	}
}

/// The offset surface the future liquidations pallet drives. Collateral
/// travels as a `Credit`.
impl<T: Config>
	StabilityPoolOffsetApi<
		T::CollateralAssetId,
		T::StableAssetId,
		BalanceOf<T>,
		CollateralCreditOf<T>,
	> for Pallet<T>
{
	fn offset_liquidation(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		max_debt_to_offset: BalanceOf<T>,
		collateral: CollateralCreditOf<T>,
	) -> (PoolOffsetResult<BalanceOf<T>>, CollateralCreditOf<T>) {
		Self::do_offset_liquidation(collateral_id, stable_id, max_debt_to_offset, collateral)
	}

	fn offset_pending_liquidation(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		remaining_debt: BalanceOf<T>,
		max_pending_iterations: u32,
		collateral: CollateralCreditOf<T>,
	) -> (PendingOffsetResult<BalanceOf<T>>, CollateralCreditOf<T>) {
		Self::do_offset_pending_liquidation(
			collateral_id,
			stable_id,
			remaining_debt,
			max_pending_iterations,
			collateral,
		)
	}
}
