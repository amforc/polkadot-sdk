//! Storage-free math helpers for vault accounting.
//!
//! Interest calculations retain fractional residue until attribution. A terminal settlement
//! rounds the remaining fraction up once for the protocol.
//!
//! Overflow triggers a debug failure and saturates in release.

use frame::{
	arithmetic::{
		helpers_128bit::multiply_by_rational_with_rounding, FixedPointNumber, FixedPointOperand,
		FixedU128, One, Rounding, Zero,
	},
	deps::sp_core::U256,
	traits::Defensive,
};
use pusd_primitives::MILLIS_PER_YEAR;

/// Returns simple interest rounded up.
///
/// Used for market interest and upfront fees.
///
/// Uses `ceil(principal * rate * delta_millis / MILLIS_PER_YEAR)` to preserve precision.
pub fn simple_interest_ceil<Balance: FixedPointOperand>(
	principal: Balance,
	rate: FixedU128,
	delta_millis: u64,
) -> Balance {
	if principal.is_zero() || rate.is_zero() || delta_millis == 0 {
		return Balance::zero();
	}
	let p: u128 = principal.unique_saturated_into();
	let rate_times_delta = rate.into_inner().saturating_mul(u128::from(delta_millis));
	let denom = FixedU128::DIV.saturating_mul(u128::from(MILLIS_PER_YEAR));
	multiply_by_rational_with_rounding(p, rate_times_delta, denom, Rounding::Up)
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

/// Returns a floored redistribution increment and its numerator residue.
///
/// The result preserves this identity:
/// `amount * DIV + carry_in = increment_inner * total_stake + carry_out`.
/// The residue conserves indivisible value without selecting a recipient.
pub fn redistribution_per_stake_with_carry<Balance: FixedPointOperand>(
	amount: Balance,
	total_stake: Balance,
	carry: u128,
) -> Option<(FixedU128, u128)> {
	let amount: u128 = amount.unique_saturated_into();
	let total: u128 = total_stake.unique_saturated_into();
	if total == 0 {
		return None;
	}
	let numerator = U256::from(amount)
		.checked_mul(U256::from(FixedU128::DIV))?
		.checked_add(U256::from(carry % total))?;
	let (quotient, remainder) = numerator.div_mod(U256::from(total));
	if quotient > U256::from(u128::MAX) {
		return None;
	}
	Some((FixedU128::from_inner(quotient.low_u128()), remainder.low_u128()))
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame::arithmetic::Saturating;

	#[test]
	fn simple_interest_ceil_zero_inputs() {
		assert_eq!(simple_interest_ceil::<u128>(0, FixedU128::one(), 1_000), 0);
		assert_eq!(simple_interest_ceil::<u128>(1_000, FixedU128::zero(), 1_000), 0);
		assert_eq!(simple_interest_ceil::<u128>(1_000, FixedU128::one(), 0), 0);
	}

	#[test]
	fn simple_interest_ceil_exact_year_has_no_rounding() {
		let r = FixedU128::saturating_from_rational(10u32, 100u32);
		let got = simple_interest_ceil::<u128>(1_000_000, r, MILLIS_PER_YEAR);
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
	fn redistribution_carry_telescopes_repeated_rounds() {
		let stake = 3u128;
		let (first, carry) = redistribution_per_stake_with_carry(1u128, stake, 0).unwrap();
		let (second, carry) = redistribution_per_stake_with_carry(1u128, stake, carry).unwrap();
		let (third, carry) = redistribution_per_stake_with_carry(1u128, stake, carry).unwrap();
		assert_eq!(first.into_inner() + second.into_inner() + third.into_inner(), FixedU128::DIV);
		assert_eq!(carry, 0);
	}
}
