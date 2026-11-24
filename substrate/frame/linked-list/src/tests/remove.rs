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
use frame::testing_prelude::{assert_err, assert_noop, assert_ok};

#[test]
fn remove_only_item_drops_meta_row() {
	build_and_execute(|| {
		insert(1, 100, 50);
		assert_ok!(LinkedList::remove(&1, &100));
		assert!(!ListMetas::<Test>::contains_key(1));
		System::assert_has_event(Event::ItemRemoved { list_id: 1, item: 100, priority: 50 }.into());
		System::assert_last_event(Event::ListRemoved { list_id: 1 }.into());
	});
}

/// Seeds list 1 with 100(90), 150(70) and 200(50): a head, a middle and a
/// tail to splice out.
fn build_three_item_list() {
	insert(1, 100, 90);
	insert(1, 150, 70);
	insert(1, 200, 50);
}

#[test]
fn remove_splices_out_head() {
	build_and_execute(|| {
		build_three_item_list();
		assert_ok!(LinkedList::remove(&1, &100));
		assert_eq!(dump(1), vec![(150, 70), (200, 50)]);
	});
}

#[test]
fn remove_splices_out_middle() {
	build_and_execute(|| {
		build_three_item_list();
		assert_ok!(LinkedList::remove(&1, &150));
		assert_eq!(dump(1), vec![(100, 90), (200, 50)]);
	});
}

#[test]
fn remove_splices_out_tail() {
	build_and_execute(|| {
		build_three_item_list();
		assert_ok!(LinkedList::remove(&1, &200));
		assert_eq!(dump(1), vec![(100, 90), (150, 70)]);
	});
}

#[test]
fn operations_on_absent_item_or_empty_list() {
	build_and_execute(|| {
		assert_noop!(LinkedList::remove(&1, &100), ListError::ItemNotFound);
		assert_noop!(
			LinkedList::re_insert(1, 100, 50, Position::endpoints_only()),
			ListError::ItemNotFound
		);
	});
}

#[test]
#[cfg_attr(debug_assertions, should_panic = "Defensive failure has been triggered")]
fn remove_at_missing_meta_row_is_defensive() {
	build_and_execute_no_post_check(|| {
		insert(1, 100, 50);
		// Drop the metadata row while the node persists: a present node without a
		// meta row violates the all-or-nothing invariant.
		ListMetas::<Test>::remove(1);
		assert_err!(LinkedList::remove(&1, &100), ListError::CorruptList);
	});
}

#[test]
#[cfg_attr(debug_assertions, should_panic = "prev and next name the same neighbor")]
fn remove_at_aliased_neighbors_is_defensive() {
	build_and_execute_no_post_check(|| {
		insert(1, 100, 90);
		insert(1, 200, 50);
		// Corrupt 200 into a two-node cycle with 100: both sides name the same
		// neighbor. Without the guard the two splice writes would target the
		// same row and the second would clobber the first.
		corrupt_node(1, 200, |n| {
			n.prev = Some(100);
			n.next = Some(100);
		});
		assert_err!(LinkedList::remove(&1, &200), ListError::CorruptList);
	});
}
