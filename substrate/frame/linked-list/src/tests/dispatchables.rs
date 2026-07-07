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

use crate::{mock::*, Error, Event, ListError, Position, PriorityProvider, SortedListInterface};
use frame::testing_prelude::{
	assert_err_ignore_postinfo, assert_noop, assert_ok, assert_storage_noop,
};

/// Pin the authoritative priority via the same trait entry point the
/// benchmarks use.
fn set_real_priority(list_id: ListId, item: ItemId, priority: Priority) {
	StaticPriorityProvider::set_priority(&list_id, &item, priority);
}

#[test]
fn reprioritize_no_op_when_priority_unchanged() {
	build_and_execute(|| {
		insert(1, 100, 50);
		set_real_priority(1, 100, 50);
		assert_ok!(LinkedList::reprioritize(
			RuntimeOrigin::signed(1),
			1,
			100,
			Position::endpoints_only()
		));
		assert_eq!(dump(1), vec![(100, 50)]);
	});
}

#[test]
fn reprioritize_repositions_when_priority_changes() {
	build_and_execute(|| {
		insert(1, 1, 90);
		insert(1, 2, 50);
		insert(1, 3, 10);
		// Real priority for item 2 just rose to 99; reprioritize should move it to head.
		set_real_priority(1, 2, 99);
		// Hint: target's new neighbors (None, Some(1)): head insertion.
		assert_ok!(LinkedList::reprioritize(RuntimeOrigin::signed(1), 1, 2, Position::at_head(1)));
		assert_eq!(dump(1), vec![(2, 99), (1, 90), (3, 10)]);
		// `ItemReinserted` is the single event surface for priority changes,
		// whether they arrive via the trait or via the dispatchable.
		System::assert_has_event(
			Event::ItemReinserted { list_id: 1, item: 2, old_priority: 50, new_priority: 99 }
				.into(),
		);
	});
}

#[test]
fn reprioritize_unknown_item_errors() {
	build_and_execute(|| {
		// No priority in StaticPriorities → PriorityProvider returns None.
		assert_storage_noop!(assert_err_ignore_postinfo!(
			LinkedList::reprioritize(RuntimeOrigin::signed(1), 1, 100, Position::endpoints_only()),
			Error::<Test>::List(ListError::ItemNotFound)
		));
	});
}

/// The early item-not-found exit refunds down to the cheapest benchmarked
/// path instead of charging the full relocate-budget reservation.
#[test]
fn reprioritize_unknown_item_refunds_weight() {
	build_and_execute(|| {
		use crate::weights::WeightInfo;
		let err =
			LinkedList::reprioritize(RuntimeOrigin::signed(1), 1, 100, Position::endpoints_only())
				.expect_err("item is not in the list");
		assert_eq!(err.post_info.actual_weight, Some(<() as WeightInfo>::reprioritize_no_op()),);
	});
}

#[test]
fn reprioritize_removes_existing_item_when_priority_disappears() {
	build_and_execute(|| {
		insert(1, 1, 90);
		insert(1, 2, 50);
		insert(1, 3, 10);

		assert_ok!(LinkedList::reprioritize(
			RuntimeOrigin::signed(1),
			1,
			2,
			Position::endpoints_only()
		));

		assert_eq!(dump(1), vec![(1, 90), (3, 10)]);
		System::assert_has_event(Event::ItemRemoved { list_id: 1, item: 2, priority: 50 }.into());
	});
}

#[test]
fn reprioritize_with_stale_hint_within_budget_succeeds() {
	build_and_execute(|| {
		insert(1, 1, 90);
		insert(1, 2, 50);
		insert(1, 3, 10);
		// Real priority for item 2 just rose to 99; the caller's hint is stale
		// (tail region) but the correct head position is within budget.
		set_real_priority(1, 2, 99);
		assert_ok!(LinkedList::reprioritize(RuntimeOrigin::signed(1), 1, 2, Position::at_tail(3)));
		assert_eq!(dump(1), vec![(2, 99), (1, 90), (3, 10)]);
	});
}

