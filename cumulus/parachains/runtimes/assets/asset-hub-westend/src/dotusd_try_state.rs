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

//! A try-runtime check of only the stablecoin pallets, cheap enough to sample often.
//!
//! `TryRuntime_execute_block` decodes every storage value of every pallet whenever any pallet's
//! try-state is selected. On a fork of a live Asset Hub that is the whole chain state and takes
//! tens of seconds per sample. This API runs the stablecoin pallets' try-state and decodes only
//! their storage, on the state of the block it is called at, without re-executing that block.

use super::*;
use alloc::{format, string::String};
use frame_support::traits::{TryDecodeEntireStorage, TryState, TryStateSelect};

/// The pallets the check covers: the vault list, vaults, redemptions, the stability pool and the
/// PSM.
type DotUsdPallets = (LinkedList, Vaults, Redemptions, Stability, Psm);

sp_api::decl_runtime_apis! {
	/// Checks the stablecoin pallets' invariants on the state at the call's block.
	pub trait DotUsdTryStateApi {
		/// Runs every stablecoin pallet's try-state, then decodes all of their storage. `Err`
		/// carries the first failure, formatted for a log.
		fn try_state() -> Result<(), String>;
	}
}

/// The implementation behind [`DotUsdTryStateApi::try_state`].
pub fn check() -> Result<(), String> {
	let block_number = frame_system::Pallet::<Runtime>::block_number();
	// Same guard as `Executive::try_execute_block`: a try-state hook that writes would report
	// on a state it changed, so the call panics instead.
	let _guard = frame_support::StorageNoopGuard::default();
	<DotUsdPallets as TryState<frame_system::pallet_prelude::BlockNumberFor<Runtime>>>::try_state(
		block_number,
		TryStateSelect::All,
	)
	.map_err(|error| format!("try-state failed: {error:?}"))?;
	<DotUsdPallets as TryDecodeEntireStorage>::try_decode_entire_state()
		.map_err(|errors| format!("storage decode failed: {errors:?}"))?;
	Ok(())
}
