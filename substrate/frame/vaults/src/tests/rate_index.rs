//! Rate-index (`VaultListId::Rate`) ordering: insertion, removal, and
//! re-insertion of vaults keyed by their annual borrow rate.
//!
//! The hint-repair walk (owned by `pallet-linked-list`) is exercised here too:
//! every `change_rate` / re-insert below passes `Position::endpoints_only()`, the
//! maximally-stale hint, so the pallet's repair walk lands it correctly.
//! `hint_helpers.rs` covers the rollback when a hint is past the repair budget.

use crate::{
	mock::*,
	tests::{rate_pct, ONE_DAY_MS},
};

// Open vaults in arbitrary order; walking the rate index tail-first (lowest
// rate → highest) yields ascending order.
#[test]
fn open_orders_dll_by_annual_interest_rate() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Open in scrambled order with distinct rates.
		for (who, pct) in [(3u64, 30), (1, 5), (5, 50), (2, 10), (4, 40)] {
			assert_ok!(open(who, DOT, PUSD, 1_000, 500, rate_pct(pct, 100)));
		}
		// Tail-first walk gives ascending rate. Expect [1, 2, 3, 4, 5].
		let order = LinkedList::iter_from_tail(rate_list(DOT, PUSD), 10);
		assert_eq!(order, alloc::vec![1, 2, 3, 4, 5]);
	});
}

// `change_rate` re-inserts the vault at its new rate position. Walk through
// several adjustments and assert the final ordering matches the expected
// ascending-by-rate sequence.
#[test]
fn change_rate_re_inserts_in_correct_position() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		for (who, pct) in [(1u64, 10), (2, 20), (3, 30), (4, 40), (5, 50)] {
			assert_ok!(open(who, DOT, PUSD, 1_000, 500, rate_pct(pct, 100)));
		}
		advance_time(2 * ONE_DAY_MS);

		// Move acct 3 from 30% to 5% — should land at the tail.
		assert_ok!(change_rate(3, DOT, PUSD, rate_pct(5, 100)));
		// Move acct 1 from 10% to 60% — should land at the head.
		assert_ok!(change_rate(1, DOT, PUSD, rate_pct(60, 100)));

		// Final ascending order: 3 (5%), 2 (20%), 4 (40%), 5 (50%), 1 (60%).
		let order = LinkedList::iter_from_tail(rate_list(DOT, PUSD), 10);
		assert_eq!(order, alloc::vec![3, 2, 4, 5, 1]);
	});
}
