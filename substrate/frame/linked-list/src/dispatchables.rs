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

//! Implementation of the [`Pallet::reprioritize`] dispatchable.

use crate::{
	pallet::*, weights::WeightInfo, ListError, Outcome, Position, PriorityProvider,
	SortedListInterface,
};
use frame::{
	deps::frame_support::dispatch::{DispatchErrorWithPostInfo, WithPostDispatchInfo},
	prelude::*,
};

impl<T: Config> Pallet<T> {
	/// Refresh `(list_id, item)`'s stored priority from [`crate::PriorityProvider`]
	/// and reposition it via [`SortedListInterface::re_insert`].
	pub(crate) fn do_reprioritize(
		list_id: T::ListId,
		item: T::ItemId,
		hint: Position<T::ItemId>,
	) -> Result<Weight, DispatchErrorWithPostInfo> {
		// Both fallible calls below surface `ItemNotFound` after only two reads
		// (provider + node), so it is refunded to the cheapest benchmarked
		// path; deeper failures consumed the repair-walk budget and keep the
		// full reserved weight.
		let map_early_exit = |e: ListError| match e {
			ListError::ItemNotFound => {
				Error::<T>::from(e).with_weight(T::WeightInfo::reprioritize_no_op())
			},
			other => Error::<T>::from(other).into(),
		};

		let Some(real_priority) = T::PriorityProvider::priority(&list_id, &item) else {
			Self::remove(&list_id, &item).map_err(map_early_exit)?;
			return Ok(T::WeightInfo::reprioritize_priority_removed());
		};

		// `re_insert` deposits `ItemReinserted` on the mutating paths; that is
		// the single event surface for priority changes, so nothing extra is
		// emitted here.
		let outcome =
			Self::re_insert(list_id, item, real_priority, hint).map_err(map_early_exit)?;

		Ok(match outcome {
			Outcome::NoOp => T::WeightInfo::reprioritize_no_op(),
			Outcome::InPlace => T::WeightInfo::reprioritize_in_place(),
			Outcome::Relocated { steps } => T::WeightInfo::reprioritize_relocate(steps),
		})
	}
}
