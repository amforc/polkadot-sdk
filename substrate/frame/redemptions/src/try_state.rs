//! `try_state` invariant verification.

use crate::pallet::{Config, RedemptionConfigs, RedemptionStates};
use frame::{traits::Time, try_runtime::TryRuntimeError};

pub fn do_try_state<T: Config>() -> Result<(), TryRuntimeError> {
	// Every write path validates before inserting. A stored invalid config means a path skipped the
	// shared validation.
	for (_collateral_id, _stable_id, config) in RedemptionConfigs::<T>::iter() {
		if !config.is_valid() {
			return Err("stored redemption config fails `is_valid`".into());
		}
	}

	let now = T::TimeProvider::now();
	for (collateral_id, stable_id, state) in RedemptionStates::<T>::iter() {
		// Configs are the registration proxy (seeded on registration and removed on
		// deregistration), so fee state must never outlive them.
		if !RedemptionConfigs::<T>::contains_key(&collateral_id, &stable_id) {
			return Err("redemption fee state row without a config row".into());
		}
		if state.last_fee_operation > now {
			return Err("`last_fee_operation` is ahead of now".into());
		}
	}
	Ok(())
}
