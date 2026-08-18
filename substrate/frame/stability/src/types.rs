//! The storage types of the stability pool, and the rules that keep them consistent.

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::arithmetic::{
	FixedPointNumber, FixedPointOperand, FixedU128, One, Permill, Saturating, Zero,
};
use pusd_primitives::Millis;
use scale_info::TypeInfo;

use crate::math;

/// Where one leg of a pool stands in its loss history.
///
/// A deposit keeps a copy of these coordinates and realizes against the live ones. Both a
/// [`DepositSnapshot`] and a [`PoolState`] embed the type, hence the codec derives.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy)]
#[cfg_attr(test, derive(PartialEq, Debug))]
pub struct Accumulators {
	/// The fraction of a deposit that survives every offset applied so far.
	pub p: FixedU128,
	/// Counts the offsets that emptied the leg. Each one resets `p` to one.
	pub epoch: u32,
	/// Counts the times `p` fell below `p_min` and was multiplied back up by `scale_factor`.
	pub scale: u32,
}

impl Accumulators {
	/// The coordinates of a leg that has taken no loss: `P = 1`, epoch and scale zero.
	///
	/// Realizing against them changes nothing, so a new deposit can start from them.
	pub fn fresh() -> Self {
		Self { p: FixedU128::one(), epoch: 0, scale: 0 }
	}
}

/// Which pool of capital an operation runs on.
///
/// Both legs share one set of accumulators and one implementation. The leg selects the
/// coordinates, the total and the [`PoolSums`] rows that the operation reads and writes.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy)]
pub enum Leg {
	/// Deposits that absorb offsets and earn yield.
	Active,
	/// Deposits still waiting out the entry delay. They earn no yield, but they absorb what the
	/// active pool cannot.
	Pending,
}

impl Leg {
	/// Both legs, in the order the pool seeds and checks them.
	pub const ALL: [Self; 2] = [Self::Active, Self::Pending];
}

/// What one `(epoch, scale)` coordinate of a leg has paid out per unit of deposit.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, Default)]
#[cfg_attr(test, derive(PartialEq, Debug))]
pub struct PoolSums {
	/// `S`: collateral paid per unit of deposit.
	pub s_collateral: FixedU128,
	/// `G`: stablecoin yield paid per unit of deposit. Always zero on [`Leg::Pending`].
	pub g_yield: FixedU128,
}

/// Where a deposit stood when it was last realized.
///
/// The distance between the snapshot and the live values is the loss and the gains that the
/// deposit has not settled yet. Every [`Deposit`] row embeds one, hence the codec derives.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
pub struct DepositSnapshot {
	/// The accumulator coordinates at the time of the last realization.
	pub coords: Accumulators,
	/// The sums of the row that `coords` points at, at the same moment.
	pub sums: PoolSums,
}

impl DepositSnapshot {
	/// The snapshot of a leg that has taken no loss. Realizing against it changes nothing.
	pub fn fresh() -> Self {
		Self { coords: Accumulators::fresh(), sums: PoolSums::default() }
	}
}

/// The sums rows one realization needs.
///
/// A deposit can lag the pool by up to `math::SCALE_SPAN` scale boundaries, and it earns a share
/// of every row in between, so one read of its own row is not enough.
pub struct SumsWindow {
	/// The row at the coordinates of the snapshot.
	pub snap: PoolSums,
	/// The rows at the scales that follow, in order. An absent row reads as zero.
	pub ahead: [PoolSums; math::SCALE_SPAN as usize],
}

/// How far `P` may fall before the pool rescales it.
///
/// These parameters cannot change after registration. A deposit left behind a scale boundary
/// realizes against the `scale_factor` that was live when it crossed that boundary, so a new
/// factor would misprice it.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct PoolPrecision {
	/// The value of `P` that triggers a rescale.
	pub p_min: FixedU128,
	/// What a rescale multiplies `P` by.
	///
	/// Read it through [`PoolPrecision::scale_factor`], which widens it for the accumulator math
	/// and guards against a zero decoded from corrupt storage. The field itself is public because
	/// governance passes a whole [`StabilityPoolConfig`] as a call argument.
	pub scale_factor: u64,
}

impl PoolPrecision {
	/// Returns whether these parameters keep the accumulator math sound.
	///
	/// `scale_factor` must sit between `math::SCALE_FACTOR_INT_MIN` and
	/// `math::SCALE_FACTOR_INT_MAX`, and a rescale must leave `P` at or below one, which needs
	/// `p_min * scale_factor <= 1`.
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

	/// `scale_factor` widened for the `u128` accumulator math.
	///
	/// The result is at least one, so a value decoded from corrupt storage cannot divide by zero.
	pub fn scale_factor(&self) -> u128 {
		debug_assert!(self.scale_factor >= math::SCALE_FACTOR_INT_MIN);
		debug_assert!(self.scale_factor <= math::SCALE_FACTOR_INT_MAX);
		u128::from(self.scale_factor).max(1)
	}
}

