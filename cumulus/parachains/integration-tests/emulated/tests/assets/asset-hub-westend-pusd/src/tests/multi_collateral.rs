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
	pusd_config::{StabilityCollateral, TrustBackedAssetLocation},
	Balances, Stability, Vaults,
};
use emulated_integration_tests_common::{snowbridge::SEPOLIA_ID, USDT_ID};
use frame_support::{
	assert_err,
	traits::{fungibles::Refund, tokens::Provenance},
};
use pallet_vaults::JitTerms;
use sp_runtime::traits::MaybeEquivalence;
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

fn eth_price(pusd_per_eth: u128) -> FixedU128 {
	FixedU128::from_rational(pusd_per_eth * PUSD, ETH)
}

/// Liquidates without JIT, with a throwaway keeper.
fn liquidate_on(collateral_id: VaultsCollateralId, owner: &AccountId) {
	let keeper = acct(0xEE);
	fund_collateral(&collateral_id, &keeper, 0);
	assert_ok!(Vaults::liquidate(
		RuntimeOrigin::signed(keeper),
		collateral_id,
		get_pusd_id(),
		owner.clone(),
		JitTerms { max_stable: 0, min_collateral_out: 0 },
	));
}

fn deposit_row_on(
	collateral_id: &VaultsCollateralId,
	who: &AccountId,
) -> pallet_stability::types::Deposit<Balance> {
	pallet_stability::Deposits::<Runtime>::get((collateral_id, get_pusd_id(), who.clone()))
		.expect("deposit row exists")
}

/// Pays out a depositor's realized gains and asserts the collateral moved from
/// the pool account to the depositor in the collateral's own pallet.
///
/// The payout is where the asset union must route to a pallet the pool did not
/// choose, so each collateral pallet is checked.
fn claim_collateral_out(
	collateral_id: &VaultsCollateralId,
	depositor: &AccountId,
	expected: Balance,
) {
	let pool = pool_account_on(collateral_id);
	let depositor_before = collateral_free(collateral_id, depositor);
	let pool_before = collateral_free(collateral_id, &pool);

	assert_ok!(Stability::claim_collateral(
		RuntimeOrigin::signed(depositor.clone()),
		collateral_id.clone(),
		get_pusd_id(),
		None,
	));

	assert_eq!(collateral_free(collateral_id, depositor) - depositor_before, expected);
	assert_eq!(pool_before - collateral_free(collateral_id, &pool), expected);
	// The payout zeroes the claimable row.
	assert_err!(
		Stability::claim_collateral(
			RuntimeOrigin::signed(depositor.clone()),
			collateral_id.clone(),
			get_pusd_id(),
			None,
		),
		pallet_stability::Error::<Runtime>::NoClaimableCollateral,
	);
}

/// One stablecoin, three markets: native WND, trust-backed USDT, bridged ETH.
///
/// Each vault pledges 20,000 pUSD of value against 10,000 pUSD of debt. The
/// three differ only in the collateral pallet and the decimals.
#[test]
fn native_trust_backed_and_foreign_collateral_markets_coexist() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());

		feed_price_for(usdt_id(), FixedU128::from_rational(PUSD, USDT)); // at par
		create_market_signed(usdt_id(), &BranchSpec::default());

		feed_price_for(eth_id(), eth_price(2_000));
		create_market_signed(eth_id(), &BranchSpec::default());

		let native_owner = acct(1);
		open_vault(&native_owner, 10_000 * WND, 10_000 * PUSD, FixedU128::zero());
		let usdt_owner = acct(2);
		open_vault_on(usdt_id(), &usdt_owner, 20_000 * USDT, 10_000 * PUSD, FixedU128::zero());
		let eth_owner = acct(3);
		open_vault_on(eth_id(), &eth_owner, 10 * ETH, 10_000 * PUSD, FixedU128::zero());

		assert_eq!(vault(&native_owner).collateral, 10_000 * WND);
		assert_eq!(vault_on(&usdt_id(), &usdt_owner).collateral, 20_000 * USDT);
		assert_eq!(vault_on(&eth_id(), &eth_owner).collateral, 10 * ETH);
		// Same debt, same stablecoin, three collaterals.
		assert_eq!(pusd_balance(&native_owner), 10_000 * PUSD);
		assert_eq!(pusd_balance(&usdt_owner), 10_000 * PUSD);
		assert_eq!(pusd_balance(&eth_owner), 10_000 * PUSD);

		// Custody is a hold in the collateral's own pallet. Only the minimum-balance float is free.
		assert_eq!(collateral_on_hold(&get_native_id(), &native_owner), 10_000 * WND);
		assert_eq!(collateral_on_hold(&usdt_id(), &usdt_owner), 20_000 * USDT);
		assert_eq!(collateral_free(&usdt_id(), &usdt_owner), collateral_min_balance(&usdt_id()));
		assert_eq!(collateral_on_hold(&eth_id(), &eth_owner), 10 * ETH);
		assert_eq!(collateral_free(&eth_id(), &eth_owner), collateral_min_balance(&eth_id()));
	});
}

