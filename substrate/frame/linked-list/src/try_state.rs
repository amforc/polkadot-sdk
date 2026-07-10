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

//! `try-runtime` invariant checks.
//!
//! For every list with rows in either [`ListNodes`] or [`ListMetas`]:
//!
//! 1. The meta row and the node rows are all-or-nothing (`ListMetas[list_id]` present iff at least
//!    one `ListNodes` row exists for `list_id`).
//! 2. When the meta row exists, its `head`/`tail` are both `Some(_)` and `len >= 1`; the head node
//!    has `prev = None` and the tail node has `next = None`.
//! 3. Forward and reverse walks visit exactly `meta.len` nodes (capped at `len + 1` so cyclic
//!    corruption terminates), every visited node's `prev` matches the walk, and the reverse walk is
//!    the forward walk reversed. Self-loops, dangling neighbor references, and cross-list
//!    references all surface through these checks.
//! 4. Priorities are non-increasing from head to tail.
//! 5. `ListNodes` has no orphan rows (total row count equals the chain length).

use crate::pallet::*;

#[cfg(feature = "try-runtime")]
use alloc::{collections::BTreeSet, vec::Vec};

#[cfg(feature = "try-runtime")]
use codec::Encode;

#[cfg(feature = "try-runtime")]
use frame::try_runtime::TryRuntimeError;

/// Cap on the walk vectors' initial capacity. `meta.len` is the value we are
/// checking, so a corrupt near-`u32::MAX` len must not size a huge allocation.
/// A longer list just grows its vector.
#[cfg(feature = "try-runtime")]
const MAX_WALK_PREALLOC: usize = 1 << 12;

impl<T: Config> Pallet<T> {
	/// Run the per-list invariant checks across every list with stored state.
	/// Returns the first violation found.
	#[cfg(feature = "try-runtime")]
	pub fn do_try_state() -> Result<(), TryRuntimeError> {
		// Dedup on the SCALE encoding to avoid an `Ord` bound on `ListId`
		// while staying O(n log n) over the total row count. `try_state_list`
		// only reads storage, so running it mid-iteration is safe.
		let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
		for list_id in
			ListMetas::<T>::iter_keys().chain(ListNodes::<T>::iter_keys().map(|(b, _)| b))
		{
			if seen.insert(list_id.encode()) {
				Self::try_state_list(&list_id)?;
			}
		}
		Ok(())
	}

	#[cfg(feature = "try-runtime")]
	fn try_state_list(list_id: &T::ListId) -> Result<(), TryRuntimeError> {
		let meta = ListMetas::<T>::get(list_id);
		let nodes_present = ListNodes::<T>::iter_key_prefix(list_id).next().is_some();

		let Some(meta) = meta else {
			if nodes_present {
				return Err("ListNodes present without ListMetas".into());
			}
			return Ok(());
		};
		if !nodes_present {
			return Err("ListMetas present without ListNodes".into());
		}

		let head_id =
			meta.head.clone().ok_or::<TryRuntimeError>("ListMetas.head is None".into())?;
		let tail_id =
			meta.tail.clone().ok_or::<TryRuntimeError>("ListMetas.tail is None".into())?;
		if meta.len == 0 {
			return Err("ListMetas.len is zero on present row".into());
		}
		let stored_size = meta.len;
		let head_node = ListNodes::<T>::get(list_id, &head_id)
			.ok_or::<TryRuntimeError>("ListMetas.head points to missing node".into())?;
		let tail_node = ListNodes::<T>::get(list_id, &tail_id)
			.ok_or::<TryRuntimeError>("ListMetas.tail points to missing node".into())?;
		if head_node.prev.is_some() {
			return Err("head node has non-None prev".into());
		}
		if tail_node.next.is_some() {
			return Err("tail node has non-None next".into());
		}

		// Forward walk: count, link consistency, monotone priorities. Wrong,
		// missing, or self-looping neighbor links are all caught by the
		// `node.prev != prev` check, the next iteration's failed fetch, or
		// the walk cap; per-node `contains_key` probes would be redundant.
		// `cap` bounds the walk (cycle detection); `prealloc` is the capped
		// capacity.
		let cap = stored_size.saturating_add(1) as usize;
		let prealloc = cap.min(MAX_WALK_PREALLOC);
		let mut forward: Vec<T::ItemId> = Vec::with_capacity(prealloc);
		let mut prev: Option<T::ItemId> = None;
		let mut cursor = Some(head_id);
		let mut last_priority: Option<T::Priority> = None;
		while let Some(cur) = cursor {
			if forward.len() >= cap {
				return Err(
					"forward walk exceeded ListMetas.len + 1 (cycle or undercounted len)".into()
				);
			}
			let node = ListNodes::<T>::get(list_id, &cur)
				.ok_or::<TryRuntimeError>("forward walk: missing node".into())?;
			if node.prev != prev {
				return Err("forward walk: node.prev != expected".into());
			}
			if let Some(ls) = last_priority {
				if node.priority > ls {
					return Err("priorities not non-increasing head→tail".into());
				}
			}
			last_priority = Some(node.priority);
			prev = Some(cur.clone());
			cursor = node.next;
			forward.push(cur);
		}
		if u32::try_from(forward.len()).map_or(true, |n| n != stored_size) {
			return Err("forward walk count != ListMetas.len".into());
		}

		// Reverse walk must match the reverse of the forward walk.
		let mut reverse: Vec<T::ItemId> = Vec::with_capacity(prealloc);
		let mut cursor = Some(tail_id);
		while let Some(cur) = cursor {
			if reverse.len() >= cap {
				return Err(
					"reverse walk exceeded ListMetas.len + 1 (cycle or undercounted len)".into()
				);
			}
			let node = ListNodes::<T>::get(list_id, &cur)
				.ok_or::<TryRuntimeError>("reverse walk: missing node".into())?;
			reverse.push(cur);
			cursor = node.prev;
		}
		if reverse.len() != forward.len() {
			return Err("reverse walk length != forward walk length".into());
		}
		reverse.reverse();
		if reverse != forward {
			return Err("reverse walk does not equal forward walk reversed".into());
		}

		// Catch unreachable nodes: total rows must equal chain length.
		let total_nodes =
			u32::try_from(ListNodes::<T>::iter_key_prefix(list_id).count()).unwrap_or(u32::MAX);
		if total_nodes != stored_size {
			return Err("orphan ListNodes rows: total count differs from chain length".into());
		}

		Ok(())
	}
}
