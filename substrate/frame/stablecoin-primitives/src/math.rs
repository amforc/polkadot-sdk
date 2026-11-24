//! Checked arithmetic for stablecoin accounting and price conversions. Callers pick the rounding
//! and handle failure.

use crate::{CollateralRatio, DebtCollateral};
use frame::arithmetic::{
	helpers_128bit::multiply_by_rational_with_rounding, ArithmeticError, EnsureFixedPointNumber,
	FixedPointOperand, FixedU128, One, Rounding, Zero,
};

/// Returns `value * numerator / denominator` with `rounding`, converted to `Output`.
///
/// The intermediate product may exceed `u128`, and raw operands let fixed-point factors exceed
/// the caller's balance type. Returns `None` for a zero denominator or an out-of-range result.
pub fn mul_div<Output: TryFrom<u128>>(
	value: u128,
	numerator: u128,
	denominator: u128,
	rounding: Rounding,
) -> Option<Output> {
	multiply_by_rational_with_rounding(value, numerator, denominator, rounding)
		.and_then(|raw| Output::try_from(raw).ok())
}

/// Returns `value * numerator / denominator` with `rounding`, for fixed-point factors.
///
/// The factors are divided at full precision, so `numerator / denominator` is never rounded on
/// its own. Returns `None` for a zero denominator or a result that does not fit in `Balance`.
pub fn mul_ratio<Balance: FixedPointOperand>(
	value: Balance,
	numerator: FixedU128,
	denominator: FixedU128,
	rounding: Rounding,
) -> Option<Balance> {
	mul_div(
		value.unique_saturated_into(),
		numerator.into_inner(),
		denominator.into_inner(),
		rounding,
	)
}

/// Returns `ceil(value * rate)`, or `None` if the result does not fit in `Balance`.
pub fn mul_rate_ceil<Balance: FixedPointOperand>(
	value: Balance,
	rate: FixedU128,
) -> Option<Balance> {
	mul_ratio(value, rate, FixedU128::one(), Rounding::Up)
}

/// Returns the collateralization ratio of `position` at `price`.
///
/// `DebtFree` for zero debt; `Overflow` if the value or the ratio does not fit.
pub fn collateralization_ratio<Balance: FixedPointOperand>(
	position: &DebtCollateral<Balance>,
	price: FixedU128,
) -> Result<CollateralRatio, ArithmeticError> {
	if position.debt.is_zero() {
		return Ok(CollateralRatio::DebtFree);
	}
	// A nonzero debt leaves `Overflow` as the only failure of either step.
	let value = price.ensure_mul_int(position.collateral)?;
	FixedU128::ensure_from_rational(value, position.debt).map(CollateralRatio::Ratio)
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

/// Returns `floor(value * rate / denominator)` as a [`FixedU128`] per-unit delta.
///
/// `Some(0)` for a zero `value` or `rate`; `None` for a zero denominator or overflow.
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
	mul_ratio(value, FixedU128::one(), price, Rounding::Down)
}

