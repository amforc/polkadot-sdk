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

//! # Linked-list pallet
//!
//! A generic sorted doubly-linked list, one list per `ListId`. Each list is
//! kept in priority order, head (highest) to tail (lowest). Same-priority items
//! go to the tail side of their cluster, so tail-first iteration is LIFO.
//!
//! Insertion takes a [`Position`] hint (a typed `(prev, next)` pair, endpoints
//! as `None`) and repairs stale hints on-chain up to `MaxHintRepairSteps`.
//!
//! ## Overview
//!
//! Consumer pallets use the [`SortedListInterface`] trait. The one dispatchable,
//! [`Pallet::reprioritize`], is permissionless: it re-reads an item's
//! authoritative priority from [`PriorityProvider`] to fix drift.
//!
//! ## Interface
//!
//! - [`SortedListInterface::insert`]: O(1) with valid hints, otherwise a bounded repair walk.
//! - [`SortedListInterface::remove`]: O(1) splice.
//! - [`SortedListInterface::pop_tail`]: O(1) tail pop for LIFO consumers.
//! - [`SortedListInterface::re_insert`]: in-place when the existing position still admits the new
//!   priority, otherwise splice + repair + re-insert.
//! - [`SortedListInterface::iter_from_tail`]: bounded tail-first iteration.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use frame::prelude::*;

// Re-exported so existing `pallet_linked_list::*` paths keep working; new
// consumers should depend on `linked-list-interface` directly.
pub use linked_list_interface::{
	fifo_append, ListError, Outcome, Position, PriorityProvider, SortedListInterface,
};
pub use list::Node;
pub use pallet::*;
pub use types::ListMeta;

mod dispatchables;
mod list;
mod sorted_list_interface;
mod try_state;
mod types;
mod view_helpers;
pub mod weights;

pub(crate) const LOG_TARGET: &str = "runtime::linked-list";

// Logging helper.
macro_rules! log {
	($level:tt, $pattern:expr $(, $values:expr)* $(,)?) => {
		frame::log::$level!(
			target: $crate::LOG_TARGET,
			concat!("[{:?}] [{}] ", $pattern),
			<frame_system::Pallet<T>>::block_number(),
			<$crate::Pallet::<T> as frame::deps::frame_support::traits::PalletInfoAccess>::name()
			$(, $values)*
		)
	};
}
pub(crate) use log;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

#[frame::pallet]
pub mod pallet {
	use super::*;
	use crate::weights::WeightInfo;

	pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Outer key partitioning the lists.
		type ListId: Parameter + Member + MaxEncodedLen;

		/// Inner key identifying an item within a list.
		type ItemId: Parameter + Member + MaxEncodedLen;

		/// Sort key. Higher priorities sit near the head, lower ones near the tail.
		type Priority: Parameter + Member + Copy + Ord + MaxEncodedLen;

		/// Authoritative source of an item's priority. Consulted by
		/// [`Pallet::reprioritize`] to detect drift.
		type PriorityProvider: PriorityProvider<
			Self::ListId,
			Self::ItemId,
			Priority = Self::Priority,
		>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: weights::WeightInfo;

