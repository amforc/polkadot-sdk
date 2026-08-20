//! Pure arithmetic and precision limits for stability-pool accounting.
//!
//! Pure functions keep accounting results independent of storage access order. The limits in this
//! module prevent accumulator overflow and unsupported precision loss.
//!
//! User payouts round down. The pool keeps each remainder so recorded obligations do not exceed
//! custody.

use crate::types::{Accumulators, DepositSnapshot, PUpdate, PoolPrecision, Realized, SumsWindow};
use frame::{
	arithmetic::{
		helpers_128bit::multiply_by_rational_with_rounding, FixedPointOperand, FixedU128, One,
		Rounding, Saturating, Zero,
	},
	traits::Defensive,
};
use pusd_primitives::Millis;

/// Maximum number of `P` rescale operations in one offset.
///
/// More crossings indicate that `minimum_active_pool_balance` is too small relative to the pool.
pub const MAX_SCALE_CROSSINGS: u32 = 2;

/// Smallest permitted `scale_factor`. A smaller factor does not restore sufficient precision.
pub const SCALE_FACTOR_INT_MIN: u64 = 1_000;

/// Largest permitted `scale_factor`.
///
/// It keeps the worst case of [`update_p_after_offset`] inside `u128`:
/// `P.inner * scale_factor^MAX_SCALE_CROSSINGS <= 1e18 * 1e20 = 1e38`.
pub const SCALE_FACTOR_INT_MAX: u64 = 10_000_000_000;

/// Maximum scale distance at which a deposit retains compounded principal.
///
/// Each scale adds a `scale_factor` divisor. Beyond this span, the deposit is worth less than one
/// part in 1e6 of its initial value.
///
/// The finite span also bounds the accumulator rows required for one settlement.
pub const SCALE_SPAN: u32 = 2;

/// Applies the principal formula used by [`realize`] and cohort aggregates:
/// `amount * p_to / (p_from * scale_factor^behind)`, rounded per `rounding`.
///
/// An earlier epoch or a distance above [`SCALE_SPAN`] leaves no principal. Within the supported
/// span, the result cannot exceed `amount`, even with upward rounding.
fn compound<Balance: FixedPointOperand>(
	amount: Balance,
	from: &Accumulators,
	to: &Accumulators,
	sf_int: u128,
	rounding: Rounding,
) -> Balance {
	debug_assert!(!from.p.is_zero());
	debug_assert!(to.p <= FixedU128::one());
	debug_assert!(from.epoch <= to.epoch);
	if amount.is_zero() {
		return Balance::zero();
	}
	if from.epoch != to.epoch {
		return Balance::zero();
	}
	// Each boundary behind adds a `scale_factor` divisor. The `is_valid` bounds keep the worst
	// case, `1e18 * (1e10)^SCALE_SPAN = 1e38`, inside `u128`.
	let denominator = match to.scale.checked_sub(from.scale) {
		Some(behind) if behind <= SCALE_SPAN => sf_int
			.checked_pow(behind)
			.and_then(|factor| from.p.into_inner().checked_mul(factor)),
		Some(_) | None => None,
	};
	let compounded = match denominator {
		Some(denominator) => mul_ratio(amount, to.p.into_inner(), denominator, rounding),
		None => Balance::zero(),
	};
	debug_assert!(compounded <= amount);
	compounded
}

/// Applies [`compound`] with upward rounding for a cohort aggregate.
///
/// Member settlement rounds down. Upward aggregate rounding therefore keeps activated capital at
/// or above the sum of all member claims.
pub fn compound_ceil<Balance: FixedPointOperand>(
	amount: Balance,
	from: &Accumulators,
	to: &Accumulators,
	sf_int: u128,
) -> Balance {
	compound(amount, from, to, sf_int, Rounding::Up)
}

/// Returns `now + entry_delay`, rounded up to the next multiple of `entry_delay`.
///
/// Deadline grouping bounds the number of open cohorts. Upward rounding keeps the wait in
/// `[entry_delay, 2 * entry_delay)` and never below the configured delay.
pub fn cohort_deadline(now: Millis, entry_delay: Millis) -> Millis {
	debug_assert!(entry_delay > 0);
	let width = entry_delay.max(1);
	let earliest = now.saturating_add(entry_delay);
	earliest.checked_next_multiple_of(width).unwrap_or(Millis::MAX)
}

