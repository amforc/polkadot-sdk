//! pUSD primitive trait implementations: the surfaces sibling pallets drive
//! on the pool.

use crate::pallet::{
	BalanceOf, CollateralCreditOf, CollateralIdOf, Config, Pallet, Pools, StabilityPoolOf,
	StableCreditOf, StableIdOf,
};
use frame::{deps::frame_support::transactional, prelude::*, traits::tokens::Preservation};
use pusd_primitives::{OnBranchYield, StabilityOffsetSession, StabilityPoolOffsetApi};

/// The vault engine hands every minted branch credit through here; the pool
/// takes `floor(yield_share * credit)` and returns the rest for the fee
/// destination. Infallible: whatever cannot be distributed (no pool row, a
/// zero share, an empty or frozen pool) comes back with the remainder. The
/// pool row is loaded once and handed down to the distribution engine.
impl<T: Config> OnBranchYield<CollateralIdOf<T>, StableIdOf<T>, StableCreditOf<T>> for Pallet<T> {
	fn distribute_yield(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		credit: StableCreditOf<T>,
	) -> StableCreditOf<T> {
		if credit.asset() != *stable_id {
			return credit;
		}
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

/// One loaded pool draft shared by every stage of a liquidation.
#[must_use = "the StabilityPoolOffsetApi boundary commits this session"]
pub struct LiquidationOffsetSession<T: Config> {
	collateral_id: CollateralIdOf<T>,
	stable_id: StableIdOf<T>,
	pool_account: T::AccountId,
	pool: Option<StabilityPoolOf<T>>,
	active: Option<OffsetReservation<BalanceOf<T>>>,
	pending: Option<OffsetReservation<BalanceOf<T>>>,
	dirty: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct OffsetReservation<Balance> {
	pub(crate) debt: Balance,
	pub(crate) preservation: Preservation,
}

impl<T: Config> LiquidationOffsetSession<T> {
	fn commit(self) {
		if self.dirty {
			if let Some(pool) = self.pool {
				Pools::<T>::insert(&self.collateral_id, &self.stable_id, pool);
			}
		}
	}
}

impl<T: Config> StabilityOffsetSession<BalanceOf<T>, CollateralCreditOf<T>>
	for LiquidationOffsetSession<T>
{
	fn reserve_active(&mut self, max_debt: BalanceOf<T>) -> BalanceOf<T> {
		if let Some(reservation) = self.active {
			return reservation.debt;
		}
		let Some(pool) = &self.pool else {
			return BalanceOf::<T>::zero();
		};
		let Some((debt, preservation)) = Pallet::<T>::size_active_offset(
			pool,
			&self.stable_id,
			&self.pool_account,
			max_debt,
			BalanceOf::<T>::zero(),
		) else {
			return BalanceOf::<T>::zero();
		};
		self.active = Some(OffsetReservation { debt, preservation });
		debt
	}

	fn reserve_pending(&mut self, max_debt: BalanceOf<T>) -> BalanceOf<T> {
		if let Some(reservation) = self.pending {
			return reservation.debt;
		}
		let Some(pool) = &self.pool else {
			return BalanceOf::<T>::zero();
		};
		let reserved =
			self.active.map_or_else(BalanceOf::<T>::zero, |reservation| reservation.debt);
		let Some((debt, preservation)) = Pallet::<T>::size_pending_offset(
			pool,
			&self.stable_id,
			&self.pool_account,
			max_debt,
			reserved,
		) else {
			return BalanceOf::<T>::zero();
		};
		self.pending = Some(OffsetReservation { debt, preservation });
		debt
	}

	fn settle_active(&mut self, collateral: CollateralCreditOf<T>) -> DispatchResult {
		let (Some(reservation), Some(pool)) = (self.active.take(), self.pool.as_mut()) else {
			drop(collateral);
			return Err(crate::Error::<T>::OffsetSettlementFailed.into());
		};
		Pallet::<T>::settle_active_offset(
			&self.collateral_id,
			&self.stable_id,
			&self.pool_account,
			pool,
			reservation,
			collateral,
		)?;
		self.dirty = true;
		Ok(())
	}

	fn settle_pending(&mut self, collateral: CollateralCreditOf<T>) -> DispatchResult {
		let (Some(reservation), Some(pool)) = (self.pending.take(), self.pool.as_mut()) else {
			drop(collateral);
			return Err(crate::Error::<T>::OffsetSettlementFailed.into());
		};
		Pallet::<T>::settle_pending_offset(
			&self.collateral_id,
			&self.stable_id,
			&self.pool_account,
			pool,
			reservation,
			collateral,
		)?;
		self.dirty = true;
		Ok(())
	}
}

/// Opens the transaction-local pool draft Vaults drives.
impl<T: Config>
	StabilityPoolOffsetApi<CollateralIdOf<T>, StableIdOf<T>, BalanceOf<T>, CollateralCreditOf<T>>
	for Pallet<T>
{
	type Session = LiquidationOffsetSession<T>;

	#[transactional]
	fn with_offset_session<R>(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		settle: impl FnOnce(&mut Self::Session) -> Result<R, DispatchError>,
	) -> Result<R, DispatchError> {
		let pool = Pallet::<T>::ensure_not_frozen(collateral_id, stable_id)
			.ok()
			.and_then(|_| Pools::<T>::get(collateral_id, stable_id));
		let mut session = LiquidationOffsetSession {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			pool_account: Pallet::<T>::pool_account(collateral_id, stable_id),
			pool,
			active: None,
			pending: None,
			dirty: false,
		};
		let result = settle(&mut session)?;
		session.commit();
		Ok(result)
	}
}
