//! [`BenchmarkVaults`] for the pallets built on the vault engine, on top of the fixtures this
//! pallet's own benchmarks use.

use super::fixtures;
use crate::{
	pallet::{BalanceOf, Branches, CollateralIdOf, Pallet, StableIdOf},
	BenchmarkConfig,
};
use frame::{deps::sp_runtime::DispatchError, prelude::*};
use stablecoin_primitives::{
	BenchmarkVault, BenchmarkVaults, Millis, RecoveryHeadSeed, RedemptionTargetsSeed,
};

impl<T: BenchmarkConfig>
	BenchmarkVaults<CollateralIdOf<T>, StableIdOf<T>, T::AccountId, BalanceOf<T>> for Pallet<T>
{
	fn register_market() -> Result<(CollateralIdOf<T>, StableIdOf<T>), DispatchError> {
		let (asset, stable_id) = (fixtures::collateral::<T>(), fixtures::stable::<T>());
		if !Branches::<T>::contains_key(&asset, &stable_id) {
			fixtures::register_default_branch::<T>()?;
		}
		Ok((asset, stable_id))
	}

	fn collateral_units(units: u128) -> BalanceOf<T> {
		fixtures::collateral_units::<T>(units)
	}

	fn set_unit_price(stable_per_unit: u128) {
		fixtures::set_unit_price::<T>(stable_per_unit);
	}

	fn advance_time(ms: Millis) {
		fixtures::advance_time::<T>(ms);
	}

	fn open_vault(owner: &T::AccountId, vault: BenchmarkVault<BalanceOf<T>>) -> DispatchResult {
		fixtures::open_funded_vault::<T>(owner, vault)
	}

	fn seed_redemption_targets(
		count: u32,
		elapsed: Millis,
	) -> Result<RedemptionTargetsSeed<T::AccountId, BalanceOf<T>>, DispatchError> {
		fixtures::seed_redemption_targets::<T>(count, elapsed)
	}

	fn stage_recovery_head(
		elapsed: Millis,
	) -> Result<RecoveryHeadSeed<T::AccountId, BalanceOf<T>>, DispatchError> {
		fixtures::stage_recovery_head::<T>(elapsed)
	}
}