/// Ether is a sufficient asset, so its accounts need no deposit. A market for it
/// still needs one.
///
/// The stability pool must accept an offset gain below the collateral's minimum
/// balance. That needs a pool account created in advance, and someone must pay
/// its asset-account deposit. Root supplies no depositor, so the full admin pays.
/// Root creation already charges that account for collateral custody.
#[test]
fn root_registers_a_foreign_collateral_market_charging_the_admin() {
	AssetHubWestend::execute_with(|| {
		create_pusd();
		feed_price_for(eth_id(), eth_price(2_000));

		// The full admin funds both the custody seed and the pool account's deposit.
		assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(&admin(), 1_000 * WND));
		fund_collateral(&eth_id(), &admin(), collateral_min_balance(&eth_id()));

		assert_ok!(Vaults::create_branch(
			RuntimeOrigin::root(),
			eth_id(),
			get_pusd_id(),
			branch_admins(),
			branch_config(&eth_id(), &BranchSpec::default()),
			registration_config(),
		));
		assert!(pallet_vaults::Branches::<Runtime>::get(eth_id(), get_pusd_id()).is_some());

		// The pool can now take a gain below the collateral's minimum balance. The
		// full admin paid the deposit.
		let pool = pallet_stability::Pallet::<Runtime>::pool_account(&eth_id(), &get_pusd_id());
		let (depositor, deposit) =
			<StabilityCollateral as Refund<AccountId>>::deposit_held(eth_id(), pool.clone())
				.expect("registration touched the pool account");
		assert_eq!(depositor, admin(), "Root creation charges the full admin");
		assert!(deposit > 0);
		assert_ok!(<StabilityCollateral as Inspect<AccountId>>::can_deposit(
			eth_id(),
			&pool,
			1,
			Provenance::Extant,
		)
		.into_result());
	});
}

/// The native example's figures, scaled by the ETH price. Seizure caps at
/// debt × 1.05 in value. The keeper takes 2 pUSD flat plus 0.1% of the seizure.
/// The rest goes to the pool, and the sole depositor claims all of it.
#[test]
fn foreign_collateral_liquidation_offsets_and_claims_out_of_the_pool() {
	AssetHubWestend::execute_with(|| {
		create_pusd();
		// The vault deposit is priced in WND, so the WND feed must exist too.
		feed_price(dot_price(2, 1));
		feed_price_for(eth_id(), eth_price(4_000));
		create_market_signed(eth_id(), &liquidation_spec());
		lift_global_ceiling(1_000_000_000 * PUSD);

		// 6 ETH against 10,000 pUSD debt: CR 240% at 4,000, 120% at 2,000.
		let liquidated_owner = acct(1);
		open_vault_on(eth_id(), &liquidated_owner, 6 * ETH, 10_000 * PUSD, FixedU128::zero());
		// The deposit is priced at open; the price move below does not re-price the ticket.
		let (deposit_asset, deposit) = expected_vault_deposit(&eth_id(), &liquidated_owner);
		assert_eq!(deposit_asset, eth_id());
		// A healthy vault keeps the branch populated after the liquidation. Both
		// vaults stay under the ether bridged to Westend.
		let filler_owner = acct(2);
		open_vault_on(
			eth_id(),
			&filler_owner,
			ETHER_SUPPLY_LIVE - 6 * ETH,
			10_000 * PUSD,
			FixedU128::zero(),
		);

		let depositor = acct(3);
		sp_deposit_matured_on(eth_id(), &depositor, 20_000 * PUSD);

		feed_price_for(eth_id(), eth_price(2_000)); // CR 120% < MCR 125%

		let keeper = acct(4);
		fund_collateral(&eth_id(), &keeper, 0);
		let owner_free_before = collateral_free(&eth_id(), &liquidated_owner);
		assert_ok!(Vaults::liquidate(
			RuntimeOrigin::signed(keeper.clone()),
			eth_id(),
			get_pusd_id(),
			liquidated_owner.clone(),
			JitTerms { max_stable: 0, min_collateral_out: 0 },
		));

		// seized = min(6, 10,000 × 1.05 / 2,000) = 5.25 ETH. The 0.75 ETH
		// surplus returns to the owner, with the vault's storage deposit in ETH.
		assert_eq!(
			collateral_free(&eth_id(), &liquidated_owner) - owner_free_before,
			750_000_000_000_000_000 + deposit,
		);
		// keeper = 2 pUSD flat / 2,000 + 5.25 × 0.1% = 0.001 + 0.00525 ETH.
		assert_eq!(
			collateral_free(&eth_id(), &keeper) - collateral_min_balance(&eth_id()),
			6_250_000_000_000_000,
		);
		// The pool burns the full 10,000 pUSD debt and receives 5.25 − 0.00625 = 5.24375 ETH.
		let pool = pool_account_on(&eth_id());
		assert_eq!(pusd_balance(&pool), 10_000 * PUSD);
		assert_eq!(collateral_free(&eth_id(), &pool), 5_243_750_000_000_000_000);
		// The liquidated vault is removed.
		assert_eq!(Vaults::vault_status(eth_id(), get_pusd_id(), liquidated_owner.clone()), None);

		// The sole depositor's gain is the whole pool collateral.
		claim_collateral_out(&eth_id(), &depositor, 5_243_750_000_000_000_000);
		// The claim realizes the row. Half of the 20,000 pUSD deposit burned, so P halved.
		assert_eq!(deposit_row_on(&eth_id(), &depositor).active_deposit, 10_000 * PUSD);
	});
}

