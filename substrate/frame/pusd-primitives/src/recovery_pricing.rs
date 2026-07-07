//! This module calculates settlement prices for `FinalRecovery` vaults.
//!
//! Price-conversion functions return `None` if `price` is zero or a calculation overflows.
//! [`insurance_adjusted`] returns `None` when the vault is outside its pricing regime. A caller
//! must treat `None` as an error.

use frame::deps::{
	frame_support::traits::Defensive,
	sp_runtime::{
		helpers_128bit::multiply_by_rational_with_rounding,
		traits::{CheckedAdd, One, Saturating, Zero},
		FixedPointNumber, FixedPointOperand, FixedU128, Permill, Rounding,
	},
};

/// Calculates collateral for a stablecoin `value` at `price`.
///
/// `price` is the stablecoin value of one collateral unit. The function divides `value` by `price`
/// and rounds the result in the specified direction.
///
/// Public functions select the rounding direction that is correct for each economic action.
///
/// Returns `None` if `price` is zero or if the result does not fit in `Balance`.
fn collateral_for_value<Balance: FixedPointOperand>(
	value: Balance,
	price: FixedU128,
	rounding: Rounding,
) -> Option<Balance> {
	multiply_by_rational_with_rounding(
		value.unique_saturated_into(),
		FixedU128::DIV,
		price.into_inner(),
		rounding,
	)
	.and_then(|raw| Balance::try_from(raw).ok())
}

/// Calculates collateral for `value` at `price` and rounds the result down.
///
/// The result is `floor(value / price)`.
///
/// Returns `None` if `price` is zero or if the result does not fit in `Balance`.
pub fn collateral_for_value_floor<Balance: FixedPointOperand>(
	value: Balance,
	price: FixedU128,
) -> Option<Balance> {
	collateral_for_value(value, price, Rounding::Down)
}

/// Calculates collateral for `value` at `price` and rounds the result up.
///
/// The result is `ceil(value / price)`.
///
/// Returns `None` if `price` is zero or if the result does not fit in `Balance`.
pub fn collateral_for_value_ceil<Balance: FixedPointOperand>(
	value: Balance,
	price: FixedU128,
) -> Option<Balance> {
	collateral_for_value(value, price, Rounding::Up)
}

/// Calculates the bonus for a recovery vault with `CR >= 100%`.
///
/// The function uses this formula:
/// `min(max(0, cr - 100% - buffer), redistribution_penalty)`.
///
/// The `buffer` makes `bonus <= cr - 1`. Thus, the bonus does not decrease the vault CR after a
/// redemption.
pub fn recovery_bonus(
	cr: FixedU128,
	buffer: FixedU128,
	redistribution_penalty: Permill,
) -> FixedU128 {
	let excess = cr.saturating_sub(FixedU128::one()).saturating_sub(buffer);
	let bonus = excess.min(FixedU128::from(redistribution_penalty));
	debug_assert!(
		bonus <= cr.saturating_sub(FixedU128::one()),
		"recovery bonus must not worsen CR"
	);
	bonus
}

/// Calculates the collateral payout for a recovery settlement with `CR >= 100%`.
///
/// The calculation applies `bonus` to `debt_cancelled` stablecoin at face value:
/// `floor(floor(debt_cancelled * (1 + bonus)) / price)`.
///
/// Returns `None` if `price` is zero or if a calculation overflows.
pub fn recovery_bonus_collateral_out<Balance: FixedPointOperand>(
	debt_cancelled: Balance,
	bonus: FixedU128,
	price: FixedU128,
) -> Option<Balance> {
	let value = FixedU128::one().checked_add(&bonus)?.checked_mul_int(debt_cancelled)?;
	collateral_for_value_floor(value, price)
}

/// Contains an insurance-adjusted settlement split.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InsuranceAdjusted<Balance> {
	/// Debt that redeemers and offset providers can cancel against this vault's collateral.
	pub market_cancel_debt: Balance,
	/// Debt that the Insurance Fund covers.
	pub effective_cover: Balance,
	/// Stablecoin value paid per unit of market-cancelled debt. This value is at most one.
	pub recovery_rate: FixedU128,
}

