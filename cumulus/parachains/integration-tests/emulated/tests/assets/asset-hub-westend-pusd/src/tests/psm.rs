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
use emulated_integration_tests_common::USDT_ID;
use frame_support::{assert_noop, hypothetically_ok};
use sp_runtime::{traits::MaybeEquivalence, DispatchResult, Permill};
use xcm::v5::Location;

/// USDt, the PSM's external asset: trust-backed asset 1984 on Asset Hub. The
/// emulated genesis creates it but sets no metadata; the live asset carries six
/// decimals.
const USDT_DECIMALS: u8 = 6;
/// One whole USDt.
const USDT: Balance = 10u128.pow(USDT_DECIMALS as u32);

/// Aggregate debt ceiling of the dotUSD PSM: ten million dotUSD.
const PSM_MAX_DEBT: Balance = 10_000_000 * PUSD;
/// Smallest swap the PSM accepts: one dotUSD.
const PSM_MIN_SWAP: Balance = PUSD;
/// Minting is free.
const PSM_MINTING_FEE: Permill = Permill::zero();
/// Redemption charges one basis point.
const PSM_REDEMPTION_FEE: Permill = Permill::from_parts(100);

/// Trust-backed location of the stablecoin, the key of its PSM instance.
fn pusd_location() -> Location {
	TrustBackedAssetLocation::convert_back(&PUSD_ID).expect("trust-backed ids convert to locations")
}

/// Trust-backed location of USDt.
fn usdt_location() -> Location {
	TrustBackedAssetLocation::convert_back(&USDT_ID).expect("trust-backed ids convert to locations")
}

/// Gives USDt its live metadata. The emulated genesis creates the asset with
/// no metadata, so its decimals read as zero until this runs.
fn set_usdt_metadata() {
	assert_ok!(Assets::force_set_metadata(
		RuntimeOrigin::root(),
		USDT_ID.into(),
		b"Tether USD".to_vec(),
		b"USDt".to_vec(),
		USDT_DECIMALS,
		false,
	));
}

fn create_pusd_psm() -> (Location, Location) {
	let internal = pusd_location();
	let external = usdt_location();
	let root: OriginCaller = frame_system::RawOrigin::<AccountId>::Root.into();
	assert_ok!(Psm::create_psm(
		RuntimeOrigin::root(),
		internal.clone(),
		Box::new(root.clone()),
		Box::new(root),
		insurance_account(),
		PSM_MAX_DEBT,
		PSM_MIN_SWAP,
	));
	assert_ok!(Psm::add_external_asset(RuntimeOrigin::root(), internal.clone(), external.clone()));
	assert_ok!(Psm::set_asset_ceiling_weight(
		RuntimeOrigin::root(),
		internal.clone(),
		external.clone(),
		Permill::from_percent(100),
	));
	assert_ok!(Psm::set_minting_fee(
		RuntimeOrigin::root(),
		internal.clone(),
		external.clone(),
		PSM_MINTING_FEE,
	));
	assert_ok!(Psm::set_redemption_fee(
		RuntimeOrigin::root(),
		internal.clone(),
		external.clone(),
		PSM_REDEMPTION_FEE,
	));
	(internal, external)
}

/// Steps 1 and 2 of the referendum on top of a vault market, so vault and PSM
/// share the stablecoin. `create_branch` already plays step 1.
fn launch_pusd_psm() -> (Location, Location) {
	feed_price(dot_price(2, 1));
	create_branch(&BranchSpec::default());
	set_usdt_metadata();
	create_pusd_psm()
}

/// Calls `Vaults::borrow` for `owner` on the native market.
fn borrow(owner: &AccountId, amount: Balance) -> DispatchResult {
	Vaults::borrow(
		RuntimeOrigin::signed(owner.clone()),
		get_native_id(),
		PUSD_ID,
		amount,
		None,
		None,
		pallet_linked_list::Position::endpoints_only(),
	)
}

