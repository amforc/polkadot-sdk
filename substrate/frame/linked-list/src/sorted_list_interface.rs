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

//! [`SortedListInterface`] implementation for the pallet.
//!
//! The trait itself, [`PriorityProvider`](crate::PriorityProvider), and the
//! shape types live in `linked-list-interface`; this module wires them to the
//! pallet's storage.

use crate::{list, pallet::*, view_helpers, ListError, Outcome, Position, SortedListInterface};
use alloc::vec::Vec;
use frame::{
	deps::frame_support::{
		storage::{transactional::with_transaction_opaque_err, TransactionOutcome},
		traits::DefensiveOption,
	},
	prelude::*,
};

impl<T: Config> Pallet<T> {
	/// Shared event tail of every removal path: `ItemRemoved`, then
	/// `ListRemoved` when the removal emptied the list.
	fn deposit_removed(
		list_id: &T::ListId,
		item: &T::ItemId,
		priority: T::Priority,
		list_removed: bool,
	) {
		Self::deposit_event(Event::ItemRemoved {
			list_id: list_id.clone(),
			item: item.clone(),
			priority,
		});
		if list_removed {
			Self::deposit_event(Event::ListRemoved { list_id: list_id.clone() });
		}
	}
}

impl<T: Config> SortedListInterface<T::ListId, T::ItemId> for Pallet<T> {
	type Priority = T::Priority;

	fn insert(
		list_id: T::ListId,
		item: T::ItemId,
		priority: T::Priority,
		hint: Position<T::ItemId>,
	) -> Result<u32, ListError> {
		if ListNodes::<T>::contains_key(&list_id, &item) {
			return Err(ListError::ItemAlreadyExists);
		}
		let valid = list::walk_repair::<T>(&list_id, &priority, hint)?;
		let list_created = list::insert_at_inner::<T>(
			&list_id,
			&item,
			priority,
			valid.position,
			valid.prev_node,
			valid.next_node,
		)?;
		if list_created {
			Self::deposit_event(Event::ListCreated { list_id: list_id.clone() });
		}
		Self::deposit_event(Event::ItemInserted { list_id, item, priority });
		Ok(valid.steps)
	}

	fn remove(list_id: &T::ListId, item: &T::ItemId) -> Result<(), ListError> {
		let (priority, list_removed) = list::remove_at::<T>(list_id, item)?;
		Self::deposit_removed(list_id, item, priority, list_removed);
		Ok(())
	}

	fn pop_tail(list_id: &T::ListId) -> Result<Option<(T::ItemId, T::Priority)>, ListError> {
		let Some(meta) = ListMetas::<T>::get(list_id) else { return Ok(None) };
		// A present meta row always carries a tail pointer; `None` here is
		// corruption, not an empty list.
		let item = meta.tail.defensive_ok_or(ListError::CorruptList)?;
		let (priority, list_removed) =
			list::remove_at::<T>(list_id, &item).map_err(|e| match e {
				// The item id came from the meta row, so a missing node row is
				// internal inconsistency, not a caller error.
				ListError::ItemNotFound => {
					defensive!("pop_tail: tail pointer names a missing node");
					ListError::CorruptList
				},
				other => other,
			})?;
		Self::deposit_removed(list_id, &item, priority, list_removed);
		Ok(Some((item, priority)))
	}

