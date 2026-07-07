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

use crate::{list, pallet::*, ListError, Outcome, Position, SortedListInterface};
use alloc::vec::Vec;
use frame::{
	deps::frame_support::storage::{
		transactional::with_transaction_opaque_err, TransactionOutcome,
	},
	prelude::*,
};

/// One more than the repair budget: the documented "greater than
/// `MaxHintRepairSteps`" infeasibility sentinel. Saturates at `u32::MAX`.
fn infeasible_sentinel<T: Config>() -> u32 {
	T::MaxHintRepairSteps::get().saturating_add(1)
}

/// First `n` items walking from the tail of `list_id`. Returns fewer than `n`
/// if the list has fewer items.
pub fn iter_from_tail<T: Config>(list_id: &T::ListId, n: u32) -> Vec<T::ItemId> {
	if n == 0 {
		return Vec::new();
	}
	let (tail, len) = ListMetas::<T>::get(list_id).map_or((None, 0), |m| (m.tail, m.len));
	let mut out = Vec::with_capacity(n.min(len) as usize);
	out.extend(
		core::iter::successors(tail, |item| {
			ListNodes::<T>::get(list_id, item).and_then(|node| node.prev)
		})
		.take(n.min(len) as usize),
	);
	out
}

/// Insert position for `priority` in `list_id`, treating `skip` (if any) as
/// logically removed. Walks from the head until `prev.priority >= priority >
/// next.priority` holds. Endpoints encoded as `None`.
///
/// The walk is capped at `len + 1` visited nodes so that corrupt (cyclic)
/// storage terminates instead of looping forever; the cap-exhausted result
/// then fails insert-time validation as [`ListError::CorruptList`] rather
/// than hanging the caller.
///
/// O(list size). Off-chain helper; not for hot paths.
fn find_position_skipping<T: Config>(
	list_id: &T::ListId,
	priority: T::Priority,
	skip: Option<&T::ItemId>,
) -> Position<T::ItemId> {
	let (head, len) = ListMetas::<T>::get(list_id).map_or((None, 0), |m| (m.head, m.len));
	let mut prev: Option<T::ItemId> = None;
	let mut cursor = head;
	let mut visited: u64 = 0;
	while let Some(item) = cursor {
		if visited > u64::from(len) {
			crate::log!(warn, "find_position: walk exceeded ListMetas.len (cycle?)");
			break;
		}
		visited += 1;
		let Some(node) = ListNodes::<T>::get(list_id, &item) else { break };
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
	if !ListNodes::<T>::contains_key(list_id, item) {
		return None;
	}
	Some(find_position_skipping::<T>(list_id, new_priority, Some(item)))
}

/// Steps the on-chain repair walk would take from `hint` to reach the insert
/// position for a NEW item at `priority`. Faithful to `insert` only; see
/// [`crate::SortedListInterface::repair_steps_needed`] for the full contract.
pub fn repair_steps_needed<T: Config>(
	list_id: &T::ListId,
	priority: T::Priority,
	hint: Position<T::ItemId>,
) -> u32 {
	match list::walk_repair::<T>(list_id, &priority, hint) {
		Ok(valid) => valid.steps,
		Err(_) => infeasible_sentinel::<T>(),
	}
}

/// Steps a `re_insert`/`reprioritize` moving `(list_id, item)` to
/// `new_priority` would need to repair `hint`. Dry-runs the real
/// [`crate::SortedListInterface::re_insert`] inside an always-rolled-back
/// transaction, so it is faithful to the dispatch by construction (the
/// rollback also discards the deposited events); see the trait method for
/// the full contract.
pub fn re_insert_steps_needed<T: Config>(
	list_id: &T::ListId,
	item: &T::ItemId,
	new_priority: T::Priority,
	hint: Position<T::ItemId>,
) -> u32 {
	let dry_run = with_transaction_opaque_err::<Outcome, ListError, _>(|| {
		TransactionOutcome::Rollback(Pallet::<T>::re_insert(
			list_id.clone(),
			item.clone(),
			new_priority,
			hint,
		))
	});
	match dry_run {
		Ok(Ok(Outcome::NoOp | Outcome::InPlace)) => 0,
		Ok(Ok(Outcome::Relocated { steps })) => steps,
		Ok(Err(_)) | Err(()) => infeasible_sentinel::<T>(),
	}
}
