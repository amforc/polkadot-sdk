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

//! Scenario tests for the stablecoin vaults, redemptions, and stability pool on
//! the real Asset Hub Westend runtime.

#[cfg(test)]
mod imports {
	pub(crate) use frame_support::{
		assert_ok,
		traits::{
			fungible::{Inspect as FungibleInspect, Mutate as FungibleMutate},
			fungibles::{Inspect, InspectHold, Mutate},
		},
	};
	pub(crate) use parachains_common::{AccountId, Balance};
	pub(crate) use sp_runtime::{
		traits::{One, Zero},
		FixedU128, MultiAddress, Permill,
	};

	pub(crate) use emulated_integration_tests_common::xcm_emulator::TestExt;
	pub(crate) use westend_system_emulated_network::{
		asset_hub_westend_emulated_chain::asset_hub_westend_runtime::{
			self, pusd_config::VaultsCollateralId, Runtime, RuntimeOrigin,
		},
		AssetHubWestendPara as AssetHubWestend,
	};

	pub(crate) use crate::setup::*;
}

#[cfg(test)]
mod setup;

#[cfg(test)]
mod tests;
