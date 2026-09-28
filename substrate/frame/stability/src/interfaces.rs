//! Cross-pallet yield and liquidation contracts of the stability pool.
//!
//! `pusd-primitives` defines these contracts so callers depend on protocol behavior, not on this
//! pallet's storage model.

use crate::{
	dispatchable_impls::PoolCustody,
	pallet::{
		BalanceOf, CollateralCreditOf, CollateralIdOf, Config, Pallet, Pools, StabilityPoolOf,
		StableCreditOf, StableIdOf,
	},
	types::{Leg, LegCoords},
};
use frame::{prelude::*, traits::tokens::Preservation};
use pusd_primitives::{
	BranchMode, BranchSnapshot, OffsetLegs, OnBranchYield, StabilityPoolInspect,
	StabilityPoolOffset,
};

/// Allocates `floor(yield_share * credit)` to active depositors and returns the remainder.
///
/// Yield routing must not fail the operation that produced the yield. Therefore, an unavailable
/// pool returns all credit to the caller.
impl<T: Config> OnBranchYield<CollateralIdOf<T>, StableCreditOf<T>> for Pallet<T> {
	fn distribute_yield(
		collateral_id: &CollateralIdOf<T>,
		branch: BranchSnapshot,
		credit: StableCreditOf<T>,
	) -> StableCreditOf<T> {
		// The asset of the credit names the market; an unregistered pair has no pool row, and the
		// credit comes back whole.
		let stable_id = &credit.asset();
		let Some(pool) = Pools::<T>::get(collateral_id, stable_id) else {
			return credit;
		};
		let take = pool.config.yield_share.mul_floor(credit.peek());
		if take.is_zero() {
			return credit;
		}
		let (taken, mut remainder) = credit.split(take);
		let leftover = Self::do_distribute_yield(collateral_id, stable_id, branch, pool, taken);
		if let Err(leftover) = remainder.subsume(leftover) {
			// Both halves came from one credit, so they cannot disagree. Burning the leftover
			// keeps issuance on the conservative side.
			debug_assert!(false, "yield credit halves diverged");
			drop(leftover);
		}
		remainder
	}
}

/// Exact debt amount of one offset leg and its validated asset-preservation rule.
#[derive(Clone, Copy)]
pub(crate) struct OffsetReservation<Balance> {
	pub(crate) debt: Balance,
	pub(crate) preservation: Preservation,
}

impl<T: Config> Pallet<T> {
	/// Returns the pool available for an offset.
	///
	/// A missing or frozen market returns `None`. This result gives zero capacity and prevents
	/// settlement.
	fn offset_pool(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		mode: BranchMode,
	) -> Option<StabilityPoolOf<T>> {
		match mode {
			BranchMode::Normal | BranchMode::Safety => Pools::<T>::get(collateral_id, stable_id),
			BranchMode::Frozen => None,
		}
	}

	/// Returns the offset pool with all due cohort activations applied in memory.
	///
	/// This simulation gives inspection and settlement the same capital classification without a
	/// storage change.
	fn offset_pool_advanced(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		branch: BranchSnapshot,
	) -> Option<StabilityPoolOf<T>> {
		let mut pool = Self::offset_pool(collateral_id, stable_id, branch.mode)?;
		Self::roll_due_cohorts(&mut pool, branch.now).ok()?;
		Some(pool)
	}

	/// Confirms that `leg` still has the quoted capacity for `requested` debt. A zero request
	/// reserves nothing.
	///
	/// A mismatch means that the quote is stale. The complete offset must then fail without value
	/// movement.
	fn reserve_leg(
		pool: &StabilityPoolOf<T>,
		custody: &PoolCustody<T>,
		leg: Leg,
		requested: BalanceOf<T>,
		reserved: BalanceOf<T>,
	) -> Result<Option<OffsetReservation<BalanceOf<T>>>, DispatchError> {
		if requested.is_zero() {
			return Ok(None);
		}
		let (debt, preservation) = Self::size_offset(pool, custody, leg, requested, reserved)
			.ok_or(crate::Error::<T>::OffsetSettlementFailed)?;
		ensure!(debt == requested, crate::Error::<T>::OffsetSettlementFailed);
		Ok(Some(OffsetReservation { debt, preservation }))
	}
}

/// One market as its offset quotes read it: the pool with due cohorts activated in memory and its
/// stablecoin custody.
pub struct OffsetQuote<T: Config> {
	pool: StabilityPoolOf<T>,
	custody: PoolCustody<T>,
}

impl<T: Config> StabilityPoolInspect<CollateralIdOf<T>, StableIdOf<T>, BalanceOf<T>> for Pallet<T> {
	type Quote = OffsetQuote<T>;

	fn quote(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		branch: BranchSnapshot,
	) -> Option<OffsetQuote<T>> {
		let pool = Self::offset_pool_advanced(collateral_id, stable_id, branch)?;
		let pool_account = Self::pool_account(collateral_id, stable_id);
		Some(OffsetQuote { pool, custody: PoolCustody::read(stable_id, pool_account) })
	}

	fn quote_active(quote: &OffsetQuote<T>, max_debt: BalanceOf<T>) -> BalanceOf<T> {
		let reserved = BalanceOf::<T>::zero();
		Self::size_offset(&quote.pool, &quote.custody, Leg::Active, max_debt, reserved)
			.map_or_else(BalanceOf::<T>::zero, |(debt, _)| debt)
	}

