//! TODO: Oracle trait surface and price conversions.

use core::marker::PhantomData;
use frame::{
	arithmetic::{helpers_128bit::multiply_by_rational_with_rounding, Rounding, Zero},
	deps::{
		frame_support::pallet_prelude::DispatchError,
		sp_runtime::{ArithmeticError, FixedPointOperand, FixedU128},
	},
	traits::{tokens::ConversionToAssetBalance, Get},
};

/// Error for a feed that quotes a price of zero. Distinct from [`DispatchError::Unavailable`] so
/// that a worthless feed never permits a fallback price source.
pub const ZERO_ORACLE_PRICE: DispatchError = DispatchError::Other("zero oracle price");

/// Read-only access to a normalised price for a given collateral.
pub trait ProvidePrice {
	type AssetId;

	/// Latest price for `collateral_id`.
	///
	/// If no feed exists, this function returns [`DispatchError::Unavailable`] and permits another
	/// price source. Any other error identifies an unusable feed and prohibits a fallback.
	fn provide_price(collateral_id: &Self::AssetId) -> Result<FixedU128, DispatchError>;
}

/// Converts a `Reference` amount to an asset amount from oracle prices.
///
/// The conversion uses the ratio of two [`ProvidePrice`] quotes and rounds up. If `asset` is
/// `Reference`, it returns `balance` unchanged without an oracle query.
///
/// If a feed does not exist, the conversion returns [`DispatchError::Unavailable`] unless the
/// other feed has a different error. A zero quote is an unusable feed and returns
/// `DispatchError::Other("zero oracle price")`.
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
		// Read both feeds so that an unusable feed takes precedence when the other feed is absent.
		// A zero quote is classified as unusable before precedence so that it never resolves to
		// `Unavailable` and permits a fallback.
		let asset_price = Self::usable_price(&asset);
		let reference_price = Self::usable_price(&reference);
		let (asset_price, reference_price) = match (asset_price, reference_price) {
			(Ok(asset_price), Ok(reference_price)) => (asset_price, reference_price),
			// This arm must precede the catch-all asset error: only `Unavailable` permits a
			// fallback, so a missing asset feed beside an unusable reference feed must surface the
			// reference error, or the fallback would be unlocked by a feed that cannot be trusted.
			(Err(DispatchError::Unavailable), Err(reference_error)) => return Err(reference_error),
			(Err(asset_error), _) => return Err(asset_error),
			(Ok(_), Err(reference_error)) => return Err(reference_error),
		};
		// Rounding up means a deposit is never undercharged by a sub-unit.
		let amount = multiply_by_rational_with_rounding(
			balance.unique_saturated_into(),
			reference_price.into_inner(),
			asset_price.into_inner(),
			Rounding::Up,
		)
		.ok_or(ArithmeticError::Overflow)?;
		Balance::try_from(amount).map_err(|_| ArithmeticError::Overflow.into())
	}
}

impl<Oracle: ProvidePrice, Reference> OraclePriceConversion<Oracle, Reference> {
	/// A feed's quote, with a zero quote reported as an unusable feed.
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
