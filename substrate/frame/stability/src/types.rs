//! Data types for `pallet-stability`.

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::arithmetic::{
	FixedPointNumber, FixedPointOperand, FixedU128, One, Permill, Saturating, Zero,
};
use pusd_primitives::Millis;
use scale_info::TypeInfo;

use crate::math;

/// Live product-sum coordinates of a pool. Embedded verbatim in every encoded
/// [`DepositSnapshot`] and [`PoolState`], hence the codec derives.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
)]
pub struct Accumulators {
	pub p: FixedU128,
	pub epoch: u32,
	pub scale: u32,
}

impl Accumulators {
	/// The genesis coordinates a fresh pool hands out: `P = 1`, epoch and
	/// scale 0. Realization against these is the identity.
	pub fn fresh() -> Self {
		Self { p: FixedU128::one(), epoch: 0, scale: 0 }
	}
}

/// `S` and `G` for one `(epoch, scale)` coordinate (SPEC.md §5.2).
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
	Default,
)]
pub struct PoolSums {
	pub s_collateral: FixedU128,
	pub g_yield: FixedU128,
}

/// A deposit's stored snapshot at its last realization: the accumulator
/// coordinates ([`Accumulators`]) plus the `(epoch, scale)` sums row they point
/// at. Embedded in every [`Deposit`] storage row, hence the codec derives.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
)]
pub struct DepositSnapshot {
	pub coords: Accumulators,
	pub sums: PoolSums,
}

impl DepositSnapshot {
	/// A fresh pool's snapshot: [`Accumulators::fresh`] coordinates and zero
	/// sums. Realization against it is the identity.
	pub fn fresh() -> Self {
		Self { coords: Accumulators::fresh(), sums: PoolSums::default() }
	}
}

/// Gain sums a deposit realizes against: the row at its snapshot
/// `(epoch, scale)` plus the [`SCALE_SPAN`](math::SCALE_SPAN) rows after it
/// (zero when absent).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SumsWindow {
	pub snap: PoolSums,
	pub ahead: [PoolSums; math::SCALE_SPAN as usize],
}

/// Accumulator precision parameters from [`StabilityPoolConfig`]. Immutable
/// per branch: historical snapshots realize against the `scale_factor` that
/// was live when their scale was crossed, so changing it would misprice
/// every deposit left behind a scale boundary.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
)]
pub struct PoolPrecision {
	pub p_min: FixedU128,
	// TODO: Check visibility
	pub scale_factor: u64,
}

impl PoolPrecision {
	/// Out-of-range parameters break product-sum accounting: `scale_factor`
	/// must lie in `[SCALE_FACTOR_INT_MIN, SCALE_FACTOR_INT_MAX]`, and a
	/// rescale must land `P` back at or below one: `p_min * scale_factor <= 1`.
	pub fn is_valid(&self) -> bool {
		if self.p_min.is_zero() {
			return false;
		}
		if self.scale_factor < math::SCALE_FACTOR_INT_MIN {
			return false;
		}
		if self.scale_factor > math::SCALE_FACTOR_INT_MAX {
			return false;
		}
		self.p_min.saturating_mul(FixedU128::saturating_from_integer(self.scale_factor)) <=
			FixedU128::one()
	}

	/// `scale_factor` widened for the u128 accumulator math, floored at 1 as a
	/// guard against a decoded zero dividing by zero in the gain division.
	pub fn scale_factor(&self) -> u128 {
		debug_assert!(self.scale_factor >= math::SCALE_FACTOR_INT_MIN);
		debug_assert!(self.scale_factor <= math::SCALE_FACTOR_INT_MAX);
		u128::from(self.scale_factor).max(1)
	}

	#[cfg(test)]
	pub(crate) fn set_scale_factor(&mut self, scale_factor: u64) {
		self.scale_factor = scale_factor;
	}
}

/// Outcome of realizing a deposit against the current accumulators.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Realized<Balance> {
	pub compounded: Balance,
	pub collateral_gain: Balance,
	pub yield_gain: Balance,
}

/// Outcome of shrinking `P` after an active-pool offset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PUpdate {
	Updated { new_p: FixedU128, scales_crossed: u32 },
	Depleted,
}