/// Settles deposit `d0` from its `snapshot` to the specified accumulator state.
///
/// Within its current epoch, a deposit compounds across at most [`SCALE_SPAN`] boundaries.
///
/// `compounded = floor(d0 * P / (P0 * scale_factor^k))`.
///
/// An earlier epoch or a distance above [`SCALE_SPAN`] leaves no principal. The gain formula
/// preserves gains earned before the principal reached zero.
///
/// `gain = floor(d0 * ((sum_snap - sum0) + Σ ahead[k] / scale_factor^(k+1)) / P0)`.
pub fn realize<Balance: FixedPointOperand>(
	d0: Balance,
	snapshot: &DepositSnapshot,
	current: &Accumulators,
	window: &SumsWindow,
	precision: &PoolPrecision,
) -> Realized<Balance> {
	debug_assert!(!snapshot.coords.p.is_zero());
	debug_assert!(current.p <= FixedU128::one());
	debug_assert!(snapshot.coords.epoch <= current.epoch);
	debug_assert!(window.snap.s_collateral >= snapshot.sums.s_collateral);
	debug_assert!(window.snap.g_yield >= snapshot.sums.g_yield);
	if d0.is_zero() {
		return Realized {
			compounded: Balance::zero(),
			collateral_gain: Balance::zero(),
			yield_gain: Balance::zero(),
		};
	}
	let sf_int = precision.scale_factor();

	let compounded = compound(d0, &snapshot.coords, current, sf_int, Rounding::Down);
	debug_assert!(compounded <= d0);

	let collateral_gain = gain(
		d0,
		window.snap.s_collateral,
		snapshot.sums.s_collateral,
		window.ahead.map(|row| row.s_collateral),
		sf_int,
		snapshot.coords.p,
	);
	let yield_gain = gain(
		d0,
		window.snap.g_yield,
		snapshot.sums.g_yield,
		window.ahead.map(|row| row.g_yield),
		sf_int,
		snapshot.coords.p,
	);

	Realized { compounded, collateral_gain, yield_gain }
}

/// Applies the gain formula of [`realize`] to collateral sum `S` or yield sum `G`:
/// `floor(d * ((sum_snap - sum0) + Σ ahead[k] / sf_int^(k+1)) / p0)`.
fn gain<Balance: FixedPointOperand>(
	d: Balance,
	sum_snap: FixedU128,
	sum0: FixedU128,
	ahead: [FixedU128; SCALE_SPAN as usize],
	sf_int: u128,
	p0: FixedU128,
) -> Balance {
	// `scale_factor` keeps `1 <= sf_int <= 1e10`, so the largest divisor, `sf_int^SCALE_SPAN =
	// 1e20`, stays inside `u128`.
	debug_assert!(sf_int >= 1);
	let mut delta = sum_snap.saturating_sub(sum0);
	let mut divisor = sf_int;
	for row in ahead {
		// A deposit at the live coordinates carries no look-ahead rows. Skip their divisions.
		if !row.is_zero() {
			delta = delta.saturating_add(FixedU128::from_inner(row.into_inner() / divisor));
		}
		divisor = divisor.saturating_mul(sf_int);
	}
	if delta.is_zero() {
		return Balance::zero();
	}
	mul_ratio(d, delta.into_inner(), p0.into_inner(), Rounding::Down)
}

/// Caps a partial offset at the `min_active_pool` floor.
///
/// The floor protects the precision of deposits that remain. Full depletion remains valid because
/// a new epoch resets `P` to one.
///
/// If the pool is already below the floor, the function permits only full depletion.
pub fn clamp_offset_debt<Balance: FixedPointOperand + Ord>(
	max_debt: Balance,
	total_active: Balance,
	min_active_pool: Balance,
) -> Balance {
	let raw = max_debt.min(total_active);
	if raw == total_active {
		return raw;
	}
	let remainder = total_active.saturating_sub(raw);
	let clamped = if remainder < min_active_pool {
		total_active.saturating_sub(min_active_pool)
	} else {
		raw
	};
	debug_assert!(clamped <= max_debt);
	debug_assert!(
		clamped.is_zero() ||
			clamped == total_active ||
			total_active.saturating_sub(clamped) >= min_active_pool
	);
	clamped
}

