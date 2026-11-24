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

//! Storage shape types of the pallet. The consumer-facing shape types
//! ([`Position`](crate::Position), [`ListError`](crate::ListError),
//! [`Outcome`](crate::Outcome)) live in `linked-list-interface`.
//!
//! [`ListMeta`] bundles the head and tail pointers of a single list into one
//! storage row so they can be read/written together.

use frame::prelude::*;

/// Per-list head/tail pair, stored as a single row in [`crate::ListMetas`].
/// Absence of the row encodes the empty list.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct ListMeta<ItemId> {
	/// Highest-priority item in the list.
	pub head: ItemId,
	/// Lowest-priority item in the list.
	pub tail: ItemId,
}
