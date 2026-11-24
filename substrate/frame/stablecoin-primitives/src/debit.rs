//! Debit sizing around the minimum-balance boundary.

use frame::deps::frame_support::{
	defensive_assert,
	traits::{
		fungibles,
		tokens::{Fortitude, Preservation},
	},
};

/// Returns the [`Preservation`] for debiting exactly `amount` without folding dust.
///
/// `Expendable` only when `amount` takes the whole reducible balance, `Preserve` otherwise, so
/// the implementation rejects a debit that would strand a sub-minimum remainder. Fixed-amount
/// callers need no other checks: the debit moves exactly `amount` or fails.
pub fn debit_preservation<Assets, AccountId>(
	asset: Assets::AssetId,
	who: &AccountId,
	amount: Assets::Balance,
) -> Preservation
where
	Assets: fungibles::Inspect<AccountId>,
{
	let expendable =
		Assets::reducible_balance(asset, who, Preservation::Expendable, Fortitude::Polite);
	if amount < expendable {
		Preservation::Preserve
	} else {
		Preservation::Expendable
	}
}

/// Returns the largest amount up to `limit` that a `Precision::Exact` debit removes cleanly,
/// with the [`Preservation`] to pass.
///
/// Implementations fold a sub-minimum remainder into `Expendable` debits and reject `Preserve`
/// debits that would leave one. So a request that would leave less than the minimum balance
/// rounds down to keep it, and one at or above the whole reducible balance becomes a full
/// `Expendable` drain.
///
/// For operations sized to what the account can pay; fixed amounts need only
/// [`debit_preservation`]. To refuse on a shortfall, compare the result with `limit`.
pub fn reducible_debit<Assets, AccountId>(
	asset: Assets::AssetId,
	who: &AccountId,
	limit: Assets::Balance,
) -> (Assets::Balance, Preservation)
where
	Assets: fungibles::Inspect<AccountId>,
{
	let preserved =
		Assets::reducible_balance(asset.clone(), who, Preservation::Preserve, Fortitude::Polite);
	refine_debit(limit, preserved, || {
		Assets::reducible_balance(asset, who, Preservation::Expendable, Fortitude::Polite)
	})
}

/// [`reducible_debit`] over balances already read.
///
/// `preserved` is the `Preserve` reducible balance. `expendable` returns the `Expendable` one
/// and is called only when `limit > preserved`.
fn refine_debit<Balance: Ord + Copy>(
	limit: Balance,
	preserved: Balance,
	expendable: impl FnOnce() -> Balance,
) -> (Balance, Preservation) {
	if limit <= preserved {
		return (limit, Preservation::Preserve);
	}
	let expendable = expendable();
	defensive_assert!(preserved <= expendable);
	if limit < expendable {
		(preserved, Preservation::Preserve)
	} else {
		(expendable, Preservation::Expendable)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame::deps::frame_support::traits::tokens::{
		DepositConsequence, Provenance, WithdrawConsequence,
	};

	/// Minimal Inspect over one account: `BALANCE` with `MIN` minimum, no
	/// freezes, so preserved = balance - min and expendable = balance.
	struct OneAccount<const BALANCE: u64, const MIN: u64>;

	impl<const BALANCE: u64, const MIN: u64> fungibles::Inspect<u8> for OneAccount<BALANCE, MIN> {
		type AssetId = ();
		type Balance = u64;

		fn total_issuance(_: ()) -> u64 {
			BALANCE
		}
		fn minimum_balance(_: ()) -> u64 {
			MIN
		}
		fn total_balance(_: (), _: &u8) -> u64 {
			BALANCE
		}
		fn balance(_: (), _: &u8) -> u64 {
			BALANCE
		}
		fn reducible_balance(_: (), _: &u8, preservation: Preservation, _: Fortitude) -> u64 {
			match preservation {
				Preservation::Expendable => BALANCE,
				_ => BALANCE.saturating_sub(MIN),
			}
		}
		fn can_deposit(_: (), _: &u8, _: u64, _: Provenance) -> DepositConsequence {
			DepositConsequence::Success
		}
		fn can_withdraw(_: (), _: &u8, _: u64) -> WithdrawConsequence<u64> {
			WithdrawConsequence::Success
		}
		fn asset_exists(_: ()) -> bool {
			true
		}
	}

	type Funded = OneAccount<100, 10>;

	#[test]
	fn preserving_requests_pass_through() {
		// `Funded` holds 100 with a minimum balance of 10.

		let nothing = reducible_debit::<Funded, _>((), &0, 0);
		let down_to_minimum = reducible_debit::<Funded, _>((), &0, 90);

		assert_eq!(nothing, (0, Preservation::Preserve));
		assert_eq!(down_to_minimum, (90, Preservation::Preserve));
	}

	#[test]
	fn dead_zone_rounds_down_to_the_preserving_limit() {
		// 91..=99 all leave a sub-minimum remainder.
		for limit in [91, 95, 99] {
			let debit = reducible_debit::<Funded, _>((), &0, limit);

			// The amount caps at 90.
			assert_eq!(debit, (90, Preservation::Preserve), "limit = {limit}");
		}
	}

	#[test]
	fn full_or_overshooting_requests_drain() {
		// Limits at or beyond the full balance of 100.
		for limit in [100, 101, u64::MAX] {
			let debit = reducible_debit::<Funded, _>((), &0, limit);

			assert_eq!(debit, (100, Preservation::Expendable), "limit = {limit}");
		}
	}

	#[test]
	fn empty_account_yields_a_zero_drain() {
		// An account holding nothing.
		type Empty = OneAccount<0, 10>;

		let positive_request = reducible_debit::<Empty, _>((), &0, 5);
		let zero_request = reducible_debit::<Empty, _>((), &0, 0);

		assert_eq!(positive_request, (0, Preservation::Expendable));
		assert_eq!(zero_request, (0, Preservation::Preserve));
	}

	#[test]
	fn preservation_expends_only_a_full_drain() {
		// Amounts that keep the minimum balance, and amounts that would
		// leave less than it, stay `Preserve`: the implementation then either
		// succeeds exactly (90) or rejects outright (95), never folding dust.
		// The full balance and overshoots may consume the account; overshoots
		// fail at the implementation as plain insufficient balance.
		let cases = [
			(0, Preservation::Preserve),
			(90, Preservation::Preserve),
			(95, Preservation::Preserve),
			(99, Preservation::Preserve),
			(100, Preservation::Expendable),
			(101, Preservation::Expendable),
			(u64::MAX, Preservation::Expendable),
		];
		for (amount, expected) in cases {
			let preservation = debit_preservation::<Funded, _>((), &0, amount);

			assert_eq!(preservation, expected, "amount = {amount}");
		}
	}
}