/// Returns `ceil(value / price)`, or `None` for a zero price or output overflow.
pub fn collateral_for_value_ceil<Balance: FixedPointOperand>(
	value: Balance,
	price: FixedU128,
) -> Option<Balance> {
	mul_ratio(value, FixedU128::one(), price, Rounding::Up)
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame::arithmetic::{FixedPointNumber, One};

	#[test]
	fn wrappers_preserve_zero_and_output_precision_rules() {
		// A zero numerator, rate or denominator.

		// A zero denominator or zero price fails, but `mul_div_rate_floor`
		// returns zero for a zero value or rate before dividing, and zero times any rate is zero.
		assert_eq!(mul_div_floor(0u64, 1, 0), None);
		assert_eq!(mul_div_rate_floor(0u64, FixedU128::one(), 0), Some(FixedU128::zero()));
		assert_eq!(mul_div_rate_floor(1u64, FixedU128::zero(), 0), Some(FixedU128::zero()));
		assert_eq!(mul_div_rate_floor(1u64, FixedU128::one(), 0), None);
		assert_eq!(collateral_for_value_floor(0u64, FixedU128::zero()), None);
		assert_eq!(collateral_for_value_ceil(0u64, FixedU128::zero()), None);
		assert_eq!(mul_rate_ceil(0u64, FixedU128::from_inner(u128::MAX)), Some(0));

		// Values at the edge of the input Balance's representation.

		// A fixed-point result may exceed the input Balance's representation,
		// while a Balance result may not.
		assert_eq!(
			mul_div_rate_floor(u64::MAX, FixedU128::one(), 1),
			Some(FixedU128::from_inner(u128::from(u64::MAX) * FixedU128::DIV)),
		);
		assert_eq!(mul_rate_ceil(u64::MAX, FixedU128::from_u32(2)), None);

		// A product with a fractional part.

		// 3 · 0.5 = 1.5 rounds up.
		assert_eq!(mul_rate_ceil(3u64, FixedU128::from_rational(1, 2)), Some(2));
	}

	#[test]
	fn mul_div_rate_floor_overflow_returns_none() {
		let overflowing = u128::MAX / 2;
		let fitting = u128::MAX / (FixedU128::DIV * 2);

		let got = mul_div_rate_floor(overflowing, FixedU128::one(), 1);
		let safe = mul_div_rate_floor(fitting, FixedU128::one(), 1);

		// The function rejects only values past the overflow limit.
		assert!(got.is_none());
		assert!(safe.is_some());
	}

	/// The per-unit delta feeds accumulators that pay out `delta * units`, so a delta rounded up
	/// would pay out more than came in.
	#[test]
	fn mul_div_rate_floor_is_exact_or_rounds_down() {
		// (value, rate, denominator, expected).
		let cases = [
			// 100 · 1 / 1_000 = 0.1.
			(100, FixedU128::one(), 1_000, FixedU128::from_inner(100_000_000_000_000_000)),
			// 10_000 · 5% / 100 = 5.
			(10_000, FixedU128::from_rational(5u128, 100u128), 100, FixedU128::from_u32(5)),
			// 1/3 and 2/3 have no exact value. The nearest value to 2/3 ends in 7.
			(1, FixedU128::one(), 3, FixedU128::from_inner(333_333_333_333_333_333)),
			(2, FixedU128::one(), 3, FixedU128::from_inner(666_666_666_666_666_666)),
		];
		for (value, rate, denominator, expected) in cases {
			let got = mul_div_rate_floor::<u128>(value, rate, denominator);

			assert_eq!(got, Some(expected), "{value} · {rate:?} / {denominator}");
		}
	}

	#[test]
	fn mul_div_rate_floor_avoids_two_step_overflow() {
		// The complete formula fits although `value / denominator` does not fit in
		// `FixedU128`.
		let rate = FixedU128::from_inner(1);

		let got = mul_div_rate_floor(u128::MAX / 4, rate, 1);

		assert_eq!(got, Some(FixedU128::from_inner(u128::MAX / 4)));
	}

	#[test]
	fn collateral_for_value_floors() {
		// (value, price, expected).
		let cases = [
			// 100 stablecoin at price 10 → 10 collateral.
			(100, FixedU128::from_rational(10, 1), 10),
			// 105 stablecoin at price 10 → floor(10.5) = 10.
			(105, FixedU128::from_rational(10, 1), 10),
			// Sub-1.0 price scales up.
			(100, FixedU128::from_rational(1, 2), 200),
			(0, FixedU128::one(), 0),
		];
		for (value, price, expected) in cases {
			let collateral = collateral_for_value_floor::<u128>(value, price);

			assert_eq!(collateral, Some(expected), "value = {value}, price = {price:?}");
		}
	}

	#[test]
	fn collateral_for_value_ceils() {
		// (value, price, expected).
		let cases = [
			// 100 stablecoin at price 10 → exactly 10 collateral.
			(100, FixedU128::from_rational(10, 1), 10),
			// 105 stablecoin at price 10 → ceil(10.5) = 11 (the floor variant gives 10).
			(105, FixedU128::from_rational(10, 1), 11),
			// Sub-1.0 price scales up: 100 / 0.9 = 111.1… → 112.
			(100, FixedU128::from_rational(9, 10), 112),
			(0, FixedU128::one(), 0),
		];
		for (value, price, expected) in cases {
			let collateral = collateral_for_value_ceil::<u128>(value, price);

			assert_eq!(collateral, Some(expected), "value = {value}, price = {price:?}");
		}
	}

	#[test]
	fn collateral_for_value_fails_loudly() {
		let half = FixedU128::from_rational(1, 2);

		// A zero price cannot size anything, in either rounding direction.
		assert_eq!(collateral_for_value_floor::<u128>(100, FixedU128::zero()), None);
		assert_eq!(collateral_for_value_ceil::<u128>(100, FixedU128::zero()), None);
		// A sub-1.0 price doubles the value past u128::MAX.
		assert_eq!(collateral_for_value_floor::<u128>(u128::MAX, half), None);
		// The result fits u128 but not the caller's narrower Balance.
		assert_eq!(collateral_for_value_floor::<u64>(u64::MAX, half), None);
	}
}
