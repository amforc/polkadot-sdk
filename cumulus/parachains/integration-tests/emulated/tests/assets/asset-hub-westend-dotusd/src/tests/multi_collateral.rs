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
	dotusd_config::{TrustBackedAssetLocation, VaultsCollateral},
	Vaults,
};
use emulated_integration_tests_common::{snowbridge::SEPOLIA_ID, USDT_ID};
use frame_support::{
	assert_noop,
	traits::tokens::{Fortitude, Preservation},
};
use sp_runtime::{traits::MaybeEquivalence, TokenError};
use xcm::v5::{Junction::GlobalConsensus, Location, NetworkId};

const ETH: Balance = 1_000_000_000_000_000_000;
const USDT: Balance = 1_000_000;

/// Ether bridged to live Westend Asset Hub, rounded down. Vault sizes stay inside
/// it, so the live chain could fund the scenario.
const ETHER_SUPPLY_LIVE: Balance = 21 * ETH;

/// Sepolia ETH on Asset Hub. The emulated genesis registers it.
fn eth_id() -> VaultsCollateralId {
	Location::new(2, [GlobalConsensus(NetworkId::Ethereum { chain_id: SEPOLIA_ID })])
}

fn usdt_id() -> VaultsCollateralId {
	TrustBackedAssetLocation::convert_back(&USDT_ID).expect("trust-backed ids have a location")
}

fn eth_price(dotusd_per_eth: u128) -> FixedU128 {
	FixedU128::from_rational(dotusd_per_eth * DOTUSD, ETH)
}

/// One stablecoin, three branches: native WND, trust-backed USDT, bridged ETH.
///
/// Each vault pledges 20,000 dotUSD of value against 10,000 dotUSD of debt. The
/// three differ only in the collateral pallet and the decimals.
#[test]
fn native_trust_backed_and_foreign_collateral_markets_coexist() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());

		feed_price_for(usdt_id(), FixedU128::from_rational(DOTUSD, USDT)); // at par
		create_market_signed(usdt_id(), &BranchSpec::default());

		feed_price_for(eth_id(), eth_price(2_000));
		create_market_signed(eth_id(), &BranchSpec::default());

		let native_owner = acct(1);
		open_vault(&native_owner, 10_000 * WND, 10_000 * DOTUSD, FixedU128::zero());
		let usdt_owner = acct(2);
		open_vault_on(usdt_id(), &usdt_owner, 20_000 * USDT, 10_000 * DOTUSD, FixedU128::zero());
		let eth_owner = acct(3);
		open_vault_on(eth_id(), &eth_owner, 10 * ETH, 10_000 * DOTUSD, FixedU128::zero());

		assert_eq!(vault(&native_owner).collateral, 10_000 * WND);
		assert_eq!(vault_on(&usdt_id(), &usdt_owner).collateral, 20_000 * USDT);
		assert_eq!(vault_on(&eth_id(), &eth_owner).collateral, 10 * ETH);
		// Same debt, same stablecoin, three collaterals.
		assert_eq!(dotusd_balance(&native_owner), 10_000 * DOTUSD);
		assert_eq!(dotusd_balance(&usdt_owner), 10_000 * DOTUSD);
		assert_eq!(dotusd_balance(&eth_owner), 10_000 * DOTUSD);

		// Each asset pallet holds exactly the vault collateral. The minimum-balance float
		// is free but not reducible while the hold keeps the account alive.
		for (collateral_id, owner) in
			[(get_native_id(), native_owner), (usdt_id(), usdt_owner), (eth_id(), eth_owner)]
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

