use frame::deps::{
	frame_support::traits::Defensive,
	sp_runtime::{
		helpers_128bit::multiply_by_rational_with_rounding,
		traits::{CheckedDiv, One, Saturating, Zero},
		FixedPointNumber, FixedPointOperand, FixedU128, Permill, Rounding,
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

/// `min(dynamic_fee + base_fee, fee_ceiling)`. The `Permill` bounds widen
/// losslessly into the fixed-point domain the dynamic fee lives in.
pub fn fee_rate(dynamic_fee: FixedU128, base_fee: Permill, fee_ceiling: Permill) -> FixedU128 {
	dynamic_fee.saturating_add(base_fee.into()).min(fee_ceiling.into())
}

pub fn redemption_fee<Balance: FixedPointOperand>(
	debt_cancelled: Balance,
	fee_rate: FixedU128,
) -> Balance {
	if debt_cancelled.is_zero() || fee_rate.is_zero() {
		return Balance::zero();
	}
	let a: u128 = debt_cancelled.unique_saturated_into();
	mul_ratio_or(a, fee_rate.into_inner(), FixedU128::DIV, Rounding::Up, Balance::max_value)
}

/// Greatest debt at or below `max_debt` whose debt plus monotonic `fee_for`
/// fits `budget`.
pub fn max_debt_for_budget<Balance: FixedPointOperand>(
	budget: Balance,
	max_debt: Balance,
	fee_for: impl Fn(Balance) -> Balance,
) -> Balance {
	if budget.is_zero() || max_debt.is_zero() {
		return Balance::zero();
	}
	let budget: u128 = budget.unique_saturated_into();
	let mut low = 0u128;
	let mut high = budget.min(max_debt.unique_saturated_into());
	while low < high {
		let mid = low.saturating_add(high.saturating_sub(low) / 2).saturating_add(1);
		let debt = Balance::try_from(mid).ok().defensive_unwrap_or_else(Balance::zero);
		let fee: u128 = fee_for(debt).unique_saturated_into();
		if mid.saturating_add(fee) <= budget {
			low = mid;
		} else {
			high = mid.saturating_sub(1);
		}
	}
	Balance::try_from(low).ok().defensive_unwrap_or_else(Balance::zero)
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
	pusd_primitives::mul_div_floor(value, num, denom).defensive_unwrap_or_else(Balance::max_value)
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
		let base = Permill::from_rational(5u32, 1_000u32); // 0.5%
		let ceiling = Permill::one();
		assert_eq!(fee_rate(FixedU128::zero(), base, ceiling), FixedU128::from_rational(5, 1000));
		assert_eq!(fee_rate(FixedU128::from_rational(2, 1), base, ceiling), FixedU128::one());
		assert_eq!(
			fee_rate(FixedU128::from_rational(10, 100), base, ceiling),
			FixedU128::from_rational(105, 1000)
		);
	}

	#[test]
	fn redemption_fee_rounds_up() {
		assert_eq!(redemption_fee::<u128>(100, FixedU128::from_rational(5, 1000)), 1);
		assert_eq!(redemption_fee::<u128>(1000, FixedU128::from_rational(5, 1000)), 5);
		assert_eq!(redemption_fee::<u128>(0, FixedU128::one()), 0);
	}

	#[test]
	fn max_debt_for_budget_accounts_for_fee() {
		let at = |rate| move |debt| redemption_fee(debt, rate);
		assert_eq!(max_debt_for_budget::<u128>(1000, 1000, at(FixedU128::zero())), 1000);
		assert_eq!(max_debt_for_budget::<u128>(1000, 1000, at(FixedU128::one())), 500);
		assert_eq!(
			max_debt_for_budget::<u128>(1000, 1000, at(FixedU128::from_rational(5, 1000))),
			995
		);
		assert_eq!(max_debt_for_budget::<u128>(1000, 400, at(FixedU128::zero())), 400);
	}

	#[test]
	fn scale_floor_basic() {
		assert_eq!(scale_floor::<u128>(100, 1, 2), 50);
		assert_eq!(scale_floor::<u128>(100, 3, 4), 75);
		assert_eq!(scale_floor::<u128>(100, 1, 0), 0);
	}
}
