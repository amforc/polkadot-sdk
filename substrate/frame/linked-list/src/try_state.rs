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
//! 3. The forward walk visits exactly `meta.len` nodes (capped at `len + 1` so cyclic corruption
//!    terminates), every visited node's `prev` matches the walk, and the reverse walk is the
//!    forward walk reversed (checked step by step, so it is bounded by the forward walk).
//!    Self-loops, dangling neighbor references, and cross-list references all surface through these
//!    checks.
//! 4. Priorities are non-increasing from head to tail.
//! 5. `ListNodes` has no orphan rows (total row count equals the chain length).

use crate::{pallet::*, ListMeta};
use alloc::vec::Vec;
use frame::{
	deps::{frame_support::storage::StorageDoubleMap as _, sp_runtime::TryRuntimeError},
	prelude::*,
};

/// Cap on the walk vector's initial capacity. `meta.len` is the value we are
/// checking, so a corrupt near-`u32::MAX` len must not size a huge allocation.
/// A longer list just grows its vector.
const MAX_WALK_PREALLOC: usize = 1 << 12;

impl<T: Config> Pallet<T> {
	/// Run the per-list invariant checks across every list with stored state.
	/// Returns the first violation found.
	pub fn do_try_state() -> Result<(), TryRuntimeError> {
		// `try_state_list` only reads storage, so running it mid-iteration is
		// safe. Keys rather than `iter()`, which would silently skip a row that
		// fails to decode.
		for list_id in ListMetas::<T>::iter_keys() {
			let meta = ListMetas::<T>::get(&list_id).ok_or("ListMetas row fails to decode")?;
			Self::try_state_list(&list_id, meta)?;
		}
		// The other half of the all-or-nothing invariant: no node rows without
		// their list's meta row.
		for (list_id, _) in ListNodes::<T>::iter_keys() {
			ensure!(ListMetas::<T>::contains_key(&list_id), "ListNodes present without ListMetas");
		}
		Ok(())
	}

	fn try_state_list(
		list_id: &T::ListId,
		meta: ListMeta<T::ItemId>,
	) -> Result<(), TryRuntimeError> {
		ensure!(ListNodes::<T>::contains_prefix(list_id), "ListMetas present without ListNodes");

		let ListMeta { head, tail, len: stored_size } = meta;
		let head_id = head.ok_or("ListMetas.head is None")?;
		let tail_id = tail.ok_or("ListMetas.tail is None")?;
		ensure!(stored_size != 0, "ListMetas.len is zero on present row");
		let head_node = ListNodes::<T>::get(list_id, &head_id)
			.ok_or("ListMetas.head points to missing node")?;
		let tail_node = ListNodes::<T>::get(list_id, &tail_id)
			.ok_or("ListMetas.tail points to missing node")?;
		ensure!(head_node.prev.is_none(), "head node has non-None prev");
		ensure!(tail_node.next.is_none(), "tail node has non-None next");

		// Forward walk: count, link consistency, monotone priorities. Wrong,
		// missing, or self-looping neighbor links are all caught by the
		// `node.prev == prev` check, the next iteration's failed fetch, or
		// the walk cap; per-node `contains_key` probes would be redundant.
		// `cap` bounds the walk (cycle detection); `prealloc` is the capped
		// capacity.
		let cap: usize = stored_size.saturating_add(1).saturated_into();
		let prealloc = cap.min(MAX_WALK_PREALLOC);
		let mut forward: Vec<T::ItemId> = Vec::with_capacity(prealloc);
		let mut prev: Option<T::ItemId> = None;
		let mut cursor = Some(head_id);
		let mut last_priority: Option<T::Priority> = None;
		while let Some(cur) = cursor {
			ensure!(
				forward.len() < cap,
				"forward walk exceeded ListMetas.len + 1 (cycle or undercounted len)"
			);
			let node = ListNodes::<T>::get(list_id, &cur).ok_or("forward walk: missing node")?;
			ensure!(node.prev == prev, "forward walk: node.prev != expected");
			ensure!(
				last_priority.is_none_or(|ls| node.priority <= ls),
				"priorities not non-increasing head→tail"
			);
			last_priority = Some(node.priority);
			prev = Some(cur.clone());
			cursor = node.next;
			forward.push(cur);
		}
		ensure!(
			u32::try_from(forward.len()) == Ok(stored_size),
			"forward walk count != ListMetas.len"
		);

		// Reverse walk must match the reverse of the forward walk. Checked
		// against `forward` as it goes, so a mismatch (including a reverse walk
		// longer than the forward one) stops it within `forward.len() + 1` steps.
		let mut expected = forward.iter().rev();
		let mut cursor = Some(tail_id);
		while let Some(cur) = cursor {
			let node = ListNodes::<T>::get(list_id, &cur).ok_or("reverse walk: missing node")?;
			ensure!(
				expected.next() == Some(&cur),
				"reverse walk does not equal forward walk reversed"
			);
			cursor = node.prev;
		}
		ensure!(expected.next().is_none(), "reverse walk length != forward walk length");

		// Catch unreachable nodes: total rows must equal chain length.
		let total_nodes = ListNodes::<T>::iter_key_prefix(list_id).count();
		ensure!(
			total_nodes == forward.len(),
			"orphan ListNodes rows: total count differs from chain length"
		);

		Ok(())
	}
}
