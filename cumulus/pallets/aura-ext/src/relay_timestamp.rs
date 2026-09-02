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
/// `pallet_timestamp` is pinned to the start of the parachain slot, so with a parachain slot
/// longer than the relay chain slot every block in that slot carries the same timestamp. This
/// provider returns `relay_chain_slot * RELAY_CHAIN_SLOT_DURATION_MILLIS` instead, which advances
/// with every relay chain slot. The slot is taken from the relay chain state proof as validated by
/// [`FixedVelocityConsensusHook`].
///
/// The value lies within the current parachain slot, never decreases along a chain branch and
/// trails wall clock by up to one relay chain slot plus the relay parent offset. Like
/// `pallet_timestamp::Now` it is only updated by the inherent, so in `on_initialize` it belongs to
/// the previous block. Before the first state proof it falls back to `pallet_timestamp::Now`.
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
