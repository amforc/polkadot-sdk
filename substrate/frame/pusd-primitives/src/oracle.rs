//! TODO: Oracle trait surface.

use frame::deps::{frame_support::pallet_prelude::DispatchError, sp_runtime::FixedU128};

/// Read-only access to a normalised price for a given collateral.
pub trait ProvidePrice {
	type AssetId;

	/// Latest price for `collateral_id`. Implementations should return `Err(_)`
	/// if the price is stale or unavailable.
	fn provide_price(collateral_id: &Self::AssetId) -> Result<FixedU128, DispatchError>;
}
