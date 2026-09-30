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
const ZERO_ORACLE_PRICE: DispatchError = DispatchError::Other("zero oracle price");

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
/// A zero quote fails with `DispatchError::Other("zero oracle price")`.
/// [`DispatchError::Unavailable`] is returned only if neither feed is unusable, so an untrusted
/// feed never unlocks a fallback.
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
		fn provide_price(asset: &u32) -> Result<FixedU128, DispatchError> {
			match asset {
				0 => Ok(FixedU128::from_u32(10)),
				1 => Ok(FixedU128::from_u32(4)),
				2 => Ok(FixedU128::zero()),
				3 => Err(DispatchError::Other("stale")),
				_ => Err(DispatchError::Unavailable),
			}
		}
	}

	struct Reference<const ID: u32>;
	impl<const ID: u32> Get<u32> for Reference<ID> {
		fn get() -> u32 {
			ID
		}
	}

	#[test]
	fn conversion_rounds_up_and_preserves_feed_errors() {
		type Conversion = OraclePriceConversion<Prices, Reference<0>>;
		for (balance, asset, expected) in [
			(7u64, 1, Ok(18)), // 7 * 10 / 4 = 17.5, rounded up.
			(8, 1, Ok(20)),
			(7, 9, Err(DispatchError::Unavailable)),
			(7, 2, Err(ZERO_ORACLE_PRICE)),
		] {
			assert_eq!(Conversion::to_asset_balance(balance, asset), expected);
		}
	}

	#[test]
	fn unusable_feed_wins_over_missing_feed_and_identity_skips_quotes() {
		type MissingReference = OraclePriceConversion<Prices, Reference<9>>;
		for (asset, expected) in [
			(3, Err(DispatchError::Other("stale"))),
			(8, Err(DispatchError::Unavailable)),
			(2, Err(ZERO_ORACLE_PRICE)),
			(9, Ok(7)), // Identity succeeds even without a quote.
		] {
			assert_eq!(MissingReference::to_asset_balance(7u64, asset), expected);
		}
		type ZeroReference = OraclePriceConversion<Prices, Reference<2>>;
		for (asset, expected) in [
			(9, Err(ZERO_ORACLE_PRICE)), // Missing asset cannot hide a zero reference.
			(1, Err(ZERO_ORACLE_PRICE)),
			(2, Ok(7)), // Identity does not consult the zero quote.
		] {
			assert_eq!(ZeroReference::to_asset_balance(7u64, asset), expected);
		}
	}
}
