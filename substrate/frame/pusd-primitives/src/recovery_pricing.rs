//! Settlement-pricing helpers for `FinalRecovery` vaults.

use frame::deps::{
	frame_support::traits::Defensive,
	sp_runtime::{
		helpers_128bit::multiply_by_rational_with_rounding,
		traits::{One, Saturating, Zero},
		FixedPointNumber, FixedPointOperand, FixedU128, Permill, Rounding,
	},
};

/// `floor(value / price)`: collateral units obtained for `value` pUSD at
/// `price` (pUSD per collateral unit). Zero when `value` or `price` is zero.
pub fn collateral_for_value<Balance: FixedPointOperand>(
	value: Balance,
	price: FixedU128,
) -> Balance {
	if value.is_zero() || price.is_zero() {
		return Balance::zero();
	}
	let v: u128 = value.unique_saturated_into();
	multiply_by_rational_with_rounding(v, FixedU128::DIV, price.into_inner(), Rounding::Down)
		.and_then(|raw| Balance::try_from(raw).ok())
		.defensive_unwrap_or_else(Balance::zero)
}

/// Recovery bonus for a `CR >= 100%` recovery vault:
/// `min(max(0, cr - 100% - buffer), redistribution_penalty)`.
///
/// The buffer guarantees `bonus <= cr - 1`, so applying it can never lower the
/// vault's CR below its pre-redemption value.
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

/// Collateral paid out in the `CR >= 100%` regime for cancelling `debt_cancelled`
/// pUSD at face value plus `bonus`: `floor(debt_cancelled * (1 + bonus) / price)`.
pub fn recovery_bonus_collateral_out<Balance: FixedPointOperand>(
	debt_cancelled: Balance,
	bonus: FixedU128,
	price: FixedU128,
) -> Balance {
	let factor = FixedU128::one().saturating_add(bonus);
	let value = factor.saturating_mul_int(debt_cancelled);
	collateral_for_value(value, price)
}

/// Outcome of the insurance-adjusted settlement split.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InsuranceAdjusted<Balance> {
	/// Debt redeemers/offsetters can cancel against this vault's collateral.
	pub market_cancel_debt: Balance,
	/// Debt the Insurance Fund covers once the market portion is exhausted.
	pub effective_cover: Balance,
	/// pUSD-value paid out per unit of market-cancelled debt (`<= 1`).
	pub recovery_rate: FixedU128,
}

/// Split a sub-100%-CR recovery vault's debt into the market-cancellable
/// portion and the Insurance-Fund-covered residual, and derive the recovery
/// rate. `collateral_value` is the held collateral priced in pUSD.
///
/// `effective_cover = min(insurance_available, max(debt - collateral_value, 0))`,
/// so the residual is always backed by available insurance and
/// `recovery_rate = collateral_value / market_cancel_debt <= 1`.
pub fn insurance_adjusted<Balance: FixedPointOperand + Saturating + Ord>(
	debt: Balance,
	collateral_value: Balance,
	insurance_available: Balance,
) -> InsuranceAdjusted<Balance> {
	let shortfall = debt.saturating_sub(collateral_value);
	let effective_cover = core::cmp::min(insurance_available, shortfall);
	let market_cancel_debt = debt.saturating_sub(effective_cover);
	let recovery_rate = if market_cancel_debt.is_zero() {
		FixedU128::zero()
	} else {
		FixedU128::checked_from_rational(collateral_value, market_cancel_debt)
			.defensive_unwrap_or_else(FixedU128::zero)
	};
	InsuranceAdjusted { market_cancel_debt, effective_cover, recovery_rate }
}

