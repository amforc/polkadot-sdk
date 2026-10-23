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

use crate::{mock::*, ListNodes, Node, Position};

#[test]
fn find_position_orders_by_priority_with_ties_on_tail_side() {
	build_and_execute(|| {
		assert_eq!(LinkedList::find_position(1, 50), Position::endpoints_only());

		insert(1, 1, 90);
		insert(1, 2, 50);
		insert(1, 3, 10);
		assert_eq!(LinkedList::find_position(1, 100), Position::at_head(1));
		assert_eq!(LinkedList::find_position(1, 5), Position::at_tail(3));
		assert_eq!(LinkedList::find_position(1, 70), Position::between(1, 2));
		// A tie lands on the tail side of the cluster: the walk only stops once
		// `priority > node.priority`, and 50 > 50 is false, so it steps past 2.
		assert_eq!(LinkedList::find_position(1, 50), Position::between(2, 3));

		// Re-inserting 2 at 40 treats it as already removed: without skipping
		// itself the walk would stop at between(2, 3).
		assert_eq!(LinkedList::find_re_insert_position(1, 2, 40), Some(Position::between(1, 3)));
		assert_eq!(LinkedList::find_re_insert_position(1, 999, 40), None);
	});
}

/// A cyclic corrupt list must terminate the head→tail walk (capped at
/// `len + 1` visits) instead of hanging the caller.
#[test]
fn find_position_terminates_on_cyclic_corruption() {
	build_and_execute_no_post_check(|| {
		insert(1, 1, 90);
		insert(1, 2, 50);
		// Forge a cycle through the head: 2.next = 1.
		ListNodes::<Test>::insert(
			1,
			2,
			Node { prev: Some(1u64), next: Some(1u64), priority: 50u32 },
		);
		// Priority 5 never satisfies `priority > node.priority`, so an
		// unbounded walk would loop forever chasing the cycle.
		let position = LinkedList::find_position(1, 5);
		assert_eq!(position.next, None);
		// The skip-aware variant shares the same bounded walk.
		let re_insert_position = LinkedList::find_re_insert_position(1, 1, 5);
		assert!(re_insert_position.is_some());
	});
}

#[test]
fn node_returns_priority_and_position_or_none() {
	build_and_execute(|| {
		insert(1, 1, 90);
		insert(1, 2, 50);
		insert(1, 3, 10);
		assert_eq!(LinkedList::node(1, 1), Some((90, Position::at_head(2))));
		assert_eq!(LinkedList::node(1, 2), Some((50, Position::between(1, 3))));
		assert_eq!(LinkedList::node(1, 3), Some((10, Position::at_tail(2))));
		assert_eq!(LinkedList::node(1, 999), None);
	});
}
