//! Benchmark staging is delegated because redemptions cannot create vaults.

#![cfg(feature = "runtime-benchmarks")]

use crate::{
	pallet::{BalanceOf, Call, Config, Pallet},
	BenchmarkHelper as _,
};
use frame::{
	benchmarking::prelude::*,
	deps::{frame_support::traits::EnsureOriginWithArg, sp_runtime::traits::Zero},
};
use frame_system::RawOrigin;

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
			budget,
			BalanceOf::<T>::zero(),
			recipient,
			s,
		);

		Ok(())
	}

	#[benchmark]
	fn set_redemption_config() -> Result<(), BenchmarkError> {
		let (_, stable_id, _, _) = T::BenchmarkHelper::setup_redeemable_branch(1);
		let config = T::DefaultRedemptionConfig::get();
		let origin = T::UpdateOrigin::try_successful_origin(&stable_id)
			.map_err(|_| BenchmarkError::Weightless)?;

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, stable_id, config);

		Ok(())
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
