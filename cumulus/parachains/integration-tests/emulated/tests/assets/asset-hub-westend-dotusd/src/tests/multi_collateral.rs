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

//! Collateral from every asset pallet on Asset Hub: the native token, a
//! trust-backed asset, and a bridged foreign asset.

use crate::imports::*;
use asset_hub_westend_runtime::{
	dotusd_config::{
		sepolia_usdt_location, TrustBackedAssetLocation, VaultsCollateral, VaultsOracleAdapter,
	},
	Balances, ForeignAssets, Vaults,
};
use emulated_integration_tests_common::{snowbridge::SEPOLIA_ID, RESERVABLE_ASSET_ID, USDT_ID};
use frame_support::{
	assert_noop,
	traits::{
		fungibles::Create,
		tokens::{Fortitude, Preservation},
	},
};
use sp_runtime::{traits::MaybeEquivalence, TokenError};
use stablecoin_primitives::ProvidePrice;
use xcm::v5::{Junction::GlobalConsensus, Location, NetworkId};

const USDT: Balance = 1_000_000;

/// Asset Hub Westend registers this token as insufficient with a minimum balance of one.
/// The tests mint the collateral used by each scenario.
fn register_bridged_usdt() -> VaultsCollateralId {
	let id = sepolia_usdt_location();
	assert_ok!(<ForeignAssets as Create<AccountId>>::create(id.clone(), admin(), false, 1));
	id
}

/// Sepolia ETH on Asset Hub. The emulated genesis registers it.
fn eth_id() -> VaultsCollateralId {
	Location::new(2, [GlobalConsensus(NetworkId::Ethereum { chain_id: SEPOLIA_ID })])
}

fn usdt_id() -> VaultsCollateralId {
	TrustBackedAssetLocation::convert_back(&USDT_ID).expect("trust-backed ids have a location")
}

/// One stablecoin, three branches: native WND, trust-backed USDT, bridged USDT.
///
/// Each vault pledges 20,000 dotUSD of value against 10,000 dotUSD of debt. The
/// three differ only in the collateral pallet and the decimals.
#[test]
fn native_trust_backed_and_foreign_collateral_markets_coexist() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());

		feed_price_for(usdt_id(), FixedU128::from_u32(1));
		create_market_signed(usdt_id(), &BranchSpec::default());

		// Both USDT assets use the same published price.
		let bridged = register_bridged_usdt();
		create_market_signed(bridged.clone(), &BranchSpec::default());

		let unmapped = TrustBackedAssetLocation::convert_back(&RESERVABLE_ASSET_ID)
			.expect("trust-backed ids have a location; qed");
		assert_eq!(
			VaultsOracleAdapter::provide_price(&unmapped),
			Err(sp_runtime::DispatchError::Unavailable)
		);
		// Sepolia ETH exists in genesis. The oracle has no ETH/USD pair.
		assert_eq!(
			VaultsOracleAdapter::provide_price(&eth_id()),
			Err(sp_runtime::DispatchError::Unavailable)
		);
		assert_noop!(
			Vaults::create_branch(
				RuntimeOrigin::root(),
				eth_id(),
				DOTUSD_ID,
				branch_admins(),
				branch_config(&eth_id(), &BranchSpec::default()),
				registration_config(),
			),
			pallet_vaults::Error::<Runtime>::OraclePriceNotAvailable
		);

		let native_owner = acct(1);
		open_vault(&native_owner, 10_000 * WND, 10_000 * DOTUSD, FixedU128::zero());
		let usdt_owner = acct(2);
		open_vault_on(usdt_id(), &usdt_owner, 20_000 * USDT, 10_000 * DOTUSD, FixedU128::zero());
		let bridged_owner = acct(3);
		open_vault_on(
			bridged.clone(),
			&bridged_owner,
			20_000 * USDT,
			10_000 * DOTUSD,
			FixedU128::zero(),
		);

		assert_eq!(vault(&native_owner).collateral, 10_000 * WND);
		assert_eq!(vault_on(&usdt_id(), &usdt_owner).collateral, 20_000 * USDT);
		assert_eq!(vault_on(&bridged, &bridged_owner).collateral, 20_000 * USDT);
		// Same debt, same stablecoin, three collaterals.
		assert_eq!(dotusd_balance(&native_owner), 10_000 * DOTUSD);
		assert_eq!(dotusd_balance(&usdt_owner), 10_000 * DOTUSD);
		assert_eq!(dotusd_balance(&bridged_owner), 10_000 * DOTUSD);

		// Each asset pallet holds exactly the vault collateral. The minimum-balance float
		// is free but not reducible while the hold keeps the account alive.
		for (collateral_id, owner) in
			[(get_native_id(), native_owner), (usdt_id(), usdt_owner), (bridged, bridged_owner)]
		{
			assert_eq!(
				collateral_on_hold(&collateral_id, &owner),
				vault_on(&collateral_id, &owner).collateral,
			);
			assert_eq!(
				collateral_free(&collateral_id, &owner),
				collateral_min_balance(&collateral_id)
			);
			assert_eq!(
				<VaultsCollateral as Inspect<AccountId>>::reducible_balance(
					collateral_id.clone(),
					&owner,
					Preservation::Expendable,
					Fortitude::Polite,
				),
				0,
			);
		}
	});
}

