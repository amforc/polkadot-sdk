use crate::types::{Millis, RedemptionConfig, RedemptionState};
use frame::deps::{
	frame_support::traits::Defensive,
	sp_runtime::{
		helpers_128bit::multiply_by_rational_with_rounding,
		traits::{CheckedAdd, CheckedDiv, One, Saturating, Zero},
		ArithmeticError, FixedPointNumber, FixedPointOperand, FixedU128, Permill, Rounding,
	},
};

/// One half in the fixed-point domain of the dynamic fee.
const HALF: FixedU128 = FixedU128::from_rational(1, 2);

/// Decays the dynamic fee deterministically and monotonically over `elapsed_ms`.
///
/// More elapsed time cannot produce a higher rate. The fractional half-life uses the secant upper
/// bound of `2^(-f)` on `[0, 1)`.
///
/// Thus, the result is continuous across period boundaries. It is equal to or slightly above the
/// exact decay, which favors the system.
fn decay_dynamic_fee(dynamic_fee: FixedU128, elapsed_ms: u64, period_ms: u64) -> FixedU128 {
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
	let factor = FixedU128::one().saturating_sub(fraction.saturating_mul(HALF));
	halved.saturating_mul(factor)
}

impl RedemptionState {
	/// Gets the dynamic fee at `now` after time decay and application of the policy bounds.
	pub(crate) fn dynamic_fee_at<Balance>(
		&self,
		now: Millis,
		config: &RedemptionConfig<Balance>,
	) -> FixedU128 {
		decay_dynamic_fee(
			self.dynamic_fee,
			now.saturating_sub(self.last_fee_operation),
			config.dynamic_fee_decay_period,
		)
		.max(config.dynamic_fee_floor)
		.min(config.dynamic_fee_ceiling)
	}
}
/// The model behind an ordinary redemption's fee.
///
/// Redemption of debt share `u` raises the dynamic fee by `u / divisor`. The rate climbs at a
/// constant pace from the arrival fee to that terminal fee while the redemption cancels its debt.
/// The redemption pays the integral of the rate over the debt that it cancels. Without a cap, this
/// is the arrival rate plus half of the rise, applied to the whole amount.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DynamicFeeCurve {
	/// Dynamic fee at the start of the redemption, after decay and policy bounds.
	decayed: FixedU128,
	/// Stablecoin-wide debt before the redemption.
	debt: u128,
	/// Divisor applied to the redeemed share before that share increases the dynamic fee.
	divisor: FixedU128,
	/// Caps the stored dynamic fee.
	dynamic_fee_ceiling: FixedU128,
	/// Constant component that each unit pays.
	base_fee: Permill,
	/// Maximum charged rate, including the base fee.
	fee_ceiling: Permill,
}

impl DynamicFeeCurve {
	/// Builds a curve in its `u128` arithmetic domain without debt saturation.
	pub(crate) fn try_new<Balance>(
		decayed: FixedU128,
		debt: Balance,
		config: &RedemptionConfig<Balance>,
	) -> Result<Self, ArithmeticError>
	where
		Balance: FixedPointOperand + TryInto<u128>,
	{
		Ok(Self {
			decayed,
			debt: debt.try_into().map_err(|_| ArithmeticError::Overflow)?,
			divisor: config.dynamic_fee_increase_divisor,
			dynamic_fee_ceiling: config.dynamic_fee_ceiling,
			base_fee: config.base_fee,
			fee_ceiling: config.fee_ceiling,
		})
	}

	/// Calculates the share of stablecoin debt that `redeemed` represents.
	///
	/// The result has a maximum of one because an amount above the total debt cannot represent a
	/// larger share.
	fn redeemed_share(&self, redeemed: u128) -> FixedU128 {
		FixedU128::checked_from_rational(redeemed, self.debt)
			.unwrap_or_else(FixedU128::one)
			.min(FixedU128::one())
	}

	/// Calculates the rise of the dynamic fee that redemption of `redeemed` causes: its debt share
	/// over the divisor.
	///
	/// Returns `None` for a zero divisor, which policy validation rejects.
	fn rise(&self, redeemed: u128) -> Option<FixedU128> {
		self.redeemed_share(redeemed).checked_div(&self.divisor)
	}

	/// Calculates `decayed + rise` after redemption of `redeemed`.
	///
	/// Returns `None` if the rise has no bound or the sum does not fit.
	fn dynamic_fee_after(&self, redeemed: u128) -> Option<FixedU128> {
		self.decayed.checked_add(&self.rise(redeemed)?)
	}

	/// Calculates the stored dynamic fee after redemption of `redeemed`.
	pub fn raised_dynamic_fee(&self, redeemed: u128) -> FixedU128 {
		self.dynamic_fee_after(redeemed)
			.unwrap_or(self.dynamic_fee_ceiling)
			.min(self.dynamic_fee_ceiling)
	}

