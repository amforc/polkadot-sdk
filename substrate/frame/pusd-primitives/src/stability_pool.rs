//! Stability Pool offset interfaces for liquidation settlement.

use frame::deps::{
	frame_support::{
		pallet_prelude::{DispatchError, DispatchResult},
		traits::TryDrop,
	},
	sp_runtime::traits::Zero,
};

/// Per-leg values of a Stability Pool offset: debt amounts or collateral credits.
pub struct OffsetLegs<T> {
	/// Active-pool leg.
	pub active: T,
	/// Pending-deposit leg.
	pub pending: T,
}

/// Provides read-only offset limits for Stability Pool markets.
///
/// Valid offset amounts do not form a contiguous range. Full depletion is always valid.
/// A partial offset must not leave a remainder below the pool minimum.
/// It also must not leave the stablecoin account in its minimum-balance dead zone.
/// Thus, each limit depends on the requested debt.
pub trait StabilityPoolInspect<CollateralId, StableId, Balance> {
	/// Returns the active-pool debt that the pool can cancel, up to `max_debt`.
	///
	/// The result is a quote, not a reservation. It remains valid while the caller does not change
	/// the pool.
	fn reducible_active(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		max_debt: Balance,
	) -> Balance;

	/// Returns the pending-deposit debt that the pool can cancel, up to `max_debt`.
	///
	/// `active_debt` is the active-pool debt for the same offset. Both legs burn stablecoin from
	/// one custody account.
	fn reducible_pending(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		max_debt: Balance,
		active_debt: Balance,
	) -> Balance;
}

/// Applies a liquidation offset to one Stability Pool market.
pub trait StabilityPoolOffset<CollateralId, StableId, Balance, CollateralCredit>:
	StabilityPoolInspect<CollateralId, StableId, Balance>
{
	/// Cancels exactly `debt` against pool deposits and pays `collateral` to each leg.
	///
	/// Fails unless, at the start:
	///
	/// - `debt.active == Self::reducible_active(collateral_id, stable_id, debt.active)`.
	/// - `debt.pending == Self::reducible_pending(collateral_id, stable_id, debt.pending,
	///   debt.active)`.
	///
	/// The function returns an error if one equality does not hold. On success, the pool burns the
	/// specified stablecoin debt. The caller reduces vault debt by the same amounts.
	///
	/// TODO: The function consumes the collateral credits even when it returns an error, so it must run
	/// inside a storage transaction that restores them on rollback. Implementations must reject a
	/// call outside a transactional layer; annotate them with `#[require_transactional]`.
	fn offset(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		debt: OffsetLegs<Balance>,
		collateral: OffsetLegs<CollateralCredit>,
	) -> DispatchResult;
}

/// No-op pool for runtimes without one: nothing is reducible, and an offset succeeds only when
/// all debts and credits are zero.
impl<CollateralId, StableId, Balance: Zero> StabilityPoolInspect<CollateralId, StableId, Balance>
	for ()
{
	fn reducible_active(_: &CollateralId, _: &StableId, _: Balance) -> Balance {
		Balance::zero()
	}

	fn reducible_pending(_: &CollateralId, _: &StableId, _: Balance, _: Balance) -> Balance {
		Balance::zero()
	}
}

impl<CollateralId, StableId, Balance: Zero, CollateralCredit: TryDrop>
	StabilityPoolOffset<CollateralId, StableId, Balance, CollateralCredit> for ()
{
	fn offset(
		_: &CollateralId,
		_: &StableId,
		debt: OffsetLegs<Balance>,
		collateral: OffsetLegs<CollateralCredit>,
	) -> DispatchResult {
		let debt_is_zero = debt.active.is_zero() && debt.pending.is_zero();
		// `TryDrop` succeeds only on zero credits, so a nonzero credit cannot vanish here.
		let active_is_zero = collateral.active.try_drop().is_ok();
		let pending_is_zero = collateral.pending.try_drop().is_ok();
		if debt_is_zero && active_is_zero && pending_is_zero {
			Ok(())
		} else {
			Err(DispatchError::Other("no Stability Pool to offset against"))
		}
	}
}
