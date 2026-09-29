//! Settlement pricing for `FinalRecovery` vaults.
//!
//! `None` means a zero price, an overflow, or inputs outside the function's regime. Callers must
//! treat it as an error.

use crate::{math::collateral_for_value_floor, mul_div_floor};
use frame::deps::sp_runtime::{
	traits::{CheckedAdd, One, Saturating},
	FixedPointNumber, FixedPointOperand, FixedU128, Permill,
};

/// Calculates the stablecoin value of `collateral` at `price` and rounds the result up.
///
/// The result is `ceil(collateral * price)`. Below-par settlement sizes the shortfall the
/// Insurance Fund covers from this value, so the fund never covers more than the collateral is
/// actually short and the redeemer never pays less than the collateral is worth.
///
/// Returns `None` if the result does not fit in `Balance`.
pub use crate::math::mul_rate_ceil as collateral_value_ceil;

/// Calculates the bonus for a recovery vault with `CR >= 100%`.
///
/// The function uses this formula:
/// `min(max(0, cr - 100% - buffer), redistribution_penalty)`.
///
/// Since `bonus <= cr - 100%`, paying it never lowers the vault's CR.
pub fn recovery_bonus(
	cr: FixedU128,
	buffer: Permill,
	redistribution_penalty: Permill,
) -> FixedU128 {
	let excess = cr.saturating_sub(FixedU128::one()).saturating_sub(FixedU128::from(buffer));
	let bonus = excess.min(FixedU128::from(redistribution_penalty));
	debug_assert!(
		bonus <= cr.saturating_sub(FixedU128::one()),
		"recovery bonus must not worsen CR"
	);
	bonus
}

/// Returns the collateral paid for `debt_cancelled` at `CR >= 100%`:
/// `floor(floor(debt_cancelled * (1 + bonus)) / price)`.
pub fn recovery_bonus_collateral_out<Balance: FixedPointOperand>(
	debt_cancelled: Balance,
	bonus: FixedU128,
	price: FixedU128,
) -> Option<Balance> {
	let value = FixedU128::one().checked_add(&bonus)?.checked_mul_int(debt_cancelled)?;
	collateral_for_value_floor(value, price)
}

/// Split of a below-par vault's debt between the market and the Insurance Fund.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct InsuranceAdjusted<Balance> {
	/// Debt redeemers and offset providers cancel against the vault's collateral.
	pub market_cancel_debt: Balance,
	/// Debt the Insurance Fund covers.
	pub effective_cover: Balance,
}

/// Splits the debt of a recovery vault with `CR < 100%`:
///
/// - `effective_cover = min(insurance_available, debt - collateral_value)`
/// - `market_cancel_debt = debt - effective_cover`
///
/// - `effective_cover = min(insurance_available, debt - collateral_value)`.
/// - `market_cancel_debt = debt - effective_cover`.
///
/// The Insurance Fund backs `effective_cover`. `market_cancel_debt` is at least
/// `collateral_value`, so the market settles at a recovery rate of
/// `collateral_value / market_cancel_debt <= 1`.
///
/// Returns `None` if `collateral_value > debt`. Such a vault is above par and outside this
/// function's regime.
pub fn insurance_adjusted<Balance: Copy + Ord + Saturating>(
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
	debug_assert!(market_cancel_debt >= collateral_value);
	Some(InsuranceAdjusted { market_cancel_debt, effective_cover })
}