/// The same offset and claim over a trust-backed asset.
#[test]
fn trust_backed_collateral_gains_claim_out_to_the_depositor() {
	AssetHubWestend::execute_with(|| {
		create_pusd();
		// The vault deposit is priced in WND, so the WND feed must exist too.
		feed_price(dot_price(2, 1));
		feed_price_for(usdt_id(), FixedU128::from_rational(PUSD, USDT)); // at par
		create_market_signed(usdt_id(), &accounting_spec());
		lift_global_ceiling(1_000_000_000 * PUSD);

		// 14,000 USDT against 10,000 pUSD debt: CR 140% at par, 117.6% at 0.84.
		let liquidated_owner = acct(1);
		open_vault_on(
			usdt_id(),
			&liquidated_owner,
			14_000 * USDT,
			10_000 * PUSD,
			FixedU128::zero(),
		);
		// The deposit is priced at open; the price move below does not re-price the ticket.
		let (deposit_asset, deposit) = expected_vault_deposit(&usdt_id(), &liquidated_owner);
		assert_eq!(deposit_asset, usdt_id());
		// A healthy vault keeps the branch populated and out of Safety Mode after the liquidation.
		let filler_owner = acct(2);
		open_vault_on(usdt_id(), &filler_owner, 1_000_000 * USDT, 10_000 * PUSD, FixedU128::zero());

		// The depositor holds no USDT. A stablecoin deposit is enough to earn collateral gains.
		let depositor = acct(3);
		sp_deposit_matured_on(usdt_id(), &depositor, 25_000 * PUSD);
		assert_eq!(collateral_free(&usdt_id(), &depositor), 0);

		feed_price_for(usdt_id(), FixedU128::from_rational(84 * PUSD, 100 * USDT));
		liquidate_on(usdt_id(), &liquidated_owner);

		// seized = min(14,000, 10,000 × 1.05 / 0.84) = 12,500 USDT, all to the
		// pool. This spec pays the keeper nothing. The USDT storage deposit is refunded.
		assert_eq!(
			collateral_free(&usdt_id(), &liquidated_owner),
			1_500 * USDT + collateral_min_balance(&usdt_id()) + deposit,
		);
		claim_collateral_out(&usdt_id(), &depositor, 12_500 * USDT);
		// The claim realizes the row. 10,000 of the 25,000 pUSD deposit burned, so P = 0.6.
		assert_eq!(deposit_row_on(&usdt_id(), &depositor).active_deposit, 15_000 * PUSD);
	});
}

/// Trust-backed asset registered without `is_sufficient`, the kind whose accounts need a
/// native provider.
fn reservable_id() -> VaultsCollateralId {
	use emulated_integration_tests_common::RESERVABLE_ASSET_ID;
	TrustBackedAssetLocation::convert_back(&RESERVABLE_ASSET_ID)
		.expect("trust-backed ids have a location")
}

