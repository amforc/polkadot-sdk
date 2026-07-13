//! Recovery-offset surface: lets the Stability Pool cancel debt against the
//! current `FinalRecovery` FIFO head at the SAME settlement pricing as
//! recovery redemptions. Implemented by the redemptions pallet — the owner
//! of that pricing — so the two paths cannot diverge.

use frame::deps::sp_runtime::DispatchError;

/// What an applied recovery offset actually did.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RecoveryOffsetOutcome<AccountId, Balance> {
	pub vault_owner: AccountId,
	pub collateral_out: Balance,
}

/// Result of one execution attempt against the current `FinalRecovery` FIFO
/// head.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RecoveryOffsetResult<AccountId, Balance> {
	NoTarget,
	BelowPar,
	Applied(RecoveryOffsetOutcome<AccountId, Balance>),
}

/// Execution of recovery offsets against the `FinalRecovery` FIFO head,
/// restricted to the `CR >= 100%` (recovery-bonus) regime.
///
/// One head per call, mirroring the redemption loop's rule that recovery
/// stops after one FIFO head — a single call can never cross into a
/// different recovery price.
pub trait RecoveryOffsetInterface {
	type CollateralId;
	type AccountId;
	type Balance;
	type Credit;

	/// Cancel head debt against the `payment` credit at the shared
	/// settlement pricing — the credit's value is the budget — and deliver
	/// the priced collateral to `collateral_recipient`, atomically within
	/// the underlying vault step. The unconsumed change returns with the
	/// result: the whole payment on `NoTarget`/`BelowPar`, which are
	/// ordinary results rather than errors. Fee-free: the redemption
	/// dynamic fee is neither charged nor moved.
	///
	/// The credit is authoritative twice over: its asset selects the
	/// market's stablecoin (only the branch's collateral needs naming), and
	/// conservation is structural — the implementation can only burn value
	/// the credit carries, so callers derive the cancelled debt as
	/// `payment - change` instead of trusting a reported figure.
	///
	/// On `Err` the payment was consumed in memory while its storage
	/// effects unwind with the caller's transaction: callers must abort
	/// the whole extrinsic, never continue past an error.
	fn execute_recovery_offset(
		collateral_id: &Self::CollateralId,
		payment: Self::Credit,
		collateral_recipient: &Self::AccountId,
	) -> Result<(RecoveryOffsetResult<Self::AccountId, Self::Balance>, Self::Credit), DispatchError>;
}
