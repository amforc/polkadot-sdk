//! Amount selection for fungible debits around the minimum-balance boundary.

use frame::deps::frame_support::traits::{
	fungibles,
	tokens::{Fortitude, Preservation},
};

/// The [`Preservation`] under which debiting exactly `amount` cannot fold
/// dust: `Expendable` only when the debit takes the whole reducible balance —
/// leaving no remainder to fold — and `Preserve` otherwise, so the
/// implementation itself rejects a debit that would strand a sub-minimum
/// remainder.
///
/// The fungibles traits validate every debit except this one input, which is
/// caller intent they cannot derive: may the operation consume the account?
/// With the flag chosen here, fixed-amount call sites need no further checks
/// of their own — the trait method either moves exactly `amount` or fails.
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

/// [`fungibles::Inspect::reducible_balance`] refined for a single debit: the
/// greatest amount at or below `limit` that a `Precision::Exact` debit removes
/// with no side effects on the rest of the account, paired with the
/// [`Preservation`] to pass. For operations that size themselves to what an
/// account can pay; fixed-amount operations only need [`debit_preservation`].
///
/// Implementations fold a sub-minimum remainder into `Expendable` debits even
/// under `Precision::Exact`, and hard-fail `Preserve` debits that would leave
/// one. Picking the amount here instead resolves that dead zone up front:
/// requests inside it round down to the preserving limit, and requests at or
/// above the whole reducible balance become a full `Expendable` drain. The
/// caller then invokes the fungibles traits directly with the returned pair.
///
/// Callers deciding *whether* to debit rather than *how much* compare the
/// returned amount against their request and refuse on a shortfall.
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
	if limit <= preserved {
		return (limit, Preservation::Preserve);
	}
	let expendable =
		Assets::reducible_balance(asset, who, Preservation::Expendable, Fortitude::Polite);
	debug_assert!(preserved <= expendable);
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
		assert_eq!(reducible_debit::<Funded, _>((), &0, 0), (0, Preservation::Preserve));
		assert_eq!(reducible_debit::<Funded, _>((), &0, 90), (90, Preservation::Preserve));
	}

	#[test]
	fn dead_zone_rounds_down_to_the_preserving_limit() {
		// 91..=99 all leave a sub-minimum remainder; the amount caps at 90.
		for limit in [91, 95, 99] {
			assert_eq!(reducible_debit::<Funded, _>((), &0, limit), (90, Preservation::Preserve));
		}
	}

	#[test]
	fn full_or_overshooting_requests_drain() {
		for limit in [100, 101, u64::MAX] {
			assert_eq!(
				reducible_debit::<Funded, _>((), &0, limit),
				(100, Preservation::Expendable)
			);
		}
	}

	#[test]
	fn empty_account_yields_a_zero_drain() {
		type Empty = OneAccount<0, 10>;
		assert_eq!(reducible_debit::<Empty, _>((), &0, 5), (0, Preservation::Expendable));
		assert_eq!(reducible_debit::<Empty, _>((), &0, 0), (0, Preservation::Preserve));
	}

	#[test]
	fn preservation_expends_only_a_full_drain() {
		// Below and inside the dead zone stay `Preserve`: the implementation
		// then either succeeds exactly (90) or rejects outright (95), never
		// folding dust.
		for amount in [0, 90, 95, 99] {
			assert_eq!(debit_preservation::<Funded, _>((), &0, amount), Preservation::Preserve);
		}
		// The full balance and overshoots may consume the account; overshoots
		// fail at the implementation as plain insufficient balance.
		for amount in [100, 101, u64::MAX] {
			assert_eq!(debit_preservation::<Funded, _>((), &0, amount), Preservation::Expendable);
		}
	}
}
