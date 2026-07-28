//! `try_state` invariant verification.

use crate::pallet::{Config, MarketCounts, RedemptionConfigs, RedemptionStates};
use frame::{traits::Time, try_runtime::TryRuntimeError};

pub fn do_try_state<T: Config>() -> Result<(), TryRuntimeError> {
	// Every write path validates before inserting. A stored invalid config means a path skipped the
	// shared validation.
	for (_stable_id, config) in RedemptionConfigs::<T>::iter() {
		if !config.is_valid() {
			return Err("stored redemption config fails `is_valid`".into());
		}
	}

	// The market count is the registration proxy: a coin carries a config exactly while at least
	// one market issues it, and an exhausted count is removed rather than stored as zero.
	for (stable_id, count) in MarketCounts::<T>::iter() {
		if count == 0 {
			return Err("zero `MarketCounts` record stored".into());
		}
		if !RedemptionConfigs::<T>::contains_key(&stable_id) {
			return Err("registered stablecoin without a redemption config row".into());
		}
	}
	for (stable_id, _) in RedemptionConfigs::<T>::iter() {
		if !MarketCounts::<T>::contains_key(&stable_id) {
			return Err("redemption config row without a registered market".into());
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
