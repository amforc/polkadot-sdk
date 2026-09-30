//! Storage-free math helpers for vault accounting.
//!
//! Every rounding favors the protocol. A vault's debt never falls below its exact accrued interest.
//! Aggregates stay exact, as wide integers, so markets issue their yield without vault touches.
//!
//! Overflow triggers a debug failure and saturates in release.

use frame::{
	arithmetic::{CheckedAdd, FixedPointNumber, FixedPointOperand, FixedU128, One, Rounding, Zero},
	deps::sp_core::U256,
	traits::Defensive,
};
use pusd_primitives::{math::mul_div, MILLIS_PER_YEAR};

/// Denominator of every interest numerator, `principal × rate-inner × millis`: one whole unit
/// of interest per year of one whole unit of principal at rate one.
pub const INTEREST_DENOMINATOR: u128 = FixedU128::DIV * MILLIS_PER_YEAR as u128;

const _: () = assert!(INTEREST_DENOMINATOR < 1 << 127);

/// Returns the exact accrual rate of `principal` at `rate`: `principal × rate-inner`.
///
/// Both factors fit `u128`, so the product always fits `U256`.
pub fn accrual_rate<Balance: FixedPointOperand>(principal: Balance, rate: FixedU128) -> U256 {
	let principal: u128 = principal.unique_saturated_into();
	U256::from(principal) * U256::from(rate.into_inner())
}

/// Returns the interest numerator an accrual rate earns over `elapsed` milliseconds.
pub fn interest_numerator(accrual_rate: U256, elapsed: u64) -> Option<U256> {
	accrual_rate.checked_mul(U256::from(elapsed))
}

/// Returns the accrual rate a vault claims from the pending redistribution pool, rounded up.
///
/// The pool is posted rounded down, so the vaults' claims always cover it.
pub fn claimable_accrual_rate(
	stake_accrual_rate: U256,
	unclaimed_per_stake: FixedU128,
) -> Option<U256> {
	scale_wide(stake_accrual_rate, U256::from(unclaimed_per_stake.into_inner()), Rounding::Up)
}

/// Splits an interest numerator into whole units and the residue below one unit.
pub fn split_interest<Balance: FixedPointOperand>(numerator: U256) -> Option<(Balance, u128)> {
	let (whole, residue) = numerator.div_mod(U256::from(INTEREST_DENOMINATOR));
	let whole = Balance::try_from(u128::try_from(whole).ok()?).ok()?;
	Some((whole, residue.low_u128()))
}

/// Returns the whole units of an interest numerator, rounded up.
pub fn interest_units_ceil<Balance: FixedPointOperand + CheckedAdd + One>(
	numerator: U256,
) -> Option<Balance> {
	let (whole, residue) = split_interest::<Balance>(numerator)?;
	if residue == 0 {
		Some(whole)
	} else {
		whole.checked_add(&Balance::one())
	}
}

/// Charges accrued vault interest rounded up, against the sub-unit excess charged before.
///
/// `accrued` is a numerator over [`INTEREST_DENOMINATOR`] and `prepaid` the excess the vault
/// already paid, below one unit. Returns the whole units to add and the new excess. A touch that
/// accrues less than the excess charges nothing, so repeated touches cannot add units: the
/// recorded debt stays within one unit above the exact debt.
pub fn charge_interest<Balance: FixedPointOperand>(
	accrued: U256,
	prepaid: u128,
) -> Option<(Balance, u128)> {
	debug_assert!(prepaid < INTEREST_DENOMINATOR);
	let prepaid = U256::from(prepaid);
	if accrued <= prepaid {
		return Some((Balance::zero(), (prepaid - accrued).low_u128()));
	}
	let (whole, excess) = (accrued - prepaid).div_mod(U256::from(INTEREST_DENOMINATOR));
	let (whole, prepaid) = if excess.is_zero() {
		(whole, 0)
	} else {
		(whole.checked_add(U256::one())?, INTEREST_DENOMINATOR - excess.low_u128())
	};
	let whole = Balance::try_from(u128::try_from(whole).ok()?).ok()?;
	Some((whole, prepaid))
}

/// Returns `value · factor / FixedU128::DIV` with `rounding`.
///
/// Splitting `value` at `DIV` keeps the product within 256 bits: the whole part scales exactly and
/// only the fraction is divided.
pub fn scale_wide(value: U256, factor: U256, rounding: Rounding) -> Option<U256> {
	let (whole, fraction) = value.div_mod(U256::from(FixedU128::DIV));
	let whole = whole.checked_mul(factor)?;
	let (fraction, residue) = fraction.checked_mul(factor)?.div_mod(U256::from(FixedU128::DIV));
	let fraction = match rounding {
		Rounding::Up if !residue.is_zero() => fraction.checked_add(U256::one())?,
		_ => fraction,
	};
	whole.checked_add(fraction)
}

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
	mul_div(p, rate_times_delta, denom, Rounding::Up).defensive_unwrap_or_else(Balance::max_value)
}