/// 15,000 bridged USDT against 10,000 dotUSD debt: CR 150% at par, 120% at 0.80.
/// The pool receives 13,125 USDT and burns half of its 20,000 dotUSD deposit.
#[test]
fn foreign_collateral_liquidation_offsets_and_claims_out() {
	AssetHubWestend::execute_with(|| {
		liquidate_and_claim_out(LiquidationOptions {
			collateral_id: register_bridged_usdt(),
			price_open: FixedU128::from_u32(1),
			collateral: 15_000 * USDT,
			price_crash: FixedU128::from_rational(80, 100),
			filler_collateral: 30_000 * USDT,
			pool_deposit: 20_000 * DOTUSD,
			seized: 13_125 * USDT,
			active_deposit_after: 10_000 * DOTUSD,
		});
	});
}

/// 14,000 USDT against 10,000 dotUSD debt: CR 140% at par, 117.6% at 0.84. seized =
/// min(14,000, 10,000 × 1.05 / 0.84) = 12,500 USDT; 10,000 of the 25,000 dotUSD deposit
/// burns, so P = 0.6.
#[test]
fn trust_backed_collateral_liquidation_offsets_and_claims_out() {
	AssetHubWestend::execute_with(|| {
		liquidate_and_claim_out(LiquidationOptions {
			collateral_id: usdt_id(),
			price_open: FixedU128::from_u32(1),
			collateral: 14_000 * USDT,
			price_crash: FixedU128::from_rational(84, 100),
			filler_collateral: 1_000_000 * USDT,
			pool_deposit: 25_000 * DOTUSD,
			seized: 12_500 * USDT,
			active_deposit_after: 15_000 * DOTUSD,
		});
	});
}

struct LiquidationOptions {
	collateral_id: VaultsCollateralId,
	/// dotUSD per unit of collateral, fed before the market is created.
	price_open: FixedU128,
	collateral: Balance,
	/// dotUSD per unit of collateral after the crash.
	price_crash: FixedU128,
	/// Collateral of a healthy vault that keeps the branch populated afterwards.
	filler_collateral: Balance,
	pool_deposit: Balance,
	/// `min(collateral, debt × 1.05 / price_crash)`.
	seized: Balance,
	active_deposit_after: Balance,
}

/// Liquidates a 10,000 dotUSD vault on a non-native asset pallet below MCR 125%.
///
/// Seizure caps at debt × 1.05 in value and this spec pays the keeper nothing,
/// so the whole seizure moves from the vault hold into the pool, the surplus and
/// the storage deposit return to the owner, and the sole depositor
/// claims all of the pool's gain.
fn liquidate_and_claim_out(options: LiquidationOptions) {
	let id = options.collateral_id;
	create_dotusd();
	// The vault deposit is priced in WND, so the WND feed must exist too.
	feed_price(dot_price(2, 1));
	feed_price_for(id.clone(), options.price_open);
	create_market_signed(id.clone(), &accounting_spec());

	let liquidated_owner = acct(1);
	open_vault_on(
		id.clone(),
		&liquidated_owner,
		options.collateral,
		10_000 * DOTUSD,
		FixedU128::zero(),
	);
	// The deposit is priced at open; the price move below does not re-price the ticket.
	let (deposit_asset, deposit) = expected_vault_deposit(&id, &liquidated_owner);
	let filler_owner = acct(2);
	open_vault_on(
		id.clone(),
		&filler_owner,
		options.filler_collateral,
		10_000 * DOTUSD,
		FixedU128::zero(),
	);

	// The depositor holds no collateral. WND provides the account reference needed to
	// receive an insufficient foreign asset.
	let depositor = acct(3);
	assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(&depositor, get_native_ed()));
	sp_deposit_matured_on(id.clone(), &depositor, options.pool_deposit);
	assert_eq!(collateral_free(&id, &depositor), 0);

	feed_price_for(id.clone(), options.price_crash); // below MCR 125%
	let owner_free_before = collateral_free(&id, &liquidated_owner);
	let native_before = native_balance(&liquidated_owner);
	liquidate_on(id.clone(), &liquidated_owner);

	assert_eq!(
		collateral_free(&id, &liquidated_owner) - owner_free_before,
		options.collateral - options.seized + if deposit_asset == id { deposit } else { 0 },
	);
	if deposit_asset == get_native_id() {
		assert_eq!(native_balance(&liquidated_owner) - native_before, deposit);
	}
	// The pool burns the full 10,000 dotUSD debt and receives the whole seizure,
	// on top of the custody float of one minimum per asset.
	let pool = pool_account_on(&id);
	assert_eq!(dotusd_balance(&pool) - DOTUSD_MIN_BALANCE, options.pool_deposit - 10_000 * DOTUSD);
	assert_eq!(collateral_free(&id, &pool) - collateral_min_balance(&id), options.seized);
	// The liquidated vault is removed.
	assert_eq!(vault_status_on(&id, &liquidated_owner), None);

	// The sole depositor's gain is the whole seizure.
	claim_collateral_out(&id, &depositor, options.seized);
	// The claim realizes the row.
	assert_eq!(deposit_row_on(&id, &depositor).active_deposit, options.active_deposit_after);
}

