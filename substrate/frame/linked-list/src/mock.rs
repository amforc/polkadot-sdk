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

//! Tests mock for `pallet-linked-list`.

#![cfg(test)]

use crate as pallet_linked_list;
use alloc::collections::BTreeMap;
use frame::testing_prelude::*;

pub type ListId = u32;
pub type ItemId = u64;
pub type Priority = u32;

type Block = MockBlock<Test>;

#[frame_construct_runtime]
mod runtime {
	#[runtime::runtime]
	#[runtime::derive(RuntimeCall, RuntimeEvent, RuntimeError, RuntimeOrigin, RuntimeTask)]
	pub struct Test;

	#[runtime::pallet_index(0)]
	pub type System = frame_system;

	#[runtime::pallet_index(1)]
	pub type LinkedList = pallet_linked_list;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
}

parameter_types! {
	pub static StaticPriorities: BTreeMap<(ListId, ItemId), Priority> = BTreeMap::new();
	pub static MaxHintRepairSteps: u32 = 4;
}

pub struct StaticPriorityProvider;

impl StaticPriorityProvider {
	pub(crate) fn pin_priority(list_id: ListId, item: ItemId, priority: Priority) {
		StaticPriorities::mutate(|m| {
			m.insert((list_id, item), priority);
		});
	}
}

impl pallet_linked_list::PriorityProvider<ListId, ItemId> for StaticPriorityProvider {
	type Priority = Priority;
	fn priority(list_id: &ListId, item: &ItemId) -> Option<Priority> {
		StaticPriorities::get().get(&(*list_id, *item)).copied()
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn set_priority(list_id: &ListId, item: &ItemId, priority: Priority) {
		Self::pin_priority(*list_id, *item, priority);
	}
}

impl pallet_linked_list::Config for Test {
	type WeightInfo = ();
	type ListId = ListId;
	type ItemId = ItemId;
	type Priority = Priority;
	type MaxHintRepairSteps = MaxHintRepairSteps;
	type PriorityProvider = StaticPriorityProvider;
}

pub(crate) fn new_test_ext() -> TestState {
	let mut ext: TestState =
		frame_system::GenesisConfig::<Test>::default().build_storage().unwrap().into();
	ext.execute_with(|| System::set_block_number(1));
	ext
}

/// Run `test` against a fresh externality and unconditionally re-check the
/// pallet's invariants afterwards.
pub(crate) fn build_and_execute(test: impl FnOnce()) {
	new_test_ext().execute_with(|| {
		test();
		LinkedList::do_try_state().expect("invariants hold post-test");
	});
}

/// Like [`build_and_execute`], but skips the post-test invariant check. Use
/// only for tests that deliberately leave storage corrupt to exercise
/// `do_try_state` directly. For `should_panic` tests of `defensive!` paths
/// use [`build_and_execute_defensive`] instead.
pub(crate) fn build_and_execute_no_post_check(test: impl FnOnce()) {
	new_test_ext().execute_with(test);
}

/// Like [`build_and_execute_no_post_check`], for `should_panic` tests of
/// `defensive!` paths: `defensive!` logs an ERROR before panicking, and the
/// hidden `construct_runtime` integrity test installs a global stderr
/// subscriber that leaks that log past libtest's capture even when the test
/// passes. The capture below keeps it silent on pass, visible on failure.
///
/// NOTE: Fold back into `build_and_execute_no_post_check` once the generated
/// integrity test uses `with_test_writer` instead of `try_init_simple`. We can
/// get rid of this by updating `try_init_simple` in `#[frame_construct_runtime]`
pub(crate) fn build_and_execute_defensive(test: impl FnOnce()) {
	sp_tracing::capture_test_logs!(sp_tracing::Level::ERROR, true, {
		build_and_execute_no_post_check(test);
	});
}

/// Convenience: insert via the `SortedListInterface` with the hint fetched
/// from `find_position`. Returns the `repair_steps` count.
pub(crate) fn insert(list_id: ListId, item: ItemId, priority: Priority) -> u32 {
	use pallet_linked_list::SortedListInterface;
	let hint = LinkedList::find_position(list_id, priority);
	LinkedList::insert(list_id, item, priority, hint).expect("insert succeeds in tests")
}

/// Seed list `1` with items `1..=MaxHintRepairSteps + 4` at priorities 100, 90,
/// 80, … in head→tail order: longer than the repair budget, so a wrong-end hint
/// cannot be bridged. Returns the chain length, which is also the tail item id.
pub(crate) fn build_long_chain() -> u32 {
	let chain_len = MaxHintRepairSteps::get() + 4;
	for i in 1..=chain_len {
		insert(1, u64::from(i), 100 - 10 * i + 10);
	}
	chain_len
}

/// Rewrite the stored node of `(list_id, item)` in place, bypassing every list
/// invariant. The node must exist.
pub(crate) fn corrupt_node(
	list_id: ListId,
	item: ItemId,
	corrupt: impl FnOnce(&mut pallet_linked_list::Node<ItemId, Priority>),
) {
	pallet_linked_list::ListNodes::<Test>::mutate(list_id, item, |maybe| {
		corrupt(maybe.as_mut().expect("corrupt_node: the node is stored; qed"));
	});
}

/// Items in `list_id` head-to-tail.
pub(crate) fn dump(list_id: ListId) -> alloc::vec::Vec<(ItemId, Priority)> {
	use pallet_linked_list::SortedListInterface;
	let count = LinkedList::count(&list_id);
	let count = usize::try_from(count).expect("u32 fits in usize on test targets; qed");
	let mut out = alloc::vec::Vec::with_capacity(count);
	let mut cursor = LinkedList::head(&list_id);
	while let Some(item) = cursor {
		assert!(out.len() <= count, "dump: walk exceeded count (cycle?)");
		let node = pallet_linked_list::ListNodes::<Test>::get(list_id, item)
			.expect("listed items are stored; qed");
		out.push((item, node.priority));
		cursor = node.next;
	}
	out
}
