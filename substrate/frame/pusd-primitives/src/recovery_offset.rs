//! Lets the Stability Pool cancel debt against the `FinalRecovery` FIFO head at the same pricing
//! as recovery redemptions. The redemptions pallet owns that pricing and implements this, so the
//! two paths cannot diverge.

use frame::deps::sp_runtime::DispatchError;

/// Outcome of one recovery offset against the `FinalRecovery` FIFO head.
#[derive(PartialEq, Debug)]
pub enum RecoveryOffsetResult<Balance> {
	/// No `FinalRecovery` vault is queued.
	NoTarget,
	/// The head is below par (`CR < 100%`); nothing is offset.
	BelowPar,
	/// Debt was cancelled and `collateral_out` sent to the recipient.
	Applied { collateral_out: Balance },
}

/// Execution of recovery offsets against the `FinalRecovery` FIFO head,
/// restricted to the `CR >= 100%` (recovery-bonus) regime.
///
/// Like redemptions, each call stops after one head, so it never crosses into another recovery
/// price.
pub trait RecoveryOffsetInterface {
	type CollateralId;
	type AccountId;
	type Balance;
	type Credit;

	/// Cancels head debt in the market of `collateral_id` and the payment's asset, spending up to
	/// the payment, and sends the priced collateral to `collateral_recipient`. No redemption fee.
	///
	/// Returns the outcome and the unspent change; `NoTarget` and `BelowPar` return the whole
	/// payment. The implementation can only burn what the credit carries, so callers derive the
	/// cancelled debt as `payment - change`.
	///
	/// An `Err` consumes the payment and storage unwinds only with the caller's transaction, so
	/// callers must abort the extrinsic.
	fn execute_recovery_offset(
		collateral_id: &Self::CollateralId,
		payment: Self::Credit,
		collateral_recipient: &Self::AccountId,
	) -> Result<(RecoveryOffsetResult<Self::Balance>, Self::Credit), DispatchError>;
}
