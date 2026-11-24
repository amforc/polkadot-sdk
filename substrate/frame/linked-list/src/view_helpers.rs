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

//! Read-only helpers used by the [`crate::SortedListInterface`] impl and the
//! `#[pallet::view_functions]` block in `lib.rs`.

#[cfg(any(test, feature = "runtime-benchmarks"))]
use crate::{list, ListError, Outcome, SortedListInterface};
use crate::{pallet::*, Position};
#[cfg(any(test, feature = "runtime-benchmarks"))]
use frame::{
	deps::frame_support::storage::{with_transaction, TransactionOutcome},
	prelude::*,
};

/// One more than the repair budget: the documented "greater than
/// `MaxHintRepairSteps`" infeasibility sentinel. Saturates at `u32::MAX`.
#[cfg(any(test, feature = "runtime-benchmarks"))]
fn infeasible_sentinel<T: Config>() -> u32 {
	T::MaxHintRepairSteps::get().saturating_add(1)
}

/// Insert position for `priority` in `list_id`, treating `skip` (if any) as
/// logically removed. Walks from the head until `prev.priority >= priority >
/// next.priority` holds. Endpoints encoded as `None`.
///
/// Each visited node's `prev` back-link is checked against the actual previous
/// node in the walk so that corrupt (cyclic) storage terminates immediately in
/// O(1) memory instead of looping forever; the broken result then fails
/// insert-time validation as [`ListError::CorruptList`] rather than hanging
/// the caller.
///
/// O(list size). Off-chain helper; not for hot paths.
fn find_position_skipping<T: Config>(
	list_id: &T::ListId,
	priority: T::Priority,
	skip: Option<&T::ItemId>,
) -> Position<T::ItemId> {
	let head = ListMetas::<T>::get(list_id).map(|m| m.head);
	let mut prev: Option<T::ItemId> = None;
	let mut expected_prev: Option<T::ItemId> = None;
	let mut cursor = head;
	while let Some(item) = cursor {
		let Some(node) = ListNodes::<T>::get(list_id, &item) else { break };
		if node.prev != expected_prev {
			crate::log!(warn, "find_position: node.prev disagrees with walk (cycle?)");
			break;
		}
		expected_prev = Some(item.clone());
		if skip == Some(&item) {
			cursor = node.next;
			continue;
		}
		if priority > node.priority {
			return Position { prev, next: Some(item) };
		}
		prev = Some(item);
		cursor = node.next;
	}
	Position { prev, next: None }
}

/// Insert position for `priority` in `list_id`. See [`find_position_skipping`].
pub fn find_position<T: Config>(list_id: &T::ListId, priority: T::Priority) -> Position<T::ItemId> {
	find_position_skipping::<T>(list_id, priority, None)
}

/// Like [`find_position`], but the result is the position `item` should
/// re-occupy at `new_priority` (i.e. `item`'s own node is skipped during the
/// walk). `None` if the item is not in the list.
pub fn find_re_insert_position<T: Config>(
	list_id: &T::ListId,
	item: &T::ItemId,
	new_priority: T::Priority,
) -> Option<Position<T::ItemId>> {
	ListNodes::<T>::contains_key(list_id, item)
		.then(|| find_position_skipping::<T>(list_id, new_priority, Some(item)))
}

/// Steps the on-chain repair walk would take from `hint` to reach the insert
/// position for a NEW item at `priority`. Faithful to `insert` only.
#[cfg(any(test, feature = "runtime-benchmarks"))]
pub fn repair_steps_needed<T: Config>(
	list_id: &T::ListId,
	priority: T::Priority,
	hint: Position<T::ItemId>,
) -> u32 {
	list::walk_repair::<T>(list_id, &priority, hint)
		.map_or_else(|_| infeasible_sentinel::<T>(), |valid| valid.steps)
}

/// Steps a `re_insert`/`reprioritize` moving `(list_id, item)` to
/// `new_priority` would need to repair `hint`. Dry-runs the real
/// [`crate::SortedListInterface::re_insert`] inside an always-rolled-back
/// transaction, so it is faithful to the dispatch by construction (the
/// rollback also discards the deposited events).
#[cfg(any(test, feature = "runtime-benchmarks"))]
pub fn re_insert_steps_needed<T: Config>(
	list_id: &T::ListId,
	item: &T::ItemId,
	new_priority: T::Priority,
	hint: Position<T::ItemId>,
) -> u32 {
	// A transactional-layer overflow surfaces as `ListError::Internal`.
	let dry_run = with_transaction::<Outcome, ListError, _>(|| {
		TransactionOutcome::Rollback(Pallet::<T>::re_insert(
			list_id.clone(),
			item.clone(),
			new_priority,
			hint,
		))
	});
	match dry_run {
		Ok(Outcome::NoOp | Outcome::InPlace) => 0,
		Ok(Outcome::Relocated { steps }) => steps,
		Err(_) => infeasible_sentinel::<T>(),
	}
}