	/// Calculates the rate that `redeemed` pays.
	///
	/// The rate is the base fee plus the mean dynamic fee during the redemption, limited by the fee
	/// ceiling.
	pub fn charged_rate(&self, redeemed: u128) -> FixedU128 {
		// The dynamic component uses the lower of its ceiling and the fee-ceiling space above the
		// base fee.
		let ceiling = self
			.dynamic_fee_ceiling
			.min(self.fee_ceiling.saturating_sub(self.base_fee).into());
		let average = match self.dynamic_fee_after(redeemed) {
			Some(terminal) if terminal <= ceiling => midpoint(self.decayed, terminal),
			_ => self.capped_average_dynamic_fee(redeemed, ceiling),
		};
		fee_rate(average, self.base_fee, self.fee_ceiling)
	}

	/// Calculates the fee that `redeemed` pays and rounds it up.
	pub fn fee<Balance: FixedPointOperand>(&self, redeemed: Balance) -> Balance {
		redemption_fee(redeemed, self.charged_rate(redeemed.unique_saturated_into()))
	}

	/// Calculates the mean dynamic fee when redemption of `redeemed` reaches `ceiling`.
	///
	/// The dynamic fee reaches the ceiling after the fraction `(ceiling − decayed) / rise` of the
	/// redemption. That fraction pays the mean of its climb, and the rest pays the ceiling.
	fn capped_average_dynamic_fee(&self, redeemed: u128, ceiling: FixedU128) -> FixedU128 {
		let headroom = ceiling.saturating_sub(self.decayed);
		// A zero divisor reaches the ceiling at once. An arrival at or above the ceiling has no
		// climb.
		let climb = self
			.rise(redeemed)
			.and_then(|rise| headroom.checked_div(&rise))
			.unwrap_or_else(FixedU128::zero)
			.min(FixedU128::one());
		let climbing = midpoint(self.decayed, ceiling);
		let flat = FixedU128::one().saturating_sub(climb).saturating_mul(ceiling);
		climb.saturating_mul(climbing).saturating_add(flat)
	}
}

/// Calculates the mean of two dynamic fees. It rounds down at the `1e-18` resolution.
fn midpoint(a: FixedU128, b: FixedU128) -> FixedU128 {
	let low = a.min(b);
	low.saturating_add(a.max(b).saturating_sub(low).saturating_mul(HALF))
}

/// Calculates `min(dynamic_fee + base_fee, fee_ceiling)`.
///
/// The `Permill` bounds convert without loss to the fixed-point domain of the dynamic fee.
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

/// Calculates the maximum debt that `budget` can buy, including the fee.
///
/// This debt and the monotonic `fee_for` that it causes have a sum no greater than `budget`.
pub fn max_debt_for_budget<Balance: FixedPointOperand>(
	budget: Balance,
	fee_for: impl Fn(Balance) -> Balance,
) -> Balance {
	if budget.is_zero() {
		return Balance::zero();
	}
	let budget: u128 = budget.unique_saturated_into();
	let fits = |debt: u128| -> bool {
		let fee: u128 = Balance::try_from(debt)
			.ok()
			.map(&fee_for)
			.defensive_unwrap_or_else(Balance::zero)
			.unique_saturated_into();
		debt.checked_add(fee).is_some_and(|total| total <= budget)
	};
	// A budget without a fee buys itself. Only a nonzero fee requires the search.
	let mut high = budget;
	if fits(high) {
		return Balance::try_from(high).ok().defensive_unwrap_or_else(Balance::zero);
	}
	let mut low = 0u128;
	while low < high {
		let mid = low.midpoint(high).saturating_add(1);
		if fits(mid) {
			low = mid;
		} else {
			high = mid.saturating_sub(1);
		}
	}
	Balance::try_from(low).ok().defensive_unwrap_or_else(Balance::zero)
}

/// Scales the user's slippage floor to the amount spent during a partial fill.
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

