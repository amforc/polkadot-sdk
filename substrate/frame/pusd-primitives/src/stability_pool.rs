//! Offset surface the liquidation orchestrator drives on the Stability Pool.
//!
//! Quote-then-execute, the shape `fungibles`-style traits use elsewhere: the
//! capacity methods are pure reads that size a stage, and the offset methods
//! move the value, re-deriving their own amount and taking at most what they
//! were asked for. Collateral travels as a fungibles `Credit`, and the pool
//! returns whatever it did not consume.

use frame::deps::sp_runtime::traits::Zero;

/// The offset surface a liquidation flow drives on the Stability Pool:
/// active-pool capital first, pending deposits as the last resort before
/// redistribution.
///
/// A caller that must allocate collateral across several paths before any of
/// them runs — penalty-weighted liquidation does — sizes every path through the
/// capacity methods first, then hands each offset exactly its share.
pub trait StabilityPoolOffsetApi<CollateralId, StableId, Balance, CollateralCredit> {
	/// Debt [`Self::offset_liquidation`] would cancel from active capital for
	/// `max_debt`, without moving anything.
	///
	/// `reserved` is stablecoin this transaction has already promised out of
	/// the pool account. Active and pending offsets burn from the same account,
	/// so sizing the second without netting the first double-counts the
	/// minimum-balance headroom they share. A first stage passes zero.
	fn active_offset_capacity(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		max_debt: Balance,
		reserved: Balance,
	) -> Balance;

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

	/// Debt [`Self::offset_pending_liquidation`] would consume from the pending
	/// FIFO for `max_debt`, without moving anything. `reserved` carries the same
	/// meaning as in [`Self::active_offset_capacity`].
	fn pending_offset_capacity(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		max_debt: Balance,
		reserved: Balance,
	) -> Balance;

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

/// No-pool runtime: zero capacity, zero offsets, every credit passed straight
/// back.
impl<CollateralId, StableId, Balance: Zero, CollateralCredit>
	StabilityPoolOffsetApi<CollateralId, StableId, Balance, CollateralCredit> for ()
{
	fn active_offset_capacity(_: &CollateralId, _: &StableId, _: Balance, _: Balance) -> Balance {
		Balance::zero()
	}

	fn offset_liquidation(
		_: &CollateralId,
		_: &StableId,
		_: Balance,
		collateral: CollateralCredit,
	) -> (Balance, CollateralCredit) {
		(Balance::zero(), collateral)
	}

	fn pending_offset_capacity(_: &CollateralId, _: &StableId, _: Balance, _: Balance) -> Balance {
		Balance::zero()
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