/// Collateral paid out in the `CR < 100%` regime for `pusd_in` pUSD:
/// `floor(floor(pusd_in * recovery_rate) / price)`.
pub fn recovery_rate_collateral_out<Balance: FixedPointOperand>(
	pusd_in: Balance,
	recovery_rate: FixedU128,
	price: FixedU128,
) -> Balance {
	let value = recovery_rate.saturating_mul_int(pusd_in);
	collateral_for_value(value, price)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn collateral_for_value_floors() {
		// 100 pUSD at price 10 → 10 collateral.
		assert_eq!(collateral_for_value::<u128>(100, FixedU128::from_rational(10, 1)), 10);
		// 105 pUSD at price 10 → floor(10.5) = 10.
		assert_eq!(collateral_for_value::<u128>(105, FixedU128::from_rational(10, 1)), 10);
		// Sub-1.0 price scales up.
		assert_eq!(collateral_for_value::<u128>(100, FixedU128::from_rational(1, 2)), 200);
		assert_eq!(collateral_for_value::<u128>(0, FixedU128::one()), 0);
		assert_eq!(collateral_for_value::<u128>(100, FixedU128::zero()), 0);
	}

	#[test]
	fn recovery_bonus_capped_by_penalty_and_buffer() {
		let penalty = Permill::from_percent(5);
		let buffer = FixedU128::from_rational(1, 100); // 1%
												 // CR = 130% → excess = 30% - 1% = 29%, capped at 5%.
		let cr = FixedU128::from_rational(130, 100);
		assert_eq!(recovery_bonus(cr, buffer, penalty), FixedU128::from_rational(5, 100));
		// CR = 102% → excess = 2% - 1% = 1% < 5% cap.
		let cr = FixedU128::from_rational(102, 100);
		assert_eq!(recovery_bonus(cr, buffer, penalty), FixedU128::from_rational(1, 100));
		// CR = 100% → excess saturates to 0.
		assert_eq!(recovery_bonus(FixedU128::one(), buffer, penalty), FixedU128::zero());
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
		let out = recovery_bonus_collateral_out::<u128>(
			100,
			FixedU128::from_rational(5, 100),
			FixedU128::from_rational(10, 1),
		);
		assert_eq!(out, 10);
		// 200 debt, 5% bonus, price 10 → floor(210 / 10) = 21.
		let out = recovery_bonus_collateral_out::<u128>(
			200,
			FixedU128::from_rational(5, 100),
			FixedU128::from_rational(10, 1),
		);
		assert_eq!(out, 21);
	}

	#[test]
	fn insurance_adjusted_partial_cover() {
		// D = 1000, C = 800 (shortfall 200), IF = 50.
		// effective_cover = 50, market_cancel = 950, rate = 800/950 ≈ 0.8421.
		let r = insurance_adjusted::<u128>(1000, 800, 50);
		assert_eq!(r.effective_cover, 50);
		assert_eq!(r.market_cancel_debt, 950);
		assert!(r.recovery_rate <= FixedU128::one());
		assert!(r.recovery_rate > FixedU128::from_rational(84, 100));
		assert!(r.recovery_rate < FixedU128::from_rational(85, 100));
	}

	#[test]
	fn insurance_adjusted_empty_fund_uses_c_over_d() {
		// IF = 0 → effective_cover = 0, market_cancel = D, rate = C/D.
		let r = insurance_adjusted::<u128>(1000, 800, 0);
		assert_eq!(r.effective_cover, 0);
		assert_eq!(r.market_cancel_debt, 1000);
		assert_eq!(r.recovery_rate, FixedU128::from_rational(800, 1000));
	}

	#[test]
	fn insurance_adjusted_full_cover_zero_market() {
		// IF covers the whole shortfall and then some: market_cancel = C.
		// D = 1000, C = 800, IF = 500 → cover = min(500, 200) = 200,
		// market_cancel = 800, rate = 800/800 = 1.0.
		let r = insurance_adjusted::<u128>(1000, 800, 500);
		assert_eq!(r.effective_cover, 200);
		assert_eq!(r.market_cancel_debt, 800);
		assert_eq!(r.recovery_rate, FixedU128::one());
	}

	#[test]
	fn insurance_adjusted_fund_covers_all_debt() {
		// C = 0 (collateral worthless), IF >= D → market_cancel = 0.
		let r = insurance_adjusted::<u128>(1000, 0, 1000);
		assert_eq!(r.effective_cover, 1000);
		assert_eq!(r.market_cancel_debt, 0);
		assert_eq!(r.recovery_rate, FixedU128::zero());
	}

	#[test]
	fn recovery_rate_collateral_out_floors() {
		// x = 950 at rate 800/950, price 10 → value floor(950 * 0.8421..) = 800,
		// collateral floor(800/10) = 80.
		let r = insurance_adjusted::<u128>(1000, 800, 50);
		let out = recovery_rate_collateral_out::<u128>(
			950,
			r.recovery_rate,
			FixedU128::from_rational(10, 1),
		);
		// Rounding down keeps payout at or just below 80.
		assert!(out <= 80);
		assert!(out >= 79);
	}
}
