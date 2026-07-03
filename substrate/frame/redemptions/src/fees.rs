use frame::deps::{
	frame_support::traits::Defensive,
	sp_runtime::{
		helpers_128bit::multiply_by_rational_with_rounding,
		traits::{CheckedDiv, One, Saturating, Zero},
		FixedPointNumber, FixedPointOperand, FixedU128, Rounding,
	},
};

/// Deterministic and monotonic in `elapsed_ms`: more elapsed time never yields
/// a higher decayed rate. The fractional half-life is the secant upper bound of
/// `2^(-f)` on `[0, 1)`, so the result is at or slightly above the exact decay
/// (favoring the system) and is continuous across period boundaries.
pub fn decay_dynamic_fee(dynamic_fee: FixedU128, elapsed_ms: u64, period_ms: u64) -> FixedU128 {
	if dynamic_fee.is_zero() || elapsed_ms == 0 {
		return dynamic_fee;
	}
	if period_ms == 0 {
		return FixedU128::zero();
	}
	let whole = elapsed_ms / period_ms;
	if whole >= 128 {
		return FixedU128::zero();
	}
	let halved = FixedU128::from_inner(dynamic_fee.into_inner() >> whole);
	let remainder = elapsed_ms % period_ms;
	if remainder == 0 {
		return halved;
	}
	let fraction = FixedU128::from_rational(u128::from(remainder), u128::from(period_ms));
	let half = FixedU128::from_rational(1u128, 2u128);
	let factor = FixedU128::one().saturating_sub(fraction.saturating_mul(half));
	halved.saturating_mul(factor)
}

pub fn increased_dynamic_fee(
	decayed: FixedU128,
	redeemed_fraction: FixedU128,
	divisor: FixedU128,
	floor: FixedU128,
	ceiling: FixedU128,
) -> FixedU128 {
	let increase = redeemed_fraction.checked_div(&divisor).unwrap_or_else(FixedU128::zero);
	decayed.saturating_add(increase).max(floor).min(ceiling)
}

pub fn fee_rate(dynamic_fee: FixedU128, base_fee: FixedU128, fee_ceiling: FixedU128) -> FixedU128 {
	dynamic_fee.saturating_add(base_fee).min(fee_ceiling)
}

pub fn fee_pusd<Balance: FixedPointOperand>(
	debt_cancelled: Balance,
	fee_rate: FixedU128,
) -> Balance {
	if debt_cancelled.is_zero() || fee_rate.is_zero() {
		return Balance::zero();
	}
	let a: u128 = debt_cancelled.unique_saturated_into();
	mul_ratio_or(a, fee_rate.into_inner(), FixedU128::DIV, Rounding::Up, Balance::max_value)
}

pub fn max_debt_for_budget<Balance: FixedPointOperand>(
	budget: Balance,
	fee_rate: FixedU128,
) -> Balance {
	if budget.is_zero() {
		return Balance::zero();
	}
	let denom = FixedU128::one().saturating_add(fee_rate);
	let b: u128 = budget.unique_saturated_into();
	mul_ratio_or(b, FixedU128::DIV, denom.into_inner(), Rounding::Down, Balance::zero)
}

/// Partial fills scale the user's slippage floor to the amount actually spent.
pub fn scale_floor<Balance: FixedPointOperand>(
	value: Balance,
	num: Balance,
	denom: Balance,
) -> Balance {
	if denom.is_zero() {
		return Balance::zero();
	}
	let v: u128 = value.unique_saturated_into();
	let n: u128 = num.unique_saturated_into();
	let d: u128 = denom.unique_saturated_into();
	mul_ratio_or(v, n, d, Rounding::Down, Balance::max_value)
}

/// `a * num / denom` at `Balance` precision with the given rounding, falling
/// back defensively when the product cannot be represented as a `Balance`.
fn mul_ratio_or<Balance: FixedPointOperand>(
	a: u128,
	num: u128,
	denom: u128,
	rounding: Rounding,
	fallback: fn() -> Balance,
) -> Balance {
	multiply_by_rational_with_rounding(a, num, denom, rounding)
		.and_then(|raw| Balance::try_from(raw).ok())
		.defensive_unwrap_or_else(fallback)
}