/// Calculates `a * num / denom` at `Balance` precision with the specified rounding.
///
/// The function uses the defensive fallback if the product cannot have a `Balance`
/// representation.
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

	/// Builds the default test policy.
	///
	/// The policy uses each unit's full share, a 0.5% base fee, and 100% ceilings that do not bind.
	fn curve(decayed: FixedU128, debt: u128) -> DynamicFeeCurve {
		let config = RedemptionConfig {
			minimum_redemption_amount: 1,
			dynamic_fee_decay_period: 6 * HOUR,
			dynamic_fee_floor: FixedU128::zero(),
			dynamic_fee_ceiling: FixedU128::one(),
			base_fee: Permill::from_rational(5u32, 1_000u32),
			fee_ceiling: Permill::one(),
			dynamic_fee_increase_divisor: FixedU128::one(),
			final_recovery_bonus_buffer: Permill::zero(),
		};
		DynamicFeeCurve::try_new(decayed, debt, &config).expect("u128 debt cannot overflow u128")
	}

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

	/// Verifies decay behavior at the time-span boundaries.
	///
	/// The policy rejects a zero half-life, but the decay function treats it as instant decay. A
	/// `u64::MAX` span with a one-millisecond half-life exceeds 128 halvings.
	///
	/// A span and a half-life that both equal `u64::MAX` give exactly one halving.
	#[test]
	fn decay_edge_spans() {
		let base = FixedU128::from_rational(1, 2);
		assert_eq!(decay_dynamic_fee(base, 1, 0), FixedU128::zero());
		assert_eq!(decay_dynamic_fee(base, u64::MAX, 1), FixedU128::zero());
		assert_eq!(decay_dynamic_fee(base, u64::MAX, u64::MAX), FixedU128::from_rational(1, 4));
		assert_eq!(
			decay_dynamic_fee(base, 127 * HOUR, HOUR),
			FixedU128::from_inner(base.into_inner() >> 127)
		);
		assert_eq!(decay_dynamic_fee(base, 128 * HOUR, HOUR), FixedU128::zero());
	}

	#[test]
	fn redemption_state_exposes_the_bounded_fee_effective_at_now() {
		let config = RedemptionConfig {
			minimum_redemption_amount: 1u128,
			dynamic_fee_decay_period: 6 * HOUR,
			dynamic_fee_floor: FixedU128::from_rational(1, 10),
			dynamic_fee_ceiling: FixedU128::from_rational(2, 5),
			base_fee: Permill::zero(),
			fee_ceiling: Permill::one(),
			dynamic_fee_increase_divisor: FixedU128::one(),
			final_recovery_bonus_buffer: Permill::zero(),
		};
		let state = RedemptionState {
			dynamic_fee: FixedU128::from_rational(1, 2),
			last_fee_operation: HOUR,
		};

		assert_eq!(state.dynamic_fee_at(7 * HOUR, &config), FixedU128::from_rational(1, 4));
		assert_eq!(state.dynamic_fee_at(19 * HOUR, &config), config.dynamic_fee_floor);
		// A clock before the last operation causes no decay. The bounds still apply.
		assert_eq!(state.dynamic_fee_at(0, &config), config.dynamic_fee_ceiling);
		assert_eq!(state.dynamic_fee_at(HOUR - 1, &config), config.dynamic_fee_ceiling);
	}

	/// Verifies the rise for a debt of `1e18` units, where each unit is one `1e-18` of share.
	#[test]
	fn rise_is_the_redeemed_share_over_the_divisor() {
		let mut curve = curve(FixedU128::zero(), FixedU128::DIV);
		curve.divisor = FixedU128::from_rational(2, 1);
		assert_eq!(curve.rise(0), Some(FixedU128::zero()));
		assert_eq!(curve.rise(2), Some(FixedU128::from_inner(1)));
		let third = FixedU128::DIV / 3;
		assert_eq!(curve.rise(third), Some(FixedU128::from_inner(third / 2)));
		// The share of all debt or more is one.
		assert_eq!(curve.rise(FixedU128::DIV), Some(FixedU128::from_rational(1, 2)));
		assert_eq!(curve.rise(FixedU128::DIV + 1), Some(FixedU128::from_rational(1, 2)));
		assert_eq!(curve.rise(u128::MAX), Some(FixedU128::from_rational(1, 2)));
	}

	#[test]
	fn raised_dynamic_fee_caps_at_the_ceiling() {
		let curve = curve(FixedU128::from_rational(90, 100), 1_000);
		assert_eq!(curve.dynamic_fee_after(900), Some(FixedU128::from_rational(180, 100)));
		assert_eq!(curve.raised_dynamic_fee(900), FixedU128::one());
	}

	#[test]
	fn raised_dynamic_fee_adds_the_share_over_the_divisor() {
		let curve = curve(FixedU128::from_rational(10, 100), 1_000);
		assert_eq!(curve.raised_dynamic_fee(400), FixedU128::from_rational(50, 100));
	}

	#[test]
	fn charged_rate_is_base_plus_the_mean_dynamic_fee() {
		// The dynamic fee climbs from 10% to 50%. Its mean is 30%.
		let curve = curve(FixedU128::from_rational(10, 100), 1_000);
		assert_eq!(curve.charged_rate(400), FixedU128::from_rational(305, 1_000));
	}

	#[test]
	fn charged_rate_climbs_to_the_ceiling_then_pays_it_flat() {
		let mut curve = curve(FixedU128::zero(), 1_000_000);
		curve.dynamic_fee_ceiling = FixedU128::from_rational(1, 100);
		// The dynamic fee reaches 1% after the share `0.01 · 1`.
		let climb = 10_000u128;
		// Up to the crossing, the plain mean applies: `0.5% + 0.9999% / 2`.
		assert_eq!(
			curve.charged_rate(climb - 1),
			FixedU128::from_rational(9_999_500_000_000_000, FixedU128::DIV)
		);
		assert_eq!(curve.charged_rate(climb), FixedU128::from_rational(1, 100));
		// After the crossing, the mean has the climb weighted at its share and the ceiling for the
		// other share. A quarter of a 4% share climbs at a 0.5% mean, and the rest pays 1%.
		assert_eq!(curve.charged_rate(4 * climb), FixedU128::from_rational(1_375, 100_000));
		// At the crossing, the dynamic-fee area is the triangle `climb · ceiling / 2`. After it,
		// the rectangle `(redeemed − climb) · ceiling` is added. Dividing their sum by `redeemed`
		// gives the mean dynamic fee. The two shapes meet without a jump in the rate or its slope.
		// Thus, no second difference exceeds the curvature at the crossing plus the `1e-18`
		// rounding of each rate.
		let rates: [i128; 5] = core::array::from_fn(|i| {
			i128::try_from(curve.charged_rate(climb - 2 + i as u128).into_inner())
				.expect("a rate is at most one")
		});
		let curvature: i128 = 100_000_000;
		for window in rates.windows(3) {
			let jump = (window[2] - window[1]) - (window[1] - window[0]);
			assert!(jump.abs() <= curvature + 4, "slope jumps by {jump} around the crossing");
		}
		// Arriving above the ceiling pays it flat from the first unit.
		curve.decayed = FixedU128::from_rational(1, 100);
		assert_eq!(curve.charged_rate(500_000), FixedU128::from_rational(15, 1_000));
	}

	#[test]
	fn charged_rate_respects_the_fee_ceiling_above_the_dynamic_one() {
		let mut curve = curve(FixedU128::from_rational(30, 100), 1_000);
		curve.fee_ceiling = Permill::from_percent(20);
		// The dynamic component can increase to 100%. The 20% fee ceiling includes the base fee and
		// applies from the first unit.
		assert_eq!(curve.charged_rate(500), FixedU128::from_rational(20, 100));
		assert_eq!(curve.raised_dynamic_fee(500), FixedU128::from_rational(80, 100));
	}

	// These tests verify the curve-domain boundaries. Execution cannot reach most of them because a
	// redemption target has debt and the walk cannot cancel more than the stablecoin debt.
	// Configuration validation supplies the other bounds. The arithmetic is total and uses
	// saturation instead of a panic or overflow.

	/// Verifies the rate for redemption of all stablecoin debt or more.
	///
	/// All debt has a share of one, so the dynamic fee rises by `1 / divisor`. Above the total
	/// debt, the share stays at one and the rate equals the full-redemption rate.
	#[test]
	fn full_and_over_full_redemptions_rise_by_the_divisor_reciprocal() {
		let mut curve = curve(FixedU128::zero(), 1_000);
		assert_eq!(curve.raised_dynamic_fee(1_000), FixedU128::one());
		assert_eq!(curve.raised_dynamic_fee(2_000), FixedU128::one());
		// The fee ceiling stops the dynamic climb at 99.5%, so the whole coin pays 50.49875%.
		assert_eq!(curve.charged_rate(1_000), FixedU128::from_rational(40_399, 80_000));
		assert_eq!(curve.fee(1_000u128), 505);
		assert_eq!(curve.charged_rate(2_000), curve.charged_rate(1_000));
		assert_eq!(curve.fee(2_000u128), 1_010);
		// A large divisor keeps even the whole coin far below the ceiling.
		curve.divisor = FixedU128::from_rational(1_000_000, 1);
		assert_eq!(curve.raised_dynamic_fee(1_000), FixedU128::from_rational(1, 1_000_000));
		assert_eq!(curve.raised_dynamic_fee(2_000), FixedU128::from_rational(1, 1_000_000));
	}

	/// Verifies the curve for a stablecoin without debt.
	///
	/// Each nonzero redemption has a share of one. It uses the full-redemption rate and rise.
	#[test]
	fn a_coin_without_debt_prices_every_redemption_as_the_whole_coin() {
		let empty = curve(FixedU128::zero(), 0);
		let whole = curve(FixedU128::zero(), 1_000);
		assert_eq!(empty.fee(0u128), 0);
		assert_eq!(empty.raised_dynamic_fee(0), whole.raised_dynamic_fee(1_000));
		assert_eq!(empty.charged_rate(1), whole.charged_rate(1_000));
		assert_eq!(empty.fee(1_000u128), whole.fee(1_000u128));
		assert_eq!(empty.raised_dynamic_fee(1), FixedU128::one());
	}

	/// Verifies the curve for a zero divisor that policy validation rejects.
	///
	/// The curve treats the zero divisor as an infinite accelerator and applies the ceiling from
	/// the first unit.
	#[test]
	fn a_zero_divisor_pays_the_ceiling_from_the_first_unit() {
		let mut curve = curve(FixedU128::zero(), 1_000);
		curve.divisor = FixedU128::zero();
		assert_eq!(curve.raised_dynamic_fee(1), FixedU128::one());
		assert_eq!(curve.charged_rate(1), FixedU128::one());
		assert_eq!(curve.fee(100u128), 100);
		curve.dynamic_fee_ceiling = FixedU128::from_rational(10, 100);
		assert_eq!(curve.raised_dynamic_fee(1), FixedU128::from_rational(10, 100));
		assert_eq!(curve.charged_rate(1), FixedU128::from_rational(105, 1_000));
	}

	/// Verifies a redemption that starts at or above the dynamic-fee ceiling.
	///
	/// This state can occur after a policy decreases its ceiling below a stored fee. The redemption
	/// pays the ceiling and does not increase the stored fee.
	#[test]
	fn arriving_at_or_above_the_ceiling_pays_it_flat() {
		let ceiling = FixedU128::from_rational(10, 100);
		let flat = FixedU128::from_rational(105, 1_000);
		let mut curve = curve(ceiling, 1_000);
		curve.dynamic_fee_ceiling = ceiling;
		assert_eq!(curve.charged_rate(300), flat);
		assert_eq!(curve.fee(300u128), 32);
		assert_eq!(curve.raised_dynamic_fee(300), ceiling);
		curve.decayed = FixedU128::from_rational(20, 100);
		assert_eq!(curve.charged_rate(300), flat);
		assert_eq!(curve.charged_rate(0), flat);
		assert_eq!(curve.raised_dynamic_fee(300), ceiling);
		assert_eq!(curve.raised_dynamic_fee(0), ceiling);
	}

	/// Verifies a policy in which the base fee equals the fee ceiling.
	///
	/// The policy permits equality but rejects an inverted range. The charged rate stays at the fee
	/// ceiling while the stored dynamic fee increases.
	#[test]
	fn no_headroom_above_the_base_fee_charges_the_fee_ceiling_flat() {
		let mut curve = curve(FixedU128::zero(), 1_000);
		curve.fee_ceiling = Permill::from_rational(5u32, 1_000u32);
		assert_eq!(curve.charged_rate(400), FixedU128::from_rational(5, 1_000));
		assert_eq!(curve.fee(400u128), 2);
		let climbed = curve.raised_dynamic_fee(400);
		assert_eq!(climbed, FixedU128::from_rational(40, 100));
		curve.base_fee = Permill::from_percent(2);
		curve.fee_ceiling = Permill::from_percent(1);
		assert_eq!(curve.charged_rate(400), FixedU128::from_rational(1, 100));
		assert_eq!(curve.fee(400u128), 4);
		assert_eq!(curve.raised_dynamic_fee(400), climbed);
	}

	/// Verifies that a zero redemption has zero cost and does not increase the dynamic fee.
	///
	/// The charged rate equals the arrival rate, subject to the ceiling.
	#[test]
	fn a_redemption_of_nothing_pays_nothing_and_moves_nothing() {
		let curve = curve(FixedU128::from_rational(3, 100), 1_000);
		assert_eq!(curve.fee(0u128), 0);
		assert_eq!(curve.charged_rate(0), FixedU128::from_rational(35, 1_000));
		assert_eq!(curve.raised_dynamic_fee(0), FixedU128::from_rational(3, 100));
	}

	/// Verifies that the arithmetic supports the largest balances.
	///
	/// One unit of a `u128::MAX` debt has a negligible share and pays the base fee. All but one
	/// unit has the same rate as a small full redemption.
	#[test]
	fn the_largest_balances_stay_inside_the_arithmetic() {
		let vast = curve(FixedU128::zero(), u128::MAX);
		assert_eq!(vast.fee(1u128), 1);
		assert_eq!(vast.raised_dynamic_fee(1), FixedU128::zero());
		let nearly_all = u128::MAX - 1;
		let rate = vast.charged_rate(nearly_all);
		let small = curve(FixedU128::zero(), 1_000).charged_rate(1_000);
		assert!(rate.into_inner().abs_diff(small.into_inner()) <= 4, "{rate:?} vs {small:?}");
		let fee = vast.fee(nearly_all);
		assert!(fee <= nearly_all);
		assert!(fee > nearly_all / 2);
		let full = FixedU128::one();
		let raised = vast.raised_dynamic_fee(nearly_all);
		assert!(raised.into_inner().abs_diff(full.into_inner()) <= 1, "{raised:?}");
		assert_eq!(vast.raised_dynamic_fee(u128::MAX), full);
	}

	/// Applies `tranches` in sequence against the debt that each prior tranche leaves.
	///
	/// Each tranche starts at the dynamic fee from the prior tranche. The function returns the
	/// total fees and the final dynamic fee.
	fn redeem_in_tranches(mut curve: DynamicFeeCurve, tranches: &[u128]) -> (u128, FixedU128) {
		let mut fee = 0u128;
		for &tranche in tranches {
			fee += curve.fee(tranche);
			curve.decayed = curve.raised_dynamic_fee(tranche);
			curve.debt -= tranche;
		}
		(fee, curve.decayed)
	}

	/// Verifies the worked example: 20_000 of a 500_480 debt from a zero dynamic fee, at once and
	/// in halves.
	///
	/// At once, the share is 3.996%, so the dynamic fee climbs by 3.996%. The redemption pays the
	/// 1.998% mean above the 0.5% base fee: `ceil(20_000 · 2.498%) = 500`.
	///
	/// In halves, the first climbs to 1.998% and pays `ceil(149.90) = 150`. The second takes its
	/// share of the remaining 490_480, climbs by 2.039% to 4.037%, and pays `ceil(351.75) = 352`.
	/// Thus, the halves pay two units more and leave a higher dynamic fee.
	#[test]
	fn the_worked_example_at_once_and_in_halves() {
		let example = curve(FixedU128::zero(), 500_480);
		assert_eq!(example.fee(20_000u128), 500);
		let raised = example.raised_dynamic_fee(20_000);
		assert!(raised.into_inner().abs_diff(39_961_636_828_644_500) <= 1, "{raised:?}");
		assert_eq!(example.fee(10_000u128), 150);
		let (split_fee, split_raised) = redeem_in_tranches(example, &[10_000, 10_000]);
		assert_eq!(split_fee, 502);
		assert!(
			split_raised.into_inner().abs_diff(40_369_009_574_002_562) <= 2,
			"{split_raised:?}"
		);
		assert!(split_raised > raised);
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
		assert_eq!(redemption_fee(100u128, FixedU128::from_rational(5, 1000)), 1);
		assert_eq!(redemption_fee(1000u128, FixedU128::from_rational(5, 1000)), 5);
		assert_eq!(redemption_fee(0u128, FixedU128::one()), 0);
	}

	#[test]
	fn max_debt_for_budget_accounts_for_fee() {
		let at = |rate| move |debt| redemption_fee(debt, rate);
		assert_eq!(max_debt_for_budget(1000u128, at(FixedU128::zero())), 1000);
		assert_eq!(max_debt_for_budget(1000u128, at(FixedU128::one())), 500);
		assert_eq!(max_debt_for_budget(1000u128, at(FixedU128::from_rational(5, 1000))), 995);
	}

	/// Verifies budget-search boundaries.
	///
	/// A zero budget buys zero debt. A budget below one debt unit plus its fee also buys zero debt.
	/// At `u128::MAX`, the debt-fee sum must not wrap to a value within the budget.
	#[test]
	fn max_debt_for_budget_edge_inputs() {
		let whole = |debt: u128| debt;
		assert_eq!(max_debt_for_budget(0, whole), 0);
		assert_eq!(max_debt_for_budget(1, whole), 0);
		assert_eq!(max_debt_for_budget(2, whole), 1);
		assert_eq!(max_debt_for_budget(u128::MAX, whole), u128::MAX / 2);
		assert_eq!(max_debt_for_budget(u128::MAX, |_| 0), u128::MAX);
	}

	#[test]
	fn max_debt_for_budget_lands_on_the_curve() {
		let curve = curve(FixedU128::zero(), 10_000);
		let budget = 1_000u128;
		let debt = max_debt_for_budget(budget, |debt| curve.fee(debt));
		assert!(debt + curve.fee(debt) <= budget);
		assert!(debt + 1 + curve.fee(debt + 1) > budget);
	}

	#[test]
	fn scale_floor_basic() {
		assert_eq!(scale_floor(100u128, 1, 2), 50);
		assert_eq!(scale_floor(100u128, 3, 4), 75);
		assert_eq!(scale_floor(100u128, 1, 0), 0);
	}

	/// Verifies curve properties across the full parameter space.
	///
	/// Each property is exact in the model. The fixed-point checks permit the `1e-18` rate
	/// precision and one unit for upward fee rounding.
	mod properties {
		use super::*;
		use frame::arithmetic::SignedRounding;
		use proptest::prelude::*;

		/// Maximum difference, in inner `FixedU128` units, between two evaluations of the same
		/// rate.
		///
		/// The plain mean rounds a share, a division, and a midpoint at `1e-18`. The capped mean
		/// adds a division and two products, so one evaluation is within four units of the model.
		/// Two evaluations can differ by twice that.
		const RATE_SLACK: u128 = 8;

		/// Maximum difference between two fees for `redeemed` from rates within [`RATE_SLACK`] of
		/// each other, plus one unit of upward rounding.
		fn fee_slack(redeemed: u128) -> u128 {
			redeemed * RATE_SLACK / FixedU128::DIV + 1
		}

		/// Selects a log-uniform magnitude across fifteen decades.
		///
		/// This distribution tests shares and balances throughout the range instead of primarily
		/// near its upper limit.
		fn log_uniform_amount() -> impl Strategy<Value = u128> {
			(0u32..=15, 1u128..=999)
				.prop_map(|(exponent, mantissa)| mantissa * 10u128.pow(exponent))
		}

		/// Splits `total` in proportion to `weights` and gives the remainder to the last part.
		fn split_by_weights(total: u128, weights: &[u32]) -> Vec<u128> {
			let sum: u128 = weights.iter().map(|&weight| u128::from(weight)).sum();
			let mut tranches: Vec<u128> =
				weights.iter().map(|&weight| total * u128::from(weight) / sum).collect();
			let assigned: u128 = tranches.iter().sum();
			*tranches.last_mut().expect("at least one weight") += total - assigned;
			tranches
		}

		prop_compose! {
			/// Generates a curve with an arbitrary policy and arrival state.
			///
			/// It also generates a log-uniform redemption with `redeemed < debt`.
			fn curve_and_redemption()(
				decayed_ppm in 0u128..=200_000,
				divisor_x100 in 50u128..=2_000,
				dynamic_ceiling_ppm in 1_000u128..=1_000_000,
				base_fee_ppm in 0u32..=20_000,
				fee_ceiling_ppm in 0u32..=1_000_000,
				redeemed in log_uniform_amount(),
				remaining in log_uniform_amount(),
			) -> (DynamicFeeCurve, u128) {
				let curve = DynamicFeeCurve {
					decayed: FixedU128::from_rational(decayed_ppm, 1_000_000),
					debt: redeemed + remaining,
					divisor: FixedU128::from_rational(divisor_x100, 100),
					dynamic_fee_ceiling: FixedU128::from_rational(dynamic_ceiling_ppm, 1_000_000),
					base_fee: Permill::from_parts(base_fee_ppm),
					fee_ceiling: Permill::from_parts(fee_ceiling_ppm),
				};
				(curve, redeemed)
			}
		}

		prop_compose! {
			/// Splits the redemption from [`curve_and_redemption`] into at most eight tranches.
			fn curve_and_tranches()(
				(curve, redeemed) in curve_and_redemption(),
				weights in prop::collection::vec(1u32..=1_000, 1..=8),
			) -> (DynamicFeeCurve, Vec<u128>) {
				(curve, split_by_weights(redeemed, &weights))
			}
		}

		/// Calculates the cap for the dynamic component of the charged rate.
		fn dynamic_cap(curve: &DynamicFeeCurve) -> FixedU128 {
			let headroom = FixedU128::from(curve.fee_ceiling.saturating_sub(curve.base_fee));
			curve.dynamic_fee_ceiling.min(headroom)
		}

		/// Calculates the debt at which a curve's dynamic fee reaches its cap.
		///
		/// The debt is `(cap − decayed) · divisor · debt`.
		fn cap_crossing(curve: &DynamicFeeCurve) -> u128 {
			let headroom = dynamic_cap(curve).saturating_sub(curve.decayed);
			headroom.saturating_mul(curve.divisor).saturating_mul_int(curve.debt)
		}

		/// Generates a curve that starts below its cap and then reaches the cap within its debt.
		///
		/// The crossing is strictly inside the debt. Thus, the test can evaluate both sides of the
		/// crossing.
		fn curve_crossing_its_cap() -> impl Strategy<Value = (DynamicFeeCurve, u128)> {
			(curve_and_redemption(), 0u128..1_000_000).prop_filter_map(
				"the dynamic fee reaches its cap inside the debt",
				|((mut curve, _), arrival_ppm)| {
					let cap = dynamic_cap(&curve);
					curve.decayed =
						cap.saturating_mul(FixedU128::from_rational(arrival_ppm, 1_000_000));
					let crossing = cap_crossing(&curve);
					let inside = crossing >= 1 && crossing + 2 < curve.debt;
					(curve.decayed < cap && inside).then_some((curve, crossing))
				},
			)
		}

		fn rate_slack() -> FixedU128 {
			FixedU128::from_inner(RATE_SLACK)
		}

		/// Calculates the largest rise that tranches can add over one redemption of `total`.
		///
		/// Each tranche takes its share of at most `debt` and at least `debt − total`. Thus, the
		/// excess is at most `total / (debt − total) − total / debt`, over the divisor. Each step
		/// rounds toward a larger bound.
		fn rise_excess_bound(curve: &DynamicFeeCurve, total: u128) -> FixedU128 {
			let after = curve.debt - total;
			// The generated redemption leaves debt, and the ratio is at most `1e18`. Thus, neither
			// conversion panics.
			let largest = FixedU128::from_rational_with_rounding(total, after, Rounding::Up);
			let smallest =
				FixedU128::from_rational_with_rounding(total, curve.debt, Rounding::Down);
			largest
				.saturating_sub(smallest)
				.checked_rounding_div(curve.divisor, SignedRounding::High)
				.expect("the generated divisor is nonzero")
		}

		/// Calculates the largest fee that tranches can add over one redemption of `total`, in
		/// whole units rounded up.
		///
		/// The tranche rate exceeds the model rate by at most [`rise_excess_bound`] at each unit,
		/// and by half of that on average.
		fn fee_excess_bound(curve: &DynamicFeeCurve, total: u128) -> u128 {
			let rise = rise_excess_bound(curve, total).into_inner();
			multiply_by_rational_with_rounding(total, rise, 2 * FixedU128::DIV, Rounding::Up)
				.expect("the excess is below the amount")
		}

		proptest! {
			#![proptest_config(ProptestConfig::with_cases(2_048))]

			/// Verifies that no tranche schedule undercuts one equivalent redemption.
			///
			/// Tranches can exceed the one-redemption fee by the upward rounding of each tranche and
			/// by their faster climb against the debt that earlier tranches leave.
			#[test]
			fn tranches_never_undercut_the_atomic_fee(
				(curve, tranches) in curve_and_tranches()
			) {
				let total: u128 = tranches.iter().sum();
				let at_once = curve.fee(total);
				let (split, _) = redeem_in_tranches(curve, &tranches);
				let slack = fee_slack(total);
				let count = tranches.len();
				prop_assert!(
					split + slack >= at_once,
					"{split} in {count} tranches undercuts {at_once} at once"
				);
				let excess = fee_excess_bound(&curve, total);
				prop_assert!(
					split <= at_once + count as u128 + excess + slack,
					"{split} in {count} tranches overshoots {at_once} at once by more than {excess}"
				);
			}

			/// Verifies that tranches leave a dynamic fee at least as high as one equivalent
			/// redemption.
			///
			/// The excess is bounded by the faster climb against the smaller debt that earlier
			/// tranches leave.
			#[test]
			fn tranches_raise_the_dynamic_fee_at_least_as_much_as_the_whole(
				(curve, tranches) in curve_and_tranches()
			) {
				let total: u128 = tranches.iter().sum();
				let at_once = curve.raised_dynamic_fee(total);
				let (_, split) = redeem_in_tranches(curve, &tranches);
				let count = tranches.len();
				let slack = RATE_SLACK * (count as u128 + 1);
				prop_assert!(
					split.into_inner() + slack >= at_once.into_inner(),
					"{split:?} in {count} tranches below {at_once:?} at once"
				);
				let excess = rise_excess_bound(&curve, total).into_inner();
				prop_assert!(
					split.into_inner() <= at_once.into_inner() + excess + slack,
					"{split:?} in {count} tranches above {at_once:?} at once by more than {excess}"
				);
			}

			/// Verifies the monotonic fee, rate, and final dynamic fee.
			///
			/// A larger redeemed amount cannot decrease these values. A higher arrival dynamic fee also
			/// cannot decrease the cost.
			#[test]
			fn fee_rate_and_dynamic_fee_are_monotonic(
				(curve, tranches) in curve_and_tranches(),
				less_ppm in 0u128..=1_000_000,
				higher_arrival_ppm in 0u128..=100_000,
			) {
				let redeemed: u128 = tranches.iter().sum();
				let less = redeemed * less_ppm / 1_000_000;
				let slack = fee_slack(redeemed);
				prop_assert!(curve.fee(less) <= curve.fee(redeemed) + slack);
				prop_assert!(
					curve.charged_rate(less) <=
						curve.charged_rate(redeemed).saturating_add(rate_slack())
				);
				prop_assert!(
					curve.raised_dynamic_fee(less) <=
						curve.raised_dynamic_fee(redeemed).saturating_add(rate_slack())
				);

				let mut higher = curve;
				higher.decayed = curve
					.decayed
					.saturating_add(FixedU128::from_rational(higher_arrival_ppm, 1_000_000));
				prop_assert!(higher.fee(redeemed) + slack >= curve.fee(redeemed));
				prop_assert!(
					higher.charged_rate(redeemed).saturating_add(rate_slack()) >=
						curve.charged_rate(redeemed)
				);
			}

			/// Verifies continuity where the charged rate changes from the plain mean to the capped mean.
			///
			/// The change occurs where the dynamic fee reaches its cap. The rate does not decrease across
			/// this point, and its slope of at most `1 / (2 · debt · divisor)` per debt unit does not
			/// change there.
			#[test]
			fn charged_rate_is_continuous_where_the_dynamic_fee_reaches_its_cap(
				(curve, crossing) in curve_crossing_its_cap()
			) {
				let before = curve.charged_rate(crossing - 1);
				let after = curve.charged_rate(crossing + 1);
				prop_assert!(
					after.saturating_add(rate_slack()) >= before,
					"{after:?} < {before:?} across the cap"
				);
				let steepest = FixedU128::checked_from_rational(1u128, curve.debt)
					.and_then(|slope| slope.checked_div(&curve.divisor))
					.expect("nonzero debt and divisor");
				prop_assert!(
					after.saturating_sub(before) <= steepest.saturating_add(rate_slack()),
					"{before:?} → {after:?} jumps across the cap"
				);
			}
		}
	}
}