/// Returns the increase of `S` or `G` for a distribution over `total_active`:
/// `floor(distributed * P / total_active)`.
///
/// Returns `None` when the pool is empty or the product overflows. The caller reports an
/// arithmetic error.
pub fn delta_sum<Balance: FixedPointOperand>(
	distributed: Balance,
	p: FixedU128,
	total_active: Balance,
) -> Option<FixedU128> {
	debug_assert!(!p.is_zero());
	debug_assert!(p <= FixedU128::one());
	pusd_primitives::mul_div_rate_floor(distributed, p, total_active)
}

/// Returns `P` after `offset_debt` reduces `total_active`.
///
/// Each rescale uses one division to prevent an intermediate loss of precision. Candidate `k` is:
///
/// `floor(P.inner * scale_factor^k * new_total / total)`.
///
/// The function selects the first candidate at or above `p_min`, for `k` from zero through
/// [`MAX_SCALE_CROSSINGS`].
///
/// Returns `None` when the offset needs more crossings than the pallet supports, when an
/// intermediate value overflows, or when `offset_debt` exceeds `total_active`. The caller must
/// reject such an offset.
pub fn update_p_after_offset<Balance: FixedPointOperand + Ord>(
	p: FixedU128,
	total_active: Balance,
	offset_debt: Balance,
	precision: &PoolPrecision,
) -> Option<PUpdate> {
	debug_assert!(!p.is_zero());
	debug_assert!(p <= FixedU128::one());
	if offset_debt > total_active {
		return None;
	}
	if offset_debt == total_active {
		return Some(PUpdate::Depleted);
	}
	if offset_debt.is_zero() {
		return Some(PUpdate::Updated { new_p: p, scales_crossed: 0 });
	}
	let new_total = total_active.saturating_sub(offset_debt);
	let sf_int = precision.scale_factor();

	let mut factor: u128 = 1;
	for scales_crossed in 0..=MAX_SCALE_CROSSINGS {
		let scaled_p = p.into_inner().checked_mul(factor)?;
		let new_p = pusd_primitives::mul_div_rate_floor(
			new_total,
			FixedU128::from_inner(scaled_p),
			total_active,
		)?;
		if new_p >= precision.p_min {
			// A crossing lands `P` in `[p_min, p_min * scale_factor]`, and `is_valid` keeps
			// `p_min * scale_factor` at or below one.
			debug_assert!(new_p <= FixedU128::one());
			return Some(PUpdate::Updated { new_p, scales_crossed });
		}
		factor = factor.checked_mul(sf_int)?;
	}
	None
}

