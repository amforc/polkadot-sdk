//! Rate-index (`VaultListId::Rate`) ordering: insertion, removal, and
//! re-insertion of vaults keyed by their annual borrow rate.
//!
//! The hint-repair walk (owned by `pallet-linked-list`) is exercised here too:
//! every `change_rate` / re-insert below passes `Position::endpoints_only()`, the
//! maximally-stale hint, so the pallet's repair walk lands it correctly.
//! `hint_helpers.rs` covers the rollback when a hint is past the repair budget.

use crate::{
	mock::*,
	tests::{percent, ONE_DAY_MS},
};
use pallet_linked_list::SortedListInterface;

// The rate index orders vaults by annual rate (tail-first walk is lowest rate → highest) whatever
// order they open in. Collateral and debt adjustments leave a vault in place; only a rate change
// re-inserts it.
#[test]
fn rate_index_orders_by_rate_and_moves_only_on_rate_change() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		let rate_order = || LinkedList::iter_from_tail(VaultList::Rate(DOT, STABLE), 10);
		for (who, pct) in [(3, 30), (1, 5), (5, 50), (2, 10), (4, 40)] {
			assert_ok!(open(who, DOT, STABLE, 1_000, 500, percent(pct)));
		}

		assert_eq!(rate_order(), alloc::vec![1, 2, 3, 4, 5]);

		assert_ok!(deposit_collateral(1, DOT, STABLE, 1, 100));
		assert_ok!(borrow(2, DOT, STABLE, 50, None));
		assert_ok!(repay(3, DOT, STABLE, 3, Some(50)));

		assert_eq!(rate_order(), alloc::vec![1, 2, 3, 4, 5]);

		advance_time(2 * ONE_DAY_MS);
		// Acct 3 moves from 30% to the tail at 1%, acct 1 from 5% to the head at 60%.
		assert_ok!(change_rate(3, DOT, STABLE, percent(1)));
		assert_ok!(change_rate(1, DOT, STABLE, percent(60)));

		assert_eq!(rate_order(), alloc::vec![3, 2, 4, 5, 1]);
	});
}

// Both lists are keyed by vault owner, and `try_state` holds each to the branch's vault rows: a
// node for an owner with no row would be redeemed against, or recovered, with nothing behind it.
#[test]
fn try_state_rejects_a_list_node_with_no_vault_row() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// given
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		let stray: AccountId = 9;
		assert!(!vault_exists(DOT, STABLE, stray));

		for (list, defect) in [
			(VaultList::Rate(DOT, STABLE), "rate index holds a node with no vault row"),
			(VaultList::FinalRecovery(DOT, STABLE), "recovery FIFO holds a node with no vault row"),
		] {
			hypothetically!({
				// when
				assert_ok!(LinkedList::insert(list, stray, percent(5), Position::endpoints_only()));

				// then
				assert_eq!(crate::try_state::do_try_state::<Test>(), Err(defect.into()));
			});
		}
	});
}
