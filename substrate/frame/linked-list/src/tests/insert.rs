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

use crate::{mock::*, Event, ListError, ListMetas, Position, SortedListInterface};
use frame::testing_prelude::{assert_noop, assert_ok};

#[test]
fn insert_into_empty_list_creates_list() {
	build_and_execute(|| {
		// The first item creates the list: `ListCreated` then `ItemInserted`.
		let steps = LinkedList::insert(1, 100, 50, Position::endpoints_only()).unwrap();
		assert_eq!(steps, 0);
		assert_eq!(dump(1), vec![(100, 50)]);
		assert_eq!(
			linked_list_events(),
			vec![
				Event::ListCreated { list_id: 1 },
				Event::ItemInserted { list_id: 1, item: 100, priority: 50 },
			]
		);

		// A second item into the same list must not re-emit `ListCreated`.
		System::reset_events();
		insert(1, 200, 40);
		assert_eq!(
			linked_list_events(),
			vec![Event::ItemInserted { list_id: 1, item: 200, priority: 40 }]
		);
	});
}

/// Seeds list 1 with 100(90) and 200(50), leaving a head, a middle and a
/// tail slot for item 150.
fn build_two_item_list() {
	insert(1, 100, 90);
	insert(1, 200, 50);
}

#[test]
fn insert_with_exact_head_hint_takes_zero_steps() {
	build_and_execute(|| {
		build_two_item_list();
		assert_eq!(LinkedList::insert(1, 150, 95, Position::at_head(100)), Ok(0));
		assert_eq!(dump(1), vec![(150, 95), (100, 90), (200, 50)]);
	});
}

#[test]
fn insert_with_exact_between_hint_takes_zero_steps() {
	build_and_execute(|| {
		build_two_item_list();
		assert_eq!(LinkedList::insert(1, 150, 70, Position::between(100, 200)), Ok(0));
		assert_eq!(dump(1), vec![(100, 90), (150, 70), (200, 50)]);
	});
}

#[test]
fn insert_with_exact_tail_hint_takes_zero_steps() {
	build_and_execute(|| {
		build_two_item_list();
		assert_eq!(LinkedList::insert(1, 150, 10, Position::at_tail(200)), Ok(0));
		assert_eq!(dump(1), vec![(100, 90), (200, 50), (150, 10)]);
	});
}

#[test]
fn insert_same_priority_lands_at_tail_side_of_cluster() {
	build_and_execute(|| {
		insert(1, 1, 50);
		insert(1, 2, 50);
		insert(1, 3, 50);
		assert_eq!(dump(1), vec![(1, 50), (2, 50), (3, 50)]);
		assert_eq!(LinkedList::iter_from_tail(1, 3), vec![3, 2, 1]);
		// Tail-side placement makes `pop_tail` LIFO within the cluster.
		assert_eq!(LinkedList::pop_tail(&1).unwrap(), Some((3, 50)));
		assert_eq!(LinkedList::pop_tail(&1).unwrap(), Some((2, 50)));
	});
}

#[test]
fn insert_existing_item_errors_before_hint_repair() {
	build_and_execute(|| {
		// Chain length must exceed `MaxHintRepairSteps` so the hint walk would
		// otherwise be exercised; the duplicate check has to fire first.
		build_long_chain();
		assert_noop!(
			LinkedList::insert(1, 1, 5, Position::at_head(1)),
			ListError::ItemAlreadyExists
		);
	});
}

#[test]
fn interior_operations_do_not_touch_list_meta() {
	build_and_execute_no_post_check(|| {
		insert(1, 1, 90);
		insert(1, 2, 70);
		insert(1, 3, 50);
		insert(1, 4, 30);
		insert(1, 5, 10);

		// Drop the meta row: interior insert, interior-to-interior re-insert,
		// and interior remove must neither consult nor recreate `ListMetas`.
		ListMetas::<Test>::remove(1);

		assert_eq!(LinkedList::insert(1, 99, 60, Position::between(2, 3)), Ok(0));
		assert!(!ListMetas::<Test>::contains_key(1));

		assert_eq!(
			LinkedList::re_insert(1, 99, 40, Position::between(3, 4)),
			Ok(crate::Outcome::Relocated { steps: 0 })
		);
		assert!(!ListMetas::<Test>::contains_key(1));

		assert_ok!(LinkedList::remove(&1, &99));
		assert!(!ListMetas::<Test>::contains_key(1));
	});
}
