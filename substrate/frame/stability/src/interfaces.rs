//! What sibling pallets drive on the pool.
//!
//! The vault engine routes yield here, and the liquidation engine cancels debt here. Both
//! surfaces are defined in `pusd-primitives`, so neither pallet needs to know the other.

use crate::{
	pallet::{
		BalanceOf, CollateralCreditOf, CollateralIdOf, Config, Pallet, Pools, StabilityPoolOf,
		StableCreditOf, StableIdOf,
	},
	types::Leg,
};
use frame::{deps::frame_support::require_transactional, prelude::*, traits::tokens::Preservation};
use pusd_primitives::{OffsetLegs, OnBranchYield, StabilityPoolInspect, StabilityPoolOffset};

/// The pool takes `floor(yield_share * credit)` and returns the rest for the fee destination.
///
/// Cannot fail. Whatever the pool cannot take, because the market is unregistered, the share is
/// zero, or the pool is empty or frozen, comes back with the remainder. The pool row is read once
/// here and handed down.
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

/// One sized offset leg: the debt to cancel, and the `Preservation` its sizing proved valid for
/// the burn.
#[derive(Clone, Copy)]
pub(crate) struct OffsetReservation<Balance> {
	pub(crate) debt: Balance,
	pub(crate) preservation: Preservation,
}

impl<T: Config> Pallet<T> {
	/// The pool an offset may draw on.
	///
	/// Returns `None` for a market that is missing or frozen, which sizes every leg to zero and
	/// refuses every settlement.
	fn offset_pool(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Option<StabilityPoolOf<T>> {
		Self::ensure_not_frozen(collateral_id, stable_id).ok()?;
		Pools::<T>::get(collateral_id, stable_id)
	}

	/// Re-sizes one non-zero leg and demands the exact requested debt.
	///
	/// Anything else means the caller acted on a stale reading, so the whole offset aborts with
	/// nothing moved.
	fn size_leg_exact(
		sized: Option<(BalanceOf<T>, Preservation)>,
		requested: BalanceOf<T>,
	) -> Result<OffsetReservation<BalanceOf<T>>, DispatchError> {
		let Some((debt, preservation)) = sized else {
			return Err(crate::Error::<T>::OffsetSettlementFailed.into());
		};
		ensure!(debt == requested, crate::Error::<T>::OffsetSettlementFailed);
		Ok(OffsetReservation { debt, preservation })
	}
}

impl<T: Config> StabilityPoolInspect<CollateralIdOf<T>, StableIdOf<T>, BalanceOf<T>> for Pallet<T> {
	fn reducible_active(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		max_debt: BalanceOf<T>,
	) -> BalanceOf<T> {
		let Some(pool) = Self::offset_pool(collateral_id, stable_id) else {
			return BalanceOf::<T>::zero();
		};
		let pool_account = Self::pool_account(collateral_id, stable_id);
		Self::size_offset(
			&pool,
			stable_id,
			&pool_account,
			Leg::Active,
			max_debt,
			BalanceOf::<T>::zero(),
		)
		.map_or_else(BalanceOf::<T>::zero, |(debt, _)| debt)
	}

	fn reducible_pending(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		max_debt: BalanceOf<T>,
		active_debt: BalanceOf<T>,
	) -> BalanceOf<T> {
		let Some(pool) = Self::offset_pool(collateral_id, stable_id) else {
			return BalanceOf::<T>::zero();
		};
		let pool_account = Self::pool_account(collateral_id, stable_id);
		Self::size_offset(&pool, stable_id, &pool_account, Leg::Pending, max_debt, active_debt)
			.map_or_else(BalanceOf::<T>::zero, |(debt, _)| debt)
	}
}

impl<T: Config>
	StabilityPoolOffset<CollateralIdOf<T>, StableIdOf<T>, BalanceOf<T>, CollateralCreditOf<T>>
	for Pallet<T>
{
	#[require_transactional]
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
		let pool_account = Self::pool_account(collateral_id, stable_id);

		// Both legs re-size against the untouched pool, in the order the caller inspected them:
		// active first, pending reserved behind it. A caller whose readings went stale therefore
		// fails here, with nothing moved.
		let active = if debt.active.is_zero() {
			None
		} else {
			let sized = Self::size_offset(
				&pool,
				stable_id,
				&pool_account,
				Leg::Active,
				debt.active,
				BalanceOf::<T>::zero(),
			);
			Some(Self::size_leg_exact(sized, debt.active)?)
		};
		let pending = if debt.pending.is_zero() {
			None
		} else {
			let sized = Self::size_offset(
				&pool,
				stable_id,
				&pool_account,
				Leg::Pending,
				debt.pending,
				debt.active,
			);
			Some(Self::size_leg_exact(sized, debt.pending)?)
		};

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
