//! Checked arithmetic shared by pUSD accounting and price conversions.
//!
//! Callers choose rounding and handle failure according to their settlement policy.

use crate::{CollateralRatio, DebtCollateral};
use frame::arithmetic::{
	helpers_128bit::multiply_by_rational_with_rounding, ArithmeticError, FixedPointNumber,
	FixedPointOperand, FixedU128, Rounding, Zero,
};

/// Returns `value * numerator / denominator`, rounded as specified and checked against `Output`.
///
/// The product uses the SDK's wide arithmetic, so it may exceed `u128` if the quotient fits.
/// Returns `None` for a zero denominator, quotient overflow, or an out-of-range output.
/// Raw operands allow fixed-point factors wider than the caller's balance type.
pub fn mul_div<Output: TryFrom<u128>>(
	value: u128,
	numerator: u128,
	denominator: u128,
	rounding: Rounding,
) -> Option<Output> {
	multiply_by_rational_with_rounding(value, numerator, denominator, rounding)
		.and_then(|raw| Output::try_from(raw).ok())
}

/// Returns `ceil(value * rate)`, or `None` if the result does not fit in `Balance`.
pub fn mul_rate_ceil<Balance: FixedPointOperand>(
	value: Balance,
	rate: FixedU128,
) -> Option<Balance> {
	mul_div(value.unique_saturated_into(), rate.into_inner(), FixedU128::DIV, Rounding::Up)
}

/// The collateralization ratio of `position` at `price`.
///
/// `DebtFree` when `debt == 0`; `Overflow` when the value or the ratio does
/// not fit.
pub fn collateralization_ratio<Balance: FixedPointOperand>(
	position: &DebtCollateral<Balance>,
	price: FixedU128,
) -> Result<CollateralRatio, ArithmeticError> {
	if position.debt.is_zero() {
		return Ok(CollateralRatio::DebtFree);
	}
	let value = price.checked_mul_int(position.collateral).ok_or(ArithmeticError::Overflow)?;
	FixedU128::checked_from_rational(value, position.debt)
		.map(CollateralRatio::Ratio)
		.ok_or(ArithmeticError::Overflow)
}

/// Returns `floor(value * numerator / denominator)`; see [`mul_div`] for failure conditions.
pub fn mul_div_floor<Balance: FixedPointOperand>(
	value: Balance,
	numerator: Balance,
	denominator: Balance,
) -> Option<Balance> {
	mul_div(
		value.unique_saturated_into(),
		numerator.unique_saturated_into(),
		denominator.unique_saturated_into(),
		Rounding::Down,
	)
}

/// Returns `floor(value * rate / denominator)` as a `FixedU128` per-unit delta.
///
/// Returns `Some(0)` for a zero value or rate. Returns `None` for a zero denominator or overflow.
pub fn mul_div_rate_floor<Balance: FixedPointOperand>(
	value: Balance,
	rate: FixedU128,
	denominator: Balance,
) -> Option<FixedU128> {
	if value.is_zero() || rate.is_zero() {
		return Some(FixedU128::zero());
	}
	mul_div(
		value.unique_saturated_into(),
		rate.into_inner(),
		denominator.unique_saturated_into(),
		Rounding::Down,
	)
	.map(FixedU128::from_inner)
}

/// Returns `floor(value / price)`, or `None` for a zero price or output overflow.
pub fn collateral_for_value_floor<Balance: FixedPointOperand>(
	value: Balance,
	price: FixedU128,
) -> Option<Balance> {
	mul_div(value.unique_saturated_into(), FixedU128::DIV, price.into_inner(), Rounding::Down)
}

