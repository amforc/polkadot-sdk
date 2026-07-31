//! Storage-free math helpers for vault accounting.
//!
//! Aggregate interest and fees round up. Vault interest and redistribution shares round down.
//!
//! Overflow in non-`Option` helpers triggers a debug failure and saturates in release. `Option`
//! helpers return `None`.

use frame::{
	arithmetic::{
		helpers_128bit::multiply_by_rational_with_rounding, FixedPointNumber, FixedPointOperand,
		FixedU128, One, Rounding, Zero,
	},
	traits::Defensive,
};
use pusd_primitives::MILLIS_PER_YEAR;

/// Returns simple interest rounded down.
///
/// Used for interest assigned to one vault.
///
/// The result is `floor(principal * rate * delta_millis / MILLIS_PER_YEAR)`.
pub fn simple_interest_floor<Balance: FixedPointOperand>(
	principal: Balance,
	rate: FixedU128,
	delta_millis: u64,
) -> Balance {
	simple_interest_with_rounding(principal, rate, delta_millis, Rounding::Down)
}

/// Returns simple interest rounded up.
///
/// Used for market interest and upfront fees.
///
/// The result is `ceil(principal * rate * delta_millis / MILLIS_PER_YEAR)`.
pub fn simple_interest_ceil<Balance: FixedPointOperand>(
	principal: Balance,
	rate: FixedU128,
	delta_millis: u64,
) -> Balance {
	simple_interest_with_rounding(principal, rate, delta_millis, Rounding::Up)
}

/// Calculates simple interest with the requested rounding.
///
/// The full formula is evaluated at once. This avoids losing precision in a small intermediate
/// rate factor.
fn simple_interest_with_rounding<Balance: FixedPointOperand>(
	principal: Balance,
	rate: FixedU128,
	delta_millis: u64,
	rounding: Rounding,
) -> Balance {
	if principal.is_zero() || rate.is_zero() || delta_millis == 0 {
		return Balance::zero();
	}
	let p: u128 = principal.unique_saturated_into();
	let rate_times_delta = rate.into_inner().saturating_mul(u128::from(delta_millis));
	let denom = FixedU128::DIV.saturating_mul(u128::from(MILLIS_PER_YEAR));
	multiply_by_rational_with_rounding(p, rate_times_delta, denom, rounding)
		.and_then(|raw| Balance::try_from(raw).ok())
		.defensive_unwrap_or_else(Balance::max_value)
}

/// Returns the market's average rate, rounded up.
///
/// `weighted_sum` is the sum of each debt multiplied by its rate. Zero debt returns `1.0`, which
/// keeps the first vault's fee calculation safe.
pub fn average_branch_rate<Balance: FixedPointOperand>(
	weighted_sum: Balance,
	total_ib_debt: Balance,
) -> FixedU128 {
	if total_ib_debt.is_zero() {
		return FixedU128::one();
	}
	let w: u128 = weighted_sum.unique_saturated_into();
	let t: u128 = total_ib_debt.unique_saturated_into();
	let inner = multiply_by_rational_with_rounding(w, FixedU128::DIV, t, Rounding::Up)
		.defensive_unwrap_or(u128::MAX);
	FixedU128::from_inner(inner)
}

/// Returns redistributed value per unit of stake, rounded down.
///
/// Returns `None` when total stake is zero or the result overflows.
pub fn redistribution_per_stake<Balance: FixedPointOperand>(
	num: Balance,
	denom: Balance,
) -> Option<FixedU128> {
	pusd_primitives::mul_div_rate_floor(num, FixedU128::one(), denom)
}