/// Returns `value * numerator / denominator` at `Balance` precision with the specified rounding.
/// Invalid arithmetic returns the conservative fallback used by payout calculations.
fn mul_ratio<Balance: FixedPointOperand>(
	value: Balance,
	numerator: u128,
	denominator: u128,
	rounding: Rounding,
) -> Balance {
	multiply_by_rational_with_rounding(
		value.unique_saturated_into(),
		numerator,
		denominator,
		rounding,
	)
	.and_then(|raw| Balance::try_from(raw).ok())
	.defensive_unwrap_or_else(Balance::zero)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::types::PoolSums;

	const SF: u64 = 1_000_000_000;
	const P_MIN: FixedU128 = FixedU128::from_inner(1_000_000_000);
	const PRECISION: PoolPrecision = PoolPrecision { p_min: P_MIN, scale_factor: SF };

	fn accumulators(p: FixedU128, epoch: u32, scale: u32) -> Accumulators {
		Accumulators { p, epoch, scale }
	}

	fn snapshot(
		p: FixedU128,
		s: FixedU128,
		g: FixedU128,
		epoch: u32,
		scale: u32,
	) -> DepositSnapshot {
		DepositSnapshot {
			coords: Accumulators { p, epoch, scale },
			sums: PoolSums { s_collateral: s, g_yield: g },
		}
	}

	fn window(s_snap: FixedU128, g_snap: FixedU128) -> SumsWindow {
		SumsWindow {
			snap: PoolSums { s_collateral: s_snap, g_yield: g_snap },
			ahead: Default::default(),
		}
	}

	#[test]
	fn realize_same_scale_basic() {
		// D0=1000 at P0=1, S0=0, G0=0. Now P=0.5, S=0.3, G=0.1:
		// compounded = floor(1000 * 0.5) = 500,
		// collateral = floor(1000 * 0.3) = 300,
		// yield      = floor(1000 * 0.1) = 100.
		let snap = snapshot(FixedU128::one(), FixedU128::zero(), FixedU128::zero(), 0, 0);
		let current = accumulators(FixedU128::from_rational(1, 2), 0, 0);
		let sums = window(FixedU128::from_rational(3, 10), FixedU128::from_rational(1, 10));
		let got = realize::<u128>(1_000, &snap, &current, &sums, &PRECISION);
		assert_eq!(got, Realized { compounded: 500, collateral_gain: 300, yield_gain: 100 });
	}

	#[test]
	fn realize_floors_payouts() {
		// D0=3, P=0.333...333 (inner 333_333_333_333_333_333):
		// compounded = floor(3 * 0.333...) = floor(0.999...) = 0.
		let snap = snapshot(FixedU128::one(), FixedU128::zero(), FixedU128::zero(), 0, 0);
		let current = accumulators(FixedU128::from_inner(333_333_333_333_333_333), 0, 0);
		let sums = window(FixedU128::from_inner(333_333_333_333_333_333), FixedU128::zero());
		let got = realize::<u128>(3, &snap, &current, &sums, &PRECISION);
		assert_eq!(got.compounded, 0);
		assert_eq!(got.collateral_gain, 0);
		assert_eq!(got.yield_gain, 0);
	}

	#[test]
	fn realize_one_scale_behind_divides_compounded_by_scale_factor() {
		// D0=1e12 at P0=0.5 on scale n; current scale n+1 with P=0.8:
		// compounded = floor(1e12 * 0.8 / (0.5 * 1e9)) = floor(8e11 / 5e8) = 1600.
		let snap =
			snapshot(FixedU128::from_rational(1, 2), FixedU128::zero(), FixedU128::zero(), 0, 3);
		let current = accumulators(FixedU128::from_rational(4, 5), 0, 4);
		let got = realize::<u128>(
			1_000_000_000_000,
			&snap,
			&current,
			&window(FixedU128::zero(), FixedU128::zero()),
			&PRECISION,
		);
		assert_eq!(got.compounded, 1_600);
	}

	#[test]
	fn realize_one_scale_behind_combines_both_sum_rows() {
		// D0=1e12 at P0=0.5, S0=0.2. Snapshot-scale row finished at 0.5 and
		// the next-scale row holds 300:
		// delta = (0.5 - 0.2) + 300/1e9 = 0.3000003,
		// gain = floor(1e12 * 0.3000003 / 0.5) = 600_000_600_000.
		let snap = snapshot(
			FixedU128::from_rational(1, 2),
			FixedU128::from_rational(1, 5),
			FixedU128::zero(),
			0,
			0,
		);
		let current = accumulators(FixedU128::from_rational(4, 5), 0, 1);
		let sums = SumsWindow {
			snap: PoolSums {
				s_collateral: FixedU128::from_rational(1, 2),
				g_yield: FixedU128::zero(),
			},
			ahead: [
				PoolSums { s_collateral: FixedU128::from_u32(300), g_yield: FixedU128::zero() },
				PoolSums::default(),
			],
		};
		let got = realize::<u128>(1_000_000_000_000, &snap, &current, &sums, &PRECISION);
		assert_eq!(got.collateral_gain, 600_000_600_000);
	}

	#[test]
	fn realize_two_scales_behind_divides_by_scale_factor_squared() {
		// D0=4e18 at P0=1 on scale 0; current scale 2 with P=0.5:
		// compounded = floor(4e18 * 0.5 / (1 * 1e18)) = 2.
		let snap = snapshot(FixedU128::one(), FixedU128::zero(), FixedU128::zero(), 0, 0);
		let current = accumulators(FixedU128::from_rational(1, 2), 0, 2);
		let sums = window(FixedU128::from_rational(1, 4), FixedU128::zero());
		let got = realize::<u128>(4_000_000_000_000_000_000, &snap, &current, &sums, &PRECISION);
		assert_eq!(got.compounded, 2);
		// gain = floor(4e18 * 0.25 / 1) = 1e18.
		assert_eq!(got.collateral_gain, 1_000_000_000_000_000_000);
	}

	#[test]
	fn realize_two_scales_behind_combines_three_sum_rows() {
		// D0=1e18 at P0=0.5, S0=0.2; the snapshot row finished at 0.5, the
		// next row holds 300 and the one after 500:
		// delta = (0.5 - 0.2) + 300/1e9 + 500/1e18
		//       (inner 3e17 + 3e11 + 500 = 300_000_300_000_000_500),
		// gain = floor(1e18 * delta / 0.5) = 600_000_600_000_001_000.
		let snap = snapshot(
			FixedU128::from_rational(1, 2),
			FixedU128::from_rational(1, 5),
			FixedU128::zero(),
			0,
			0,
		);
		let current = accumulators(FixedU128::from_rational(4, 5), 0, 2);
		let sums = SumsWindow {
			snap: PoolSums {
				s_collateral: FixedU128::from_rational(1, 2),
				g_yield: FixedU128::zero(),
			},
			ahead: [
				PoolSums { s_collateral: FixedU128::from_u32(300), g_yield: FixedU128::zero() },
				PoolSums { s_collateral: FixedU128::from_u32(500), g_yield: FixedU128::zero() },
			],
		};
		let got = realize::<u128>(1_000_000_000_000_000_000, &snap, &current, &sums, &PRECISION);
		assert_eq!(got.collateral_gain, 600_000_600_000_001_000);
		// compounded = floor(1e18 * 0.8 / (0.5 * 1e18)) = 1.
		assert_eq!(got.compounded, 1);
	}

	#[test]
	fn realize_three_scales_behind_compounds_to_zero_but_pays_window_gains() {
		// Three crossings leave less than one part in scale_factor² of the
		// deposit: compounded rounds to zero; gains inside the three-row
		// window remain claimable.
		let snap = snapshot(FixedU128::one(), FixedU128::zero(), FixedU128::zero(), 0, 0);
		let current = accumulators(FixedU128::from_rational(1, 2), 0, 3);
		let sums = window(FixedU128::from_rational(1, 4), FixedU128::zero());
		let got = realize::<u128>(4_000_000_000_000_000_000, &snap, &current, &sums, &PRECISION);
		assert_eq!(got.compounded, 0);
		// gain = floor(4e18 * 0.25 / 1) = 1e18.
		assert_eq!(got.collateral_gain, 1_000_000_000_000_000_000);
	}

	#[test]
	fn realize_epoch_behind_compounds_to_zero_but_pays_gains() {
		// The pool was fully depleted after the snapshot: the deposit is
		// gone, but gains recorded for its epoch window remain claimable.
		let snap = snapshot(FixedU128::one(), FixedU128::zero(), FixedU128::zero(), 0, 0);
		let current = accumulators(FixedU128::one(), 1, 0);
		let sums = window(FixedU128::from_rational(9, 10), FixedU128::from_rational(1, 20));
		let got = realize::<u128>(2_000, &snap, &current, &sums, &PRECISION);
		assert_eq!(got.compounded, 0);
		// collateral = floor(2000 * 0.9) = 1800; yield = floor(2000 * 0.05) = 100.
		assert_eq!(got.collateral_gain, 1_800);
		assert_eq!(got.yield_gain, 100);
	}

	#[test]
	fn realize_zero_deposit_is_all_zero() {
		let snap = snapshot(FixedU128::one(), FixedU128::zero(), FixedU128::zero(), 0, 0);
		let current = accumulators(FixedU128::one(), 0, 0);
		let sums = window(FixedU128::from_u32(5), FixedU128::from_u32(5));
		let got = realize::<u128>(0, &snap, &current, &sums, &PRECISION);
		assert_eq!(got, Realized { compounded: 0, collateral_gain: 0, yield_gain: 0 });
	}

	#[test]
	fn clamp_offset_debt_matrix() {
		// A=1000, min=100:
		// full depletion passes through unchanged;
		assert_eq!(clamp_offset_debt::<u128>(1_000, 1_000, 100), 1_000);
		// 999 would leave 1 < min, so clamp to A - min = 900;
		assert_eq!(clamp_offset_debt::<u128>(999, 1_000, 100), 900);
		// 950 would leave 50 < min, same clamp;
		assert_eq!(clamp_offset_debt::<u128>(950, 1_000, 100), 900);
		// 900 leaves exactly min — boundary passes;
		assert_eq!(clamp_offset_debt::<u128>(900, 1_000, 100), 900);
		// 500 leaves 500 >= min — untouched;
		assert_eq!(clamp_offset_debt::<u128>(500, 1_000, 100), 500);
		// request above the pool consumes the whole pool.
		assert_eq!(clamp_offset_debt::<u128>(5_000, 1_000, 100), 1_000);
	}

	#[test]
	fn clamp_offset_debt_below_minimum_pool_allows_only_full_depletion() {
		// A=50 < min=100: partial offsets clamp to zero (A - min saturates),
		// full depletion still passes.
		assert_eq!(clamp_offset_debt::<u128>(30, 50, 100), 0);
		assert_eq!(clamp_offset_debt::<u128>(50, 50, 100), 50);
	}

	#[test]
	fn delta_sum_basic_and_floors() {
		// floor(300 * 0.5 / 1000) = 0.15 → inner 1.5e17.
		let got = delta_sum::<u128>(300, FixedU128::from_rational(1, 2), 1_000).expect("fits");
		assert_eq!(got, FixedU128::from_inner(150_000_000_000_000_000));
		// floor(100 * 1.0 / 3000) truncates the infinite tail.
		let got = delta_sum::<u128>(100, FixedU128::one(), 3_000).expect("fits");
		assert_eq!(got, FixedU128::from_inner(33_333_333_333_333_333));
	}

	#[test]
	fn delta_sum_empty_pool_is_none_zero_distribution_is_zero() {
		assert_eq!(delta_sum::<u128>(300, FixedU128::one(), 0), None);
		assert_eq!(delta_sum::<u128>(0, FixedU128::one(), 0), Some(FixedU128::zero()));
	}

	#[test]
	fn update_p_partial_offset_no_crossing() {
		// P=1, A=1000, L=500 → P = 0.5, no crossing.
		let got = update_p_after_offset::<u128>(FixedU128::one(), 1_000, 500, &PRECISION);
		assert_eq!(
			got,
			Some(PUpdate::Updated { new_p: FixedU128::from_rational(1, 2), scales_crossed: 0 })
		);
	}

	#[test]
	fn update_p_single_crossing_folds_rescale_into_division() {
		// P=2e-9 (inner 2e9), A=1000, L=750: the unscaled candidate is
		// floor(2e9 * 250 / 1000) = 5e8 < p_min. One crossing:
		// floor(2e9 * 1e9 * 250 / 1000) = 5e17 → P = 0.5.
		let got = update_p_after_offset::<u128>(
			FixedU128::from_inner(2_000_000_000),
			1_000,
			750,
			&PRECISION,
		);
		assert_eq!(
			got,
			Some(PUpdate::Updated { new_p: FixedU128::from_rational(1, 2), scales_crossed: 1 })
		);
	}

	#[test]
	fn update_p_double_crossing() {
		// P=1, A=1e19, new_total=5: k=0 gives floor(1e18*5/1e19) = 0 and k=1
		// gives floor(1e27*5/1e19) = 5e8, both below p_min. k=2:
		// floor(1e36*5/1e19) = 5e17 → P = 0.5 after two crossings.
		let total = 10_000_000_000_000_000_000u128;
		let got = update_p_after_offset::<u128>(FixedU128::one(), total, total - 5, &PRECISION);
		assert_eq!(
			got,
			Some(PUpdate::Updated { new_p: FixedU128::from_rational(1, 2), scales_crossed: 2 })
		);
	}

	#[test]
	fn update_p_beyond_crossing_cap_is_rejected() {
		// A=1e28, new_total=1: even after two crossings the candidate is
		// floor(1e18 * 1e18 * 1 / 1e28) = 1e8 < p_min → None.
		let total = 10_000_000_000_000_000_000_000_000_000u128;
		let got = update_p_after_offset::<u128>(FixedU128::one(), total, total - 1, &PRECISION);
		assert_eq!(got, None);
	}

	#[test]
	fn update_p_floor_sizing_rule_needs_ceiling_division() {
		// The `minimum_active_pool_balance` doc rule: a floor of F raw units
		// protects pools up to F * scale_factor^2, worst case P = p_min.
		// At exactly 1e18 a floor of 1 survives: the k=2 candidate is
		// floor(1e9 * 1e18 * 1 / 1e18) = 1e9 = p_min.
		let total = 1_000_000_000_000_000_000u128;
		let got = update_p_after_offset::<u128>(P_MIN, total, total - 1, &PRECISION);
		assert_eq!(got, Some(PUpdate::Updated { new_p: P_MIN, scales_crossed: 2 }));

		// One raw unit past, the bound is ceil((1e18 + 1) / 1e18) = 2. A
		// floor of 1 is refused: floor(1e9 * 1e18 * 1 / (1e18 + 1)) =
		// 999_999_999 < p_min even after both crossings...
		let total = total + 1;
		let got = update_p_after_offset::<u128>(P_MIN, total, total - 1, &PRECISION);
		assert_eq!(got, None);

		// ...while the ceiling floor of 2 is accepted:
		// floor(1e9 * 1e18 * 2 / (1e18 + 1)) = 1_999_999_999 >= p_min.
		let got = update_p_after_offset::<u128>(P_MIN, total, total - 2, &PRECISION);
		assert_eq!(
			got,
			Some(PUpdate::Updated {
				new_p: FixedU128::from_inner(1_999_999_999),
				scales_crossed: 2
			})
		);
	}

	#[test]
	fn update_p_full_depletion_and_bad_inputs() {
		assert_eq!(
			update_p_after_offset::<u128>(FixedU128::one(), 1_000, 1_000, &PRECISION),
			Some(PUpdate::Depleted)
		);
		// Offsets above the pool must be clamped by the caller first.
		assert_eq!(update_p_after_offset::<u128>(FixedU128::one(), 1_000, 1_001, &PRECISION), None);
		// A zero offset is the identity.
		assert_eq!(
			update_p_after_offset::<u128>(FixedU128::one(), 1_000, 0, &PRECISION),
			Some(PUpdate::Updated { new_p: FixedU128::one(), scales_crossed: 0 })
		);
	}

	#[test]
	fn compound_rounds_per_caller_and_is_exact_on_clean_ratios() {
		// 1000 from P0 = 0.3 to P = 0.2: the true value 666.66... floors for a member and ceils
		// for a cohort aggregate.
		let from = accumulators(FixedU128::from_rational(3, 10), 0, 0);
		let to = accumulators(FixedU128::from_rational(1, 5), 0, 0);
		assert_eq!(compound::<u128>(1_000, &from, &to, SF as u128, Rounding::Down), 666);
		assert_eq!(compound::<u128>(1_000, &from, &to, SF as u128, Rounding::Up), 667);
		// An exact ratio neither gains nor loses under either rounding.
		let to = accumulators(FixedU128::from_rational(3, 20), 0, 0);
		assert_eq!(compound::<u128>(1_000, &from, &to, SF as u128, Rounding::Down), 500);
		assert_eq!(compound::<u128>(1_000, &from, &to, SF as u128, Rounding::Up), 500);
		assert_eq!(compound::<u128>(0, &from, &to, SF as u128, Rounding::Up), 0);
	}

	#[test]
	fn compound_divides_per_scale_behind_and_truncates_outside_the_window() {
		let from = accumulators(FixedU128::one(), 0, 0);
		let half = FixedU128::from_rational(1, 2);
		let sf = SF as u128;
		// One scale behind at P = 0.5 with sf = 1e9: floor(2e12 * 0.5 / 1e9) = 1000.
		let to = accumulators(half, 0, 1);
		assert_eq!(compound::<u128>(2_000_000_000_000, &from, &to, sf, Rounding::Down), 1_000);
		// Two behind: the floor drops the last sliver, the ceiling still carries it.
		let to = accumulators(half, 0, 2);
		assert_eq!(compound::<u128>(2_000_000_000_000, &from, &to, sf, Rounding::Down), 0);
		assert_eq!(compound::<u128>(2_000_000_000_000, &from, &to, sf, Rounding::Up), 1);
		// Beyond the span, or into a later epoch, nothing is left under either rounding.
		let to = accumulators(half, 0, 3);
		assert_eq!(compound::<u128>(2_000_000_000_000, &from, &to, sf, Rounding::Up), 0);
		let to = accumulators(half, 1, 0);
		assert_eq!(compound::<u128>(2_000_000_000_000, &from, &to, sf, Rounding::Up), 0);
	}

	/// A cohort aggregate uses upward rounding at each update. Thus, it cannot fall below the sum
	/// of member claims that [`realize`] rounds down.
	#[test]
	fn aggregate_ceiling_covers_member_floors() {
		let amounts: [u128; 6] = [1, 3, 99, 1_000, 123_457, 999_999_999_999];
		// Where the pending leg stands at each member's join, in the order it moves.
		let joins = [
			accumulators(FixedU128::one(), 0, 0),
			accumulators(FixedU128::from_rational(2, 3), 0, 0),
			accumulators(FixedU128::from_rational(1, 7), 0, 0),
			accumulators(FixedU128::from_inner(1_000_000_001), 0, 0),
			accumulators(FixedU128::from_rational(1, 2), 0, 1),
			accumulators(FixedU128::from_rational(1, 3), 0, 1),
		];
		let end = accumulators(FixedU128::from_rational(1, 4), 0, 1);
		let sf = SF as u128;

		let mut aggregate: u128 = 0;
		let mut coords = joins[0];
		let mut claims: u128 = 0;
		for (amount, joined) in amounts.into_iter().zip(joins) {
			// The revaluation a join performs, then the member claim as `realize` floors it.
			aggregate = compound(aggregate, &coords, &joined, sf, Rounding::Up) + amount;
			coords = joined;
			claims += compound(amount, &joined, &end, sf, Rounding::Down);
		}
		let activated = compound::<u128>(aggregate, &coords, &end, sf, Rounding::Up);
		assert!(claims <= activated);
		// Each ceiling and each floor costs less than one raw unit of dust.
		assert!(activated - claims <= 2 * amounts.len() as u128);
	}

	#[test]
	fn cohort_deadline_rounds_up_never_down() {
		// Mid-window: 1_000 + 5_000 = 6_000 rounds up to 10_000.
		assert_eq!(cohort_deadline(1_000, 5_000), 10_000);
		// On the boundary the wait is exactly the delay.
		assert_eq!(cohort_deadline(5_000, 5_000), 10_000);
		assert_eq!(cohort_deadline(0, 5_000), 5_000);
		// One past the boundary rolls a whole width forward.
		assert_eq!(cohort_deadline(5_001, 5_000), 15_000);
		// The wait always lands in [delay, 2 * delay).
		for now in [0, 1, 999, 4_999, 5_000, 7_331] {
			let deadline = cohort_deadline(now, 5_000);
			assert!(deadline - now >= 5_000);
			assert!(deadline - now < 10_000);
		}
	}

	#[test]
	fn update_p_result_stays_at_or_below_one() {
		// A crossing right at the p_min boundary: P=p_min, L makes the ratio
		// just under 1, so the rescaled result approaches p_min * SF = 1
		// from below but never exceeds it.
		let got = update_p_after_offset::<u128>(P_MIN, 1_000_000, 1, &PRECISION);
		match got {
			Some(PUpdate::Updated { new_p, scales_crossed }) => {
				assert!(new_p <= FixedU128::one());
				assert_eq!(scales_crossed, 1);
			},
			other => panic!("expected a single crossing, got {other:?}"),
		}
	}
}
