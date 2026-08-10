//! pUSD primitive trait implementations: the surfaces sibling pallets drive
//! on the pool.

use crate::pallet::{
	BalanceOf, CollateralCreditOf, CollateralIdOf, Config, Pallet, Pools, StabilityPoolOf,
	StableCreditOf, StableIdOf,
};
use frame::{
	deps::frame_support::require_transactional,
	prelude::*,
	traits::{
		fungibles::Inspect as _,
		tokens::{Preservation, Provenance},
	},
};
use pusd_primitives::{OffsetLegs, OnBranchYield, StabilityPoolInspect, StabilityPoolOffset};

/// The vault engine hands every minted branch credit through here; the pool
/// takes `floor(yield_share * credit)` and returns the rest for the fee
/// destination. Infallible: whatever cannot be distributed (no pool row, a
/// zero share, an empty or frozen pool) comes back with the remainder. The
/// pool row is loaded once and handed down to the distribution engine.
impl<T: Config> OnBranchYield<CollateralIdOf<T>, StableCreditOf<T>> for Pallet<T> {
	fn distribute_yield(
		collateral_id: &CollateralIdOf<T>,
		credit: StableCreditOf<T>,
	) -> StableCreditOf<T> {
		// The credit's own asset names the market; an unregistered pair has
		// no pool row and the credit comes back whole.
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
			// Both halves came from one credit, so a mismatch cannot
			// happen; burning the leftover keeps issuance conservative.
			debug_assert!(false, "yield credit halves diverged");
			drop(leftover);
		}
		remainder
	}
}

/// One sized offset leg: the debt to burn and the `Preservation` its sizing
/// pass proved valid for the burn debit.
#[derive(Clone, Copy)]
pub(crate) struct OffsetReservation<Balance> {
	pub(crate) debt: Balance,
	pub(crate) preservation: Preservation,
}

impl<T: Config> Pallet<T> {
	/// The pool row offsets may draw on: `None` when the branch is missing or
	/// frozen, which sizes every leg to zero and refuses every settlement.
	fn offset_pool(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Option<StabilityPoolOf<T>> {
		Self::ensure_not_frozen(collateral_id, stable_id).ok()?;
		Pools::<T>::get(collateral_id, stable_id)
	}

	/// Exact re-validation of one non-zero leg: the sizing pass must reproduce
	/// precisely the requested debt, or the caller's inspection reads went
	/// stale and the whole offset aborts with nothing moved.
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
		Self::size_active_offset(&pool, stable_id, &pool_account, max_debt, BalanceOf::<T>::zero())
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
		Self::size_pending_offset(&pool, stable_id, &pool_account, max_debt, active_debt)
			.map_or_else(BalanceOf::<T>::zero, |(debt, _)| debt)
	}

	fn can_receive_collateral(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		amount: BalanceOf<T>,
	) -> bool {
		if Self::offset_pool(collateral_id, stable_id).is_none() {
			return false;
		}
		let pool_account = Self::pool_account(collateral_id, stable_id);
		T::CollateralAssets::can_deposit(
			collateral_id.clone(),
			&pool_account,
			amount,
			Provenance::Extant,
		)
		.into_result()
		.is_ok()
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
		// A zero-debt leg must carry a provably-zero credit: anything else
		// would donate collateral to the pool without cancelling debt.
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

		// Both legs re-size against the untouched pool in the inspection
		// order — active first, pending reserved behind it — so a caller
		// whose reads went stale fails here with nothing moved.
		let active = if debt.active.is_zero() {
			None
		} else {
			let sized = Self::size_active_offset(
				&pool,
				stable_id,
				&pool_account,
				debt.active,
				BalanceOf::<T>::zero(),
			);
			Some(Self::size_leg_exact(sized, debt.active)?)
		};
		let pending = if debt.pending.is_zero() {
			None
		} else {
			let sized = Self::size_pending_offset(
				&pool,
				stable_id,
				&pool_account,
				debt.pending,
				debt.active,
			);
			Some(Self::size_leg_exact(sized, debt.pending)?)
		};

		// Active settles before pending: the pending `Preservation` was sized
		// against the combined limit and holds only once the active tranche
		// has left the pool account.
		if let Some(reservation) = active {
			Self::settle_active_offset(
				collateral_id,
				stable_id,
				&pool_account,
				&mut pool,
				reservation,
				collateral.active,
			)?;
		}
		if let Some(reservation) = pending {
			Self::settle_pending_offset(
				collateral_id,
				stable_id,
				&pool_account,
				&mut pool,
				reservation,
				collateral.pending,
			)?;
		}
		Pools::<T>::insert(collateral_id, stable_id, pool);
		Ok(())
	}
}
