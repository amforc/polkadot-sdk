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
use frame::{
	deps::{codec::MaxEncodedLen, frame_support::storage::with_storage_layer},
	prelude::*,
	traits::Footprint,
};

impl<T: Config> SortedListInterface<T::ListId, T::ItemId> for Pallet<T> {
	type Priority = T::Priority;

	fn insert(
		list_id: T::ListId,
		item: T::ItemId,
		priority: T::Priority,
		hint: Position<T::ItemId>,
	) -> Result<u32, ListError> {
		ensure!(!ListNodes::<T>::contains_key(&list_id, &item), ListError::ItemAlreadyExists);
		let valid = list::walk_repair::<T>(&list_id, &priority, hint)?;
		let steps = valid.steps;
		let list_created = list::insert_at_inner::<T>(&list_id, &item, priority, valid)?;
		if list_created {
			Self::deposit_event(Event::ListCreated { list_id: list_id.clone() });
		}
		Self::deposit_event(Event::ItemInserted { list_id, item, priority });
		Ok(steps)
	}

	fn remove(list_id: &T::ListId, item: &T::ItemId) -> Result<(), ListError> {
		let (priority, list_removed) = list::remove_at::<T>(list_id, item)?;
		Self::deposit_event(Event::ItemRemoved {
			list_id: list_id.clone(),
			item: item.clone(),
			priority,
		});
		if list_removed {
			Self::deposit_event(Event::ListRemoved { list_id: list_id.clone() });
		}
		Ok(())
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
		// rows double as the in-place admissibility inputs and the unlink
		// splice below.
		let existing_position = existing.into_position();
		let (prev_node, next_node, meta) =
			list::linked_neighbors::<T>(&list_id, &item, &existing_position)?;

		// Fast path: existing neighbors still admit the new priority, mutate in place.
		if list::neighbor_priorities_admit(
			&new_priority,
			&existing_position,
			prev_node.as_ref(),
			next_node.as_ref(),
		) {
			// The node was read above; rewrite it rather than re-read it through `mutate`.
			let Position { prev, next } = existing_position;
			ListNodes::<T>::insert(
				&list_id,
				&item,
				list::Node { prev, next, priority: new_priority },
			);
			Self::deposit_event(Event::ItemReinserted {
				list_id,
				item,
				old_priority,
				new_priority,
			});
			return Ok(Outcome::InPlace);
		}

		// Slow path: splice + re-insert. Wrapped in a nested storage layer so
		// that an `InvalidPositionHints` after `unlink` rolls back cleanly.
		let steps = with_storage_layer(|| -> Result<u32, ListError> {
			// The item never leaves the list, so the lifecycle flags from
			// `unlink`/`insert_at_inner` are intentionally dropped — emitting
			// `ListRemoved`/`ListCreated` here would churn a single-item relocate.
			// The rows validated above are unchanged, so unlink with them
			// rather than re-reading and re-validating through `remove_at`.
			list::unlink::<T>(&list_id, &item, existing_position, prev_node, next_node, meta)?;
			let valid = list::walk_repair::<T>(&list_id, &new_priority, hint)?;
			let steps = valid.steps;
			list::insert_at_inner::<T>(&list_id, &item, new_priority, valid)?;
			Ok(steps)
		})?;
		Self::deposit_event(Event::ItemReinserted { list_id, item, old_priority, new_priority });
		Ok(Outcome::Relocated { steps })
	}

	fn head(list_id: &T::ListId) -> Option<T::ItemId> {
		ListMetas::<T>::get(list_id).map(|m| m.head)
	}

	fn tail(list_id: &T::ListId) -> Option<T::ItemId> {
		ListMetas::<T>::get(list_id).map(|m| m.tail)
	}

	fn contains(list_id: &T::ListId, item: &T::ItemId) -> bool {
		ListNodes::<T>::contains_key(list_id, item)
	}

	fn node_footprint(list_id: &T::ListId, item: &T::ItemId) -> Footprint {
		let key = ListNodes::<T>::hashed_key_for(list_id, item).len();
		let node = list::NodeOf::<T>::max_encoded_len();
		Footprint::from_parts(1, key.saturating_add(node))
	}

	fn node(list_id: &T::ListId, item: &T::ItemId) -> Option<(T::Priority, Position<T::ItemId>)> {
		ListNodes::<T>::get(list_id, item).map(|n| (n.priority, n.into_position()))
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

	#[cfg(feature = "runtime-benchmarks")]
	fn repair_steps_needed(
		list_id: &T::ListId,
		priority: T::Priority,
		hint: Position<T::ItemId>,
	) -> u32 {
		view_helpers::repair_steps_needed::<T>(list_id, priority, hint)
	}

	#[cfg(feature = "runtime-benchmarks")]
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
