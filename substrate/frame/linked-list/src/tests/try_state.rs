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

//! `try_state` invariant tests. The pallet's `try_state` is exercised
//! continuously by `build_and_execute`'s post-test check; these tests
//! additionally prove that deliberate corruption is detected.

use crate::{mock::*, ListMeta, ListMetas, ListNodes, Node};
use frame::testing_prelude::assert_err;

#[test]
fn corrupt_link_cycle_detected() {
	build_and_execute_no_post_check(|| {
		insert(1, 1, 50);
		insert(1, 2, 30);
		// Forge a cycle: 2.next = 1.
		corrupt_node(1, 2, |n| n.next = Some(1));
		// Patch the meta to match. The forged tail (1) still links onward, so
		// the tail-endpoint check catches the cycle before the walk starts.
		ListMetas::<Test>::mutate_extant(1, |m| {
			m.tail = 1;
		});
		assert_err!(<LinkedList>::do_try_state(), "tail node has non-None next");
	});
}

#[test]
fn empty_list_with_stale_meta_detected() {
	build_and_execute_no_post_check(|| {
		ListMetas::<Test>::insert(1, ListMeta { head: 1, tail: 1 });
		assert_err!(<LinkedList>::do_try_state(), "ListMetas present without ListNodes");
	});
}

#[test]
fn dangling_node_pointer_detected() {
	build_and_execute_no_post_check(|| {
		insert(1, 1, 50);
		insert(1, 2, 30);
		// 1.next points to a non-existent item.
		ListNodes::<Test>::insert(1, 1, Node { prev: None, next: Some(999u64), priority: 50u32 });
		assert_err!(<LinkedList>::do_try_state(), "forward walk: missing node");
	});
}

#[test]
fn orphan_unreachable_node_detected() {
	build_and_execute_no_post_check(|| {
		insert(1, 1, 50);
		insert(1, 2, 30);
		// Add a row not on the head→tail chain. The forward/reverse walks
		// still see exactly the 2 reachable nodes, so only the total-node-count
		// check catches this.
		ListNodes::<Test>::insert(1, 999u64, Node { prev: None, next: None, priority: 100u32 });
		assert_err!(
			<LinkedList>::do_try_state(),
			"orphan ListNodes rows: total count differs from chain length"
		);
	});
}

#[test]
fn list_nodes_without_meta_detected() {
	build_and_execute_no_post_check(|| {
		ListNodes::<Test>::insert(1, 1u64, Node { prev: None, next: None, priority: 50u32 });
		assert_err!(<LinkedList>::do_try_state(), "ListNodes present without ListMetas");
	});
}