/// Calculates the collateral payout for a recovery settlement with `CR < 100%`.
///
/// `debt_cancelled` buys its pro-rata share of the vault `collateral`:
/// `floor(debt_cancelled * collateral / market_cancel_debt)`.
///
/// The share is sized in collateral units rather than priced through the recovery rate, so
/// cancelling all of `market_cancel_debt` pays the whole collateral exactly and no rounding
/// dust stays on the vault. `debt_cancelled` must not exceed `market_cancel_debt`.
///
/// Returns `None` if `market_cancel_debt` is zero while `debt_cancelled` is not.
pub fn recovery_collateral_out<Balance: FixedPointOperand>(
	debt_cancelled: Balance,
	collateral: Balance,
	market_cancel_debt: Balance,
) -> Option<Balance> {
	debug_assert!(debt_cancelled <= market_cancel_debt);
	if debt_cancelled.is_zero() {
		return Some(Balance::zero());
	}
	mul_div_floor(debt_cancelled, collateral, market_cancel_debt)
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame::deps::sp_runtime::traits::Zero;

	#[test]
	fn recovery_bonus_capped_by_penalty_and_buffer() {
		let penalty = Permill::from_percent(5);
		// A 1% buffer below which no excess is paid out.
		let buffer = Permill::from_percent(1);
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
		// Without a buffer, a 100% penalty cannot lift the bonus past the CR excess, so paying it
		// never worsens the CR.
		let cr = FixedU128::from_rational(105, 100);
		let bonus = recovery_bonus(cr, Permill::zero(), Permill::from_percent(100));
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
		// Keep the intermediate floor: floor(floor(1 * 1.5) / 1.5) = 0, while a fused
		// calculation would pay one collateral unit.
		assert_eq!(
			recovery_bonus_collateral_out::<u128>(
				1,
				FixedU128::from_rational(1, 2),
				FixedU128::from_rational(3, 2),
			),
			Some(0),
		);
	}

	/// Verifies the below-par split of debt `D` against collateral value `C` and fund `IF`.
	///
	/// The cover is `min(IF, D - C)` and the market cancels the rest, so its recovery rate
	/// `C / market_cancel` stays at most 1.
	#[test]
	fn insurance_adjusted_splits_the_shortfall() {
		// C > D is the `CR > 100%` regime. An unchecked split would price the payout above par.
		assert_eq!(insurance_adjusted::<u128>(1000, 1001, 0), None);
		// (D, C, IF, effective_cover, market_cancel_debt)
		let cases: [(u128, u128, u128, u128, u128); 5] = [
			// The boundary C == D stays in range, with the whole debt on the market side.
			(1000, 1000, 0, 0, 1000),
			// A partial cover gives an effective rate of 800/950 ≈ 0.8421.
			(1000, 800, 50, 50, 950),
			// An empty fund puts the whole debt on the market, at an effective rate of C/D.
			(1000, 800, 0, 0, 1000),
			// A fund beyond the shortfall covers only the shortfall: the market cancels C, at par.
			(1000, 800, 500, 200, 800),
			// Worthless collateral and a fund of at least D leave nothing on the market side.
			(1000, 0, 1000, 1000, 0),
		];
		for (debt, value, fund, cover, market) in cases {
			let split = insurance_adjusted(debt, value, fund).expect("below-par split");
			assert_eq!(
				split,
				InsuranceAdjusted { market_cancel_debt: market, effective_cover: cover },
				"D = {debt}, C = {value}, IF = {fund}"
			);
		}
	}

	#[test]
	fn recovery_collateral_out_is_pro_rata_and_exact_in_full() {
		// D = 10_000, C = 8_000, IF = 1_000 → market_cancel = 9_000 against 4_000 collateral.
		let r = insurance_adjusted::<u128>(10_000, 8_000, 1_000).expect("below-par split");
		assert_eq!(r.market_cancel_debt, 9_000);
		// x = 3_000: floor(3_000 · 4_000 / 9_000) = floor(1_333.3…) = 1_333, rounded against
		// the redeemer.
		assert_eq!(
			recovery_collateral_out::<u128>(3_000, 4_000, r.market_cancel_debt),
			Some(1_333)
		);
		// x = 9_000, the whole market debt: the share is the whole collateral, with no loss.
		assert_eq!(
			recovery_collateral_out::<u128>(9_000, 4_000, r.market_cancel_debt),
			Some(4_000)
		);
		// A zero payment buys nothing, whatever the market debt.
		assert_eq!(recovery_collateral_out::<u128>(0, 4_000, r.market_cancel_debt), Some(0));
		assert_eq!(recovery_collateral_out::<u128>(0, 4_000, 0), Some(0));
	}
}
