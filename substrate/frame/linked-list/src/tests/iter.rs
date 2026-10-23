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

use crate::mock::*;

#[test]
fn iter_from_tail_takes_up_to_n() {
	build_and_execute(|| {
		assert!(LinkedList::iter_from_tail(1, 5).is_empty());

		insert(1, 1, 90);
		insert(1, 2, 50);
		insert(1, 3, 10);
		assert!(LinkedList::iter_from_tail(1, 0).is_empty());
		assert_eq!(LinkedList::iter_from_tail(1, 2), vec![3, 2]);
		assert_eq!(LinkedList::iter_from_tail(1, 99), vec![3, 2, 1]);
	});
}