/// What realizing a deposit against the live accumulators produced.
#[cfg_attr(test, derive(PartialEq, Debug))]
pub struct Realized<Balance> {
	/// What is left of the deposit after every offset it lived through.
	pub compounded: Balance,
	/// The collateral it earned.
	pub collateral_gain: Balance,
	/// The stablecoin yield it earned.
	pub yield_gain: Balance,
}

/// What an offset did to `P`.
#[cfg_attr(test, derive(PartialEq, Debug))]
pub enum PUpdate {
	/// The leg survives with a smaller `P`. `scales_crossed` counts the rescales it took to keep
	/// `P` at or above `p_min`.
	Updated { new_p: FixedU128, scales_crossed: u32 },
	/// The offset consumed the whole leg. The caller starts a new epoch.
	Depleted,
}

/// What one account holds in one market.
///
/// `active_deposit` is the amount as of the last realization, so it can lag the live `P`. Every
/// operation on the row realizes it first, then applies its own change.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
pub struct Deposit<Balance> {
	/// Stablecoin that absorbs offsets and earns yield.
	pub active_deposit: Balance,

	/// Where `active_deposit` stood when it was last realized.
	pub snapshot: DepositSnapshot,

	/// Collateral earned and not yet paid out. Realizations add to it; a claim empties it.
	pub claimable_collateral: Balance,
	/// Stablecoin yield earned and not yet paid out or compounded.
	pub claimable_yield: Balance,

	/// Stablecoin still waiting out the entry delay.
	pub pending_deposit: Option<PendingDeposit<Balance>>,
	/// An announced Safety-Mode withdrawal.
	pub withdrawal_request: Option<WithdrawalRequest<Balance>>,
}

impl<Balance: Zero> Deposit<Balance> {
	/// An empty row at the given snapshot. Realizing it changes nothing.
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

	/// Returns whether the row holds no user value and can be removed.
	///
	/// A leftover `withdrawal_request` does not count. With nothing left to withdraw it is dead
	/// state, and it goes with the row.
	pub fn is_empty(&self) -> bool {
		self.active_deposit.is_zero() &&
			self.claimable_collateral.is_zero() &&
			self.claimable_yield.is_zero() &&
			self.pending_deposit.is_none()
	}
}

/// Stablecoin supplied recently and still waiting out the entry delay.
///
/// It is not part of `total_active_deposits` and it earns no yield. It is still at risk: an offset
/// that the active pool cannot absorb takes from every pending deposit, in proportion to its size.
/// The pending leg tracks that loss through its own accumulators, exactly as the active leg does.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
pub struct PendingDeposit<Balance> {
	/// The amount as of the last realization, so it can lag the live pending `P`.
	pub amount: Balance,
	/// When the amount may join the active pool.
	pub activatable_at: Millis,
	/// Where the amount stood on the pending leg when it was last realized. Its `g_yield` is
	/// always zero; the type is shared with the active leg so that one implementation serves both.
	pub snapshot: DepositSnapshot,
}

/// An announced withdrawal, waiting out the Safety-Mode delay.
///
/// Only Safety Mode records one. A Normal-Mode request withdraws instead, and a Normal-Mode
/// withdrawal ignores any request left over from an earlier period of stress.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
#[cfg_attr(test, derive(Clone, PartialEq, Debug))]
pub struct WithdrawalRequest<Balance> {
	/// How much active stablecoin the request still covers.
	pub amount: Balance,
	/// When the withdrawal may run.
	pub executable_at: Millis,
}

/// What a pool holds, and where both of its legs stand.
///
/// The pending leg has no queue. An offset that reaches it takes from every pending deposit in
/// proportion to its size, which a second pair of accumulators tracks in constant time.
///
/// The totals carry the rounding remainders of every payout, so the pool never reports owing more
/// than its account holds. Separate surplus fields would be zero by construction.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
#[cfg_attr(test, derive(PartialEq, Debug))]
pub struct PoolState<Balance> {
	/// Stablecoin in the active pool, as the pool tracks it.
	pub total_active_deposits: Balance,
	/// Stablecoin in pending deposits, as the pool tracks it.
	pub total_pending_deposits: Balance,
	/// The live coordinates of [`Leg::Active`].
	pub coords: Accumulators,
	/// The live coordinates of [`Leg::Pending`].
	pub pending_coords: Accumulators,
	/// Collateral the pool holds for depositors who have not claimed it.
	pub total_collateral_gains_unclaimed: Balance,
	/// Stablecoin yield the pool holds for depositors who have not claimed or compounded it.
	pub total_yield_unclaimed: Balance,
}