/// Returns `ceil(value / price)`, or `None` for a zero price or output overflow.
pub fn collateral_for_value_ceil<Balance: FixedPointOperand>(
	value: Balance,
	price: FixedU128,
) -> Option<Balance> {
	mul_div(value.unique_saturated_into(), FixedU128::DIV, price.into_inner(), Rounding::Up)
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame::arithmetic::{FixedPointNumber, One, Saturating};

	#[test]
	fn wrappers_preserve_zero_and_output_precision_rules() {
		assert_eq!(mul_div_floor(0u64, 1, 0), None);
		assert_eq!(mul_div_rate_floor(0u64, FixedU128::one(), 0), Some(FixedU128::zero()));
		assert_eq!(mul_div_rate_floor(1u64, FixedU128::zero(), 0), Some(FixedU128::zero()));
		assert_eq!(mul_div_rate_floor(1u64, FixedU128::one(), 0), None);
		assert_eq!(collateral_for_value_floor(0u64, FixedU128::zero()), None);
		assert_eq!(collateral_for_value_ceil(0u64, FixedU128::zero()), None);
		// A fixed-point result may exceed the input Balance's representation.
		assert_eq!(
			mul_div_rate_floor(u64::MAX, FixedU128::one(), 1),
			Some(FixedU128::from_inner(u128::from(u64::MAX) * FixedU128::DIV)),
		);
		assert_eq!(mul_rate_ceil(3u64, FixedU128::from_rational(1, 2)), Some(2));
		assert_eq!(mul_rate_ceil(u64::MAX, FixedU128::from_u32(2)), None);
		assert_eq!(mul_rate_ceil(0u64, FixedU128::from_inner(u128::MAX)), Some(0));
	}

	#[test]
	fn mul_div_rate_floor_round_trips_small_inputs() {
		let got = mul_div_rate_floor::<u128>(100, FixedU128::one(), 1_000).expect("fits");
		assert_eq!(got, FixedU128::from_rational(1u128, 10u128));
	}

	#[test]
	fn mul_div_rate_zero_value_returns_zero() {
		assert_eq!(mul_div_rate_floor::<u128>(0, FixedU128::one(), 1_000), Some(FixedU128::zero()));
	}

	#[test]
	fn mul_div_rate_floor_overflow_returns_none() {
		let got = mul_div_rate_floor(u128::MAX / 2, FixedU128::one(), 1);
		assert!(got.is_none());
		// Confirm that the function does not reject values below the overflow limit.
		let safe = mul_div_rate_floor(u128::MAX / (FixedU128::DIV * 2), FixedU128::one(), 1);
		assert!(safe.is_some());
	}

	#[test]
	fn mul_div_rate_floor_matches_two_step_when_safe() {
		let rate = FixedU128::from_rational(5u128, 100u128);
		let got = mul_div_rate_floor::<u128>(10_000, rate, 100).expect("fits");
		let two_step = FixedU128::from_rational(10_000u128, 100u128).saturating_mul(rate);
		assert!(got.into_inner().abs_diff(two_step.into_inner()) <= 1);
	}

	#[test]
	fn mul_div_rate_floor_avoids_two_step_overflow() {
		// The complete formula fits although `value / denominator` does not fit in `FixedU128`.
		let rate = FixedU128::from_inner(1);
		let got = mul_div_rate_floor(u128::MAX / 4, rate, 1);
		assert_eq!(got, Some(FixedU128::from_inner(u128::MAX / 4)));
	}

	#[test]
	fn collateral_for_value_floors() {
		// 100 stablecoin at price 10 → 10 collateral.
		let price = FixedU128::from_rational(10, 1);
		assert_eq!(collateral_for_value_floor::<u128>(100, price), Some(10));
		// 105 stablecoin at price 10 → floor(10.5) = 10.
		assert_eq!(collateral_for_value_floor::<u128>(105, price), Some(10));
		// Sub-1.0 price scales up.
		assert_eq!(
			collateral_for_value_floor::<u128>(100, FixedU128::from_rational(1, 2)),
			Some(200)
		);
		assert_eq!(collateral_for_value_floor::<u128>(0, FixedU128::one()), Some(0));
	}

	#[test]
	fn collateral_for_value_ceils() {
		// 100 stablecoin at price 10 → exactly 10 collateral.
		let price = FixedU128::from_rational(10, 1);
		assert_eq!(collateral_for_value_ceil::<u128>(100, price), Some(10));
		// 105 stablecoin at price 10 → ceil(10.5) = 11 (the floor variant gives 10).
		assert_eq!(collateral_for_value_ceil::<u128>(105, price), Some(11));
		// Sub-1.0 price scales up: 100 / 0.9 = 111.1… → 112.
		assert_eq!(
			collateral_for_value_ceil::<u128>(100, FixedU128::from_rational(9, 10)),
			Some(112)
		);
		assert_eq!(collateral_for_value_ceil::<u128>(0, FixedU128::one()), Some(0));
	}

	#[test]
	fn collateral_for_value_fails_loudly() {
		// A zero price cannot size anything, in either rounding direction.
		assert_eq!(collateral_for_value_floor::<u128>(100, FixedU128::zero()), None);
		assert_eq!(collateral_for_value_ceil::<u128>(100, FixedU128::zero()), None);
		// A sub-1.0 price doubles the value past u128::MAX.
		let half = FixedU128::from_rational(1, 2);
		assert_eq!(collateral_for_value_floor::<u128>(u128::MAX, half), None);
		// The result fits u128 but not the caller's narrower Balance.
		assert_eq!(collateral_for_value_floor::<u64>(u64::MAX, half), None);
	}
}
