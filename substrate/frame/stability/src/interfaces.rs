//! Cross-pallet yield and liquidation contracts of the stability pool.
//!
//! `pusd-primitives` defines these contracts so callers depend on protocol behavior, not on this
//! pallet's storage model.

use crate::{
	pallet::{
		BalanceOf, CollateralCreditOf, CollateralIdOf, Config, Pallet, Pools, StabilityPoolOf,
		StableCreditOf, StableIdOf,
	},
	types::Leg,
};
use frame::{
	prelude::*,
	traits::{tokens::Preservation, Time},
};
use pusd_primitives::{OffsetLegs, OnBranchYield, StabilityPoolInspect, StabilityPoolOffset};

/// Allocates `floor(yield_share * credit)` to active depositors and returns the remainder.
///
/// Yield routing must not fail the operation that produced the yield. Therefore, an unavailable
/// pool returns all credit to the caller.
impl<T: Config> OnBranchYield<CollateralIdOf<T>, StableCreditOf<T>> for Pallet<T> {
	fn distribute_yield(
		collateral_id: &CollateralIdOf<T>,
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
		let leftover = Self::do_distribute_yield(collateral_id, stable_id, pool, taken);
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
	) -> Option<StabilityPoolOf<T>> {
		Self::ensure_not_frozen(collateral_id, stable_id).ok()?;
		Pools::<T>::get(collateral_id, stable_id)
	}

	/// Returns the offset pool with all due cohort activations applied in memory.
	///
	/// This simulation gives inspection and settlement the same capital classification without a
	/// storage change.
	fn offset_pool_advanced(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Option<StabilityPoolOf<T>> {
		let mut pool = Self::offset_pool(collateral_id, stable_id)?;
		Self::roll_due_cohorts(&mut pool, T::TimeProvider::now()).ok()?;
		Some(pool)
	}

	/// Returns the debt that `leg` can cancel now, with all due cohorts activated in memory.
	///
	/// A missing or frozen market has zero capacity.
	fn reducible(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		leg: Leg,
		max_debt: BalanceOf<T>,
		reserved: BalanceOf<T>,
	) -> BalanceOf<T> {
		let Some(pool) = Self::offset_pool_advanced(collateral_id, stable_id) else {
			return BalanceOf::<T>::zero();
		};
		let pool_account = Self::pool_account(collateral_id, stable_id);
		Self::size_offset(&pool, stable_id, &pool_account, leg, max_debt, reserved)
			.map_or_else(BalanceOf::<T>::zero, |(debt, _)| debt)
	}

	/// Confirms that `leg` still has the quoted capacity for `requested` debt. A zero request
	/// reserves nothing.
	///
	/// A mismatch means that the quote is stale. The complete offset must then fail without value
	/// movement.
	fn reserve_leg(
		pool: &StabilityPoolOf<T>,
		stable_id: &StableIdOf<T>,
		pool_account: &T::AccountId,
		leg: Leg,
		requested: BalanceOf<T>,
		reserved: BalanceOf<T>,
	) -> Result<Option<OffsetReservation<BalanceOf<T>>>, DispatchError> {
		if requested.is_zero() {
			return Ok(None);
		}
		let (debt, preservation) =
			Self::size_offset(pool, stable_id, pool_account, leg, requested, reserved)
				.ok_or(crate::Error::<T>::OffsetSettlementFailed)?;
		ensure!(debt == requested, crate::Error::<T>::OffsetSettlementFailed);
		Ok(Some(OffsetReservation { debt, preservation }))
	}
}

impl<T: Config> StabilityPoolInspect<CollateralIdOf<T>, StableIdOf<T>, BalanceOf<T>> for Pallet<T> {
	fn reducible_active(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		max_debt: BalanceOf<T>,
	) -> BalanceOf<T> {
		Self::reducible(collateral_id, stable_id, Leg::Active, max_debt, BalanceOf::<T>::zero())
	}

	fn reducible_pending(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		max_debt: BalanceOf<T>,
		active_debt: BalanceOf<T>,
	) -> BalanceOf<T> {
		Self::reducible(collateral_id, stable_id, Leg::Pending, max_debt, active_debt)
	}
}

impl<T: Config>
	StabilityPoolOffset<CollateralIdOf<T>, StableIdOf<T>, BalanceOf<T>, CollateralCreditOf<T>>
	for Pallet<T>
{
	fn offset(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		debt: OffsetLegs<BalanceOf<T>>,
		collateral: OffsetLegs<CollateralCreditOf<T>>,
	) -> DispatchResult {
		// A leg that cancels no debt must carry no collateral. Anything else would give the pool
		// collateral for free and break the link between the two sides.
		if debt.active.is_zero() {
			ensure!(collateral.active.peek().is_zero(), crate::Error::<T>::OffsetSettlementFailed);
		}
		if debt.pending.is_zero() {
			ensure!(collateral.pending.peek().is_zero(), crate::Error::<T>::OffsetSettlementFailed);
			if debt.active.is_zero() {
				return Ok(());
			}
		}
		let mut pool = Self::offset_pool(collateral_id, stable_id)
			.ok_or(crate::Error::<T>::OffsetSettlementFailed)?;
		// The same advancement the read-only sizing simulated, committed for real: inspection and
		// settlement must agree on which capital is active.
		Self::advance_cohorts(collateral_id, stable_id, &mut pool, T::TimeProvider::now())?;
		let pool_account = Self::pool_account(collateral_id, stable_id);

		// Both legs re-size against the untouched pool, in the order the caller inspected them:
		// active first, pending reserved behind it. A caller whose readings went stale therefore
		// fails here, with nothing moved.
		let active = Self::reserve_leg(
			&pool,
			stable_id,
			&pool_account,
			Leg::Active,
			debt.active,
			BalanceOf::<T>::zero(),
		)?;
		let pending = Self::reserve_leg(
			&pool,
			stable_id,
			&pool_account,
			Leg::Pending,
			debt.pending,
			debt.active,
		)?;

		// Active settles first. The pending `Preservation` was sized against the combined limit,
		// so it only holds once the active part has left the pool account.
		if let Some(reservation) = active {
			Self::settle_offset(
				collateral_id,
				stable_id,
				&pool_account,
				Leg::Active,
				&mut pool,
				reservation,
				collateral.active,
			)?;
		}
		if let Some(reservation) = pending {
			Self::settle_offset(
				collateral_id,
				stable_id,
				&pool_account,
				Leg::Pending,
				&mut pool,
				reservation,
				collateral.pending,
			)?;
		}
		Pools::<T>::insert(collateral_id, stable_id, pool);
		Ok(())
	}
}