/// The runtime deposit policy over real sufficiency flags and decimals: a
/// sufficient collateral carries its own vault deposit, priced from the oracle
/// (WND at 2 dotUSD and USDT at par, so one WND of deposit costs two USDT).
/// Opening the vault requires both prices.
#[test]
fn vault_deposit_asset_follows_runtime_policy() {
	AssetHubWestend::execute_with(|| {
		create_dotusd();
		// No WND feed yet: branch creation needs only the collateral's own price.
		feed_price_for(usdt_id(), FixedU128::from_u32(1));
		create_market_signed(usdt_id(), &BranchSpec::default());
		let bridged = register_bridged_usdt();
		create_market_signed(bridged.clone(), &BranchSpec::default());

		let usdt_owner = acct(1);
		fund_collateral(&usdt_id(), &usdt_owner, 20_000 * USDT);
		let open_usdt = || {
			Vaults::open_vault(
				RuntimeOrigin::signed(usdt_owner.clone()),
				usdt_id(),
				DOTUSD_ID,
				20_000 * USDT,
				10_000 * DOTUSD,
				FixedU128::zero(),
				pallet_linked_list::Position::endpoints_only(),
			)
		};
		assert_noop!(open_usdt(), sp_runtime::DispatchError::Other("oracle price unavailable"));

		// Insufficient collateral requires a WND deposit even without a WND price.
		let owner = acct(3);
		let (deposit_asset, deposit) = expected_vault_deposit(&bridged, &owner);
		assert_eq!(deposit_asset, get_native_id());
		// The bridged asset's longer location increases the vault footprint.
		let (_, native_deposit) = expected_vault_deposit(&get_native_id(), &owner);
		assert!(deposit > native_deposit);

		fund_collateral(&bridged, &owner, 20_000 * USDT);
		let open = || {
			Vaults::open_vault(
				RuntimeOrigin::signed(owner.clone()),
				bridged.clone(),
				DOTUSD_ID,
				20_000 * USDT,
				10_000 * DOTUSD,
				FixedU128::zero(),
				pallet_linked_list::Position::endpoints_only(),
			)
		};
		assert_noop!(open(), TokenError::FundsUnavailable);
		fund_vault_deposit(&bridged, &owner);
		assert_ok!(open());
		assert_eq!(vault_deposit_on_hold(&get_native_id(), &owner), deposit);
		assert_eq!(vault_deposit_on_hold(&bridged, &owner), 0);

		feed_price(dot_price(2, 1));
		let (native_deposit_asset, native_deposit) =
			expected_vault_deposit(&get_native_id(), &usdt_owner);
		assert_eq!(native_deposit_asset, get_native_id());
		let (usdt_deposit_asset, usdt_deposit) = expected_vault_deposit(&usdt_id(), &usdt_owner);
		assert_eq!(usdt_deposit_asset, usdt_id());
		// 2 dotUSD per WND at 1 dotUSD per USDT, in each asset's own decimals, rounded up and
		// floored at the asset's minimum balance. The USDT row is a few bytes larger than the
		// WND row (a longer collateral id in key and ticket), which the WND price ignores.
		let usdt_quoted = (native_deposit * 2 * USDT).div_ceil(WND);
		assert!(usdt_deposit >= usdt_quoted.max(collateral_min_balance(&usdt_id())));
		assert!(usdt_deposit <= (usdt_quoted * 11).div_ceil(10));

		// The open refused above succeeds once the deposit can be priced.
		fund_vault_deposit(&usdt_id(), &usdt_owner);
		assert_ok!(open_usdt());
		assert_eq!(vault_deposit_on_hold(&usdt_id(), &usdt_owner), usdt_deposit);
		assert_eq!(vault_deposit_on_hold(&get_native_id(), &usdt_owner), 0);
	});
}