impl<Balance> PoolState<Balance> {
	/// The live coordinates of `leg`.
	pub fn coords(&self, leg: Leg) -> &Accumulators {
		match leg {
			Leg::Active => &self.coords,
			Leg::Pending => &self.pending_coords,
		}
	}

	/// The live coordinates of `leg`, for update.
	pub fn coords_mut(&mut self, leg: Leg) -> &mut Accumulators {
		match leg {
			Leg::Active => &mut self.coords,
			Leg::Pending => &mut self.pending_coords,
		}
	}

	/// The deposit total of `leg`, for update.
	pub fn total_mut(&mut self, leg: Leg) -> &mut Balance {
		match leg {
			Leg::Active => &mut self.total_active_deposits,
			Leg::Pending => &mut self.total_pending_deposits,
		}
	}

	/// A snapshot at the live coordinates of `leg`. Pass that leg's live sums row as `sums`.
	pub fn snapshot(&self, leg: Leg, sums: &PoolSums) -> DepositSnapshot {
		DepositSnapshot { coords: *self.coords(leg), sums: *sums }
	}
}

impl<Balance: Copy> PoolState<Balance> {
	/// The deposit total of `leg`.
	pub fn total(&self, leg: Leg) -> Balance {
		match leg {
			Leg::Active => self.total_active_deposits,
			Leg::Pending => self.total_pending_deposits,
		}
	}
}

impl<Balance: Zero> PoolState<Balance> {
	/// The state a market starts from: an empty pool at `P = 1` on both legs.
	pub fn fresh() -> Self {
		Self {
			total_active_deposits: Balance::zero(),
			total_pending_deposits: Balance::zero(),
			coords: Accumulators::fresh(),
			pending_coords: Accumulators::fresh(),
			total_collateral_gains_unclaimed: Balance::zero(),
			total_yield_unclaimed: Balance::zero(),
		}
	}
}

impl<Balance: FixedPointOperand> PoolState<Balance> {
	/// How much `S` or `G` grows when `distributed` is shared out over the active pool.
	///
	/// Returns `None` when the active pool is empty or the product overflows.
	pub fn delta_sum(&self, distributed: Balance) -> Option<FixedU128> {
		math::delta_sum(distributed, self.coords.p, self.total_active_deposits)
	}
}

/// What governance sets per market.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct StabilityPoolConfig<Balance> {
	/// The smallest deposit the pool accepts. Keeps rows worth more than they cost to store.
	pub minimum_deposit: Balance,
	/// The smallest active pool an offset may leave behind. An offset either respects this floor
	/// or empties the pool.
	pub minimum_active_pool_balance: Balance,
	/// How long a new deposit waits before it joins the active pool.
	pub entry_delay: Millis,
	/// How long a Safety-Mode withdrawal waits after its announcement.
	pub safety_withdrawal_delay: Millis,
	/// How far `P` may fall before a rescale. Cannot change after registration.
	pub precision: PoolPrecision,
	/// The share of market yield the active pool takes. The rest goes to the fee destination of
	/// the vault engine.
	pub yield_share: Permill,
}

impl<Balance: Zero> StabilityPoolConfig<Balance> {
	/// Returns whether these parameters can be stored.
	///
	/// A zero `minimum_deposit` admits rows too small to be worth their storage, and a zero
	/// `minimum_active_pool_balance` removes the floor that keeps `P` precise. The precision
	/// bounds are those of [`PoolPrecision::is_valid`].
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

/// The pool of one market, parameters and state together.
///
/// The two halves are created and removed as one, so neither can exist without the other.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
pub struct StabilityPool<Balance> {
	/// What governance sets. Only `set_stability_pool_config` changes it.
	pub config: StabilityPoolConfig<Balance>,
	/// What the pool holds. Every pool operation rewrites it.
	pub state: PoolState<Balance>,
}

impl<Balance: Zero> StabilityPool<Balance> {
	/// The record a market starts from: the given parameters over an empty [`PoolState`].
	pub fn fresh(config: StabilityPoolConfig<Balance>) -> Self {
		Self { config, state: PoolState::fresh() }
	}
}

/// Which stablecoin paid for a recovery offset.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub enum RecoveryOffsetSource {
	/// The active pool paid. Its depositors take the collateral through `S`.
	ActivePool,
	/// An incoming deposit paid. The depositor takes the collateral directly.
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
		config.precision.scale_factor = 999;
		assert!(!config.is_valid());

		// Above the u128 overflow guard (1e10).
		let mut config = valid_config();
		config.precision.scale_factor = 10_000_000_001;
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

		deposit.pending_deposit = Some(PendingDeposit {
			amount: 1,
			activatable_at: 6_000,
			snapshot: DepositSnapshot::fresh(),
		});
		assert!(!deposit.is_empty());

		deposit.pending_deposit = None;
		deposit.claimable_yield = 1;
		assert!(!deposit.is_empty());
	}
}
