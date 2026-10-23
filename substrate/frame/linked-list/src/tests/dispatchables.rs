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

use crate::{mock::*, Error, Event, ListError, Position, SortedListInterface};
use frame::testing_prelude::{assert_noop, assert_ok, assert_storage_noop};

#[test]
fn reprioritize_no_op_when_priority_unchanged() {
	build_and_execute(|| {
		insert(1, 100, 50);
		StaticPriorityProvider::pin_priority(1, 100, 50);
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
fn reprioritize_relocates_with_stale_hint() {
	build_and_execute(|| {
		use crate::weights::WeightInfo;
		insert(1, 1, 90);
		insert(1, 2, 50);
		insert(1, 3, 10);
		// Real priority for item 2 just rose to 99; the caller's hint is stale
		// (tail region) but the correct head position is within budget.
		StaticPriorityProvider::pin_priority(1, 2, 99);
		let post_info =
			LinkedList::reprioritize(RuntimeOrigin::signed(1), 1, 2, Position::at_tail(3))
				.expect("stale hint is repaired within budget");
		assert_eq!(dump(1), vec![(2, 99), (1, 90), (3, 10)]);
		// Charged for the two repair steps actually walked, not the full budget.
		assert_eq!(post_info.actual_weight, Some(<() as WeightInfo>::reprioritize_relocate(2)));
		// `ItemReinserted` is the single event surface for priority changes,
		// whether they arrive via the trait or via the dispatchable.
		System::assert_has_event(
			Event::ItemReinserted { list_id: 1, item: 2, old_priority: 50, new_priority: 99 }
				.into(),
		);
	});
}

/// The early item-not-found exit refunds down to the cheapest benchmarked
/// path instead of charging the full relocate-budget reservation.
#[test]
fn reprioritize_unknown_item_errors_and_refunds() {
	build_and_execute(|| {
		use crate::weights::WeightInfo;
		// No priority in StaticPriorities → PriorityProvider returns None.
		assert_storage_noop!({
			let err = LinkedList::reprioritize(
				RuntimeOrigin::signed(1),
				1,
				100,
				Position::endpoints_only(),
			)
			.expect_err("item is not in the list");
			assert_eq!(err.error, Error::<Test>::List(ListError::ItemNotFound).into());
			assert_eq!(err.post_info.actual_weight, Some(<() as WeightInfo>::reprioritize_no_op()));
		});
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
		let insert_steps = LinkedList::repair_steps_needed(&1, 55, hint.clone());
		assert_eq!(insert_steps, 3);
		// The dispatch-faithful view sees the post-splice walk exceed the budget.
		let re_insert_steps = LinkedList::re_insert_steps_needed(&1, &2, 55, hint.clone());
		assert!(re_insert_steps > MaxHintRepairSteps::get());
		// And the dispatch agrees with it, not with `repair_steps_needed`.
		StaticPriorityProvider::pin_priority(1, 2, 55);
		assert_noop!(
			LinkedList::reprioritize(RuntimeOrigin::signed(1), 1, 2, hint),
			Error::<Test>::List(ListError::InvalidPositionHints)
		);
	});
}

/// `re_insert_steps_needed` dry-runs the real `re_insert` and rolls it back:
/// a relocation reports the walk it took without leaving any trace in storage,
/// and the fast paths never consult the hint, so even a garbage hint must
/// yield `0` — and the dispatch must succeed.
#[test]
fn re_insert_steps_needed_dry_runs_re_insert() {
	build_and_execute(|| {
		insert(1, 1, 100);
		insert(1, 2, 90);
		insert(1, 3, 80);
		// After splicing 2 out, the head hint walks 1 → 3 → tail: two steps.
		assert_storage_noop!(assert_eq!(
			LinkedList::re_insert_steps_needed(&1, &2, 5, Position::at_head(1)),
			2
		));

		// 85 still fits between 1 (100) and 3 (80): in-place fast path.
		let garbage_hint = Position::between(98, 99);
		assert_eq!(LinkedList::re_insert_steps_needed(&1, &2, 85, garbage_hint.clone()), 0);
		StaticPriorityProvider::pin_priority(1, 2, 85);
		assert_ok!(LinkedList::reprioritize(RuntimeOrigin::signed(1), 1, 2, garbage_hint));
		assert_eq!(dump(1), vec![(1, 100), (2, 85), (3, 80)]);
	});
}