	fn quote_pending(
		quote: &OffsetQuote<T>,
		max_debt: BalanceOf<T>,
		active_debt: BalanceOf<T>,
	) -> BalanceOf<T> {
		Self::size_offset(&quote.pool, &quote.custody, Leg::Pending, max_debt, active_debt)
			.map_or_else(BalanceOf::<T>::zero, |(debt, _)| debt)
	}
}

impl<T: Config>
	StabilityPoolOffset<
		CollateralIdOf<T>,
		StableIdOf<T>,
		BalanceOf<T>,
		CollateralCreditOf<T>,
		StableCreditOf<T>,
	> for Pallet<T>
{
	fn offset(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		branch: BranchSnapshot,
		debt: OffsetLegs<BalanceOf<T>>,
		collateral: OffsetLegs<CollateralCreditOf<T>>,
	) -> Result<Option<StableCreditOf<T>>, DispatchError> {
		// A leg that cancels no debt must carry no collateral. Anything else would give the pool
		// collateral for free and break the link between the two sides.
		if debt.active.is_zero() {
			ensure!(collateral.active.peek().is_zero(), crate::Error::<T>::OffsetSettlementFailed);
		}
		if debt.pending.is_zero() {
			ensure!(collateral.pending.peek().is_zero(), crate::Error::<T>::OffsetSettlementFailed);
			if debt.active.is_zero() {
				return Ok(None);
			}
		}
		let mut pool = Self::offset_pool(collateral_id, stable_id, branch.mode)
			.ok_or(crate::Error::<T>::OffsetSettlementFailed)?;
		// The same advancement the read-only sizing simulated, committed for real: inspection and
		// settlement must agree on which capital is active.
		Self::advance_cohorts(collateral_id, stable_id, &mut pool, branch)?;
		// One custody read serves both legs: nothing moves the pool's stablecoin before the
		// single withdrawal below.
		let custody =
			PoolCustody::<T>::read(stable_id, Self::pool_account(collateral_id, stable_id));

		// Both legs re-size against the untouched pool, in the order the caller inspected them:
		// active first, pending reserved behind it. A caller whose readings went stale therefore
		// fails here, with nothing moved.
		let active =
			Self::reserve_leg(&pool, &custody, Leg::Active, debt.active, BalanceOf::<T>::zero())?;
		let pending = Self::reserve_leg(&pool, &custody, Leg::Pending, debt.pending, debt.active)?;

		// Each leg records its own accounting, then the value moves once for both: one stablecoin
		// withdrawal of the summed debt and one collateral deposit of the merged credits. The
		// pending `Preservation` was sized behind the active debt, so it already covers the sum.
		let active = Self::record_offset(
			collateral_id,
			stable_id,
			Leg::Active,
			&mut pool,
			active,
			collateral.active,
		)?;
		let pending = Self::record_offset(
			collateral_id,
			stable_id,
			Leg::Pending,
			&mut pool,
			pending,
			collateral.pending,
		)?;
		let leg_coords = |taken: bool, leg| {
			let coords = pool.state.coords(leg);
			taken.then_some(LegCoords { epoch: coords.epoch, scale: coords.scale })
		};
		Self::deposit_event(crate::Event::OffsetApplied {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			active: leg_coords(active.is_some(), Leg::Active),
			pending: leg_coords(pending.is_some(), Leg::Pending),
		});
		let (reservation, collateral) = Self::merge_offset_movement(active, pending)?;
		let burned =
			Self::settle_reservation_exact(stable_id, custody.account(), reservation, collateral)?;
		Pools::<T>::insert(collateral_id, stable_id, pool);
		Ok(Some(burned))
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_queue_deposit(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		depositor: u32,
		entry_delay: pusd_primitives::Millis,
		amount: BalanceOf<T>,
	) -> Result<pusd_primitives::Millis, DispatchError> {
		use frame::traits::fungibles::Mutate as FungiblesMutate;
		assert!(entry_delay > 0, "a zero entry delay queues nothing");
		assert!(!amount.is_zero());

		// The benchmark registration config has no entry delay; a pending leg needs one.
		Pools::<T>::try_mutate(collateral_id, stable_id, |pool| {
			let pool = pool.as_mut().ok_or(crate::Error::<T>::PoolNotRegistered)?;
			pool.config.entry_delay = entry_delay;
			Ok::<_, DispatchError>(())
		})?;
		let who: T::AccountId = frame::benchmarking::prelude::account("sp_depositor", depositor, 0);
		if frame_system::Pallet::<T>::providers(&who) == 0 {
			frame_system::Pallet::<T>::inc_providers(&who);
		}
		T::StableAssets::mint_into(stable_id.clone(), &who, amount)?;
		Self::deposit(
			frame_system::RawOrigin::Signed(who.clone()).into(),
			collateral_id.clone(),
			stable_id.clone(),
			amount,
		)?;

		let cohort = crate::pallet::Deposits::<T>::get((collateral_id, stable_id, &who))
			.and_then(|deposit| deposit.pending_deposit)
			.map(|pending| pending.cohort)
			.ok_or(DispatchError::Other("benchmark deposit did not queue"))?;
		let deadline = Pools::<T>::get(collateral_id, stable_id)
			.and_then(|pool| pool.state.cohort(cohort).map(|open| open.deadline))
			.ok_or(DispatchError::Corruption)?;
		assert!(deadline > <T::TimeProvider as frame::traits::Time>::now());
		Ok(deadline)
	}
}
