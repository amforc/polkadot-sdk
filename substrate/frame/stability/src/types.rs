//! Storage types for `pallet-stability`.

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::arithmetic::{FixedPointNumber, FixedU128, One, Permill, Saturating, Zero};
use scale_info::TypeInfo;

use crate::math;

/// Per-branch depositor state (SPEC.md §5.1).
///
/// `active_deposit` is the amount last realized against the accumulators; it
/// may be stale relative to the live `P`. Every user operation realizes
/// losses and gains through the snapshot fields first, then applies its
/// change and resets the snapshots.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct Deposit<Balance, Moment> {
	pub active_deposit: Balance,

	pub snapshot_p: FixedU128,
	pub snapshot_s: FixedU128,
	pub snapshot_g: FixedU128,
	pub snapshot_epoch: u32,
	pub snapshot_scale: u32,

	/// Realized-but-unclaimed gains; accumulate across realizations.
	pub claimable_collateral: Balance,
	pub claimable_yield: Balance,

	pub pending_deposit: Option<PendingDeposit<Balance, Moment>>,
	pub withdrawal_request: Option<WithdrawalRequest<Balance, Moment>>,
}

impl<Balance: Zero, Moment> Deposit<Balance, Moment> {
	/// A value-free row snapshotted at the given accumulator coordinates
	/// (realization on a fresh row is the identity).
	pub fn fresh(p: FixedU128, sums: &PoolSums, epoch: u32, scale: u32) -> Self {
		Self {
			active_deposit: Balance::zero(),
			snapshot_p: p,
			snapshot_s: sums.s_collateral,
			snapshot_g: sums.g_yield,
			snapshot_epoch: epoch,
			snapshot_scale: scale,
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
pub struct PendingDeposit<Balance, Moment> {
	pub amount: Balance,
	pub activatable_at: Moment,
}

/// Two-step withdrawal state; only load-bearing in Safety Mode (§6.9).
/// `executable_at` is fixed at request time, so a request made in Normal
/// Mode keeps its delay if the branch enters Safety before execution.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct WithdrawalRequest<Balance, Moment> {
	pub amount: Balance,
	pub executable_at: Moment,
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

	pub p: FixedU128,
	pub epoch: u32,
	pub scale: u32,

	pub total_collateral_gains_unclaimed: Balance,
	pub total_yield_unclaimed: Balance,
}

impl<Balance: Zero> PoolState<Balance> {
	/// State seeded at branch registration: empty pool at `P = 1`,
	/// epoch 0, scale 0.
	pub fn fresh() -> Self {
		Self {
			total_active_deposits: Balance::zero(),
			total_pending_deposits: Balance::zero(),
			p: FixedU128::one(),
			epoch: 0,
			scale: 0,
			total_collateral_gains_unclaimed: Balance::zero(),
			total_yield_unclaimed: Balance::zero(),
		}
	}

	pub fn accumulators(&self) -> math::Accumulators {
		math::Accumulators { p: self.p, epoch: self.epoch, scale: self.scale }
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

/// Per-branch governance parameters (SPEC.md §5.3, plus `yield_share`).
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct StabilityPoolConfig<Balance, Moment> {
	/// Smallest accepted deposit; prevents dust rows.
	pub minimum_deposit: Balance,
	/// Post-offset floor for the active pool: an offset either leaves at
	/// least this much active or fully depletes the pool (§6.5).
	pub minimum_active_pool_balance: Balance,
	pub entry_delay: Moment,
	pub safety_withdrawal_delay: Moment,
	/// Precision floor for `P`; immutable after registration (see
	/// `math::PoolPrecision` for why).
	pub p_min: FixedU128,
	/// Rescale multiplier applied when `P` falls below `p_min`; immutable
	/// after registration.
	pub scale_factor: FixedU128,
	/// Share of routed branch yield the active pool takes; the remainder
	/// goes to the vault engine's fee destination.
	pub yield_share: Permill,
}

impl<Balance: Zero, Moment> StabilityPoolConfig<Balance, Moment> {
	/// Zero thresholds break dust protection and the §6.5 offset floor;
	/// out-of-range precision parameters break product-sum accounting
	/// (`scale_factor` must be an integer in
	/// `[SCALE_FACTOR_INT_MIN, SCALE_FACTOR_INT_MAX]`, and a rescale must
	/// land `P` back at or below one: `p_min * scale_factor <= 1`).
	pub fn is_valid(&self) -> bool {
		if self.minimum_deposit.is_zero() {
			return false;
		}
		if self.minimum_active_pool_balance.is_zero() {
			return false;
		}
		if self.p_min.is_zero() {
			return false;
		}
		if !self.scale_factor.frac().is_zero() {
			return false;
		}
		let scale_factor_int = self.scale_factor.into_inner() / FixedU128::DIV;
		if scale_factor_int < math::SCALE_FACTOR_INT_MIN {
			return false;
		}
		if scale_factor_int > math::SCALE_FACTOR_INT_MAX {
			return false;
		}
		self.p_min.saturating_mul(self.scale_factor) <= FixedU128::one()
	}

	pub(crate) fn precision(&self) -> math::PoolPrecision {
		math::PoolPrecision { p_min: self.p_min, scale_factor: self.scale_factor }
	}
}

/// The offset result shapes live in `pusd-primitives` beside the
/// `StabilityPoolOffsetApi` they belong to (SPEC.md §7.1 / §7.2).
pub use pusd_primitives::{PendingOffsetResult, PoolOffsetResult};

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

	fn valid_config() -> StabilityPoolConfig<u128, u64> {
		StabilityPoolConfig {
			minimum_deposit: 100,
			minimum_active_pool_balance: 100,
			entry_delay: 5_000,
			safety_withdrawal_delay: 600_000,
			p_min: FixedU128::from_inner(1_000_000_000),
			scale_factor: FixedU128::from_u32(1_000_000_000),
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
		config.p_min = FixedU128::zero();
		assert!(!config.is_valid());

		// Fractional scale factor.
		let mut config = valid_config();
		config.scale_factor = FixedU128::from_rational(3, 2);
		assert!(!config.is_valid());

		// Below the minimum useful rescale.
		let mut config = valid_config();
		config.scale_factor = FixedU128::from_u32(999);
		assert!(!config.is_valid());

		// Above the u128 overflow guard (1e10).
		let mut config = valid_config();
		config.scale_factor = FixedU128::saturating_from_integer(10_000_000_001u64);
		assert!(!config.is_valid());

		// A rescale would push P above one: p_min * scale_factor > 1.
		let mut config = valid_config();
		config.p_min = FixedU128::from_inner(2_000_000_000);
		assert!(!config.is_valid());
	}

	#[test]
	fn fresh_pool_state_starts_at_p_one_epoch_zero() {
		let state = PoolState::<u128>::fresh();
		assert_eq!(state.p, FixedU128::one());
		assert_eq!(state.epoch, 0);
		assert_eq!(state.scale, 0);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.total_pending_deposits, 0);
	}

	#[test]
	fn deposit_emptiness_ignores_withdrawal_requests() {
		let mut deposit = Deposit::<u128, u64>::fresh(FixedU128::one(), &PoolSums::default(), 0, 0);
		deposit.withdrawal_request = Some(WithdrawalRequest { amount: 10, executable_at: 601_000 });
		assert!(deposit.is_empty());

		deposit.pending_deposit = Some(PendingDeposit { amount: 1, activatable_at: 6_000 });
		assert!(!deposit.is_empty());

		deposit.pending_deposit = None;
		deposit.claimable_yield = 1;
		assert!(!deposit.is_empty());
	}
}