	fn re_insert(
		list_id: T::ListId,
		item: T::ItemId,
		new_priority: T::Priority,
		hint: Position<T::ItemId>,
	) -> Result<Outcome, ListError> {
		let existing = ListNodes::<T>::get(&list_id, &item).ok_or(ListError::ItemNotFound)?;
		let old_priority = existing.priority;

		// Fast path: same priority. No write, no event, no link check — a no-op
		// mutates nothing, so it cannot make corruption worse.
		if old_priority == new_priority {
			return Ok(Outcome::NoOp);
		}

		// Every mutating path validates the node's stored links up front so
		// corruption surfaces as `CorruptList` here, matching the posture of
		// `insert`/`remove`. Interior nodes pay no extra reads: the neighbor
		// rows double as the in-place admissibility inputs below.
		let existing_position = existing.into_position();
		let (prev_node, next_node) = list::neighbor_nodes::<T>(&list_id, &existing_position);
		list::validate_node_links::<T>(
			&list_id,
			&item,
			&existing_position,
			prev_node.as_ref(),
			next_node.as_ref(),
		)?;

		// Fast path: existing neighbors still admit the new priority, mutate in place.
		if list::neighbor_priorities_admit(
			&new_priority,
			&existing_position,
			prev_node.as_ref(),
			next_node.as_ref(),
		) {
			ListNodes::<T>::mutate(&list_id, &item, |maybe| {
				if let Some(n) = maybe {
					n.priority = new_priority;
				}
			});
			Self::deposit_event(Event::ItemReinserted {
				list_id,
				item,
				old_priority,
				new_priority,
			});
			return Ok(Outcome::InPlace);
		}

		// Slow path: splice + re-insert. Wrapped in a nested storage layer so
		// that an `InvalidPositionHints` after `remove_at` rolls back cleanly.
		let outer = with_transaction_opaque_err::<u32, ListError, _>(|| {
			let inner = (|| -> Result<u32, ListError> {
				// The item never leaves the list, so the lifecycle flags from
				// `remove_at`/`insert_at_inner` are intentionally dropped — emitting
				// `ListRemoved`/`ListCreated` here would churn a single-item relocate.
				list::remove_at::<T>(&list_id, &item)?;
				let valid = list::walk_repair::<T>(&list_id, &new_priority, hint)?;
				list::insert_at_inner::<T>(
					&list_id,
					&item,
					new_priority,
					valid.position,
					valid.prev_node,
					valid.next_node,
				)?;
				Ok(valid.steps)
			})();
			if inner.is_ok() {
				TransactionOutcome::Commit(inner)
			} else {
				TransactionOutcome::Rollback(inner)
			}
		});
		// `Err(())` only fires on transactional-layer nesting overflow: an
		// environmental limit, not a hint problem — surface it as such.
		let steps = outer.map_err(|()| ListError::Internal)??;
		Self::deposit_event(Event::ItemReinserted { list_id, item, old_priority, new_priority });
		Ok(Outcome::Relocated { steps })
	}

	fn head(list_id: &T::ListId) -> Option<T::ItemId> {
		ListMetas::<T>::get(list_id).and_then(|m| m.head)
	}

	fn tail(list_id: &T::ListId) -> Option<T::ItemId> {
		ListMetas::<T>::get(list_id).and_then(|m| m.tail)
	}

	fn count(list_id: &T::ListId) -> u32 {
		ListMetas::<T>::get(list_id).map_or(0, |m| m.len)
	}

	fn contains(list_id: &T::ListId, item: &T::ItemId) -> bool {
		ListNodes::<T>::contains_key(list_id, item)
	}

	fn node(list_id: &T::ListId, item: &T::ItemId) -> Option<(T::Priority, Position<T::ItemId>)> {
		ListNodes::<T>::get(list_id, item).map(|n| (n.priority, n.into_position()))
	}

	fn iter_from_tail(list_id: &T::ListId, n: u32) -> Vec<T::ItemId> {
		view_helpers::iter_from_tail::<T>(list_id, n)
	}

	fn find_position(list_id: &T::ListId, priority: T::Priority) -> Position<T::ItemId> {
		view_helpers::find_position::<T>(list_id, priority)
	}

	fn find_re_insert_position(
		list_id: &T::ListId,
		item: &T::ItemId,
		new_priority: T::Priority,
	) -> Option<Position<T::ItemId>> {
		view_helpers::find_re_insert_position::<T>(list_id, item, new_priority)
	}

	fn repair_steps_needed(
		list_id: &T::ListId,
		priority: T::Priority,
		hint: Position<T::ItemId>,
	) -> u32 {
		view_helpers::repair_steps_needed::<T>(list_id, priority, hint)
	}

	fn re_insert_steps_needed(
		list_id: &T::ListId,
		item: &T::ItemId,
		new_priority: T::Priority,
		hint: Position<T::ItemId>,
	) -> u32 {
		view_helpers::re_insert_steps_needed::<T>(list_id, item, new_priority, hint)
	}

	fn repair_budget() -> u32 {
		T::MaxHintRepairSteps::get()
	}
}
