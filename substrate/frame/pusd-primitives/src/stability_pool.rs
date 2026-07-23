//! Offset surface the liquidation orchestrator drives on the Stability Pool.
//!
//! Collateral travels as a fungibles `Credit`: the pool resolves the slice
//! backing the offset debt into its own account inside the call and returns
//! the remainder.

use frame::deps::sp_runtime::traits::Zero;

/// The offset surface a liquidation flow drives on the Stability Pool:
/// active-pool capital first, pending deposits as the last resort before
/// redistribution.
pub trait StabilityPoolOffsetApi<CollateralId, StableId, Balance, CollateralCredit> {
	/// Burn up to `max_debt_to_offset` of active pool stablecoin against
	/// liquidation debt, resolving the pro-rata slice of `collateral` into
	/// the pool account for active depositors. Returns the debt actually
	/// cancelled and the unconsumed remainder of `collateral`; the pool's
	/// collateral take is `input - returned` at the caller. A zero-capacity
	/// (or unregistered, or frozen) pool returns zero with the credit
	/// untouched.
	fn offset_liquidation(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		max_debt_to_offset: Balance,
		collateral: CollateralCredit,
	) -> (Balance, CollateralCredit);

	/// Consume pending deposits oldest-first against liquidation debt that
	/// survived the active pool and JIT liquidity. The pool bounds the walk
	/// by its own iteration constant. Each consumed step resolves its
	/// collateral slice into the pool account. Returns the debt actually
	/// cancelled and the unconsumed remainder of `collateral`.
	fn offset_pending_liquidation(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		max_debt_to_offset: Balance,
		collateral: CollateralCredit,
	) -> (Balance, CollateralCredit);
}

/// No-pool runtime: zero offsets, every credit passed straight back.
impl<CollateralId, StableId, Balance: Zero, CollateralCredit>
	StabilityPoolOffsetApi<CollateralId, StableId, Balance, CollateralCredit> for ()
{
	fn offset_liquidation(
		_: &CollateralId,
		_: &StableId,
		_: Balance,
		collateral: CollateralCredit,
	) -> (Balance, CollateralCredit) {
		(Balance::zero(), collateral)
	}

	fn offset_pending_liquidation(
		_: &CollateralId,
		_: &StableId,
		_: Balance,
		collateral: CollateralCredit,
	) -> (Balance, CollateralCredit) {
		(Balance::zero(), collateral)
	}
}