/// 6 ETH against 10,000 dotUSD debt: CR 240% at 4,000, 120% at 2,000. seized =
/// min(6, 10,000 × 1.05 / 2,000) = 5.25 ETH; half of the 20,000 dotUSD deposit burns,
/// so P halves. Both vaults stay under the ether bridged to Westend.
#[test]
fn foreign_collateral_liquidation_offsets_and_claims_out() {
	AssetHubWestend::execute_with(|| {
		liquidate_and_claim_out(LiquidationOptions {
			collateral_id: eth_id(),
			price_open: eth_price(4_000),
			collateral: 6 * ETH,
			price_crash: eth_price(2_000),
			filler_collateral: ETHER_SUPPLY_LIVE - 6 * ETH,
			pool_deposit: 20_000 * DOTUSD,
			seized: 5_250_000_000_000_000_000,
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
			price_open: FixedU128::from_rational(DOTUSD, USDT), // at par
			collateral: 14_000 * USDT,
			price_crash: FixedU128::from_rational(84 * DOTUSD, 100 * USDT),
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
/// the in-asset storage deposit return to the owner, and the sole depositor
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
	assert_eq!(deposit_asset, id);
	let filler_owner = acct(2);
	open_vault_on(
		id.clone(),
		&filler_owner,
		options.filler_collateral,
		10_000 * DOTUSD,
		FixedU128::zero(),
	);

	// The depositor holds none of the collateral. A stablecoin deposit is enough
	// to earn collateral gains.
	let depositor = acct(3);
	sp_deposit_matured_on(id.clone(), &depositor, options.pool_deposit);
	assert_eq!(collateral_free(&id, &depositor), 0);

	feed_price_for(id.clone(), options.price_crash); // below MCR 125%
	let owner_free_before = collateral_free(&id, &liquidated_owner);
	liquidate_on(id.clone(), &liquidated_owner);

	assert_eq!(
		collateral_free(&id, &liquidated_owner) - owner_free_before,
		options.collateral - options.seized + deposit,
	);
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

/// Trust-backed asset registered without `is_sufficient`, the kind whose accounts need a
/// native provider.
fn reservable_id() -> VaultsCollateralId {
	use emulated_integration_tests_common::RESERVABLE_ASSET_ID;
	TrustBackedAssetLocation::convert_back(&RESERVABLE_ASSET_ID)
		.expect("trust-backed ids have a location")
}

/// The runtime deposit policy over real sufficiency flags and decimals: a
/// sufficient collateral carries its own vault deposit, priced from the oracle
/// (WND at 2 dotUSD and USDT at par, so one WND of deposit costs two USDT). An
/// insufficient collateral cannot carry it, so WND does. Without a WND feed and
/// without a WND/USDT pool, a sufficient collateral's deposit cannot be priced and
/// the open fails rather than silently charging WND.
#[test]
fn vault_deposit_asset_follows_runtime_policy() {
	AssetHubWestend::execute_with(|| {
		create_dotusd();
		// No WND feed yet: branch creation needs only the collateral's own price.
		feed_price_for(usdt_id(), FixedU128::from_rational(DOTUSD, USDT)); // at par
		create_market_signed(usdt_id(), &BranchSpec::default());
		feed_price_for(eth_id(), eth_price(2_000));
		create_market_signed(eth_id(), &BranchSpec::default());
		feed_price_for(reservable_id(), dot_price(2, 1));
		create_market_signed(reservable_id(), &BranchSpec::default());

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
		assert_noop!(open_usdt(), sp_runtime::DispatchError::Unavailable);

		// A WND-settled deposit needs no WND price.
		let reservable_owner = acct(3);
		let (deposit_asset, deposit) = expected_vault_deposit(&reservable_id(), &reservable_owner);
		assert_eq!(deposit_asset, get_native_id());
		// Settled in WND, but sized for the reservable asset's longer id: strictly above the
		// deposit of a native vault.
		let (_, native_deposit) = expected_vault_deposit(&get_native_id(), &reservable_owner);
		assert!(deposit > native_deposit);
		// The collateral alone cannot pay a WND-settled deposit.
		fund_collateral(&reservable_id(), &reservable_owner, 10_000 * WND);
		let open_reservable = || {
			Vaults::open_vault(
				RuntimeOrigin::signed(reservable_owner.clone()),
				reservable_id(),
				DOTUSD_ID,
				10_000 * WND,
				10_000 * DOTUSD,
				FixedU128::zero(),
				pallet_linked_list::Position::endpoints_only(),
			)
		};
		assert_noop!(open_reservable(), TokenError::FundsUnavailable);
		fund_vault_deposit(&reservable_id(), &reservable_owner);
		assert_ok!(open_reservable());
		assert_eq!(vault_deposit_on_hold(&get_native_id(), &reservable_owner), deposit);
		assert_eq!(vault_deposit_on_hold(&reservable_id(), &reservable_owner), 0);

		feed_price(dot_price(2, 1));
		let eth_owner = acct(2);
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
		let (eth_deposit_asset, eth_deposit) = expected_vault_deposit(&eth_id(), &eth_owner);
		assert_eq!(eth_deposit_asset, eth_id());
		let eth_quoted = (native_deposit * 2 * ETH).div_ceil(2_000 * WND);
		assert!(eth_deposit >= eth_quoted.max(collateral_min_balance(&eth_id())));
		assert!(eth_deposit <= (eth_quoted * 11).div_ceil(10));

		// The open refused above succeeds once the deposit can be priced.
		fund_vault_deposit(&usdt_id(), &usdt_owner);
		assert_ok!(open_usdt());
		assert_eq!(vault_deposit_on_hold(&usdt_id(), &usdt_owner), usdt_deposit);
		assert_eq!(vault_deposit_on_hold(&get_native_id(), &usdt_owner), 0);

		open_vault_on(eth_id(), &eth_owner, 10 * ETH, 10_000 * DOTUSD, FixedU128::zero());
		assert_eq!(vault_deposit_on_hold(&eth_id(), &eth_owner), eth_deposit);
		assert_eq!(vault_deposit_on_hold(&get_native_id(), &eth_owner), 0);
	});
}