/// Returns rate-weighted redistributed debt per unit of stake, rounded down.
///
/// Returns `None` when total stake is zero or the result overflows.
pub fn redistribution_weight_per_stake<Balance: FixedPointOperand>(
	redistributed_debt: Balance,
	avg_rate: FixedU128,
	denom: Balance,
) -> Option<FixedU128> {
	pusd_primitives::mul_div_rate_floor(redistributed_debt, avg_rate, denom)
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame::arithmetic::Saturating;

	#[test]
	fn simple_interest_floor_zero_inputs() {
		assert_eq!(simple_interest_floor::<u128>(0, FixedU128::one(), 1_000), 0);
		assert_eq!(simple_interest_floor::<u128>(1_000, FixedU128::zero(), 1_000), 0);
		assert_eq!(simple_interest_floor::<u128>(1_000, FixedU128::one(), 0), 0);
	}

	#[test]
	fn simple_interest_floor_basic() {
		// Ten percent of 1,000,000 over one year is 100,000.
		let r = FixedU128::saturating_from_rational(10u32, 100u32);
		let got = simple_interest_floor::<u128>(1_000_000, r, MILLIS_PER_YEAR);
		assert_eq!(got, 100_000);
	}

	#[test]
	fn simple_interest_ceil_rounds_up_on_remainder() {
		// A positive fraction rounds up to one.
		let got = simple_interest_ceil::<u128>(3, FixedU128::one(), 1);
		assert_eq!(got, 1);
	}

	#[test]
	fn average_branch_rate_recovers_rate_fraction() {
		// A weighted value of 500 over 10,000 debt is a 5% rate.
		let avg = average_branch_rate::<u128>(500, 10_000);
		assert_eq!(avg, FixedU128::from_rational(5u128, 100u128));
	}

	#[test]
	fn average_branch_rate_ceils_in_protocol_favor() {
		// An exact 7% rate does not need rounding.
		let avg = average_branch_rate::<u128>(700, 10_000);
		assert_eq!(avg, FixedU128::from_rational(7u128, 100u128));

		// One third rounds up by one smallest fixed-point unit.
		let avg = average_branch_rate::<u128>(1, 3);
		assert!(avg > FixedU128::from_rational(1u128, 3u128));
		// The difference is at most one smallest unit.
		assert!(
			avg.saturating_sub(FixedU128::from_rational(1u128, 3u128)) <= FixedU128::from_inner(1)
		);
	}

	#[test]
	fn average_branch_rate_zero_debt_returns_one() {
		// An empty market uses 1.0 for its first fee calculation.
		let avg = average_branch_rate::<u128>(0, 0);
		assert_eq!(avg, FixedU128::one());
	}

	#[test]
	fn redistribution_per_stake_round_trips_small_inputs() {
		// 100 divided by 1,000 is 0.1.
		let got = redistribution_per_stake::<u128>(100, 1_000).expect("fits");
		assert_eq!(got, FixedU128::from_rational(1u128, 10u128));
	}

	#[test]
	fn redistribution_per_stake_zero_num_returns_zero() {
		assert_eq!(redistribution_per_stake::<u128>(0, 1_000), Some(FixedU128::zero()));
		assert_eq!(redistribution_per_stake::<u128>(0, 1), Some(FixedU128::zero()));
	}

	#[test]
	fn redistribution_per_stake_overflow_returns_none() {
		let got = redistribution_per_stake::<u128>(u128::MAX / 2, 1);
		assert!(got.is_none());
		// A value below the overflow limit still fits.
		let safe = redistribution_per_stake::<u128>(u128::MAX / (FixedU128::DIV * 2), 1);
		assert!(safe.is_some());
	}

	#[test]
	fn redistribution_weight_per_stake_matches_two_step_when_safe() {
		// Both safe calculation paths give 5.0 per unit of stake.
		let avg = FixedU128::from_rational(5u128, 100u128);
		let got = redistribution_weight_per_stake::<u128>(10_000, avg, 100).expect("fits");
		let two_step = FixedU128::from_rational(10_000u128, 100u128).saturating_mul(avg);
		assert!(got.into_inner().abs_diff(two_step.into_inner()) <= 1);
	}

	#[test]
	fn redistribution_weight_per_stake_avoids_two_step_overflow() {
		// The debt-per-stake intermediate would overflow, but the small rate keeps the final value
		// valid.
		let avg = FixedU128::from_rational(1u128, u128::from(u64::MAX));
		let got = redistribution_weight_per_stake::<u128>(u128::MAX / 4, avg, 1);
		assert!(
			got.is_some(),
			"one-shot weight helper should survive when avg_rate keeps the quotient bounded"
		);
	}
}
