// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

//! Relay-chain-derived time for parachain runtimes.
use super::{pallet, FixedVelocityConsensusHook};
use core::{marker::PhantomData, time::Duration};
use frame_support::traits::{Time, UnixTime};

/// A [`Time`] and [`UnixTime`] provider that reports the start of the relay parent's slot.
///
/// The reported time is `relay_chain_slot * RELAY_CHAIN_SLOT_DURATION_MILLIS` — a Unix timestamp,
/// since BABE derives slot numbers from Unix time and [`FixedVelocityConsensusHook`] validates
/// the slot against the relay chain state proof. It advances every relay chain slot rather than
/// once per parachain slot.
///
/// # Guarantees
///
/// After the validation-data inherent runs, with `dur` the parachain slot duration and
/// `para_slot` the current parachain slot:
///
/// - `para_slot * dur <= Self::now() < (para_slot + 1) * dur`
/// - Differs from the `pallet_timestamp` value by less than one parachain slot.
/// - Does not decrease across blocks of the same chain branch.
/// - Trails wall clock by at least one relay chain block plus the configured relay parent offset.
/// - Blocks sharing a relay chain slot report the same value.
///
/// # Fallback
///
/// Before the validation-data inherent invokes the hook for the first time, no relay chain slot
/// is recorded and the latest parachain timestamp is returned instead — the same staleness
/// `pallet_timestamp` itself has at that point.
///
/// # Example Configuration
///
/// ```ignore
/// type ConsensusHook = cumulus_pallet_aura_ext::FixedVelocityConsensusHook<
/// 	Runtime,
/// 	RELAY_CHAIN_SLOT_DURATION_MILLIS,
/// 	BLOCK_PROCESSING_VELOCITY,
/// 	UNINCLUDED_SEGMENT_CAPACITY,
/// >;
/// type RelayTime = cumulus_pallet_aura_ext::RelayTimestamp<ConsensusHook>;
/// ```
pub struct RelayTimestamp<Hook>(PhantomData<Hook>);

impl<
		T: pallet::Config,
		const RELAY_CHAIN_SLOT_DURATION_MILLIS: u32,
		const V: u32,
		const C: u32,
	> Time for RelayTimestamp<FixedVelocityConsensusHook<T, RELAY_CHAIN_SLOT_DURATION_MILLIS, V, C>>
where
	<T as pallet_timestamp::Config>::Moment: Into<u64>,
{
	type Moment = u64;

	fn now() -> Self::Moment {
		match pallet::RelaySlotInfo::<T>::get() {
			Some((relay_chain_slot, _)) => {
				u64::from(RELAY_CHAIN_SLOT_DURATION_MILLIS).saturating_mul(*relay_chain_slot)
			},
			None => pallet_timestamp::Now::<T>::get().into(),
		}
	}
}

impl<
		T: pallet::Config,
		const RELAY_CHAIN_SLOT_DURATION_MILLIS: u32,
		const V: u32,
		const C: u32,
	> UnixTime
	for RelayTimestamp<FixedVelocityConsensusHook<T, RELAY_CHAIN_SLOT_DURATION_MILLIS, V, C>>
where
	<T as pallet_timestamp::Config>::Moment: Into<u64>,
{
	fn now() -> Duration {
		Duration::from_millis(<Self as Time>::now())
	}
}
