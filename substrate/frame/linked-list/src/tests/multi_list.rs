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

use crate::{mock::*, SortedListInterface};
use frame::testing_prelude::assert_ok;

#[test]
fn lists_are_independent() {
	build_and_execute(|| {
		insert(1, 100, 50);
		insert(2, 100, 70); // same ItemId, different list.
		insert(2, 200, 30);
		assert_eq!(dump(1), vec![(100, 50)]);
		assert_eq!(dump(2), vec![(100, 70), (200, 30)]);
		// `dump` walks from `head` and only bounds the walk by `count`, so the
		// `tail` accessor (read by callers such as pallet-vaults) and an
		// over-reported `count` need their own checks.
		assert_eq!(LinkedList::tail(&2), Some(200));
		assert_eq!(LinkedList::count(&2), 2);

		assert_ok!(LinkedList::remove(&1, &100));
		assert_eq!(dump(1), vec![]);
		assert_eq!(dump(2), vec![(100, 70), (200, 30)]);
	});
}
