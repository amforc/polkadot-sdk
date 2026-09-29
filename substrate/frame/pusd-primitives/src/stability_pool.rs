//! Stability Pool offset interfaces for liquidation settlement.

use crate::BranchSnapshot;
use frame::deps::{
	frame_support::{pallet_prelude::DispatchError, traits::TryDrop},
	sp_runtime::traits::Zero,
};

/// Per-leg values of a Stability Pool offset: debt amounts or collateral credits.
pub struct OffsetLegs<T> {
	/// Active-pool leg.
	pub active: T,
	/// Pending-deposit leg.
	pub pending: T,
}

/// Read-only offset limits of Stability Pool markets.
///
/// Valid amounts are not contiguous: full depletion is always valid, but a partial offset must
/// leave at least the pool minimum and the stablecoin account's minimum balance. So each limit
/// depends on the requested debt.
///
/// Read a market once with [`Self::quote`] and size every leg from it. A quote classifies pool
/// capital as of `branch.now`; a frozen branch has none. It is not a reservation: it holds while
/// the pool is unchanged, and [`StabilityPoolOffset::offset`] revalidates it.
pub trait StabilityPoolInspect<CollateralId, StableId, Balance> {
	/// Market state the leg limits read.
	type Quote;

	/// Reads the market under `branch`; `None` if it has no capacity.
	fn quote(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		branch: BranchSnapshot,
	) -> Option<Self::Quote>;

	/// Returns the active-pool debt that `quote` can cancel, up to `max_debt`.
	fn quote_active(quote: &Self::Quote, max_debt: Balance) -> Balance;

	/// Returns the pending-deposit debt that `quote` can cancel, up to `max_debt`, given the
	/// offset's `active_debt` (both legs burn from one custody account).
	fn quote_pending(quote: &Self::Quote, max_debt: Balance, active_debt: Balance) -> Balance;
}

/// Applies liquidation offsets to Stability Pool markets.
pub trait StabilityPoolOffset<CollateralId, StableId, Balance, CollateralCredit, StableCredit>:
	StabilityPoolInspect<CollateralId, StableId, Balance>
{
	/// Cancels exactly `debt` against pool deposits and pays `collateral` to each leg.
	///
	/// Fails unless, at the start, both debts are zero or [`Self::quote`] returns a `quote` with:
	///
	/// - `debt.active == Self::quote_active(&quote, debt.active)`
	/// - `debt.pending == Self::quote_pending(&quote, debt.pending, debt.active)`
	///
	/// Returns the cancelled stablecoin as a credit (`None` if both debts are zero). Dropping it
	/// burns it; a caller minting stablecoin in the same transaction may mint from it instead,
	/// netting the supply changes. The caller reduces vault debt by the same amounts.
	///
	/// Pass the `branch` the quotes were taken under so capital is classified the same way. Pool
	/// collateral custody is prepared at branch registration.
	///
	/// Create the credits and call this in one storage transaction, rolling back on failure: the
	/// credits are consumed even on error.
	fn offset(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		branch: BranchSnapshot,
		debt: OffsetLegs<Balance>,
		collateral: OffsetLegs<CollateralCredit>,
	) -> Result<Option<StableCredit>, DispatchError>;

	/// Sets the market's entry delay to `entry_delay` and queues `amount` of freshly minted
	/// stablecoin as benchmark `depositor`'s pending deposit.
	///
	/// Returns the activation deadline: the capital is pending before it and active from it. Lets
	/// liquidation benchmarks seed both legs and a due cohort.
	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_queue_deposit(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		depositor: u32,
		entry_delay: crate::Millis,
		amount: Balance,
	) -> Result<crate::Millis, DispatchError>;
}

/// No-op pool for runtimes without one: nothing is reducible, and an offset succeeds only when
/// all debts and credits are zero.
impl<CollateralId, StableId, Balance: Zero> StabilityPoolInspect<CollateralId, StableId, Balance>
	for ()
{
	type Quote = ();

	fn quote(_: &CollateralId, _: &StableId, _: BranchSnapshot) -> Option<()> {
		None
	}

	fn quote_active(_: &(), _: Balance) -> Balance {
		Balance::zero()
	}

	fn quote_pending(_: &(), _: Balance, _: Balance) -> Balance {
		Balance::zero()
	}
}

impl<CollateralId, StableId, Balance: Zero, CollateralCredit: TryDrop, StableCredit>
	StabilityPoolOffset<CollateralId, StableId, Balance, CollateralCredit, StableCredit> for ()
{
	fn offset(
		_: &CollateralId,
		_: &StableId,
		_: BranchSnapshot,
		debt: OffsetLegs<Balance>,
		collateral: OffsetLegs<CollateralCredit>,
	) -> Result<Option<StableCredit>, DispatchError> {
		let debt_is_zero = debt.active.is_zero() && debt.pending.is_zero();
		// `TryDrop` succeeds only on zero credits, so a nonzero credit cannot vanish here.
		let active_is_zero = collateral.active.try_drop().is_ok();
		let pending_is_zero = collateral.pending.try_drop().is_ok();
		if debt_is_zero && active_is_zero && pending_is_zero {
			Ok(None)
		} else {
			Err(DispatchError::Other("no Stability Pool to offset against"))
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_queue_deposit(
		_: &CollateralId,
		_: &StableId,
		_: u32,
		_: crate::Millis,
		_: Balance,
	) -> Result<crate::Millis, DispatchError> {
		Err(DispatchError::Other("no stability pool to seed"))
	}
}
