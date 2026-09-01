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

//! The stored global ceiling as a stablecoin-wide supply cap: vault borrowing
//! yields headroom to live PSM debt and regains it on PSM redemption, through
//! the runtime's `VaultsGlobalDebtCeiling` provider.

use crate::imports::*;
use asset_hub_westend_runtime::{
	pusd_config::TrustBackedAssetLocation, Assets, OriginCaller, Psm, Vaults,
};
use frame_support::assert_err;
use sp_runtime::{traits::MaybeEquivalence, Permill};
use xcm::v5::Location;

/// Trust-backed asset id for the PSM's external stablecoin, a USDC stand-in.
/// Distinct from [`PUSD_ID`] and the assets the emulated genesis creates.
const USDC_ID: u32 = 50_000_343;

/// Registers a deposit-free, Root-administered PSM instance for pUSD, with a
/// freshly created [`USDC_ID`] approved at full ceiling weight and zero fees.
fn create_pusd_psm(max_debt: Balance) -> (Location, Location) {
	let internal = TrustBackedAssetLocation::convert_back(&get_pusd_id())
		.expect("trust-backed ids convert to locations");
	assert_ok!(Assets::force_create(
		RuntimeOrigin::root(),
		USDC_ID.into(),
		MultiAddress::Id(admin()),
		true,
		PUSD_MIN_BALANCE,
	));
	let external = TrustBackedAssetLocation::convert_back(&USDC_ID)
		.expect("trust-backed ids convert to locations");

	let root: OriginCaller = frame_system::RawOrigin::<AccountId>::Root.into();
	assert_ok!(Psm::create_psm(
		RuntimeOrigin::root(),
		internal.clone(),
		Box::new(root.clone()),
		Box::new(root),
		acct(0xFE),
		max_debt,
		1,
	));
	assert_ok!(Psm::add_external_asset(RuntimeOrigin::root(), internal.clone(), external.clone()));
	assert_ok!(Psm::set_asset_ceiling_weight(
		RuntimeOrigin::root(),
		internal.clone(),
		external.clone(),
		Permill::from_percent(100),
	));
	// Fees default to 0.5%; zero keeps swap amounts and `PsmDebt` round.
	assert_ok!(Psm::set_minting_fee(
		RuntimeOrigin::root(),
		internal.clone(),
		external.clone(),
		Permill::zero(),
	));
	assert_ok!(Psm::set_redemption_fee(
		RuntimeOrigin::root(),
		internal.clone(),
		external.clone(),
		Permill::zero(),
	));
	(internal, external)
}

// A vault borrow approved under the cap before a PSM mint fails after it, in
// the same block, and passes again once the PSM debt is redeemed. The ceiling
// provider reads live `PsmDebt`, so no governance action moves the headroom.
#[test]
fn psm_debt_consumes_and_releases_vault_headroom() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());
		// The stored ceiling is the coin's total supply cap across vaults and PSM.
		lift_global_ceiling(1_000 * PUSD);

		let owner = acct(1);
		open_vault(&owner, 1_000 * WND, 600 * PUSD, FixedU128::from_rational(5, 100));

		let (internal, external) = create_pusd_psm(10_000 * PUSD);
		let user = acct(2);
		assert_ok!(<Assets as Mutate<AccountId>>::mint_into(USDC_ID, &user, 300 * PUSD));
		assert_ok!(Psm::mint(
			RuntimeOrigin::signed(user.clone()),
			internal.clone(),
			external.clone(),
			300 * PUSD,
			Permill::zero(),
		));
		assert_eq!(pusd_balance(&user), 300 * PUSD);
		assert_eq!(pallet_psm::PsmDebt::<Runtime>::get(&internal, &external), 300 * PUSD);

		// Another 150 pUSD fits the 1_000 cap on the vault side alone
		// (600 + 150), but live PSM debt of 300 leaves vaults only 700.
		let borrow = |amount: Balance| {
			Vaults::borrow(
				RuntimeOrigin::signed(owner.clone()),
				get_native_id(),
				get_pusd_id(),
				amount,
				None,
				None,
				pallet_linked_list::Position::endpoints_only(),
			)
		};
		assert_err!(borrow(150 * PUSD), pallet_vaults::Error::<Runtime>::GlobalDebtCeilingExceeded);

		// Redeeming the PSM debt returns its headroom to vault borrowers at
		// once: the identical borrow now fits.
		assert_ok!(Psm::redeem(
			RuntimeOrigin::signed(user.clone()),
			internal.clone(),
			external,
			300 * PUSD,
			Permill::zero(),
		));
		assert_eq!(pusd_balance(&user), 0);
		assert_ok!(borrow(150 * PUSD));
		assert_eq!(vault(&owner).debt.principal, 750 * PUSD);
	});
}