/// Calculates the insurance-adjusted settlement for a recovery vault with `CR < 100%`.
///
/// `collateral_value` is the held collateral value in stablecoin units. The function uses these
/// formulas:
///
/// - `effective_cover = min(insurance_available, debt - collateral_value)`.
/// - `market_cancel_debt = debt - effective_cover`.
/// - `recovery_rate = collateral_value / market_cancel_debt <= 1`.
///
/// The Insurance Fund backs `effective_cover`.
///
/// Returns `None` if `collateral_value > debt`. Such a vault is above par and outside this
/// function's regime. Without this check, `recovery_rate` can exceed one.
pub fn insurance_adjusted<Balance: FixedPointOperand + Saturating + Ord>(
	debt: Balance,
	collateral_value: Balance,
	insurance_available: Balance,
) -> Option<InsuranceAdjusted<Balance>> {
	if collateral_value > debt {
		return None;
	}
	let shortfall = debt.saturating_sub(collateral_value);
	let effective_cover = core::cmp::min(insurance_available, shortfall);
	let market_cancel_debt = debt.saturating_sub(effective_cover);
	let recovery_rate = if market_cancel_debt.is_zero() {
		FixedU128::zero()
	} else {
		// `market_cancel_debt` is not less than `collateral_value`. Thus, the ratio is at most
		// one. This branch also guarantees that the denominator is not zero.
		FixedU128::checked_from_rational(collateral_value, market_cancel_debt)
			.defensive_unwrap_or_else(FixedU128::zero)
	};
	debug_assert!(recovery_rate <= FixedU128::one());
	Some(InsuranceAdjusted { market_cancel_debt, effective_cover, recovery_rate })
}

/// Calculates the collateral payout for a recovery settlement with `CR < 100%`.
///
/// The function uses this formula for `stablecoin_in`:
/// `floor(floor(stablecoin_in * recovery_rate) / price)`.
///
/// Returns `None` if `price` is zero or if a calculation overflows.
pub fn recovery_rate_collateral_out<Balance: FixedPointOperand>(
	stablecoin_in: Balance,
	recovery_rate: FixedU128,
	price: FixedU128,
) -> Option<Balance> {
	let value = recovery_rate.checked_mul_int(stablecoin_in)?;
	collateral_for_value_floor(value, price)
}

#[cfg(test)]
mod tests {
	use super::*;

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

	#[test]
	fn recovery_bonus_capped_by_penalty_and_buffer() {
		let penalty = Permill::from_percent(5);
		// A 1% buffer below which no excess is paid out.
		let buffer = FixedU128::from_rational(1, 100);
		// CR = 130% → excess = 30% - 1% = 29%, capped at 5%.
		let cr = FixedU128::from_rational(130, 100);
		assert_eq!(recovery_bonus(cr, buffer, penalty), FixedU128::from_rational(5, 100));
		// CR = 102% → excess = 2% - 1% = 1% < 5% cap.
		let cr = FixedU128::from_rational(102, 100);
		assert_eq!(recovery_bonus(cr, buffer, penalty), FixedU128::from_rational(1, 100));
		// CR = 101% sits exactly at 100% + buffer: the excess is zero, so the
		// buffer guarantees the bonus never reaches into CR − 100% itself.
		let cr = FixedU128::from_rational(101, 100);
		assert_eq!(recovery_bonus(cr, buffer, penalty), FixedU128::zero());
		// CR = 100% → excess saturates to 0.
		assert_eq!(recovery_bonus(FixedU128::one(), buffer, penalty), FixedU128::zero());
		// CR below 100% (an underwater vault) → still 0, no underflow.
		let cr = FixedU128::from_rational(99, 100);
		assert_eq!(recovery_bonus(cr, buffer, penalty), FixedU128::zero());
	}

	#[test]
	fn recovery_bonus_never_worsens_cr() {
		// bonus <= cr - 1 for any inputs; here a huge penalty cannot exceed the
		// CR-derived excess.
		let cr = FixedU128::from_rational(105, 100);
		let bonus = recovery_bonus(cr, FixedU128::zero(), Permill::from_percent(100));
		assert!(bonus <= cr.saturating_sub(FixedU128::one()));
		assert_eq!(bonus, FixedU128::from_rational(5, 100));
	}