/// `re_insert_steps_needed` simulates the dispatch exactly: the item is
/// spliced out before the walk, so a hint adjacent to the item's own old
/// position degrades. The insert-oriented `repair_steps_needed` green-lights
/// this hint (3 steps with the item still linked), but the dispatch fails.
#[test]
fn re_insert_steps_needed_predicts_dispatch_failure() {
	build_and_execute(|| {
		// Budget = 4 (mock). Chain 1(100)..6(50); item 2 drifts to 55, whose
		// correct post-splice position is between 5 and 6.
		for (i, p) in [(1, 100), (2, 90), (3, 80), (4, 70), (5, 60), (6, 50)] {
			insert(1, i, p);
		}
		let hint = Position::between(2, 3);

		// The insert-oriented view walks with item 2 still linked: 3 steps.
		let insert_steps =
			<LinkedList as SortedListInterface<_, _>>::repair_steps_needed(&1, 55, hint.clone());
		assert_eq!(insert_steps, 3);
		// The dispatch-faithful view sees the post-splice walk exceed the budget.
		let re_insert_steps = <LinkedList as SortedListInterface<_, _>>::re_insert_steps_needed(
			&1,
			&2,
			55,
			hint.clone(),
		);
		assert!(re_insert_steps > MaxHintRepairSteps::get());
		// And the dispatch agrees with it, not with `repair_steps_needed`.
		set_real_priority(1, 2, 55);
		assert_noop!(
			LinkedList::reprioritize(RuntimeOrigin::signed(1), 1, 2, hint),
			Error::<Test>::List(ListError::InvalidPositionHints)
		);
	});
}

/// The fast paths never consult the hint, so even a garbage hint must yield
/// `0` from the dispatch-faithful view — and the dispatch must succeed.
#[test]
fn re_insert_steps_needed_zero_for_in_place_despite_garbage_hint() {
	build_and_execute(|| {
		insert(1, 1, 100);
		insert(1, 2, 90);
		insert(1, 3, 80);
		// 85 still fits between 1 (100) and 3 (80): in-place fast path.
		let garbage_hint = Position::between(98, 99);
		assert_eq!(
			<LinkedList as SortedListInterface<_, _>>::re_insert_steps_needed(
				&1,
				&2,
				85,
				garbage_hint.clone(),
			),
			0
		);
		set_real_priority(1, 2, 85);
		assert_ok!(LinkedList::reprioritize(RuntimeOrigin::signed(1), 1, 2, garbage_hint));
		assert_eq!(dump(1), vec![(1, 100), (2, 85), (3, 80)]);
	});
}

/// The dry-run inside `re_insert_steps_needed` must not leave any trace in
/// storage.
#[test]
fn re_insert_steps_needed_is_read_only() {
	build_and_execute(|| {
		insert(1, 1, 100);
		insert(1, 2, 90);
		insert(1, 3, 80);
		assert_storage_noop!({
			let _ = <LinkedList as SortedListInterface<_, _>>::re_insert_steps_needed(
				&1,
				&2,
				5,
				Position::at_head(1),
			);
		});
	});
}

#[test]
fn reprioritize_with_hint_beyond_budget_errors() {
	build_and_execute(|| {
		// Build a chain longer than `MaxHintRepairSteps` so that a wrong-end
		// hint cannot reach the correct position.
		let chain_len = MaxHintRepairSteps::get() + 4;
		for i in 1..=chain_len {
			insert(1, u64::from(i), 100u32 - 10 * i + 10);
		}
		// Tail item drifts up to 200; correct position is the head, but the
		// supplied hint is at the tail and the budget cannot bridge that gap.
		let tail = u64::from(chain_len);
		set_real_priority(1, tail, 200);
		assert_noop!(
			LinkedList::reprioritize(
				RuntimeOrigin::signed(1),
				1,
				tail,
				Position::at_tail(tail - 1)
			),
			Error::<Test>::List(ListError::InvalidPositionHints)
		);
	});
}
