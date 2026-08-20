//! Storage types and accounting invariants of the stability pool.

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::{
	arithmetic::{FixedPointNumber, FixedPointOperand, FixedU128, One, Permill, Saturating, Zero},
	prelude::{BoundedVec, ConstU32},
};
use pusd_primitives::Millis;
use scale_info::TypeInfo;

use crate::math;

/// Loss coordinates of one pool leg.
///
/// A deposit stores these coordinates in its snapshot. Their change measures the deposit loss
/// without an update to every deposit row.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy)]
#[cfg_attr(test, derive(PartialEq, Debug))]
pub struct Accumulators {
	/// The fraction of a deposit that survives every offset applied so far.
	pub p: FixedU128,
	/// Number of offsets that emptied the leg. Each new epoch starts with `p` equal to one.
	pub epoch: u32,
	/// Number of rescale operations that kept a small `p` within the supported precision.
	pub scale: u32,
}

impl Accumulators {
	/// Returns loss-free coordinates with `P = 1`, epoch zero, and scale zero.
	///
	/// A new pool leg uses these coordinates so its first deposit has no prior loss.
	pub fn fresh() -> Self {
		Self { p: FixedU128::one(), epoch: 0, scale: 0 }
	}
}

/// Accounting domain of a pool operation.
///
/// Active and pending capital use separate loss coordinates because they have different risk
/// priority. Only active capital receives yield.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy)]
pub enum Leg {
	/// Deposits that absorb offsets and earn yield.
	Active,
	/// Deposits within the entry delay. They earn no yield and serve as the final pool backstop.
	Pending,
}

impl Leg {
	/// Both legs in risk-priority order.
	pub const ALL: [Self; 2] = [Self::Active, Self::Pending];
}

/// Gains per deposit unit at one `(epoch, scale)` coordinate.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, Default)]
#[cfg_attr(test, derive(PartialEq, Debug))]
pub struct PoolSums {
	/// `S`: collateral paid per unit of deposit.
	pub s_collateral: FixedU128,
	/// `G`: stablecoin yield paid per unit of deposit. Always zero on [`Leg::Pending`].
	pub g_yield: FixedU128,
}

/// Accounting position of a deposit at its last settlement.
///
/// The difference between this snapshot and the current accumulators gives the unsettled loss and
/// gains of the deposit.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy)]
pub struct DepositSnapshot {
	/// The accumulator coordinates at the time of the last realization.
	pub coords: Accumulators,
	/// The sums of the row that `coords` points at, at the same moment.
	pub sums: PoolSums,
}

impl DepositSnapshot {
	/// Returns a snapshot for a leg with no prior loss or gain.
	pub fn fresh() -> Self {
		Self { coords: Accumulators::fresh(), sums: PoolSums::default() }
	}
}

/// Stable identifier of one maturity cohort in one market.
///
/// Identifiers increase from zero, and the pallet does not reuse them. The identifier remains
/// valid if governance causes a cohort deadline to move later.
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
	PartialOrd,
	Ord,
	Debug,
)]
pub struct CohortId(pub u64);

/// Pending capital with one activation deadline.
///
/// The aggregate lets the pallet activate all members with bounded work. A
/// [`CohortCheckpoint`] preserves each member's later settlement.
///
/// The pool keeps at most two in [`PoolState::open_cohorts`].
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
#[cfg_attr(test, derive(PartialEq, Debug))]
pub struct OpenCohort<Balance> {
	/// Stable identifier referenced by member rows.
	pub id: CohortId,
	/// Time when the capital becomes active. A configuration change can move it later but not
	/// earlier.
	pub deadline: Millis,
	/// Deposit rows that reference this cohort.
	pub members: u32,
	/// Aggregate capital at `coords`, with upward rounding.
	///
	/// It is not less than the sum of the member claims. Thus, activation cannot create an
	/// underfunded active deposit. The rounding difference remains unowned in the pool totals.
	pub amount: Balance,
	/// Pending-leg coordinates of `amount`. A member snapshot cannot be ahead of these
	/// coordinates.
	pub coords: Accumulators,
}

/// Open maturity cohorts of one pool, ordered by deadline.
///
/// Deadline grouping permits at most two open cohorts. This bound keeps activation work
/// independent of the depositor count.
pub type OpenCohorts<Balance> = BoundedVec<OpenCohort<Balance>, ConstU32<2>>;