/// The vault deposit settles in the collateral when that is a sufficient asset, priced from
/// the oracle: WND at 2 pUSD and USDT at par, so one WND of deposit costs two USDT.
#[test]
fn sufficient_collateral_settles_the_vault_deposit_in_itself() {
	AssetHubWestend::execute_with(|| {
		create_pusd();
		feed_price(dot_price(2, 1));
		feed_price_for(usdt_id(), FixedU128::from_rational(PUSD, USDT)); // at par
		create_market_signed(usdt_id(), &BranchSpec::default());
		feed_price_for(eth_id(), eth_price(2_000));
		create_market_signed(eth_id(), &BranchSpec::default());
		lift_global_ceiling(1_000_000_000 * PUSD);

		let usdt_owner = acct(1);
		let eth_owner = acct(2);
		let (native_deposit_asset, native_deposit) =
			expected_vault_deposit(&get_native_id(), &usdt_owner);
		assert_eq!(native_deposit_asset, get_native_id());
		let (usdt_deposit_asset, usdt_deposit) = expected_vault_deposit(&usdt_id(), &usdt_owner);
		assert_eq!(usdt_deposit_asset, usdt_id());
		// 2 pUSD per WND at 1 pUSD per USDT, in each asset's own decimals, rounded up and
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

		open_vault_on(usdt_id(), &usdt_owner, 20_000 * USDT, 10_000 * PUSD, FixedU128::zero());
		assert_eq!(vault_deposit_on_hold(&usdt_id(), &usdt_owner), usdt_deposit);
		assert_eq!(vault_deposit_on_hold(&get_native_id(), &usdt_owner), 0);

		open_vault_on(eth_id(), &eth_owner, 10 * ETH, 10_000 * PUSD, FixedU128::zero());
		assert_eq!(vault_deposit_on_hold(&eth_id(), &eth_owner), eth_deposit);
		assert_eq!(vault_deposit_on_hold(&get_native_id(), &eth_owner), 0);

		// Closing the vault returns the deposit in the asset it was taken in.
		mint_pusd(&usdt_owner, 1_000 * PUSD);
		assert_ok!(Vaults::repay_for(
			RuntimeOrigin::signed(usdt_owner.clone()),
			usdt_id(),
			get_pusd_id(),
			usdt_owner.clone(),
			Some(100_000 * PUSD),
		));
		let free_before_close = collateral_free(&usdt_id(), &usdt_owner);
		assert_ok!(Vaults::close_vault(
			RuntimeOrigin::signed(usdt_owner.clone()),
			usdt_id(),
			get_pusd_id(),
			None,
		));
		assert_eq!(vault_deposit_on_hold(&usdt_id(), &usdt_owner), 0);
		assert_eq!(
			collateral_free(&usdt_id(), &usdt_owner),
			free_before_close + 20_000 * USDT + usdt_deposit
		);
	});
}

/// A collateral that is not sufficient cannot carry the deposit, so WND does.
#[test]
fn insufficient_collateral_settles_the_vault_deposit_in_wnd() {
	AssetHubWestend::execute_with(|| {
		create_pusd();
		feed_price_for(reservable_id(), dot_price(2, 1));
		create_market_signed(reservable_id(), &BranchSpec::default());
		lift_global_ceiling(1_000_000_000 * PUSD);

		let owner = acct(1);
		let (deposit_asset, deposit) = expected_vault_deposit(&reservable_id(), &owner);
		assert_eq!(deposit_asset, get_native_id());
		// Settled in WND, but sized for the reservable asset's longer id: strictly above the
		// deposit of a native vault.
		let (_, native_deposit) = expected_vault_deposit(&get_native_id(), &owner);
		assert!(deposit > native_deposit);

		open_vault_on(reservable_id(), &owner, 10_000 * WND, 10_000 * PUSD, FixedU128::zero());
		assert_eq!(vault_deposit_on_hold(&get_native_id(), &owner), deposit);
		assert_eq!(vault_deposit_on_hold(&reservable_id(), &owner), 0);
	});
}

/// Without a WND feed and without a WND/USDT pool the deposit cannot be priced in USDT, and
/// the open fails rather than silently charging WND.
#[test]
fn unpriceable_sufficient_collateral_cannot_open_a_vault() {
	AssetHubWestend::execute_with(|| {
		create_pusd();
		feed_price_for(usdt_id(), FixedU128::from_rational(PUSD, USDT)); // at par
		create_market_signed(usdt_id(), &BranchSpec::default());
		lift_global_ceiling(1_000_000_000 * PUSD);

		let owner = acct(1);
		fund_collateral(&usdt_id(), &owner, 20_000 * USDT);
		assert_err!(
			Vaults::open_vault(
				RuntimeOrigin::signed(owner.clone()),
				usdt_id(),
				get_pusd_id(),
				20_000 * USDT,
				10_000 * PUSD,
				FixedU128::zero(),
				pallet_linked_list::Position::endpoints_only(),
			),
			sp_runtime::DispatchError::Unavailable
		);
		assert_eq!(Vaults::vault_status(usdt_id(), get_pusd_id(), owner), None);
	});
}
