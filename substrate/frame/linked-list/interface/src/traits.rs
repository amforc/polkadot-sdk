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

//! Consumer-facing trait surface for the sorted list.

use crate::{ListError, Outcome, Position};
use frame::{
	arithmetic::{FixedU128, Zero},
	traits::Footprint,
};

/// Authoritative source of the priority for `(list_id, item)`. Consulted by
/// the list pallet's `reprioritize` dispatchable to detect drift against
/// stored node priorities.
pub trait PriorityProvider<ListId, ItemId> {
	/// Priority type used to order items.
	type Priority;

	/// Current authoritative priority for `(list_id, item)`.
	///
	/// Returns `None` when the item should not remain in the list.
	///
	/// # Removal contract
	///
	/// The list pallet's `reprioritize` dispatchable is permissionless: the
	/// moment this method returns `None` for a listed item, ANY signed origin
	/// can remove it, announced only by an `ItemRemoved` event. Implementations
	/// must therefore return `None` only once the consumer's own bookkeeping
	/// tolerates third-party removal at any time, and consumer cleanup paths
	/// must treat [`ListError::ItemNotFound`] from their own later
	/// [`SortedListInterface::remove`] call as "already removed", not as a
	/// hard error.
	fn priority(list_id: &ListId, item: &ItemId) -> Option<Self::Priority>;

	/// For benchmarks (and `std` test fixtures): pin the authoritative priority
	/// returned by [`Self::priority`] for `(list_id, item)`.
	#[cfg(any(feature = "runtime-benchmarks", feature = "std"))]
	fn set_priority(_list_id: &ListId, _item: &ItemId, _priority: Self::Priority) {}
}

/// Append `item` to a list driven under FIFO discipline: the insertion
/// priority is one above the current head's, so it is strictly greater than
/// every present priority — FIFO order holds across arbitrary join/leave
/// interleavings, the stored priorities stay distinct (permissionless
/// re-anchoring can never legally relocate a member), the oldest member is
/// the list tail, and the sequence resets whenever the list empties.
///
/// A FIFO list's `PriorityProvider` must treat the stored priority as
/// authoritative (FIFO priorities never drift).
///
/// # Errors
///
/// - Everything [`SortedListInterface::insert`] can return.
/// - [`ListError::CorruptList`] if the head has no stored priority, or its priority cannot be
///   incremented (unreachable in practice: `u128::MAX` consecutive occupied appends).
pub fn fifo_append<ListId, ItemId, List>(list_id: ListId, item: ItemId) -> Result<(), ListError>
where
	List: SortedListInterface<ListId, ItemId, Priority = FixedU128>,
{
	let head = List::head(&list_id);
	let priority = match &head {
		Some(head_item) => {
			let head_priority =
				List::priority(&list_id, head_item).ok_or(ListError::CorruptList)?;
			let inner = head_priority.into_inner().checked_add(1).ok_or(ListError::CorruptList)?;
			FixedU128::from_inner(inner)
		},
		None => FixedU128::zero(),
	};

	let hint = Position { prev: None, next: head };
	List::insert(list_id, item, priority, hint)?;
	Ok(())
}

/// Mutation and query surface for consumer pallets.
///
/// Position hints are [`Position`] values; endpoints are encoded as `None` in
/// each field. Mutating methods fail with [`ListError`]; the hint-taking ones
/// ([`Self::insert`], [`Self::re_insert`]) additionally return the number of
/// hint-repair steps actually walked so callers can refund unused weight via
/// `PostDispatchInfo::actual_weight` ([`Self::remove`] and [`Self::pop_tail`]
/// take no hint and return no step count).
pub trait SortedListInterface<ListId, ItemId> {
	/// Priority type used to order items within a list.
	type Priority;

	/// Insert `(list_id, item)` at `priority`, repairing stale hints if needed.
	///
	/// # Errors
	///
	/// - [`ListError::ItemAlreadyExists`] if `(list_id, item)` is already in the list.
	/// - [`ListError::ListTooLong`] if the list's size counter would overflow.
	/// - [`ListError::InvalidPositionHints`] if the hint cannot be repaired within the budget.
	///   Stale hints (a removed neighbor, no-longer-adjacent neighbors, a drifted priority bracket)
	///   are repaired transparently and are NOT errors while the budget lasts.
	/// - [`ListError::CorruptList`] only when stored state is internally inconsistent — never as a
	///   result of caller input.
	fn insert(
		list_id: ListId,
		item: ItemId,
		priority: Self::Priority,
		hint: Position<ItemId>,
	) -> Result<u32, ListError>;

	/// Remove `(list_id, item)`.
	///
	/// # Errors
	///
	/// - [`ListError::ItemNotFound`] if `(list_id, item)` is not in the list.
	/// - [`ListError::CorruptList`] if the node exists but list metadata is inconsistent.
	fn remove(list_id: &ListId, item: &ItemId) -> Result<(), ListError>;

	/// Remove and return the current tail item of `list_id`, or `None` if the
	/// list is empty.
	///
	/// This is the LIFO primitive for consumers that insert equal-priority items
	/// and consume from the tail.
	///
	/// # Errors
	///
	/// - [`ListError::CorruptList`] if the tail pointer or list metadata is inconsistent.
	fn pop_tail(list_id: &ListId) -> Result<Option<(ItemId, Self::Priority)>, ListError>;

