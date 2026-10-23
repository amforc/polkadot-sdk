//! Lets the Stability Pool cancel debt against the `FinalRecovery` FIFO head at the same pricing
//! as recovery redemptions. The implementer is the owner of that pricing, so the two paths cannot
//! diverge.

use frame::deps::sp_runtime::DispatchError;

/// Outcome of one recovery offset against the `FinalRecovery` FIFO head.
#[derive(PartialEq, Debug)]
pub enum RecoveryOffsetResult<Balance> {
	/// No `FinalRecovery` vault is queued.
	NoTarget,
	/// A queued head is unpriceable, above the exit threshold, or cancels no debt.
	/// Its lookup and pricing costs must not receive the empty-queue weight refund.
	NotOffsettable,
	/// The head is below par (`CR < 100%`); nothing is offset.
	BelowPar,
	/// Debt was cancelled and `collateral_out` sent to the recipient.
	Applied { collateral_out: Balance },
}

/// Executes recovery offsets against the `FinalRecovery` FIFO head when `100% <= CR <= ICR`.
///
/// Like redemptions, each call stops after one head, so it never crosses into another recovery
/// price.
pub trait RecoveryOffsetInterface {
	/// Collateral asset identifier; with the payment's asset it names the branch.
	type CollateralId;
	/// Account type of the collateral recipient.
	type AccountId;
	/// Amount type; [`RecoveryOffsetResult::Applied`] reports collateral planck in it.
	type Balance;
	/// Branch stablecoin credit that pays for the offset.
	type Credit;

	/// Cancels head debt in the branch of `collateral_id` and the payment's asset, spending up to
	/// the payment, and sends the priced collateral to `collateral_recipient`. No redemption fee.
	///
	/// Returns the outcome and the unspent change; `NoTarget`, `NotOffsettable` and `BelowPar`
	/// return the whole payment. The implementation can only burn what the credit carries, so
	/// callers derive the cancelled debt as `payment - change`.
	///
	/// If the head cannot consume the whole payment, the burn is capped at `max_partial_debt`.
	/// This lets a pool preserve its minimum balance without preventing full depletion.
	///
	/// An `Err` consumes the payment and storage unwinds only with the caller's transaction, so
	/// callers must abort the extrinsic.
	fn execute_recovery_offset(
		collateral_id: &Self::CollateralId,
		payment: Self::Credit,
		max_partial_debt: Self::Balance,
		collateral_recipient: &Self::AccountId,
	) -> Result<(RecoveryOffsetResult<Self::Balance>, Self::Credit), DispatchError>;
}