impl<Balance: FixedPointOperand> OpenCohort<Balance> {
	/// Returns an empty cohort at `deadline` and the current pending coordinates.
	pub fn fresh(id: CohortId, deadline: Millis, coords: Accumulators) -> Self {
		Self { id, deadline, members: 0, amount: Balance::zero(), coords }
	}

	/// Revalues the aggregate at the `live` pending coordinates.
	///
	/// Upward rounding keeps the aggregate sufficient for all downward-rounded member claims. The
	/// coordinate update also keeps each member snapshot at or behind the aggregate.
	pub fn revalue(&mut self, live: &Accumulators, sf_int: u128) {
		self.amount = math::compound_ceil(self.amount, &self.coords, live, sf_int);
		self.coords = *live;
	}
}

/// Accounting boundary of an activated cohort.
///
/// The boundary ends pending exposure at `pending_end` and starts active exposure at
/// `active_start`. This split preserves both phases until each member row settles.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
pub struct CohortCheckpoint<Balance> {
	/// End of each member's pending phase.
	pub pending_end: DepositSnapshot,
	/// Start of each member's active phase.
	pub active_start: DepositSnapshot,
	/// Cohort value at activation, before the pool-total clamp.
	///
	/// This value is an upper bound for all member claims and keeps their sum at or below
	/// activated capital.
	pub activated: Balance,
	/// Deposit rows that still require this boundary. The last settlement removes the checkpoint.
	pub members: u32,
}

/// Accumulator rows required to settle one deposit.
///
/// A deposit can lag by `math::SCALE_SPAN` scale boundaries. Settlement includes each applicable
/// row so the depositor keeps all supported gains.
pub struct SumsWindow {
	/// The row at the coordinates of the snapshot.
	pub snap: PoolSums,
	/// The rows at the scales that follow, in order. An absent row reads as zero.
	pub ahead: [PoolSums; math::SCALE_SPAN as usize],
}

/// Precision parameters for the product accumulator `P`.
///
/// These parameters cannot change after registration. Stored snapshots depend on the original
/// `scale_factor`, and a different value would produce incorrect deposit amounts.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct PoolPrecision {
	/// The value of `P` that triggers a rescale.
	pub p_min: FixedU128,
	/// Factor that a rescale applies to `P`.
	///
	/// [`PoolPrecision::scale_factor`] converts this value for accumulator arithmetic and protects
	/// division from corrupt zero values.
	pub scale_factor: u64,
}

impl PoolPrecision {
	/// Returns `true` when the parameters keep accumulator arithmetic within its limits.
	///
	/// `scale_factor` must be between `math::SCALE_FACTOR_INT_MIN` and
	/// `math::SCALE_FACTOR_INT_MAX`. A rescale must also leave `P` at or below one, which requires
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

	/// Returns `scale_factor` as a `u128` for accumulator arithmetic.
	///
	/// The result is at least one, which prevents division by zero after corrupt storage input.
	pub fn scale_factor(&self) -> u128 {
		debug_assert!(self.scale_factor >= math::SCALE_FACTOR_INT_MIN);
		debug_assert!(self.scale_factor <= math::SCALE_FACTOR_INT_MAX);
		u128::from(self.scale_factor).max(1)
	}
}

/// Settlement result for a deposit at the current accumulators.
#[cfg_attr(test, derive(PartialEq, Debug))]
pub struct Realized<Balance> {
	/// Deposit principal that remains after all applicable offsets.
	pub compounded: Balance,
	/// The collateral it earned.
	pub collateral_gain: Balance,
	/// The stablecoin yield it earned.
	pub yield_gain: Balance,
}

/// Effect of an offset on `P`.
#[cfg_attr(test, derive(PartialEq, Debug))]
pub enum PUpdate {
	/// The leg remains with a smaller `P`. `scales_crossed` counts the rescale operations that
	/// keep `P` at or above `p_min`.
	Updated { new_p: FixedU128, scales_crossed: u32 },
	/// The offset consumed the leg. A new epoch must start with `P` equal to one.
	Depleted,
}

/// Position of one account in one market.
///
/// `active_deposit` is the amount at the last settlement. Each row operation must first apply the
/// current loss and gain accumulators.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
pub struct Deposit<Balance> {
	/// Stablecoin that absorbs offsets and earns yield.
	pub active_deposit: Balance,

	/// Accounting position of `active_deposit` at its last settlement.
	pub snapshot: DepositSnapshot,

