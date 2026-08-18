// Copyright (C) Parity Technologies (UK) Ltd.
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

use crate::imports::*;
use asset_hub_westend_runtime::{governance::TreasuryAccount, Redemptions};
use pallet_redemptions::{RedemptionStates, RedemptionTerms};
use pusd_primitives::VaultStatus;

/// A 1,000 pUSD redemption against 100,000 pUSD of market-wide debt raises the
/// decayed 1.5% dynamic fee by 1,000/100,000/2 = 0.5%. The redemption pays the
/// 1.75% mean of the 1.5% arrival and 2.0% terminal dynamic fees plus the 0.5%
/// base fee. The budget covers the debt and the fee.
#[test]
fn example_01_ordinary_redemption_with_dynamic_fee_update_and_fee() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());

		// 4,000 WND = 8,000 pUSD value against 5,000 pUSD debt: CR 160%.
		let target_owner = acct(1);
		open_vault(&target_owner, 4_000 * WND, 5_000 * PUSD, FixedU128::zero());
		// Filler at a higher rate lifts market-wide debt to 100,000 pUSD while
		// keeping the target vault at the redeemable (lowest-rate) head.
		let filler_owner = acct(2);
		open_vault(&filler_owner, 100_000 * WND, 95_000 * PUSD, FixedU128::from_rational(1, 100));

		set_dynamic_fee(FixedU128::from_rational(15, 1_000)); // decayed dynamic_fee = 1.5%

		let redeemer = acct(3);
		fund_dot(&redeemer, 0);
		// 1,000 pUSD cancelled + 1,000 * 2.25% = 22.5 pUSD fee. An ordinary redemption
		// keeps the redeemer's pUSD account alive, so it holds the minimum balance on top.
		let fee = 45 * PUSD / 2;
		mint_pusd(&redeemer, 1_000 * PUSD + fee + PUSD_MIN_BALANCE);
		let treasury_pusd_before = pusd_balance(&TreasuryAccount::get());
		let redeemer_native_before = native_balance(&redeemer);

		assert_ok!(Redemptions::redeem(
			RuntimeOrigin::signed(redeemer.clone()),
			get_native_id(),
			get_pusd_id(),
			RedemptionTerms {
				max_stable_to_spend: 1_000 * PUSD + fee,
				min_collateral_out: 500 * WND,
			},
			redeemer.clone(),
			16,
		));

		// new dynamic_fee = 1.5% + 0.5% = 2.0%.
		let state = RedemptionStates::<Runtime>::get(get_pusd_id());
		assert_eq!(state.dynamic_fee, FixedU128::from_rational(2, 100));

		// total_pusd_in = 1,022.5; collateral_out = 1,000 / 2 = 500 WND.
		assert_eq!(pusd_balance(&redeemer), PUSD_MIN_BALANCE);
		assert_eq!(native_balance(&redeemer) - redeemer_native_before, 500 * WND);
		// fee_pusd = 22.5 routed to the fee handler.
		assert_eq!(pusd_balance(&TreasuryAccount::get()) - treasury_pusd_before, fee);

		// Vault after: 4,000 pUSD debt, 3,500 WND = 7,000 pUSD value, CR 175%.
		let target_vault = vault(&target_owner);
		assert_eq!(target_vault.debt.total(), 4_000 * PUSD);
		assert_eq!(target_vault.collateral, 3_500 * WND);
	});
}

/// Cancelling 2,800 of 3,000 pUSD debt leaves 200 pUSD, below the 2,000 pUSD
/// branch minimum: the vault leaves the rate index as a Dormant continuation
/// instead of closing.
#[test]
fn example_02_ordinary_redemption_creates_dormant_continuation_vault() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec { minimum_debt: 2_000 * PUSD, ..Default::default() });

		// 2,000 WND = 4,000 pUSD value against 3,000 pUSD debt: CR 133%.
		let target_owner = acct(1);
		open_vault(&target_owner, 2_000 * WND, 3_000 * PUSD, FixedU128::zero());
		// Filler lifts market-wide debt to 10,000 pUSD so the fee arithmetic
		// below stays exact.
		let filler_owner = acct(2);
		open_vault(&filler_owner, 10_000 * WND, 7_000 * PUSD, FixedU128::from_rational(1, 100));

		let redeemer = acct(3);
		fund_dot(&redeemer, 0);
		// dynamic_fee increase = 2,800/10,000/2 = 14%; the redemption pays the 7%
		// mean of the 0% arrival and 14% terminal dynamic fees plus the 0.5% base
		// fee: 2,800 * 7.5% = 210 pUSD. The redeemer keeps the minimum balance on top.
		mint_pusd(&redeemer, (2_800 + 210) * PUSD + PUSD_MIN_BALANCE);
		let redeemer_native_before = native_balance(&redeemer);

		assert_ok!(Redemptions::redeem(
			RuntimeOrigin::signed(redeemer.clone()),
			get_native_id(),
			get_pusd_id(),
			RedemptionTerms {
				max_stable_to_spend: (2_800 + 210) * PUSD,
				min_collateral_out: 1_400 * WND,
			},
			redeemer.clone(),
			16,
		));

		// collateral_out = 2,800 / 2 = 1,400 WND.
		assert_eq!(pusd_balance(&redeemer), PUSD_MIN_BALANCE);
		assert_eq!(native_balance(&redeemer) - redeemer_native_before, 1_400 * WND);

		// Vault after: 200 pUSD debt, 600 WND, status Dormant.
		let target_vault = vault(&target_owner);
		assert_eq!(target_vault.debt.total(), 200 * PUSD);
		assert_eq!(target_vault.collateral, 600 * WND);
		assert_eq!(
			asset_hub_westend_runtime::Vaults::vault_status(
				get_native_id(),
				get_pusd_id(),
				target_owner.clone(),
			),
			Some(VaultStatus::Dormant),
		);
	});
}