	#[test]
	fn recovery_bonus_collateral_out_includes_bonus() {
		// 100 debt, 5% bonus, price 10 → floor(105 / 10) = 10.
		let bonus = FixedU128::from_rational(5, 100);
		let price = FixedU128::from_rational(10, 1);
		assert_eq!(recovery_bonus_collateral_out::<u128>(100, bonus, price), Some(10));
		// 200 debt, 5% bonus, price 10 → floor(210 / 10) = 21.
		assert_eq!(recovery_bonus_collateral_out::<u128>(200, bonus, price), Some(21));
		// A zero price cannot size a payout.
		assert_eq!(recovery_bonus_collateral_out::<u128>(100, bonus, FixedU128::zero()), None);
	}

	#[test]
	fn insurance_adjusted_rejects_above_par() {
		// C > D is the `CR > 100%` regime. An unchecked split would price the payout above par.
		// The boundary C == D stays in range with rate 1.
		assert_eq!(insurance_adjusted::<u128>(1000, 1001, 0), None);
		let r = insurance_adjusted::<u128>(1000, 1000, 0).expect("C == D is below-par boundary");
		assert_eq!(r.effective_cover, 0);
		assert_eq!(r.market_cancel_debt, 1000);
		assert_eq!(r.recovery_rate, FixedU128::one());
	}

	#[test]
	fn insurance_adjusted_partial_cover() {
		// D = 1000, C = 800 (shortfall 200), IF = 50.
		// effective_cover = 50, market_cancel = 950, rate = 800/950 ≈ 0.8421.
		let r = insurance_adjusted::<u128>(1000, 800, 50).expect("below-par split");
		assert_eq!(r.effective_cover, 50);
		assert_eq!(r.market_cancel_debt, 950);
		assert!(r.recovery_rate <= FixedU128::one());
		assert!(r.recovery_rate > FixedU128::from_rational(84, 100));
		assert!(r.recovery_rate < FixedU128::from_rational(85, 100));
	}

	#[test]
	fn insurance_adjusted_empty_fund_uses_c_over_d() {
		// IF = 0 → effective_cover = 0, market_cancel = D, rate = C/D.
		let r = insurance_adjusted::<u128>(1000, 800, 0).expect("below-par split");
		assert_eq!(r.effective_cover, 0);
		assert_eq!(r.market_cancel_debt, 1000);
		assert_eq!(r.recovery_rate, FixedU128::from_rational(800, 1000));
	}

	#[test]
	fn insurance_adjusted_full_cover_zero_market() {
		// IF covers the whole shortfall and then some: market_cancel = C.
		// D = 1000, C = 800, IF = 500 → cover = min(500, 200) = 200,
		// market_cancel = 800, rate = 800/800 = 1.0.
		let r = insurance_adjusted::<u128>(1000, 800, 500).expect("below-par split");
		assert_eq!(r.effective_cover, 200);
		assert_eq!(r.market_cancel_debt, 800);
		assert_eq!(r.recovery_rate, FixedU128::one());
	}

	#[test]
	fn insurance_adjusted_fund_covers_all_debt() {
		// C = 0 (collateral worthless), IF >= D → market_cancel = 0.
		let r = insurance_adjusted::<u128>(1000, 0, 1000).expect("below-par split");
		assert_eq!(r.effective_cover, 1000);
		assert_eq!(r.market_cancel_debt, 0);
		assert_eq!(r.recovery_rate, FixedU128::zero());
	}

	#[test]
	fn recovery_rate_collateral_out_floors_twice() {
		// D = 10_000, C = 8_000, IF = 1_000 → market_cancel = 9_000 at a
		// fixed-point rate truncated just below the true 8_000/9_000.
		let r = insurance_adjusted::<u128>(10_000, 8_000, 1_000).expect("below-par split");
		assert_eq!(r.effective_cover, 1_000);
		assert_eq!(r.market_cancel_debt, 9_000);
		let price = FixedU128::from_rational(2, 1);
		// Double flooring is intentional: value first, then collateral units.
		// x = 3_000: value floor(3_000 · 0.888…) = 2_666, out floor(2_666/2) = 1_333.
		assert_eq!(
			recovery_rate_collateral_out::<u128>(3_000, r.recovery_rate, price),
			Some(1_333)
		);
		// x = 9_000 (the full market debt): the truncated rate already loses one
		// unit (value 7_999, not 8_000) and the second floor keeps the loss:
		// out floor(7_999/2) = 3_999 — both floors round against the redeemer.
		assert_eq!(
			recovery_rate_collateral_out::<u128>(9_000, r.recovery_rate, price),
			Some(3_999)
		);
	}
}