/// Returns the market's average rate, rounded up.
///
/// `accrual_rate` is the sum of each debt multiplied by its rate, in whole units. Zero debt
/// returns `1.0`, which keeps the first vault's fee calculation safe.
pub fn average_branch_rate<Balance: FixedPointOperand>(
	accrual_rate: Balance,
	total_ib_debt: Balance,
) -> FixedU128 {
	if total_ib_debt.is_zero() {
		return FixedU128::one();
	}
	let w: u128 = accrual_rate.unique_saturated_into();
	let t: u128 = total_ib_debt.unique_saturated_into();
	let inner = mul_div(w, FixedU128::DIV, t, Rounding::Up).defensive_unwrap_or(u128::MAX);
	FixedU128::from_inner(inner)
}

/// Returns the redistribution increment per unit of stake, rounded down.
///
/// Vaults claim at most the pool, and the last stake bearer claims the whole residue.
pub fn redistribution_per_stake<Balance: FixedPointOperand>(
	amount: Balance,
	total_stake: Balance,
) -> Option<FixedU128> {
	mul_div(
		amount.unique_saturated_into(),
		FixedU128::DIV,
		total_stake.unique_saturated_into(),
		Rounding::Down,
	)
	.map(FixedU128::from_inner)
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame::arithmetic::Saturating;

	#[test]
	fn charged_interest_never_falls_below_the_exact_amount() {
		let unit = INTEREST_DENOMINATOR;
		// A third of a unit rounds up to one and prepays two thirds.
		let (charged, prepaid) = charge_interest::<u128>(U256::from(unit / 3), 0).unwrap();
		assert_eq!(charged, 1);
		assert_eq!(prepaid, unit - unit / 3);
		// Touching again inside the prepaid excess charges nothing.
		let (charged, prepaid_after) =
			charge_interest::<u128>(U256::from(unit / 3), prepaid).unwrap();
		assert_eq!(charged, 0);
		assert_eq!(prepaid_after, prepaid - unit / 3);
		// An exact multiple leaves no excess.
		assert_eq!(charge_interest::<u128>(U256::from(unit * 5), 0), Some((5, 0)));
		assert_eq!(charge_interest::<u64>(U256::from(u128::MAX) * U256::from(unit), 0), None);
	}

	#[test]
	fn repeated_touches_charge_the_rounded_up_total_once() {
		let step = INTEREST_DENOMINATOR / 7;
		let mut total = 0u128;
		let mut prepaid = 0;
		for _ in 0..70 {
			let (charged, next) = charge_interest::<u128>(U256::from(step), prepaid).unwrap();
			total += charged;
			prepaid = next;
		}
		let exact = U256::from(step) * U256::from(70u32);
		let ceil = exact.div_mod(U256::from(INTEREST_DENOMINATOR));
		let expected = ceil.0.low_u128() + u128::from(!ceil.1.is_zero());
		assert_eq!(total, expected);
	}

	#[test]
	fn scale_wide_rounds_the_fraction_only() {
		let div = U256::from(FixedU128::DIV);
		assert_eq!(scale_wide(div * 3, U256::from(5u32), Rounding::Down), Some(U256::from(15u32)));
		assert_eq!(scale_wide(U256::one(), U256::one(), Rounding::Down), Some(U256::zero()));
		assert_eq!(scale_wide(U256::one(), U256::one(), Rounding::Up), Some(U256::one()));
	}

	#[test]
	fn redistribution_per_stake_rounds_down() {
		assert_eq!(
			redistribution_per_stake(1u128, 3),
			Some(FixedU128::from_inner(FixedU128::DIV / 3))
		);
		assert_eq!(redistribution_per_stake(1u128, 0), None);
	}

	#[test]
	fn simple_interest_ceil_is_exact_or_rounds_up() {
		let one = FixedU128::one();
		let ten_percent = FixedU128::saturating_from_rational(10u32, 100u32);
		// Each case is `(principal, rate, elapsed milliseconds, interest)`.
		let cases: [(u128, FixedU128, u64, u128); 5] = [
			// Any zero input yields no interest.
			(0, one, 1_000, 0),
			(1_000, FixedU128::zero(), 1_000, 0),
			(1_000, one, 0, 0),
			// An exact year has no rounding.
			(1_000_000, ten_percent, MILLIS_PER_YEAR, 100_000),
			// A positive fraction rounds up to one.
			(3, one, 1, 1),
		];
		for (principal, rate, elapsed, interest) in cases {
			assert_eq!(
				simple_interest_ceil::<u128>(principal, rate, elapsed),
				interest,
				"{principal} at {rate:?} over {elapsed} ms"
			);
		}
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
}
