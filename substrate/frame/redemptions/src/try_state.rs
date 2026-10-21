//! Verifies `try_state` invariants under `try-runtime` and after each test.

use crate::pallet::{Config, RedemptionConfigs, RedemptionStates};
use frame::{deps::sp_runtime::TryRuntimeError, prelude::ensure, traits::Time};

pub fn do_try_state<T: Config>() -> Result<(), TryRuntimeError> {
	// Every write path validates before inserting. A stored invalid config means a path skipped the
	// shared validation.
	for config in RedemptionConfigs::<T>::iter_values() {
		ensure!(config.is_valid(), "stored redemption config fails `is_valid`");
	}

	let now = T::TimeProvider::now();
	for (stable_id, state) in RedemptionStates::<T>::iter() {
		// Fee state must never outlive the config it is priced against.
		let config = RedemptionConfigs::<T>::get(&stable_id)
			.ok_or("redemption fee state row without a config row")?;
		// The write path starts from a floor-clamped decayed fee and clamps the rise to the
		// ceiling, so a stored fee outside the policy bounds means a path bypassed the curve.
		ensure!(
			state.dynamic_fee >= config.dynamic_fee_floor,
			"stored `dynamic_fee` is below the policy floor"
		);
		ensure!(
			state.dynamic_fee <= config.dynamic_fee_ceiling,
			"stored `dynamic_fee` is above the policy ceiling"
		);
		ensure!(state.last_fee_operation <= now, "`last_fee_operation` is ahead of now");
	}
	Ok(())
}
