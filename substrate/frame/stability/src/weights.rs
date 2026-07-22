//! Weights for `pallet-stability`.
//!
//! Hand-written stub: regenerate with `frame-omni-bencher` once the full
//! dispatchable surface exists (benchmarking milestone).

use frame::prelude::Weight;

/// Weight functions needed for `pallet_stability`.
pub trait WeightInfo {
	fn deposit() -> Weight;
	fn request_withdraw() -> Weight;
	fn withdraw() -> Weight;
	fn claim_collateral() -> Weight;
	fn claim_yield() -> Weight;
	fn compound_yield() -> Weight;
	fn offset_recovery() -> Weight;
	fn poke_deposit() -> Weight;
	fn set_stability_pool_config() -> Weight;
	fn on_idle_one_deposit() -> Weight;
}

impl WeightInfo for () {
	fn deposit() -> Weight {
		Weight::zero()
	}

	fn request_withdraw() -> Weight {
		Weight::zero()
	}

	fn withdraw() -> Weight {
		Weight::zero()
	}

	fn claim_collateral() -> Weight {
		Weight::zero()
	}

	fn claim_yield() -> Weight {
		Weight::zero()
	}

	fn compound_yield() -> Weight {
		Weight::zero()
	}

	fn offset_recovery() -> Weight {
		Weight::zero()
	}

	fn poke_deposit() -> Weight {
		Weight::zero()
	}

	fn set_stability_pool_config() -> Weight {
		Weight::zero()
	}

	fn on_idle_one_deposit() -> Weight {
		Weight::zero()
	}
}