	/// Re-insert `(list_id, item)` at `new_priority`. Updates the priority in place
	/// when the existing neighbors still admit it; otherwise splices the item
	/// out and re-inserts at the hint. The returned [`Outcome`] tells the
	/// caller which path ran so the matching weight can be charged.
	///
	/// When `new_priority` equals the stored priority the call is a no-op
	/// ([`Outcome::NoOp`]): no write, no event, and no link check. So it can
	/// return `Ok(NoOp)` over a corrupt node that a priority change would reject
	/// with `CorruptList`.
	///
	/// # Errors
	///
	/// - [`ListError::ItemNotFound`] if `(list_id, item)` is not in the list.
	/// - [`ListError::CorruptList`] if a mutating path finds the node's stored links, its
	///   neighbors' back-links, or the list metadata inconsistent — never from caller input.
	/// - [`ListError::InvalidPositionHints`] if the hint cannot be repaired within the budget.
	/// - [`ListError::Internal`] if the transactional storage-layer limit blocked the splice
	///   (environmental; retrying with a different hint will not help).
	fn re_insert(
		list_id: ListId,
		item: ItemId,
		new_priority: Self::Priority,
		hint: Position<ItemId>,
	) -> Result<Outcome, ListError>;

	/// Highest-priority item in `list_id`, or `None` if empty.
	fn head(list_id: &ListId) -> Option<ItemId>;

	/// Lowest-priority item in `list_id`, or `None` if empty.
	fn tail(list_id: &ListId) -> Option<ItemId>;

	/// Number of items in `list_id`.
	fn count(list_id: &ListId) -> u32;

	/// Returns `true` if `(list_id, item)` is in the list.
	fn contains(list_id: &ListId, item: &ItemId) -> bool;

	/// Returns the maximum storage footprint of the node and its key.
	fn node_footprint(list_id: &ListId, item: &ItemId) -> Footprint;

	/// Current `(prev, next)` neighbors of `(list_id, item)`, if present.
	fn neighbors(list_id: &ListId, item: &ItemId) -> Option<Position<ItemId>> {
		Self::node(list_id, item).map(|(_, position)| position)
	}

	/// Stored priority cached on `(list_id, item)`'s node, or `None` if absent.
	fn priority(list_id: &ListId, item: &ItemId) -> Option<Self::Priority> {
		Self::node(list_id, item).map(|(priority, _)| priority)
	}

	/// Stored priority and `(prev, next)` neighbors of `(list_id, item)` in a
	/// single read, or `None` if absent. The primitive behind [`Self::priority`]
	/// and [`Self::neighbors`]; prefer it when walking the list.
	fn node(list_id: &ListId, item: &ItemId) -> Option<(Self::Priority, Position<ItemId>)>;

	/// Lazily walk `list_id` from its tail toward the head: the tail is read
	/// at construction, every further element only when the iterator
	/// advances, so nothing is materialized up front.
	fn iter_from_tail(list_id: ListId) -> impl Iterator<Item = ItemId> {
		core::iter::successors(Self::tail(&list_id), move |current| {
			Self::neighbors(&list_id, current).and_then(|position| position.prev)
		})
	}

	/// Insertion position for `priority` in `list_id`. O(list size); intended
	/// for hint preparation, not hot paths.
	fn find_position(list_id: &ListId, priority: Self::Priority) -> Position<ItemId>;

	/// Position `(list_id, item)` should occupy at `new_priority`, skipping the
	/// item's own node.
	///
	/// Returns `None` if the item is not in the list. O(list size); intended
	/// for hint preparation, not hot paths.
	fn find_re_insert_position(
		list_id: &ListId,
		item: &ItemId,
		new_priority: Self::Priority,
	) -> Option<Position<ItemId>>;

	/// Steps needed to repair `hint` for inserting a NEW item at `priority`
	/// in `list_id`.
	///
	/// Returns `0` if the hint is already valid, or a value greater than
	/// `MaxHintRepairSteps` if an [`Self::insert`] with the same hint would
	/// fail. Faithful to `insert` ONLY: [`Self::re_insert`] (and the
	/// `reprioritize` dispatchable) splice the item out before walking, so
	/// their walk runs against different state — use
	/// [`Self::re_insert_steps_needed`] for those. (With `MaxHintRepairSteps
	/// == u32::MAX` the infeasibility sentinel saturates to `u32::MAX` and
	/// cannot exceed the budget.)
	fn repair_steps_needed(
		list_id: &ListId,
		priority: Self::Priority,
		hint: Position<ItemId>,
	) -> u32;

	/// Steps [`Self::re_insert`] (and therefore the `reprioritize`
	/// dispatchable) would need to repair `hint` when moving `(list_id,
	/// item)` to `new_priority`, simulating the dispatch exactly.
	///
	/// Returns `0` when the no-op or in-place fast path would run (neither
	/// consults the hint), the post-splice walk length otherwise, and a value
	/// greater than `MaxHintRepairSteps` when the same call would fail
	/// (including when the item is not in the list).
	fn re_insert_steps_needed(
		list_id: &ListId,
		item: &ItemId,
		new_priority: Self::Priority,
		hint: Position<ItemId>,
	) -> u32;

	/// Maximum hint-repair walk length the implementation will accept before
	/// returning [`ListError::InvalidPositionHints`].
	///
	/// Deliberately part of the trait even though the implementing pallet may
	/// also expose it as a constant: a generic consumer holding only
	/// `SortedListInterface` has no access to pallet constants and needs the
	/// budget to size its own hint and weight logic.
	fn repair_budget() -> u32;
}
