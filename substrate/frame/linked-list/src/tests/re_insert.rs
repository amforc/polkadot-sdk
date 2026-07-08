// This file is part of Substrate.

// Copyright (C) Amforc AG.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{mock::*, Event, ListError, ListNodes, Outcome, Position, SortedListInterface};
use frame::testing_prelude::{assert_ok, assert_storage_noop};

#[test]
fn re_insert_unchanged_priority_no_op() {
	build_and_execute(|| {
		insert(1, 100, 50);
		assert_eq!(
			LinkedList::re_insert(1, 100, 50, Position::endpoints_only()),
			Ok(Outcome::NoOp)
		);
		assert_eq!(dump(1), vec![(100, 50)]);
	});
}

#[test]
fn re_insert_emits_no_lifecycle_events() {
	build_and_execute(|| {
		insert(1, 10, 90);
		insert(1, 20, 50);
		insert(1, 30, 10);
		System::reset_events();
		// Relocate the tail (30) above the head — forces the splice + re-insert path.
		assert_eq!(
			LinkedList::re_insert(1, 30, 100, Position::at_head(10)),
			Ok(Outcome::Relocated { steps: 0 })
		);
		// The item stays in the list: only `ItemReinserted`, never `ListCreated`/`ListRemoved`.
		assert_eq!(System::events().len(), 1);
		System::assert_last_event(
			Event::ItemReinserted { list_id: 1, item: 30, old_priority: 10, new_priority: 100 }
				.into(),
		);
	});
}

#[test]
fn re_insert_in_place_when_position_still_valid() {
	build_and_execute(|| {
		insert(1, 100, 90);
		insert(1, 200, 50);
		insert(1, 300, 10);
		// Drop 200 from 50 → 30: still strictly less than 100 (90) and strictly
		// greater than 300 (10). Position-validity check passes; in-place update.
		assert_eq!(
			LinkedList::re_insert(1, 200, 30, Position::endpoints_only()),
			Ok(Outcome::InPlace)
		);
		assert_eq!(dump(1), vec![(100, 90), (200, 30), (300, 10)]);
		System::assert_has_event(
			Event::ItemReinserted { list_id: 1, item: 200, old_priority: 50, new_priority: 30 }
				.into(),
		);
	});
}

#[test]
fn re_insert_priority_increase_moves_toward_head() {
	build_and_execute(|| {
		insert(1, 1, 90);
		insert(1, 2, 50);
		insert(1, 3, 10);
		let hint = LinkedList::find_re_insert_position(1, 3, 95).unwrap();
		assert_eq!(LinkedList::re_insert(1, 3, 95, hint), Ok(Outcome::Relocated { steps: 0 }));
		assert_eq!(dump(1), vec![(3, 95), (1, 90), (2, 50)]);
	});
}

#[test]
fn re_insert_priority_decrease_moves_toward_tail() {
	build_and_execute(|| {
		insert(1, 1, 90);
		insert(1, 2, 50);
		insert(1, 3, 10);
		let hint = LinkedList::find_re_insert_position(1, 1, 5).unwrap();
		assert_ok!(LinkedList::re_insert(1, 1, 5, hint));
		assert_eq!(dump(1), vec![(2, 50), (3, 10), (1, 5)]);
	});
}

#[test]
fn re_insert_unknown_errors() {
	build_and_execute(|| {
		assert_storage_noop!(assert_eq!(
			LinkedList::re_insert(1, 100, 50, Position::endpoints_only()),
			Err(ListError::ItemNotFound)
		));
	});
}

/// Slow-path atomicity: when `walk_repair` exceeds the budget, the prior
/// `remove_at` must roll back so the item is still present after the failed
/// `re_insert`. This is the regression guard for the `with_transaction_opaque_err`
/// wrap.
#[test]
fn re_insert_slow_path_failure_leaves_storage_untouched() {
	build_and_execute(|| {
		// Build a chain longer than `MaxHintRepairSteps`.
		let chain_len = MaxHintRepairSteps::get() + 4;
		for i in 1..=chain_len {
			insert(1, u64::from(i), 100 - 10 * i + 10);
		}
		// Re-insert item 1 at priority 5 (tail-ward) but supply head hints; the
		// repair walk distance exceeds budget, so re_insert errors. The item
		// must still be in the list at its old position.
		assert_storage_noop!(assert_eq!(
			LinkedList::re_insert(1, 1, 5, Position::at_head(1)),
			Err(ListError::InvalidPositionHints)
		));
	});
}

/// The same-priority fast path deliberately skips link validation: the exact corruption the
/// mutating paths reject below goes unnoticed, nothing is written, and no event is deposited.
#[test]
fn re_insert_no_op_skips_link_validation() {
	build_and_execute_no_post_check(|| {
		insert(1, 100, 90);
		insert(1, 200, 50);
		insert(1, 300, 10);
		// 100's forward link skips 200.
		ListNodes::<Test>::mutate(1, 100, |maybe| {
			if let Some(node) = maybe {
				node.next = Some(300);
			}
		});
		System::reset_events();
		assert_storage_noop!(assert_eq!(
			LinkedList::re_insert(1, 200, 50, Position::endpoints_only()),
			Ok(Outcome::NoOp)
		));
		assert_eq!(System::events(), vec![]);
	});
}

