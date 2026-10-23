//! Benchmark staging is delegated because redemptions cannot create vaults.

#![cfg(feature = "runtime-benchmarks")]

use crate::{
	pallet::{BalanceOf, Call, Config, Event, Pallet, RedemptionConfigOf, Stablecoins},
	types::{RedemptionConfig, RedemptionTerms},
	BenchmarkHelper as _,
};
use frame::{
	arithmetic::{FixedU128, Permill},
	benchmarking::prelude::*,
	deps::{
		frame_support::traits::EnsureOriginWithArg,
		sp_runtime::traits::{One, Zero},
	},
};
use frame_system::RawOrigin;

/// The least restrictive valid policy: a one-unit minimum and a full-range fee
/// band, so nothing in it becomes the binding constraint of a benchmark.
pub(crate) fn registration_config<T: Config>() -> RedemptionConfigOf<T> {
	RedemptionConfig {
		minimum_redemption_amount: BalanceOf::<T>::one(),
		dynamic_fee_decay_period: 6 * 3_600 * 1_000,
		dynamic_fee_floor: FixedU128::zero(),
		dynamic_fee_ceiling: FixedU128::one(),
		base_fee: Permill::from_rational(5u32, 1_000u32),
		fee_ceiling: Permill::one(),
		dynamic_fee_increase_divisor: FixedU128::from_rational(2u128, 1u128),
		final_recovery_bonus_buffer: Permill::from_percent(1),
	}
}

#[benchmarks]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn redeem(s: Linear<1, { T::MaxRedemptionSteps::get() }>) -> Result<(), BenchmarkError> {
		let (collateral_id, stable_id, redeemer, budget) =
			T::BenchmarkHelper::setup_redeemable_branch(s);
		let recipient = redeemer.clone();

		#[extrinsic_call]
		_(
			RawOrigin::Signed(redeemer),
			collateral_id,
			stable_id,
			RedemptionTerms {
				max_stable_to_spend: budget,
				min_collateral_out: BalanceOf::<T>::zero(),
			},
			recipient,
			s,
		);

		Ok(())
	}

	#[benchmark]
	fn set_redemption_config() -> Result<(), BenchmarkError> {
		let (_, stable_id, _, _) = T::BenchmarkHelper::setup_redeemable_branch(1);
		let config =
			RedemptionConfig { base_fee: Permill::from_percent(1), ..registration_config::<T>() };
		let old_config = Stablecoins::<T>::get(&stable_id)
			.map(|info| info.config)
			.ok_or(BenchmarkError::Stop("registration seeded no redemption config"))?;
		assert!(old_config != config);
		// A stablecoin owner's signed origin, where the runtime admits one, reads the owner too.
		let origin = T::UpdateOrigin::try_successful_origin(&stable_id)
			.map_err(|_| BenchmarkError::Weightless)?;

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, stable_id.clone(), config.clone());

		assert_eq!(Stablecoins::<T>::get(&stable_id).map(|info| info.config), Some(config.clone()));
		frame_system::Pallet::<T>::assert_last_event(
			Event::<T>::RedemptionConfigUpdated { stable_id, old_config, new_config: config }
				.into(),
		);
		Ok(())
	}

	impl_benchmark_test_suite!(
		Pallet,
		crate::mock::ExtBuilder::default().build(),
		crate::mock::Test
	);
}
