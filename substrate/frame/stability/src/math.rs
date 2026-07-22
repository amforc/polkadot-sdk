//! Product-sum accounting for the Stability Pool: the pure functions that
//! interpret the lazy accumulator model.
//!
//! The pool tracks depositor state lazily through three global accumulators
//! (SPEC.md §6):
//! - `P` (loss product): the surviving fraction of a unit deposited at `P = 1`;
//! - `S` (collateral sum): collateral gain per unit of `P`-adjusted deposit;
//! - `G` (yield sum): stablecoin yield per unit of `P`-adjusted deposit.
//!
//! `P` only shrinks within an epoch. When it would drop below `p_min`, it is
//! rescaled by `scale_factor` and the scale index increments; full depletion
//! starts a new epoch with `P = 1`. Deposits store `(P, S, G, epoch, scale)`
//! snapshots and realize against the accumulators on their next touch.
//!
//! Rounding: user payouts (compounded deposits, collateral gains, yield
//! gains) round down; the flooring dust stays inside pool-owned totals.

use crate::types::{Accumulators, DepositSnapshot, PUpdate, PoolPrecision, Realized, SumsWindow};
use frame::{
	arithmetic::{
		helpers_128bit::multiply_by_rational_with_rounding, FixedPointOperand, FixedU128, One,
		Rounding, Saturating, Zero,
	},
	traits::Defensive,
};

/// A single offset may rescale `P` at most this many times before the pallet
/// rejects it as unresolvable precision loss. Two crossings are only
/// reachable when `new_total / total < 1e-18`, i.e. with a misconfigured
/// `minimum_active_pool_balance` on a gigantic pool.
pub const MAX_SCALE_CROSSINGS: u32 = 2;

/// Lower bound on the integer value of `scale_factor`: rescaling by less
/// buys back too little precision per crossing to be worth a scale index.
pub const SCALE_FACTOR_INT_MIN: u64 = 1_000;

/// Upper bound on the integer value of `scale_factor` (1e10). Keeps the
/// worst-case [`update_p_after_offset`] numerator inside `u128`:
/// `P.inner * scale_factor^MAX_SCALE_CROSSINGS <= 1e18 * 1e20 = 1e38`.
pub const SCALE_FACTOR_INT_MAX: u64 = 10_000_000_000;

/// How many scales past its snapshot a deposit still realizes: the row at `snapshot.scale + k`
/// contributes with an extra `scale_factor^k` divisor for `k <= SCALE_SPAN`. Anything further is
/// below one part in `scale_factor^SCALE_SPAN` (>= 1e6) of the deposit and
/// is deliberately ignored. Distinct from [`MAX_SCALE_CROSSINGS`], which bounds a single
/// offset; this bounds realization lag.
pub const SCALE_SPAN: u32 = 2;

/// Realize a deposit of `d0` (as of its snapshot) against the current
/// accumulators: SPEC.md §6.2 with the §6.4 epoch/scale rules folded in.
///
/// - `k <= SCALE_SPAN` scales behind in the same epoch: `compounded = floor(d0 * P / (P0 *
///   scale_factor^k))`;
/// - further behind, or an earlier epoch: compounded is zero.
///
/// Gains use one uniform formula across all cases:
/// `floor(d0 * ((sum_snap - sum0) + Σ ahead[k] / scale_factor^(k+1)) / P0)`
/// — in the same-scale case `sum_snap` is the live row and the look-ahead
/// rows are zero.
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

	let compounded = if snapshot.coords.epoch == current.epoch {
		// Each scale behind adds a `scale_factor` divisor; `is_valid` bounds
		// keep the worst-case denominator
		// `1e18 * (1e10)^SCALE_SPAN = 1e38` inside `u128`.
		let denominator = match current.scale.checked_sub(snapshot.coords.scale) {
			Some(behind) if behind <= SCALE_SPAN => sf_int
				.checked_pow(behind)
				.and_then(|factor| snapshot.coords.p.into_inner().checked_mul(factor)),
			Some(_) | None => None,
		};
		match denominator {
			Some(denom) => mul_ratio_floor(d0, current.p.into_inner(), denom),
			None => Balance::zero(),
		}
	} else {
		Balance::zero()
	};
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

