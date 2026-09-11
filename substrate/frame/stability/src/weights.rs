//! Weights for `pallet-stability`.

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
	fn settle_deposit() -> Weight;
	fn set_stability_pool_config() -> Weight;
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

	fn settle_deposit() -> Weight {
		Weight::zero()
	}

	fn set_stability_pool_config() -> Weight {
		Weight::zero()
	}
}
