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

use crate::{mock::*, ListError, Position, SortedListInterface};
use frame::testing_prelude::{assert_err, assert_noop};

fn build_chain(priorities: &[(ItemId, Priority)]) {
	for &(item, priority) in priorities {
		insert(1, item, priority);
	}
}

/// Dry-run `hint` through `repair_steps_needed`, then insert item 99 with it:
/// the view must predict exactly the repair steps the insert spends.
fn assert_view_matches_insert(priority: Priority, hint: Position<ItemId>) -> u32 {
	let steps = LinkedList::repair_steps_needed(&1, priority, hint.clone());
	assert_eq!(LinkedList::insert(1, 99, priority, hint), Ok(steps));
	steps
}

/// A short chain whose gaps leave room for item 99 at any position.
fn build_short_chain() {
	build_chain(&[(1, 90), (2, 70), (3, 50)]);
}

#[test]
fn repair_steps_needed_is_zero_for_valid_hint() {
	build_and_execute(|| {
		build_short_chain();
		assert_eq!(assert_view_matches_insert(80, Position::between(1, 2)), 0);
	});
}

#[test]
fn repair_steps_needed_counts_stale_hint() {
	build_and_execute(|| {
		build_short_chain();
		assert!(assert_view_matches_insert(60, Position::at_head(1)) > 0);
	});
}

/// `Some(999)` is dangling. Even though priority 80 is positionally between
/// (1, 2), `walk_repair` must spend at least one step clamping the invalid
/// `next`; a head-positional approximation of the cost once missed this.
#[test]
fn repair_steps_needed_counts_clamping_dangling_next() {
	build_and_execute(|| {
		build_short_chain();
		assert!(assert_view_matches_insert(80, Position::between(1, 999)) > 0);
	});
}

/// MaxHintRepairSteps = 4 (set in mock). The caller thinks priority 15 goes
/// between 1 and 2 (head region); the real position is between 4 and 5,
/// 3 nodes away, within budget.
#[test]
fn repair_steps_needed_matches_insert_within_budget() {
	build_and_execute(|| {
		build_chain(&[(1, 100), (2, 80), (3, 50), (4, 20), (5, 10)]);
		let steps = assert_view_matches_insert(15, Position::between(1, 2));
		assert!(steps > 0);
		assert!(steps <= MaxHintRepairSteps::get());
		assert_eq!(dump(1), vec![(1, 100), (2, 80), (3, 50), (4, 20), (99, 15), (5, 10)]);
	});
}

/// Past the budget the view reports an infeasible step count and the insert
/// fails without touching storage: priority 5 belongs at the tail of a chain
/// longer than `MaxHintRepairSteps`, but the hint claims the head.
#[test]
fn repair_steps_needed_exceeds_budget_and_insert_fails() {
	build_and_execute(|| {
		build_long_chain();
		let steps = LinkedList::repair_steps_needed(&1, 5, Position::at_head(1));
		assert!(steps > MaxHintRepairSteps::get());
		assert_noop!(
			LinkedList::insert(1, 99, 5, Position::at_head(1)),
			ListError::InvalidPositionHints
		);
	});
}

#[test]
fn strict_mode_zero_budget_accepts_valid_hint_rejects_invalid() {
	build_and_execute(|| {
		MaxHintRepairSteps::set(0);
		// An empty-list insert with the empty-position hint is already valid.
		let steps = LinkedList::insert(1, 100, 90, Position::endpoints_only()).expect("valid hint");
		assert_eq!(steps, 0);

		// A second insert at the tail with a perfect hint is also 0 steps.
		let steps = LinkedList::insert(1, 200, 50, Position::at_tail(100)).expect("valid hint");
		assert_eq!(steps, 0);

		// Any stale hint must fail immediately — no walk loop runs at budget 0.
		assert_noop!(
			LinkedList::insert(1, 300, 70, Position::at_head(100)),
			ListError::InvalidPositionHints
		);
	});
}

/// The walk detects the no-progress repair and reports corruption.
#[test]
#[cfg_attr(debug_assertions, should_panic = "repair step made no progress")]
fn walk_repair_asymmetric_links_is_defensive() {
	build_and_execute_no_post_check(|| {
		build_chain(&[(1, 90), (2, 70), (3, 50)]);
		// Break the back-link: 2.next still names 3, but 3.prev now names 1.
		corrupt_node(1, 3, |n| n.prev = Some(1));
		// Hint (2, 3) with priority 60: the prev side is consistent
		// (2.next == 3) and admits 60, but the broken back-link makes the
		// position invalid while the reanchor rewrite is a no-op.
		assert_err!(LinkedList::insert(1, 99, 60, Position::between(2, 3)), ListError::CorruptList);
	});
}

#[test]
fn inconsistent_hint_one_side_stale_re_anchors() {
	build_and_execute(|| {
		// List head→tail: items 1..=N with priorities 100, 90, 80, ..., chosen so that
		// the correct insert position for priority 85 is between item 2 (90) and
		// item 3 (80). The caller's hint claims (tail=last, Some(item 3)),
		// which is link-inconsistent; the tail is not item 3's prev.
		// Walking from prev=tail toward the head would need more than
		// `MaxHintRepairSteps` steps. Re-anchoring via `next.prev` lands at
		// (item 2, item 3) in 1 step.
		let chain_len = build_long_chain();
		let stale_prev = u64::from(chain_len); // tail item id
		let steps = LinkedList::insert(1, 99, 85, Position::between(stale_prev, 3))
			.expect("re-anchor handles partially-stale hint within budget");
		assert!(steps > 0 && steps <= MaxHintRepairSteps::get());
		// Head→tail: (1,100), (2,90), the inserted (99,85), then the unchanged
		// tail-side suffix (3,80), (4,70), …
		let mut expected = vec![(1u64, 100u32), (2, 90), (99, 85)];
		expected.extend((3..=chain_len).map(|i| (u64::from(i), 100u32 - 10 * i + 10)));
		assert_eq!(dump(1), expected);
	});
}