/// Mutating twin of [`re_insert_no_op_skips_link_validation`]: the same
/// corruption with a changed priority hits link validation and refuses to
/// write, even though the cached neighbor priorities admit the new value
/// (90 >= 60 > 10) and the pre-validation code would have mutated in place.
#[test]
#[should_panic = "validate_node_links: prev neighbor does not link back to item"]
fn re_insert_in_place_broken_prev_back_link_is_defensive() {
	build_and_execute_defensive(|| {
		insert(1, 100, 90);
		insert(1, 200, 50);
		insert(1, 300, 10);
		// 100's forward link skips 200.
		ListNodes::<Test>::mutate(1, 100, |maybe| {
			if let Some(node) = maybe {
				node.next = Some(300);
			}
		});
		let _ = LinkedList::re_insert(1, 200, 60, Position::endpoints_only());
	});
}

/// Tail-side mirror of [`re_insert_in_place_broken_prev_back_link_is_defensive`].
#[test]
#[should_panic = "validate_node_links: next neighbor does not link back to item"]
fn re_insert_in_place_broken_next_back_link_is_defensive() {
	build_and_execute_defensive(|| {
		insert(1, 100, 90);
		insert(1, 200, 50);
		insert(1, 300, 10);
		// 300's backward link skips 200.
		ListNodes::<Test>::mutate(1, 300, |maybe| {
			if let Some(node) = maybe {
				node.prev = Some(100);
			}
		});
		let _ = LinkedList::re_insert(1, 200, 60, Position::endpoints_only());
	});
}

#[test]
#[should_panic = "validate_node_links: node linked against itself"]
fn re_insert_self_loop_is_defensive() {
	build_and_execute_defensive(|| {
		insert(1, 100, 50);
		// 100's `prev` names itself, forming a self-loop.
		ListNodes::<Test>::mutate(1, 100, |maybe| {
			if let Some(node) = maybe {
				node.prev = Some(100);
			}
		});
		let _ = LinkedList::re_insert(1, 100, 60, Position::endpoints_only());
	});
}

#[test]
#[should_panic = "validate_node_links: prev and next name the same neighbor"]
fn re_insert_same_neighbor_both_sides_is_defensive() {
	build_and_execute_defensive(|| {
		insert(1, 100, 90);
		insert(1, 200, 50);
		insert(1, 300, 10);
		// 200 claims 100 on both sides.
		ListNodes::<Test>::mutate(1, 200, |maybe| {
			if let Some(node) = maybe {
				node.next = Some(100);
			}
		});
		let _ = LinkedList::re_insert(1, 200, 60, Position::endpoints_only());
	});
}

#[test]
#[should_panic = "validate_node_links: prev neighbor row is missing"]
fn re_insert_dangling_prev_is_defensive() {
	build_and_execute_defensive(|| {
		insert(1, 100, 90);
		insert(1, 200, 50);
		// 200's `prev` names an item with no stored row.
		ListNodes::<Test>::mutate(1, 200, |maybe| {
			if let Some(node) = maybe {
				node.prev = Some(999);
			}
		});
		let _ = LinkedList::re_insert(1, 200, 60, Position::endpoints_only());
	});
}

#[test]
#[should_panic = "validate_node_links: next neighbor row is missing"]
fn re_insert_dangling_next_is_defensive() {
	build_and_execute_defensive(|| {
		insert(1, 100, 90);
		insert(1, 200, 50);
		// 100's `next` names an item with no stored row.
		ListNodes::<Test>::mutate(1, 100, |maybe| {
			if let Some(node) = maybe {
				node.next = Some(999);
			}
		});
		let _ = LinkedList::re_insert(1, 100, 95, Position::endpoints_only());
	});
}

#[test]
#[should_panic = "validate_node_links: head pointer disagrees with head-claiming node"]
fn re_insert_false_head_claim_is_defensive() {
	build_and_execute_defensive(|| {
		insert(1, 100, 90);
		insert(1, 200, 50);
		// 200 falsely claims to be the head; the meta row still names 100.
		ListNodes::<Test>::mutate(1, 200, |maybe| {
			if let Some(node) = maybe {
				node.prev = None;
			}
		});
		let _ = LinkedList::re_insert(1, 200, 60, Position::endpoints_only());
	});
}

#[test]
#[should_panic = "validate_node_links: tail pointer disagrees with tail-claiming node"]
fn re_insert_false_tail_claim_is_defensive() {
	build_and_execute_defensive(|| {
		insert(1, 100, 90);
		insert(1, 200, 50);
		// 100 falsely claims to be the tail; the meta row still names 200.
		ListNodes::<Test>::mutate(1, 100, |maybe| {
			if let Some(node) = maybe {
				node.next = None;
			}
		});
		let _ = LinkedList::re_insert(1, 100, 95, Position::endpoints_only());
	});
}
