//! Verifies `try_state` invariants under `try-runtime` and after each test.

use crate::pallet::{Config, RedemptionConfigs, RedemptionStates};
use frame::{deps::sp_runtime::TryRuntimeError, traits::Time};

pub fn do_try_state<T: Config>() -> Result<(), TryRuntimeError> {
	// Every write path validates before inserting. A stored invalid config means a path skipped the
	// shared validation.
	for (_stable_id, config) in RedemptionConfigs::<T>::iter() {
		if !config.is_valid() {
			return Err("stored redemption config fails `is_valid`".into());
		}
	}

	let now = T::TimeProvider::now();
	for (stable_id, state) in RedemptionStates::<T>::iter() {
		// Fee state must never outlive the config it is priced against.
		let Some(config) = RedemptionConfigs::<T>::get(&stable_id) else {
			return Err("redemption fee state row without a config row".into());
		};
		// The write path starts from a floor-clamped decayed fee and clamps the rise to the
		// ceiling, so a stored fee outside the policy bounds means a path bypassed the curve.
		if state.dynamic_fee < config.dynamic_fee_floor {
			return Err("stored `dynamic_fee` is below the policy floor".into());
		}
		if state.dynamic_fee > config.dynamic_fee_ceiling {
			return Err("stored `dynamic_fee` is above the policy ceiling".into());
		}
		if state.last_fee_operation > now {
			return Err("`last_fee_operation` is ahead of now".into());
		}
	}
	Ok(())
}