		/// Max nodes the on-chain hint-repair walk may cross before it gives up
		/// with [`ListError::InvalidPositionHints`].
		///
		/// `0` is strict mode: any invalid hint fails at once. That gives a hard
		/// O(1) insert, but callers must supply perfect hints.
		#[pallet::constant]
		type MaxHintRepairSteps: Get<u32>;
	}

	/// Nodes of the per-list sorted list.
	#[pallet::storage]
	pub type ListNodes<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		T::ListId,
		Blake2_128Concat,
		T::ItemId,
		Node<T::ItemId, T::Priority>,
		OptionQuery,
	>;

	/// Per-list head, tail, and item count. Dropped when a list empties; a
	/// missing row means the empty list.
	#[pallet::storage]
	pub type ListMetas<T: Config> =
		StorageMap<_, Blake2_128Concat, T::ListId, ListMeta<T::ItemId>, OptionQuery>;

	/// Priority backing for benchmarks only. Production runtimes read the priority
	/// from external state (e.g. stake) via their own [`PriorityProvider`]; this
	/// storage lets the pallet be benchmarked alone, paired with
	/// [`crate::BenchPriorityProvider`].
	#[cfg(feature = "runtime-benchmarks")]
	#[pallet::storage]
	pub type BenchAuthoritativePriority<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		T::ListId,
		Blake2_128Concat,
		T::ItemId,
		T::Priority,
		OptionQuery,
	>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// An item was inserted into a list.
		ItemInserted { list_id: T::ListId, item: T::ItemId, priority: T::Priority },
		/// An item was removed from a list.
		ItemRemoved { list_id: T::ListId, item: T::ItemId, priority: T::Priority },
		/// An item's priority was changed (by [`SortedListInterface::re_insert`]
		/// or the [`Pallet::reprioritize`] dispatchable).
		ItemReinserted {
			list_id: T::ListId,
			item: T::ItemId,
			old_priority: T::Priority,
			new_priority: T::Priority,
		},
		/// A list was created by inserting its first item.
		ListCreated { list_id: T::ListId },
		/// A list was removed after its last item was removed.
		ListRemoved { list_id: T::ListId },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// A [`ListError`] surfaced through [`Pallet::reprioritize`].
		List(ListError),
	}

	impl<T> From<ListError> for Error<T> {
		fn from(e: ListError) -> Self {
			Self::List(e)
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		#[cfg(feature = "try-runtime")]
		fn try_state(_: BlockNumberFor<T>) -> Result<(), frame::try_runtime::TryRuntimeError> {
			Self::do_try_state()
		}
	}

	#[pallet::view_functions]
	impl<T: Config> Pallet<T> {
		/// Highest-priority item in `list_id`, or `None` if empty.
		pub fn head(list_id: T::ListId) -> Option<T::ItemId> {
			<Self as SortedListInterface<T::ListId, T::ItemId>>::head(&list_id)
		}

		/// Lowest-priority item in `list_id`, or `None` if empty.
		pub fn tail(list_id: T::ListId) -> Option<T::ItemId> {
			<Self as SortedListInterface<T::ListId, T::ItemId>>::tail(&list_id)
		}

		/// Number of items in `list_id`.
		pub fn count(list_id: T::ListId) -> u32 {
			<Self as SortedListInterface<T::ListId, T::ItemId>>::count(&list_id)
		}

		/// Whether `(list_id, item)` is currently in the list.
		pub fn contains(list_id: T::ListId, item: T::ItemId) -> bool {
			<Self as SortedListInterface<T::ListId, T::ItemId>>::contains(&list_id, &item)
		}

		/// Current `(prev, next)` neighbors of `(list_id, item)`, if present.
		pub fn neighbors(list_id: T::ListId, item: T::ItemId) -> Option<Position<T::ItemId>> {
			<Self as SortedListInterface<T::ListId, T::ItemId>>::neighbors(&list_id, &item)
		}

		/// Stored priority cached on `(list_id, item)`'s node, or `None` if the
		/// item is not in the list.
		pub fn priority(list_id: T::ListId, item: T::ItemId) -> Option<T::Priority> {
			<Self as SortedListInterface<T::ListId, T::ItemId>>::priority(&list_id, &item)
		}

		/// Stored priority and `(prev, next)` neighbors of `(list_id, item)` in
		/// a single read, or `None` if the item is not in the list.
		pub fn node(
			list_id: T::ListId,
			item: T::ItemId,
		) -> Option<(T::Priority, Position<T::ItemId>)> {
			<Self as SortedListInterface<T::ListId, T::ItemId>>::node(&list_id, &item)
		}

		/// First `n` items of `list_id` walking from the tail. Returns fewer
		/// than `n` if the list has fewer items.
		pub fn iter_from_tail(list_id: T::ListId, n: u32) -> alloc::vec::Vec<T::ItemId> {
			<Self as SortedListInterface<T::ListId, T::ItemId>>::iter_from_tail(&list_id, n)
		}

		/// Insertion [`Position`] for `priority` in `list_id`.
		pub fn find_position(list_id: T::ListId, priority: T::Priority) -> Position<T::ItemId> {
			<Self as SortedListInterface<T::ListId, T::ItemId>>::find_position(&list_id, priority)
		}

		/// Position `(list_id, item)` should occupy at `new_priority`. Returns
		/// `None` if the item is not in the list.
		pub fn find_re_insert_position(
			list_id: T::ListId,
			item: T::ItemId,
			new_priority: T::Priority,
		) -> Option<Position<T::ItemId>> {
			<Self as SortedListInterface<T::ListId, T::ItemId>>::find_re_insert_position(
				&list_id,
				&item,
				new_priority,
			)
		}

		/// Steps the on-chain repair walk would take from `hint` to insert a NEW
		/// item at `priority`. Matches [`SortedListInterface::insert`] only; see
		/// [`SortedListInterface::repair_steps_needed`] for the full contract and
		/// [`Pallet::re_insert_steps_needed`] for `reprioritize`.
		pub fn repair_steps_needed(
			list_id: T::ListId,
			priority: T::Priority,
			hint: Position<T::ItemId>,
		) -> u32 {
			<Self as SortedListInterface<T::ListId, T::ItemId>>::repair_steps_needed(
				&list_id, priority, hint,
			)
		}

		/// Steps a [`Pallet::reprioritize`] moving `(list_id, item)` to
		/// `new_priority` would take to repair `hint`, simulating the dispatch
		/// exactly. See [`SortedListInterface::re_insert_steps_needed`] for the
		/// full contract.
		pub fn re_insert_steps_needed(
			list_id: T::ListId,
			item: T::ItemId,
			new_priority: T::Priority,
			hint: Position<T::ItemId>,
		) -> u32 {
			<Self as SortedListInterface<T::ListId, T::ItemId>>::re_insert_steps_needed(
				&list_id,
				&item,
				new_priority,
				hint,
			)
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Move `(list_id, item)` to its authoritative priority from
		/// [`PriorityProvider`] once that has drifted from the stored one.
		///
		/// Anyone can call this. The caller passes a [`Position`] hint; stale
		/// hints are repaired up to `MaxHintRepairSteps`.
		///
		/// If [`PriorityProvider::priority`] returns `None` the item is REMOVED
		/// (announced by an `ItemRemoved` event): marking an item dead makes its
		/// removal permissionless. See the removal contract on
		/// [`PriorityProvider::priority`].
		#[pallet::call_index(0)]
		#[pallet::weight(
			T::WeightInfo::reprioritize_no_op()
				.max(T::WeightInfo::reprioritize_in_place())
				.max(T::WeightInfo::reprioritize_relocate(T::MaxHintRepairSteps::get()))
				.max(T::WeightInfo::reprioritize_priority_removed())
		)]
		pub fn reprioritize(
			origin: OriginFor<T>,
			list_id: T::ListId,
			item: T::ItemId,
			hint: Position<T::ItemId>,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;
			let actual_weight = Self::do_reprioritize(list_id, item, hint)?;
			Ok(Some(actual_weight).into())
		}
	}
}

/// [`PriorityProvider`] adapter backed by [`pallet::BenchAuthoritativePriority`].
#[cfg(feature = "runtime-benchmarks")]
pub struct BenchPriorityProvider<T>(core::marker::PhantomData<T>);

#[cfg(feature = "runtime-benchmarks")]
impl<T: Config> PriorityProvider<T::ListId, T::ItemId> for BenchPriorityProvider<T> {
	type Priority = T::Priority;

	fn priority(list_id: &T::ListId, item: &T::ItemId) -> Option<T::Priority> {
		pallet::BenchAuthoritativePriority::<T>::get(list_id, item)
	}

	fn set_priority(list_id: &T::ListId, item: &T::ItemId, priority: T::Priority) {
		pallet::BenchAuthoritativePriority::<T>::insert(list_id, item, priority);
	}
}
