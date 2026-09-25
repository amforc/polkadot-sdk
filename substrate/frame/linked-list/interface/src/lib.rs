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

//! # Linked-list interface
//!
//! Consumer-facing interface of the linked-list pallet: a generic sorted
//! doubly-linked list, one list per `ListId`, kept in priority order from head
//! (highest) to tail (lowest).
//!
//! Consumer pallets depend on this crate only. The pallet implementing
//! [`SortedListInterface`] is wired in at runtime-assembly time, so consumers
//! do not need the implementing pallet as a dependency.
//!
//! - [`SortedListInterface`] — mutation and query surface.
//! - [`PriorityProvider`] — the consumer-side callback giving the authoritative priority per item.
//! - [`Position`] — typed `(prev, next)` insertion hint.
//! - [`ListError`] — failure modes of interface operations.
//! - [`Outcome`] — which path a `re_insert` took.
//! - [`fifo_append`] — FIFO-discipline append helper on top of the interface.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod traits;
mod types;

pub use traits::{fifo_append, PriorityProvider, SortedListInterface};
pub use types::{ListError, Outcome, Position};
