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

//! Benchmarks for `pallet-linked-list`.
//!
//! Covers the `reprioritize` dispatchable, the pallet's only call. The trait
//! surface (`insert`, `remove`, `re_insert`) is not benchmarked here: consumers
//! charge it inside their own benchmarks, which run it through real call paths.

#![cfg(feature = "runtime-benchmarks")]

use super::*;
use alloc::vec::Vec;
use frame::benchmarking::prelude::*;

/// Seed `list_id` with `count` items at strictly descending priorities, leaving
/// a gap of two units between adjacent items so that `target_priority - one()`
/// is a valid in-place re-insert value. Returns the items and their priorities
/// in head→tail (priority-descending) order.
///
/// # Errors
///
/// [`BenchmarkError::Stop`] if a seed insert fails, i.e. the runtime's list
/// cannot hold `count` items.
fn seed_chain<T: Config>(
	list_id: &T::ListId,
	count: u32,
) -> Result<(Vec<T::ItemId>, Vec<T::Priority>), BenchmarkError>
where
	T::Priority: One + Saturating,
{
	let one = T::Priority::one();
	let two = one.saturating_add(one);

	let mut priorities = Vec::with_capacity(count.saturated_into());
	let mut current = two;
	for _ in 0..count {
		priorities.push(current);
		current = current.saturating_add(two);
	}
	priorities.reverse();

	let mut items = Vec::with_capacity(count.saturated_into());
	for (index, priority) in (0..count).zip(&priorities) {
		let item: T::ItemId = account("seed", index, 0);
		let hint = Pallet::<T>::find_position(list_id.clone(), *priority);
		Pallet::<T>::insert(list_id.clone(), item.clone(), *priority, hint)
			.map_err(|_| BenchmarkError::Stop("Failed to seed the benchmark list"))?;
		items.push(item);
	}

	Ok((items, priorities))
}

#[benchmarks(
	where
		T::ListId: Default,
		T::Priority: One + Saturating,
)]
mod benchmarks {
	use super::*;

	/// `reprioritize` when the stored priority already matches the authoritative
	/// priority: one `ListNodes` read and an early return.
	#[benchmark]
	fn reprioritize_no_op() -> Result<(), BenchmarkError> {
		let list_id = T::ListId::default();
		let (seeded, priorities) = seed_chain::<T>(&list_id, 3)?;
		let target = seeded[1].clone();
		// Pin authoritative priority to the stored value so the drift check
		// returns equal.
		T::PriorityProvider::set_priority(&list_id, &target, priorities[1]);
		let caller: T::AccountId = whitelisted_caller();

		#[extrinsic_call]
		reprioritize(
			RawOrigin::Signed(caller),
			list_id.clone(),
			target.clone(),
			Position::endpoints_only(),
		);

		assert_eq!(Pallet::<T>::priority(&list_id, &target), Some(priorities[1]));
		Ok(())
	}

	/// `reprioritize` on the in-place fast path: the authoritative priority
	/// differs from stored but still fits between the current neighbors, so
	/// `re_insert` mutates the node without moving it.
	#[benchmark]
	fn reprioritize_in_place() -> Result<(), BenchmarkError> {
		let list_id = T::ListId::default();
		let (seeded, priorities) = seed_chain::<T>(&list_id, 3)?;
		let target = seeded[1].clone();
		// `target - one()` keeps the value strictly between the two neighbors
		// (gap-of-two seeds), so `neighbor_priorities_admit` succeeds and
		// `re_insert` takes the in-place fast path.
		let new_priority = priorities[1].saturating_sub(T::Priority::one());
		T::PriorityProvider::set_priority(&list_id, &target, new_priority);
		let caller: T::AccountId = whitelisted_caller();

		#[extrinsic_call]
		reprioritize(
			RawOrigin::Signed(caller),
			list_id.clone(),
			target.clone(),
			Position::endpoints_only(),
		);

		assert_eq!(Pallet::<T>::priority(&list_id, &target), Some(new_priority));
		assert_eq!(Pallet::<T>::head(&list_id), Some(seeded[0].clone()));
		assert_eq!(Pallet::<T>::tail(&list_id), Some(seeded[2].clone()));
		Ok(())
	}

	/// Disjoint old and new neighbours force five node writes and `s + 5` node reads
	/// while repairing a hint `s` steps stale.
	#[benchmark]
	fn reprioritize_relocate(
		s: Linear<0, { T::MaxHintRepairSteps::get() }>,
	) -> Result<(), BenchmarkError> {
		let list_id = T::ListId::default();
		let (seeded, priorities) = seed_chain::<T>(&list_id, s + 5)?;
		let s: usize = s.saturated_into();
		let target = seeded[s + 3].clone();
		let new_priority = priorities[0].saturating_sub(T::Priority::one());
		let hint = Position::between(seeded[s].clone(), seeded[s + 1].clone());
		T::PriorityProvider::set_priority(&list_id, &target, new_priority);
		let caller: T::AccountId = whitelisted_caller();

		#[extrinsic_call]
		reprioritize(RawOrigin::Signed(caller), list_id.clone(), target.clone(), hint);

		assert_eq!(
			ListNodes::<T>::get(&list_id, &target).map(|node| (
				node.prev,
				node.next,
				node.priority
			)),
			Some((Some(seeded[0].clone()), Some(seeded[1].clone()), new_priority))
		);
		assert_eq!(
			ListNodes::<T>::get(&list_id, &seeded[s + 2]).map(|old_prev| old_prev.next),
			Some(Some(seeded[s + 4].clone()))
		);
		assert_eq!(Pallet::<T>::head(&list_id), Some(seeded[0].clone()));
		assert_eq!(Pallet::<T>::tail(&list_id), Some(seeded[s + 4].clone()));
		Ok(())
	}

	/// `reprioritize` when [`crate::PriorityProvider::priority`] returns `None`:
	/// the item is removed from the list. No `set_priority` call is made, so
	/// the static provider has no entry for `target`.
	#[benchmark]
	fn reprioritize_priority_removed() -> Result<(), BenchmarkError> {
		let list_id = T::ListId::default();
		let (seeded, _) = seed_chain::<T>(&list_id, 3)?;
		let target = seeded[1].clone();
		let caller: T::AccountId = whitelisted_caller();

		#[extrinsic_call]
		reprioritize(
			RawOrigin::Signed(caller),
			list_id.clone(),
			target.clone(),
			Position::endpoints_only(),
		);

		assert!(!ListNodes::<T>::contains_key(&list_id, &target));
		Ok(())
	}

	impl_benchmark_test_suite! {
		Pallet,
		crate::mock::new_test_ext(),
		crate::mock::Test
	}
}