/// `floor(d * ((sum_snap - sum0) + Σ ahead[k] / sf_int^(k+1)) / p0)`, the
/// shared gain leg of [`realize`] for both the collateral (`S`) and yield
/// (`G`) sums.
fn gain<Balance: FixedPointOperand>(
	d: Balance,
	sum_snap: FixedU128,
	sum0: FixedU128,
	ahead: [FixedU128; SCALE_SPAN as usize],
	sf_int: u128,
	p0: FixedU128,
) -> Balance {
	// `scale_factor` guarantees `1 <= sf_int <= 1e10`, so every divisor
	// (at most `sf_int^SCALE_SPAN = 1e20`) stays inside `u128`.
	debug_assert!(sf_int >= 1);
	let mut delta = sum_snap.saturating_sub(sum0);
	let mut divisor = sf_int;
	for row in ahead {
		// The hot same-coordinate path carries zero look-ahead rows; skip
		// their divisions outright.
		if !row.is_zero() {
			delta = delta.saturating_add(FixedU128::from_inner(row.into_inner() / divisor));
		}
		divisor = divisor.saturating_mul(sf_int);
	}
	if delta.is_zero() {
		return Balance::zero();
	}
	mul_ratio_floor(d, delta.into_inner(), p0.into_inner())
}

/// SPEC.md §7.1: cap an offset so it never leaves
/// `0 < remaining < min_active_pool` (§6.5). Full depletion is always
/// allowed; when `total_active < min_active_pool` already, only full
/// depletion can proceed (partial offsets clamp to zero).
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

/// `floor(amount * numerator / denominator)`: the pro-rata share of `amount`
/// backing `numerator` out of `denominator`. Requires
/// `numerator <= denominator`; zero when any input is zero.
pub fn pro_rata_floor<Balance: FixedPointOperand>(
	amount: Balance,
	numerator: Balance,
	denominator: Balance,
) -> Balance {
	debug_assert!(numerator <= denominator);
	if amount.is_zero() || numerator.is_zero() || denominator.is_zero() {
		return Balance::zero();
	}
	pusd_primitives::mul_div_floor(amount, numerator, denominator)
		.defensive_unwrap_or_else(Balance::zero)
}

/// `floor(distributed * P / total_active)` as a `FixedU128` delta for `S`
/// (collateral) or `G` (yield) per SPEC.md §6.3. `None` when the pool is
/// empty or the product overflows (the caller surfaces an arithmetic error).
pub fn delta_sum<Balance: FixedPointOperand>(
	distributed: Balance,
	p: FixedU128,
	total_active: Balance,
) -> Option<FixedU128> {
	debug_assert!(!p.is_zero());
	debug_assert!(p <= FixedU128::one());
	pusd_primitives::mul_div_rate_floor(distributed, p, total_active)
}

/// Shrink `P` after an offset of `offset_debt` against `total_active`,
/// folding any rescaling into the division itself (SPEC.md §6.4).
///
/// Computing `floor(P * new_total / total)` first and multiplying by
/// `scale_factor` afterwards would discard exactly the precision the rescale
/// exists to protect (and can floor to zero outright), so each candidate is
/// the one-shot `floor(P.inner * scale_factor^k * new_total / total)` for
/// `k = 0..=MAX_SCALE_CROSSINGS`, taking the first result at or above
/// `p_min`. `None` means the offset must be rejected: more crossings than
/// supported, an overflowed intermediate, or `offset_debt > total_active`.
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
			// A crossing lands `P` in `[p_min, p_min * scale_factor]` and
			// `is_valid` bounds `p_min * scale_factor <= 1`.
			debug_assert!(new_p <= FixedU128::one());
			return Some(PUpdate::Updated { new_p, scales_crossed });
		}
		factor = factor.checked_mul(sf_int)?;
	}
	None
}

/// `floor(value * numerator / denominator)` at `Balance` precision, with the
/// same defensive fallback used by the public payout paths.
fn mul_ratio_floor<Balance: FixedPointOperand>(
	value: Balance,
	numerator: u128,
	denominator: u128,
) -> Balance {
	multiply_by_rational_with_rounding(
		value.unique_saturated_into(),
		numerator,
		denominator,
		Rounding::Down,
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
	fn pro_rata_floor_floors() {
		// floor(100 * 1 / 3) = 33.
		assert_eq!(pro_rata_floor::<u128>(100, 1, 3), 33);
		assert_eq!(pro_rata_floor::<u128>(100, 3, 3), 100);
		assert_eq!(pro_rata_floor::<u128>(100, 0, 3), 0);
		assert_eq!(pro_rata_floor::<u128>(0, 1, 3), 0);
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
