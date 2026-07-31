//! Transaction-local Stability Pool access for liquidation settlement.

use frame::deps::{frame_support::pallet_prelude::DispatchError, sp_runtime::traits::Zero};

/// One in-memory view of a market's Stability Pool during a liquidation.
///
/// Vaults reserves both pool legs before pricing collateral. A reservation is
/// exact: settlement must consume that debt and the collateral assigned to it,
/// or fail the surrounding liquidation transaction.
pub trait StabilityOffsetSession<Balance, CollateralCredit> {
	/// Reserves active-pool debt, without moving value.
	fn reserve_active(&mut self, max_debt: Balance) -> Balance;

	/// Reserves pending-deposit debt after accounting for the active reservation.
	fn reserve_pending(&mut self, max_debt: Balance) -> Balance;

	/// Settles the exact active-pool reservation.
	fn settle_active(&mut self, collateral: CollateralCredit) -> Result<(), DispatchError>;

	/// Settles the exact pending-deposit reservation.
	fn settle_pending(&mut self, collateral: CollateralCredit) -> Result<(), DispatchError>;
}

/// Runs one liquidation against a single Stability Pool session.
pub trait StabilityPoolOffsetApi<CollateralId, StableId, Balance, CollateralCredit> {
	type Session: StabilityOffsetSession<Balance, CollateralCredit>;

	/// Load once, run `settle`, and commit the draft exactly once on success.
	/// Implementations must make the boundary transactional so an error after
	/// either value-moving stage cannot leave a partial offset.
	fn with_offset_session<R>(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		settle: impl FnOnce(&mut Self::Session) -> Result<R, DispatchError>,
	) -> Result<R, DispatchError>;
}

/// No-pool runtime: reserves no debt, so settlement is unreachable.
impl<Balance: Zero, CollateralCredit> StabilityOffsetSession<Balance, CollateralCredit> for () {
	fn reserve_active(&mut self, _: Balance) -> Balance {
		Balance::zero()
	}

	fn reserve_pending(&mut self, _: Balance) -> Balance {
		Balance::zero()
	}

	fn settle_active(&mut self, collateral: CollateralCredit) -> Result<(), DispatchError> {
		drop(collateral);
		Err(DispatchError::Other("active Stability reservation missing"))
	}

	fn settle_pending(&mut self, collateral: CollateralCredit) -> Result<(), DispatchError> {
		drop(collateral);
		Err(DispatchError::Other("pending Stability reservation missing"))
	}
}

impl<CollateralId, StableId, Balance: Zero, CollateralCredit>
	StabilityPoolOffsetApi<CollateralId, StableId, Balance, CollateralCredit> for ()
{
	type Session = ();

	fn with_offset_session<R>(
		_: &CollateralId,
		_: &StableId,
		settle: impl FnOnce(&mut Self::Session) -> Result<R, DispatchError>,
	) -> Result<R, DispatchError> {
		settle(&mut ())
	}
}
