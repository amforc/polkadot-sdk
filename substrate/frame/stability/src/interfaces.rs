//! pUSD primitive trait implementations: the surfaces sibling pallets drive
//! on the pool. (`OnBranchLifecycle` lives next to the storage it seeds, in
//! `lib.rs`.)

use crate::pallet::{BalanceOf, Config, Pallet, PoolStates, StabilityPoolConfigs, StableCreditOf};
use frame::{deps::frame_support::storage::with_storage_layer, prelude::*};
use pusd_primitives::{
	OnBranchYield, PendingOffsetResult, PoolOffsetResult, StabilityPoolOffsetApi,
};

/// The vault engine hands every minted branch credit through here; the pool
/// takes `floor(yield_share * credit)` and returns the rest for the fee
/// destination. Infallible: whatever cannot be distributed (no pool row, a
/// zero share, an empty or frozen pool) comes back with the remainder.
impl<T: Config> OnBranchYield<T::CollateralAssetId, T::StableAssetId, StableCreditOf<T>>
	for Pallet<T>
{
	fn distribute_yield(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		credit: StableCreditOf<T>,
	) -> StableCreditOf<T> {
		let Some(config) = StabilityPoolConfigs::<T>::get(collateral_id, stable_id) else {
			return credit;
		};
		let take = config.yield_share.mul_floor(credit.peek());
		if take.is_zero() {
			return credit;
		}
		let (taken, mut remainder) = credit.split(take);
		let leftover = Self::do_distribute_yield(collateral_id, stable_id, taken);
		if let Err(leftover) = remainder.subsume(leftover) {
			// Both halves came from one credit, so a mismatch cannot
			// happen; burning the leftover keeps issuance conservative.
			debug_assert!(false, "yield credit halves diverged");
			drop(leftover);
		}
		remainder
	}
}

/// The offset surface the future liquidations pallet drives. The engine
/// functions are not extrinsics, so this impl owns their atomicity: each call
/// runs in its own storage layer and rolls back entirely on error.
impl<T: Config>
	StabilityPoolOffsetApi<T::CollateralAssetId, T::StableAssetId, T::AccountId, BalanceOf<T>>
	for Pallet<T>
{
	fn pool_account(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Option<T::AccountId> {
		PoolStates::<T>::contains_key(collateral_id, stable_id)
			.then(|| Self::pool_account(collateral_id, stable_id))
	}

	fn offset_liquidation(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		max_debt_to_offset: BalanceOf<T>,
		collateral_for_pool: BalanceOf<T>,
	) -> Result<PoolOffsetResult<BalanceOf<T>>, DispatchError> {
		with_storage_layer(|| {
			Self::do_offset_liquidation(
				collateral_id,
				stable_id,
				max_debt_to_offset,
				collateral_for_pool,
			)
		})
	}

	fn offset_pending_liquidation(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		remaining_debt: BalanceOf<T>,
		remaining_collateral: BalanceOf<T>,
		max_pending_iterations: u32,
	) -> Result<PendingOffsetResult<BalanceOf<T>>, DispatchError> {
		with_storage_layer(|| {
			Self::do_offset_pending_liquidation(
				collateral_id,
				stable_id,
				remaining_debt,
				remaining_collateral,
				max_pending_iterations,
			)
		})
	}
}