// A vault borrow approved under the cap before a PSM mint fails after it, in
// the same block, and passes again once the PSM debt is redeemed. The ceiling
// provider reads live `PsmDebt`, so no governance action moves the headroom.
#[test]
fn psm_debt_consumes_and_releases_vault_headroom() {
	AssetHubWestend::execute_with(|| {
		let (internal, external) = launch_pusd_psm();
		// The stored ceiling is the coin's total supply cap across vaults and PSM.
		lift_global_ceiling(1_000 * PUSD);

		let owner = acct(1);
		open_vault(&owner, 1_000 * WND, 600 * PUSD, FixedU128::from_rational(5, 100));
		// Another 150 dotUSD fits the 1_000 cap while the PSM holds no debt.
		hypothetically_ok!(borrow(&owner, 150 * PUSD));

		let user = acct(2);
		assert_ok!(<Assets as Mutate<AccountId>>::mint_into(USDT_ID, &user, 300 * USDT));
		assert_ok!(Psm::mint(
			RuntimeOrigin::signed(user.clone()),
			internal.clone(),
			external.clone(),
			300 * USDT,
			PSM_MINTING_FEE,
		));
		assert_eq!(pusd_balance(&user), 300 * PUSD);
		assert_eq!(pallet_psm::PsmDebt::<Runtime>::get(&internal, &external), 300 * PUSD);

		// The same 150 dotUSD still fits on the vault side alone (600 + 150),
		// but live PSM debt of 300 leaves vaults only 700.
		assert_noop!(
			borrow(&owner, 150 * PUSD),
			pallet_vaults::Error::<Runtime>::GlobalDebtCeilingExceeded
		);
		hypothetically_ok!(borrow(&owner, PUSD));

		// A stored ceiling below the live PSM debt leaves vaults no headroom at
		// all. Existing debt stays where it is, and repayment is never gated.
		lift_global_ceiling(200 * PUSD);
		assert_noop!(
			borrow(&owner, PUSD),
			pallet_vaults::Error::<Runtime>::GlobalDebtCeilingExceeded
		);
		assert_eq!(vault(&owner).debt.principal, 600 * PUSD);
		hypothetically_ok!(Vaults::repay_for(
			RuntimeOrigin::signed(owner.clone()),
			get_native_id(),
			PUSD_ID,
			owner.clone(),
			Some(100 * PUSD),
		));
		lift_global_ceiling(1_000 * PUSD);

		// Redeeming the PSM debt returns its headroom to vault borrowers at
		// once. The live one-basis-point fee goes to the Insurance Fund, so
		// the user burns 299.97 and 0.03 stays in circulation as fee revenue.
		assert_ok!(Psm::redeem(
			RuntimeOrigin::signed(user.clone()),
			internal.clone(),
			external.clone(),
			300 * PUSD,
			PSM_REDEMPTION_FEE,
		));
		let fee = PSM_REDEMPTION_FEE.mul_ceil(300 * PUSD);
		assert_eq!(fee, 3 * PUSD / 100);
		assert_eq!(pusd_balance(&user), 0);
		assert_eq!(pusd_balance(&insurance_account()), fee);
		assert_eq!(<Assets as Inspect<AccountId>>::balance(USDT_ID, &user), 300 * USDT - fee);
		assert_eq!(pallet_psm::PsmDebt::<Runtime>::get(&internal, &external), fee);

		// Vaults regain all but the fee's worth of headroom: 1_000 − 600 − 0.03.
		assert_noop!(
			borrow(&owner, 400 * PUSD - fee + 1),
			pallet_vaults::Error::<Runtime>::GlobalDebtCeilingExceeded
		);
		assert_ok!(borrow(&owner, 150 * PUSD));
		assert_eq!(vault(&owner).debt.principal, 750 * PUSD);
	});
}

// The one basis point fee on a one dotUSD redemption is 0.0001 dotUSD, under
// the coin's 0.01 minimum balance, so it cannot open the Insurance Fund's
// account. At launch that account is empty: until a redemption of at least
// 100 dotUSD (or a deposit) opens it, every redemption under 100 dotUSD fails
// with a token error rather than a PSM one.
#[test]
fn redemption_fee_cannot_open_an_empty_insurance_fund() {
	AssetHubWestend::execute_with(|| {
		let (internal, external) = launch_pusd_psm();
		let user = acct(2);
		assert_ok!(<Assets as Mutate<AccountId>>::mint_into(USDT_ID, &user, 200 * USDT));
		assert_ok!(Psm::mint(
			RuntimeOrigin::signed(user.clone()),
			internal.clone(),
			external.clone(),
			200 * USDT,
			PSM_MINTING_FEE,
		));
		let redeem = |amount: Balance| {
			Psm::redeem(
				RuntimeOrigin::signed(user.clone()),
				internal.clone(),
				external.clone(),
				amount,
				PSM_REDEMPTION_FEE,
			)
		};

		assert_eq!(pusd_balance(&insurance_account()), 0);
		assert_noop!(redeem(PUSD), sp_runtime::TokenError::BelowMinimum);

		// A 100 dotUSD redemption pays exactly the minimum balance in fees and
		// opens the account; the one dotUSD redemption then goes through.
		assert_ok!(redeem(100 * PUSD));
		assert_eq!(pusd_balance(&insurance_account()), PUSD_MIN_BALANCE);
		assert_ok!(redeem(PUSD));
		assert_eq!(pusd_balance(&insurance_account()), PUSD_MIN_BALANCE + PUSD / 10_000);
	});
}
