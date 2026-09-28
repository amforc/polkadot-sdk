//! TODO: Oracle trait surface and price conversions.

use crate::math::mul_div;
use core::marker::PhantomData;
use frame::{
	arithmetic::{Rounding, Zero},
	deps::{
		frame_support::pallet_prelude::DispatchError,
		sp_runtime::{ArithmeticError, FixedPointOperand, FixedU128},
	},
	traits::{tokens::ConversionToAssetBalance, Get},
};

/// Error for a zero price quote. Unlike [`DispatchError::Unavailable`], it never permits a
/// fallback price source.
pub const ZERO_ORACLE_PRICE: DispatchError = DispatchError::Other("zero oracle price");

/// Read-only access to normalized collateral prices.
pub trait ProvidePrice {
	type AssetId;

	/// Returns the latest price for `collateral_id`.
	///
	/// [`DispatchError::Unavailable`] means no feed exists and permits a fallback source. Any
	/// other error marks the feed unusable and forbids one.
	fn provide_price(collateral_id: &Self::AssetId) -> Result<FixedU128, DispatchError>;
}

/// Converts a `Reference` amount to an asset amount by the ratio of two [`ProvidePrice`]
/// quotes, rounding up. `Reference` itself converts 1:1 without a query.
///
/// A zero quote fails with [`ZERO_ORACLE_PRICE`]. [`DispatchError::Unavailable`] is returned
/// only if neither feed is unusable, so an untrusted feed never unlocks a fallback.
pub struct OraclePriceConversion<Oracle, Reference>(PhantomData<(Oracle, Reference)>);

impl<Oracle, Reference, Balance> ConversionToAssetBalance<Balance, Oracle::AssetId, Balance>
	for OraclePriceConversion<Oracle, Reference>
where
	Oracle: ProvidePrice,
	Oracle::AssetId: Eq,
	Reference: Get<Oracle::AssetId>,
	Balance: FixedPointOperand,
{
	type Error = DispatchError;

	fn to_asset_balance(
		balance: Balance,
		asset: Oracle::AssetId,
	) -> Result<Balance, DispatchError> {
		let reference = Reference::get();
		if asset == reference {
			return Ok(balance);
		}
		// Read both feeds so an unusable one wins over a missing one. Zero quotes are already
		// unusable here, so they never resolve to `Unavailable`.
		let (asset_price, reference_price) =
			match (Self::usable_price(&asset), Self::usable_price(&reference)) {
				// A missing asset feed must not hide an unusable reference feed. Otherwise the
				// asset error wins.
				(Err(DispatchError::Unavailable), Err(reference_error)) => {
					return Err(reference_error)
				},
				(asset_price, reference_price) => (asset_price?, reference_price?),
			};
		// Round up so a deposit is never undercharged.
		mul_div(
			balance.unique_saturated_into(),
			reference_price.into_inner(),
			asset_price.into_inner(),
			Rounding::Up,
		)
		.ok_or_else(|| ArithmeticError::Overflow.into())
	}
}

impl<Oracle: ProvidePrice, Reference> OraclePriceConversion<Oracle, Reference> {
	/// Returns the feed's quote, treating zero as an unusable feed.
	fn usable_price(asset: &Oracle::AssetId) -> Result<FixedU128, DispatchError> {
		let price = Oracle::provide_price(asset)?;
		if price.is_zero() {
			return Err(ZERO_ORACLE_PRICE);
		}
		Ok(price)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Prices;
	impl ProvidePrice for Prices {
		type AssetId = u32;

		fn provide_price(collateral_id: &u32) -> Result<FixedU128, DispatchError> {
			match collateral_id {
				// One unit of asset 0 is worth 10, one unit of asset 1 is worth 4.
				0 => Ok(FixedU128::from_u32(10)),
				1 => Ok(FixedU128::from_u32(4)),
				2 => Ok(FixedU128::zero()),
				_ => Err(DispatchError::Unavailable),
			}
		}
	}

	struct ReferencelessPrices;
	impl ProvidePrice for ReferencelessPrices {
		type AssetId = u32;

		fn provide_price(collateral_id: &u32) -> Result<FixedU128, DispatchError> {
			match collateral_id {
				1 => Err(DispatchError::Other("stale")),
				_ => Err(DispatchError::Unavailable),
			}
		}
	}

	struct WorthlessReference;
	impl ProvidePrice for WorthlessReference {
		type AssetId = u32;

		fn provide_price(collateral_id: &u32) -> Result<FixedU128, DispatchError> {
			match collateral_id {
				0 => Ok(FixedU128::zero()),
				_ => Ok(FixedU128::from_u32(4)),
			}
		}
	}

	struct ZeroOrMissing;
	impl ProvidePrice for ZeroOrMissing {
		type AssetId = u32;

		fn provide_price(collateral_id: &u32) -> Result<FixedU128, DispatchError> {
			match collateral_id {
				2 => Ok(FixedU128::zero()),
				_ => Err(DispatchError::Unavailable),
			}
		}
	}

	struct Native;
	impl Get<u32> for Native {
		fn get() -> u32 {
			0
		}
	}

	type Conversion = OraclePriceConversion<Prices, Native>;

	#[test]
	fn reference_is_identity_without_a_feed() {
		assert_eq!(Conversion::to_asset_balance(7u64, 0), Ok(7));
	}

	#[test]
	fn reprices_by_the_ratio_of_quotes_rounding_up() {
		// 7 × 10 / 4 = 17.5 → 18.
		assert_eq!(Conversion::to_asset_balance(7u64, 1), Ok(18));
		assert_eq!(Conversion::to_asset_balance(8u64, 1), Ok(20));
	}

	#[test]
	fn missing_feed_is_unavailable_and_zero_quote_is_not() {
		assert_eq!(Conversion::to_asset_balance(7u64, 9), Err(DispatchError::Unavailable));
		assert_eq!(Conversion::to_asset_balance(7u64, 2), Err(ZERO_ORACLE_PRICE));
	}

	#[test]
	fn unusable_asset_feed_wins_over_a_missing_reference() {
		type Referenceless = OraclePriceConversion<ReferencelessPrices, Native>;
		assert_eq!(Referenceless::to_asset_balance(7u64, 1), Err(DispatchError::Other("stale")));
		assert_eq!(Referenceless::to_asset_balance(7u64, 9), Err(DispatchError::Unavailable));
	}

	#[test]
	fn zero_quote_beside_a_missing_feed_never_permits_a_fallback() {
		// Zero asset quote, missing reference feed.
		type ZeroAsset = OraclePriceConversion<ZeroOrMissing, Native>;
		assert_eq!(ZeroAsset::to_asset_balance(7u64, 2), Err(ZERO_ORACLE_PRICE));
		assert_eq!(ZeroAsset::to_asset_balance(7u64, 9), Err(DispatchError::Unavailable));
		// Zero reference quote, missing asset feed.
		struct ZeroReference;
		impl Get<u32> for ZeroReference {
			fn get() -> u32 {
				2
			}
		}
		type ZeroRef = OraclePriceConversion<ZeroOrMissing, ZeroReference>;
		assert_eq!(ZeroRef::to_asset_balance(7u64, 9), Err(ZERO_ORACLE_PRICE));
	}

	#[test]
	fn zero_reference_quote_never_prices_a_deposit_at_zero() {
		type Worthless = OraclePriceConversion<WorthlessReference, Native>;
		assert_eq!(Worthless::to_asset_balance(7u64, 1), Err(ZERO_ORACLE_PRICE));
		// The identity path does not consult the feed at all.
		assert_eq!(Worthless::to_asset_balance(7u64, 0), Ok(7));
	}
}