	/// Collateral earned and not yet paid.
	pub claimable_collateral: Balance,
	/// Stablecoin yield earned and not yet paid out or compounded.
	pub claimable_yield: Balance,

	/// Stablecoin within the entry delay.
	pub pending_deposit: Option<PendingDeposit<Balance>>,
	/// A Safety-mode withdrawal request.
	pub withdrawal_request: Option<WithdrawalRequest<Balance>>,
}

impl<Balance: Zero> Deposit<Balance> {
	/// Returns an empty position at the specified snapshot.
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

	/// Returns `true` when the row has no user value and the pallet can remove it.
	///
	/// A withdrawal request does not keep an empty row. Without an active deposit, the request has
	/// no value and cannot authorize a payment.
	pub fn is_empty(&self) -> bool {
		self.active_deposit.is_zero() &&
			self.claimable_collateral.is_zero() &&
			self.claimable_yield.is_zero() &&
			self.pending_deposit.is_none()
	}
}

/// Stablecoin within the entry delay.
///
/// Pending capital earns no yield and is not part of `total_active_deposits`. It remains exposed as
/// the final pool backstop, in proportion to each pending deposit.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
pub struct PendingDeposit<Balance> {
	/// Amount at the last settlement. It can lag the current pending `P`.
	pub amount: Balance,
	/// Maturity cohort of the amount. It identifies an [`OpenCohort`] before activation and a
	/// [`CohortCheckpoint`] after activation.
	pub cohort: CohortId,
	/// Pending-leg position at the last settlement. Its `g_yield` is always zero because pending
	/// capital earns no yield.
	pub snapshot: DepositSnapshot,
}

/// Withdrawal request subject to the Safety-mode delay.
///
/// Only Safety mode stores this request. Normal mode permits a withdrawal without one.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
#[cfg_attr(test, derive(Clone, PartialEq, Debug))]
pub struct WithdrawalRequest<Balance> {
	/// Active stablecoin amount that the request still authorizes.
	pub amount: Balance,
	/// Earliest time of the withdrawal.
	pub executable_at: Millis,
}

/// Aggregate balances and accumulator positions of one pool.
///
/// Separate active and pending accumulators preserve their risk priority. A pending offset applies
/// proportionally to all pending deposits without iteration over deposit rows.
///
/// The totals include downward-rounding remainders. Thus, user positions cannot exceed the pool
/// totals, and the totals remain equal to pool custody.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
#[cfg_attr(test, derive(PartialEq, Debug))]
pub struct PoolState<Balance> {
	/// Stablecoin tracked for the active pool.
	pub total_active_deposits: Balance,
	/// Stablecoin tracked for pending deposits.
	pub total_pending_deposits: Balance,
	/// The live coordinates of [`Leg::Active`].
	pub coords: Accumulators,
	/// The live coordinates of [`Leg::Pending`].
	pub pending_coords: Accumulators,
	/// Collateral tracked for unclaimed gains.
	pub total_collateral_gains_unclaimed: Balance,
	/// Stablecoin tracked for yield not yet claimed or compounded.
	pub total_yield_unclaimed: Balance,
	/// Open cohorts ordered from the earliest deadline to the latest deadline.
	pub open_cohorts: OpenCohorts<Balance>,
	/// Identifier reserved for the next cohort.
	pub next_cohort_id: CohortId,
}

impl<Balance> PoolState<Balance> {
	/// Returns the open cohort with `id`.
	pub fn cohort(&self, id: CohortId) -> Option<&OpenCohort<Balance>> {
		self.open_cohorts.iter().find(|cohort| cohort.id == id)
	}

	/// Returns the open cohort with `id` for mutation.
	pub fn cohort_mut(&mut self, id: CohortId) -> Option<&mut OpenCohort<Balance>> {
		self.open_cohorts.iter_mut().find(|cohort| cohort.id == id)
	}

	/// Removes the open cohort with `id`.
	pub fn remove_cohort(&mut self, id: CohortId) {
		if let Some(index) = self.open_cohorts.iter().position(|cohort| cohort.id == id) {
			self.open_cohorts.remove(index);
		}
	}

	/// Returns the current coordinates of `leg`.
	pub fn coords(&self, leg: Leg) -> &Accumulators {
		match leg {
			Leg::Active => &self.coords,
			Leg::Pending => &self.pending_coords,
		}
	}