/// Per-branch depositor state (SPEC.md §5.1).
///
/// `active_deposit` is the amount last realized against the accumulators; it
/// may be stale relative to the live `P`. Every user operation realizes
/// losses and gains through the snapshot first, then applies its change and
/// resets it.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct Deposit<Balance> {
	pub active_deposit: Balance,

	pub snapshot: DepositSnapshot,

	/// Realized-but-unclaimed gains; accumulate across realizations.
	pub claimable_collateral: Balance,
	pub claimable_yield: Balance,

	pub pending_deposit: Option<PendingDeposit<Balance>>,
	pub withdrawal_request: Option<WithdrawalRequest<Balance>>,
}

impl<Balance: Zero> Deposit<Balance> {
	/// A value-free row at the given snapshot (realization on a fresh row is
	/// the identity).
	pub fn fresh(snapshot: DepositSnapshot) -> Self {
		Self {
			active_deposit: Balance::zero(),
			snapshot,
			claimable_collateral: Balance::zero(),
			claimable_yield: Balance::zero(),
			pending_deposit: None,
			withdrawal_request: None,
		}
	}

	/// True when the row holds no user value and can be pruned. A leftover
	/// `withdrawal_request` is deliberately ignored: with nothing left to
	/// withdraw it is dead state and goes with the row.
	pub fn is_empty(&self) -> bool {
		self.active_deposit.is_zero() &&
			self.claimable_collateral.is_zero() &&
			self.claimable_yield.is_zero() &&
			self.pending_deposit.is_none()
	}
}

/// Recently supplied stablecoin waiting out the entry delay (SPEC.md §5.1).
/// Not part of `total_active_deposits`; earns no gains or yield, but may be
/// consumed by the last-resort liquidation backstop (§6.8).
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct PendingDeposit<Balance> {
	pub amount: Balance,
	pub activatable_at: Millis,
}

/// Two-step Safety-Mode withdrawal state (§6.9). Only recorded in Safety
/// Mode — a Normal-Mode `request_withdraw` forwards to the direct
/// withdrawal — and ignored by Normal-Mode withdrawals should the branch
/// recover before execution.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct WithdrawalRequest<Balance> {
	pub amount: Balance,
	pub executable_at: Millis,
}

/// Branch pool totals and current product-sum coordinates (SPEC.md §5.2).
///
/// Deviations from the spec sketch (design decisions, 2026-07-07):
/// - no `pending_head`/`pending_tail` fields — the pending-deposit FIFO lives in the runtime's
///   `pallet-linked-list` instance;
/// - no `*_rounding_surplus` fields — every flooring remainder stays inside the unclaimed totals,
///   so the fields would be identically zero. They arrive with their first writer.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct PoolState<Balance> {
	pub total_active_deposits: Balance,
	pub total_pending_deposits: Balance,
	pub coords: Accumulators,
	pub total_collateral_gains_unclaimed: Balance,
	pub total_yield_unclaimed: Balance,
}

impl<Balance> PoolState<Balance> {
	/// The deposit snapshot at the pool's current coordinates; `sums` is the
	/// live `(epoch, scale)` sums row.
	pub fn snapshot(&self, sums: &PoolSums) -> DepositSnapshot {
		DepositSnapshot { coords: self.coords, sums: *sums }
	}
}

impl<Balance: Zero> PoolState<Balance> {
	/// State seeded at branch registration: empty pool at `P = 1`,
	/// epoch 0, scale 0.
	pub fn fresh() -> Self {
		Self {
			total_active_deposits: Balance::zero(),
			total_pending_deposits: Balance::zero(),
			coords: Accumulators::fresh(),
			total_collateral_gains_unclaimed: Balance::zero(),
			total_yield_unclaimed: Balance::zero(),
		}
	}
}

impl<Balance: FixedPointOperand> PoolState<Balance> {
	/// `S`/`G` delta for distributing `distributed` over the active pool
	/// (SPEC.md §6.3); `None` when the pool is empty or the product overflows.
	pub fn delta_sum(&self, distributed: Balance) -> Option<FixedU128> {
		math::delta_sum(distributed, self.coords.p, self.total_active_deposits)
	}
}

