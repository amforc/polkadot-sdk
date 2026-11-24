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
//! 2. When the meta row exists, its `head` and `tail` nodes exist; the head node has `prev = None`
//!    and the tail node has `next = None`.
//! 3. The forward walk visits every stored node (bounded by `total_nodes + 1` so cyclic corruption
//!    terminates), every visited node's `prev` matches the walk, and the reverse walk is the
//!    forward walk reversed (checked step by step, so it is bounded by the forward walk).
//!    Self-loops, dangling neighbor references, and cross-list references all surface through these
//!    checks.
//! 4. Priorities are non-increasing from head to tail.
//! 5. `ListNodes` has no orphan rows (total row count equals the chain length).

use crate::{pallet::*, ListMeta};
use alloc::vec::Vec;
use frame::{
	deps::{
		frame_support::storage::{generator::StorageDoubleMap as _, StoragePrefixedMap as _},
		sp_io, sp_runtime::TryRuntimeError,
	},
	prelude::*,
};

/// Cap on the walk vector's initial capacity so a huge list grows incrementally
/// rather than over-allocating up front.
const MAX_WALK_PREALLOC: usize = 1 << 12;

/// Count raw storage keys under `prefix` without decoding keys or values, so
/// non-concat (`Blake2_128`) maps and even undecodable rows are counted faithfully.
fn count_raw_keys(prefix: &[u8]) -> usize {
	let mut count = 0usize;
	let mut key = prefix.to_vec();
	while let Some(next) = sp_io::storage::next_key(&key) {
		if !next.starts_with(prefix) {
			break;
		}
		count = count.saturating_add(1);
		key = next;
	}
	count
}

impl<T: Config> Pallet<T> {
	/// Run the per-list invariant checks across every list with stored state.
	/// Returns the first violation found.
	pub fn do_try_state() -> Result<(), TryRuntimeError> {
		let mut reachable_nodes = 0usize;
		// `try_state_list` only reads storage, so running it mid-iteration is
		// safe. Keys rather than `iter()`, which would silently skip a row that
		// fails to decode.
		for list_id in ListMetas::<T>::iter_keys() {
			let meta = ListMetas::<T>::get(&list_id).ok_or("ListMetas row fails to decode")?;
			let list_len = Self::try_state_list(&list_id, meta)?;
			reachable_nodes = reachable_nodes.saturating_add(list_len);
		}
		// The other half of the all-or-nothing invariant: every `ListNodes` row
		// across all lists must have been visited through a live `ListMetas` row.
		let total_rows = count_raw_keys(&ListNodes::<T>::final_prefix());
		ensure!(total_rows == reachable_nodes, "ListNodes present without ListMetas");
		Ok(())
	}

	fn try_state_list(
		list_id: &T::ListId,
		meta: ListMeta<T::ItemId>,
	) -> Result<usize, TryRuntimeError> {
		let prefix = ListNodes::<T>::storage_double_map_final_key1(list_id);
		let total_nodes = count_raw_keys(&prefix);
		ensure!(total_nodes > 0, "ListMetas present without ListNodes");

		let ListMeta { head: head_id, tail: tail_id } = meta;
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
		let cap = total_nodes.saturating_add(1);
		let prealloc = total_nodes.min(MAX_WALK_PREALLOC);
		let mut forward: Vec<T::ItemId> = Vec::with_capacity(prealloc);
		let mut prev: Option<T::ItemId> = None;
		let mut cursor = Some(head_id);
		let mut last_priority: Option<T::Priority> = None;
		while let Some(cur) = cursor {
			ensure!(forward.len() < cap, "forward walk exceeded ListNodes count (cycle?)");
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
		ensure!(
			total_nodes == forward.len(),
			"orphan ListNodes rows: total count differs from chain length"
		);

		Ok(forward.len())
	}
}