	/// Returns the current coordinates of `leg` for mutation.
	pub fn coords_mut(&mut self, leg: Leg) -> &mut Accumulators {
		match leg {
			Leg::Active => &mut self.coords,
			Leg::Pending => &mut self.pending_coords,
		}
	}

	/// Returns the deposit total of `leg` for mutation.
	pub fn total_mut(&mut self, leg: Leg) -> &mut Balance {
		match leg {
			Leg::Active => &mut self.total_active_deposits,
			Leg::Pending => &mut self.total_pending_deposits,
		}
	}

	/// Returns a snapshot of `leg` with its current coordinates and `sums`.
	pub fn snapshot(&self, leg: Leg, sums: &PoolSums) -> DepositSnapshot {
		DepositSnapshot { coords: *self.coords(leg), sums: *sums }
	}
}

impl<Balance: Copy> PoolState<Balance> {
	/// Returns the deposit total of `leg`.
	pub fn total(&self, leg: Leg) -> Balance {
		match leg {
			Leg::Active => self.total_active_deposits,
			Leg::Pending => self.total_pending_deposits,
		}
	}
}

impl<Balance: Zero> PoolState<Balance> {
	/// Returns an empty pool state with `P = 1` on both legs.
	pub fn fresh() -> Self {
		Self {
			total_active_deposits: Balance::zero(),
			total_pending_deposits: Balance::zero(),
			coords: Accumulators::fresh(),
			pending_coords: Accumulators::fresh(),
			total_collateral_gains_unclaimed: Balance::zero(),
			total_yield_unclaimed: Balance::zero(),
			open_cohorts: Default::default(),
			next_cohort_id: CohortId(0),
		}
	}
}

impl<Balance: FixedPointOperand> PoolState<Balance> {
	/// Returns the increase of `S` or `G` for `distributed` over the active pool.
	///
	/// Returns `None` when the active pool is empty or the product overflows.
	pub fn delta_sum(&self, distributed: Balance) -> Option<FixedU128> {
		math::delta_sum(distributed, self.coords.p, self.total_active_deposits)
	}
}

/// Governance parameters of one market's stability pool.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct StabilityPoolConfig<Balance> {
	/// Smallest accepted deposit. This limit prevents positions with storage cost above their
	/// value.
	pub minimum_deposit: Balance,
	/// Smallest active balance that a partial offset can leave. A full offset can empty the pool.
	/// This limit protects the precision of `P`.
	pub minimum_active_pool_balance: Balance,
	/// Minimum time before new capital can join the active pool.
	pub entry_delay: Millis,
	/// Delay between a Safety-mode withdrawal request and its execution.
	pub safety_withdrawal_delay: Millis,
	/// Precision policy for `P`. It cannot change after registration.
	pub precision: PoolPrecision,
	/// Share of market yield for active depositors. The yield caller retains the remainder.
	pub yield_share: Permill,
}

impl<Balance: Zero> StabilityPoolConfig<Balance> {
	/// Returns `true` when the parameters satisfy all pool invariants.
	///
	/// `minimum_deposit` and `minimum_active_pool_balance` must be nonzero. The precision
	/// parameters must satisfy [`PoolPrecision::is_valid`].
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

/// Configuration and accounting state of one market's pool.
///
/// Both parts share one lifecycle so configuration cannot exist without its accounting state.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
pub struct StabilityPool<Balance> {
	/// Parameters controlled by the authorized update origin.
	pub config: StabilityPoolConfig<Balance>,
	/// Aggregate balances and accumulator positions of the pool.
	pub state: PoolState<Balance>,
}

impl<Balance: Zero> StabilityPool<Balance> {
	/// Returns a registered pool with the specified parameters and an empty [`PoolState`].
	pub fn fresh(config: StabilityPoolConfig<Balance>) -> Self {
		Self { config, state: PoolState::fresh() }
	}
}

/// Source of stablecoin for a recovery offset.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub enum RecoveryOffsetSource {
	/// The active pool paid, so its depositors receive the collateral through `S`.
	ActivePool,
	/// An incoming deposit paid, so that depositor receives the collateral directly.
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
			cohort: CohortId(0),
			snapshot: DepositSnapshot::fresh(),
		});
		assert!(!deposit.is_empty());

		deposit.pending_deposit = None;
		deposit.claimable_yield = 1;
		assert!(!deposit.is_empty());
	}
}