/// Per-branch governance parameters (SPEC.md §5.3, plus `yield_share`).
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct StabilityPoolConfig<Balance> {
	/// Smallest accepted deposit; prevents dust rows.
	pub minimum_deposit: Balance,
	/// Post-offset floor for the active pool: an offset either leaves at
	/// least this much active or fully depletes the pool (§6.5).
	pub minimum_active_pool_balance: Balance,
	pub entry_delay: Millis,
	pub safety_withdrawal_delay: Millis,
	/// Accumulator precision parameters; immutable after registration.
	pub precision: PoolPrecision,
	/// Share of routed branch yield the active pool takes; the remainder
	/// goes to the vault engine's fee destination.
	pub yield_share: Permill,
}

impl<Balance: Zero> StabilityPoolConfig<Balance> {
	/// Zero thresholds break dust protection and the §6.5 offset floor;
	/// the precision bounds are [`PoolPrecision::is_valid`]'s.
	pub fn is_valid(&self) -> bool {
		if self.minimum_deposit.is_zero() {
			return false;
		}
		if self.minimum_active_pool_balance.is_zero() {
			return false;
		}
		self.precision.is_valid()
	}
}

/// One registered market's pool in one record: created whole at branch
/// registration, torn down whole at deregistration.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct StabilityPool<Balance> {
	/// Governance parameters, moved only through `set_stability_pool_config`.
	pub config: StabilityPoolConfig<Balance>,
	/// Hot accounting state, rewritten by every pool operation.
	pub state: PoolState<Balance>,
}

impl<Balance: Zero> StabilityPool<Balance> {
	/// The record seeded at branch registration: the given config over a
	/// fresh [`PoolState`].
	pub fn fresh(config: StabilityPoolConfig<Balance>) -> Self {
		Self { config, state: PoolState::fresh() }
	}
}

/// The offset result shapes live in `pusd-primitives` beside the
/// `StabilityPoolOffsetApi` they belong to (SPEC.md §7.1 / §7.2).
pub use pusd_primitives::PendingOffsetResult;

/// Which capital funded a recovery offset (SPEC.md §10).
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
)]
pub enum RecoveryOffsetSource {
	ActivePool,
	IncomingDeposit,
}

#[cfg(test)]
mod tests {
	use super::*;

	fn valid_config() -> StabilityPoolConfig<u128> {
		StabilityPoolConfig {
			minimum_deposit: 100,
			minimum_active_pool_balance: 100,
			entry_delay: 5_000,
			safety_withdrawal_delay: 600_000,
			precision: PoolPrecision {
				p_min: FixedU128::from_inner(1_000_000_000),
				scale_factor: 1_000_000_000,
			},
			yield_share: Permill::from_percent(75),
		}
	}

	#[test]
	fn config_validation_accepts_the_reference_parameters() {
		assert!(valid_config().is_valid());
		// Zero delays are legitimate governance choices (they disable the
		// respective protection), zero yield share as well.
		let mut config = valid_config();
		config.entry_delay = 0;
		config.safety_withdrawal_delay = 0;
		config.yield_share = Permill::zero();
		assert!(config.is_valid());
	}

	#[test]
	fn config_validation_rejects_broken_parameters() {
		let mut config = valid_config();
		config.minimum_deposit = 0;
		assert!(!config.is_valid());

		let mut config = valid_config();
		config.minimum_active_pool_balance = 0;
		assert!(!config.is_valid());

		let mut config = valid_config();
		config.precision.p_min = FixedU128::zero();
		assert!(!config.is_valid());

		// Below the minimum useful rescale.
		let mut config = valid_config();
		config.precision.set_scale_factor(999);
		assert!(!config.is_valid());

		// Above the u128 overflow guard (1e10).
		let mut config = valid_config();
		config.precision.set_scale_factor(10_000_000_001);
		assert!(!config.is_valid());

		// A rescale would push P above one: p_min * scale_factor > 1.
		let mut config = valid_config();
		config.precision.p_min = FixedU128::from_inner(2_000_000_000);
		assert!(!config.is_valid());
	}

	#[test]
	fn deposit_emptiness_ignores_withdrawal_requests() {
		let mut deposit = Deposit::<u128>::fresh(DepositSnapshot::fresh());
		deposit.withdrawal_request = Some(WithdrawalRequest { amount: 10, executable_at: 601_000 });
		assert!(deposit.is_empty());

		deposit.pending_deposit = Some(PendingDeposit { amount: 1, activatable_at: 6_000 });
		assert!(!deposit.is_empty());

		deposit.pending_deposit = None;
		deposit.claimable_yield = 1;
		assert!(!deposit.is_empty());
	}
}
