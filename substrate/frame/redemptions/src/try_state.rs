//! `try_state` invariant verification.

use crate::pallet::{Config, RedemptionConfigs, RedemptionStates};
use frame::{traits::Time, try_runtime::TryRuntimeError};

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
		if !RedemptionConfigs::<T>::contains_key(&stable_id) {
			return Err("redemption fee state row without a config row".into());
		}
		if state.last_fee_operation > now {
			return Err("`last_fee_operation` is ahead of now".into());
		}
	}
	Ok(())
}