#[cfg(test)]
mod tests {
	use super::*;

	const HOUR: u64 = 3_600_000;

	#[test]
	fn decay_halves_over_one_half_life() {
		let base = FixedU128::from_rational(1, 2); // 50%
		assert_eq!(decay_dynamic_fee(base, 6 * HOUR, 6 * HOUR), FixedU128::from_rational(1, 4));
		assert_eq!(decay_dynamic_fee(base, 12 * HOUR, 6 * HOUR), FixedU128::from_rational(1, 8));
	}

	#[test]
	fn decay_is_monotonic_non_increasing() {
		let base = FixedU128::from_rational(80, 100);
		let mut prev = decay_dynamic_fee(base, 0, 6 * HOUR);
		for h in 1..=48u64 {
			let now = decay_dynamic_fee(base, h * HOUR, 6 * HOUR);
			assert!(now <= prev, "decay rose at hour {h}: {now:?} > {prev:?}");
			prev = now;
		}
	}

	#[test]
	fn decay_zero_inputs() {
		let base = FixedU128::from_rational(1, 2);
		assert_eq!(decay_dynamic_fee(base, 0, HOUR), base);
		assert_eq!(decay_dynamic_fee(FixedU128::zero(), HOUR, HOUR), FixedU128::zero());
		assert_eq!(decay_dynamic_fee(base, 200 * HOUR, HOUR), FixedU128::zero());
	}

	#[test]
	fn increase_caps_at_ceiling() {
		let ceiling = FixedU128::one();
		let got = increased_dynamic_fee(
			FixedU128::from_rational(90, 100),
			FixedU128::from_rational(1, 2),
			FixedU128::from_rational(2, 1),
			FixedU128::zero(),
			ceiling,
		);
		assert_eq!(got, ceiling);
	}

	#[test]
	fn increase_adds_half_of_fraction() {
		let got = increased_dynamic_fee(
			FixedU128::from_rational(10, 100),
			FixedU128::from_rational(40, 100),
			FixedU128::from_rational(2, 1),
			FixedU128::zero(),
			FixedU128::one(),
		);
		assert_eq!(got, FixedU128::from_rational(30, 100));
	}

	#[test]
	fn fee_rate_clamps_to_bounds() {
		let floor = FixedU128::from_rational(5, 1000); // 0.5%
		let ceiling = FixedU128::one();
		assert_eq!(fee_rate(FixedU128::zero(), floor, ceiling), floor);
		assert_eq!(fee_rate(FixedU128::from_rational(2, 1), floor, ceiling), ceiling);
		assert_eq!(
			fee_rate(FixedU128::from_rational(10, 100), floor, ceiling),
			FixedU128::from_rational(105, 1000)
		);
	}

	#[test]
	fn fee_pusd_rounds_up() {
		assert_eq!(fee_pusd::<u128>(100, FixedU128::from_rational(5, 1000)), 1);
		assert_eq!(fee_pusd::<u128>(1000, FixedU128::from_rational(5, 1000)), 5);
		assert_eq!(fee_pusd::<u128>(0, FixedU128::one()), 0);
	}

	#[test]
	fn max_debt_for_budget_accounts_for_fee() {
		assert_eq!(max_debt_for_budget::<u128>(1000, FixedU128::zero()), 1000);
		assert_eq!(max_debt_for_budget::<u128>(1000, FixedU128::one()), 500);
		assert_eq!(max_debt_for_budget::<u128>(1000, FixedU128::from_rational(5, 1000)), 995);
	}

	#[test]
	fn scale_floor_basic() {
		assert_eq!(scale_floor::<u128>(100, 1, 2), 50);
		assert_eq!(scale_floor::<u128>(100, 3, 4), 75);
		assert_eq!(scale_floor::<u128>(100, 1, 0), 0);
	}
}
