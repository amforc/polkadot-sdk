//! Facilitator-attributed supply accounting for framework stablecoins.
//!
//! A framework stablecoin is a plain `pallet-assets` asset whose supply is minted and burned
//! by one or more *facilitators* — peg mechanisms such as a CDP market suite
//! (vaults + redemptions + liquidations + stability pool), a PSM, or the coin owner's
//! self-issuance. Each facilitator holds a bucket `(capacity, level)` per coin: `level` is the
//! supply it has minted and not yet burned, `capacity` the ceiling its level may not exceed.
//! Buckets make a multi-mechanism coin auditable (who minted what), give the coin's admin a
//! per-mechanism cap, and pin the ledger invariant
//!
//! ```text
//! for every stable: Σ facilitator levels == total issuance
//! ```
//!
//! # Integration pattern
//!
//! Mechanism pallets do NOT call this trait. They keep speaking
//! `fungibles::{Inspect, Mutate, Balanced}` through their `StableAssets` associated type — that
//! associated type is the attribution seam. The ledger implementation (`pallet-issuance`)
//! exposes one *wrapper* fungibles instance per facilitator; the runtime wires each mechanism's
//! `StableAssets` to its own wrapper. The wrapper records attribution around delegation:
//!
//! - `mint_into` / `issue`: [`SupplyLedger::note_mint`] first (failing the whole operation when the
//!   bucket lacks capacity), then delegate.
//! - `burn_from` / `rescind`: delegate, then [`SupplyLedger::note_burn`].
//! - imbalance drops: a `fungibles::Credit<_, F>` burns through `F`'s drop handler, and `F` is the
//!   wrapper type itself — so even credits dropped on the floor attribute to the wrapper's
//!   facilitator. This is what makes the invariant hold across the `Credit`-shaped surfaces (fee
//!   sinks, bad-debt healing, redemption burns) without per-call plumbing.
//!
//! # Attribution rules
//!
//! - A burn attributes to the facilitator whose liability it cancels, not to the pallet that
//!   happens to execute it. An orchestrator settling another mechanism's liabilities (e.g.
//!   redemptions cancelling vault debt) must be wired with that mechanism's facilitator id — in
//!   practice the whole CDP suite shares one facilitator.
//! - Sinks receiving fee/yield credits must resolve them into an account or burn them through an
//!   attributed wrapper; anonymously dropping a credit obtained from an *unwrapped* fungibles
//!   instance breaks the invariant.

use frame::deps::{
	frame_support::pallet_prelude::DispatchResult,
	sp_runtime::traits::{Bounded, Zero},
};

/// Per-facilitator supply buckets for one or more stablecoins.
///
/// Implemented by the supply-ledger pallet; consumed by its per-facilitator fungibles wrappers
/// and by governance/inspection surfaces. Mechanism pallets never see this trait (see the
/// module docs for the wrapper pattern).
pub trait SupplyLedger<StableId, FacilitatorId, Balance> {
	/// Mint ceiling of `facilitator`'s bucket. [`note_mint`](Self::note_mint) fails once the
	/// level would exceed it.
	fn capacity(stable_id: &StableId, facilitator: &FacilitatorId) -> Balance;

	/// Supply minted by `facilitator` and not yet burned.
	fn level(stable_id: &StableId, facilitator: &FacilitatorId) -> Balance;

	/// Record `amount` about to be minted by `facilitator`.
	///
	/// Errors when `level + amount` would exceed the bucket's capacity; the caller must then
	/// abort the mint (wrappers call this *before* delegating, so the whole operation rolls
	/// back cleanly).
	fn note_mint(
		stable_id: &StableId,
		facilitator: &FacilitatorId,
		amount: Balance,
	) -> DispatchResult;

	/// Record `amount` burned by `facilitator`, returning the portion actually deducted from
	/// its level (`<= amount`, saturating at zero).
	///
	/// A shortfall means supply minted by one facilitator was burned under another's id —
	/// mis-attribution, not a user error. Implementations should raise a defensive signal;
	/// callers in fallible contexts should treat a shortfall as an error, while infallible
	/// contexts (imbalance drops) can only log it.
	fn note_burn(stable_id: &StableId, facilitator: &FacilitatorId, amount: Balance) -> Balance;
}

/// Ledger-less runtimes: unlimited capacity, nothing attributed. Every mint is admitted and
/// every burn reports as fully recorded, so mechanism wrappers behave as plain pass-throughs.
impl<StableId, FacilitatorId, Balance: Bounded + Zero>
	SupplyLedger<StableId, FacilitatorId, Balance> for ()
{
	fn capacity(_stable_id: &StableId, _facilitator: &FacilitatorId) -> Balance {
		Balance::max_value()
	}

	fn level(_stable_id: &StableId, _facilitator: &FacilitatorId) -> Balance {
		Balance::zero()
	}

	fn note_mint(
		_stable_id: &StableId,
		_facilitator: &FacilitatorId,
		_amount: Balance,
	) -> DispatchResult {
		Ok(())
	}

	fn note_burn(_stable_id: &StableId, _facilitator: &FacilitatorId, amount: Balance) -> Balance {
		amount
	}
}
