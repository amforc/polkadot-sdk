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
use asset_hub_westend_runtime::{Redemptions, Vaults};
use pallet_redemptions::RedemptionTerms;
use pusd_primitives::VaultStatus;

/// Opens a vault at a healthy price, then halves the price and parks the vault
/// in the final-recovery FIFO.
fn park_in_final_recovery(owner: &AccountId, collateral: Balance, debt: Balance) {
	open_vault(owner, collateral, debt, FixedU128::zero());
	feed_price(dot_price(2, 1));
	assert_ok!(Vaults::enter_final_recovery(
		RuntimeOrigin::signed(acct(0xFE)),
		get_native_id(),
		get_pusd_id(),
		owner.clone(),
	));
	assert_eq!(
		Vaults::vault_status(get_native_id(), get_pusd_id(), owner.clone()),
		Some(VaultStatus::FinalRecovery),
	);
}

/// At CR 120% the raw bonus 120% − 100% − 1% = 19% is clamped to the 10%
/// redistribution penalty, so 2,000 pUSD buys 2,200 pUSD of collateral value.
#[test]
fn example_03_final_recovery_redemption_above_par() {
	AssetHubWestend::execute_with(|| {
		// Open at 4 pUSD/WND so the later halving produces the example's CR.
		feed_price(dot_price(4, 1));
		// MCR 125% makes the 120%-CR vault final-recovery eligible.
		create_branch(&BranchSpec {
			mcr: FixedU128::from_rational(125, 100),
			icr: FixedU128::from_rational(130, 100),
			scr: FixedU128::from_rational(130, 100),
			..Default::default()
		});
		// 6,000 WND = 12,000 pUSD value against 10,000 pUSD debt: CR 120%.
		let parked_owner = acct(1);
		park_in_final_recovery(&parked_owner, 6_000 * WND, 10_000 * PUSD);

		let redeemer = acct(3);
		fund_dot(&redeemer, 0);
		mint_pusd(&redeemer, 2_000 * PUSD);

		// collateral_out = 2,000 * 1.10 / 2 = 1,100 WND; recovery redemptions
		// charge no redemption fee.
		assert_ok!(Redemptions::redeem(
			RuntimeOrigin::signed(redeemer.clone()),
			get_native_id(),
			get_pusd_id(),
			RedemptionTerms { max_stable_to_spend: 2_000 * PUSD, min_collateral_out: 1_100 * WND },
			redeemer.clone(),
			16,
		));
		assert_eq!(pusd_balance(&redeemer), 0);
		assert_eq!(native_balance(&redeemer), 1_100 * WND + get_native_ed());

		// Vault after: 8,000 pUSD debt, 4,900 WND = 9,800 pUSD value,
		// CR 122.5%, still parked.
		let parked_vault = vault(&parked_owner);
		assert_eq!(parked_vault.debt.total(), 8_000 * PUSD);
		assert_eq!(parked_vault.collateral, 4_900 * WND);
		assert_eq!(
			Vaults::vault_status(get_native_id(), get_pusd_id(), parked_owner.clone()),
			Some(VaultStatus::FinalRecovery),
		);
	});
}

/// A 2,000 pUSD shortfall against 1,000 pUSD of insurance cover leaves
/// 9,000 pUSD to cancel externally at recovery rate 8,000/9,000; the full
/// settlement pays out all remaining collateral and burns the cover.
#[test]
fn example_04_final_recovery_redemption_below_par_with_insurance_cover() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&BranchSpec {
			mcr: FixedU128::from_rational(125, 100),
			icr: FixedU128::from_rational(130, 100),
			scr: FixedU128::from_rational(130, 100),
			..Default::default()
		});
		// 4,000 WND = 8,000 pUSD value against 10,000 pUSD debt: CR 80%.
		let parked_owner = acct(1);
		park_in_final_recovery(&parked_owner, 4_000 * WND, 10_000 * PUSD);

		// Insurance Fund balance = 1,000 pUSD.
		let insurance = insurance_account();
		mint_pusd(&insurance, 1_000 * PUSD);

		// Partial settlement: 3,000 pUSD at 8,000/9,000 buys
		// 3,000 × 8/9 = 2,666.666… pUSD of collateral value = 1,333.333… WND.
		let redeemer = acct(3);
		fund_dot(&redeemer, 0);
		mint_pusd(&redeemer, 3_000 * PUSD);
		assert_ok!(Redemptions::redeem(
			RuntimeOrigin::signed(redeemer.clone()),
			get_native_id(),
			get_pusd_id(),
			RedemptionTerms { max_stable_to_spend: 3_000 * PUSD, min_collateral_out: 1_333 * WND },
			redeemer.clone(),
			16,
		));
		assert_eq!(pusd_balance(&redeemer), 0);
		// The collateral value floors in stable units first:
		// floor(3,000e6 × 8/9) = 2,666,666,666 micro-pUSD, ÷ 2e-6 price
		// = 1,333,333,333 × 1e6 planck ≈ 1,333.333333 WND.
		let partial_out = native_balance(&redeemer) - get_native_ed();
		assert_eq!(partial_out, 1_333_333_333_000_000);

		// Full settlement: the remaining 6,000 pUSD of external cancel debt
		// takes all remaining collateral and burns the insurance cover.
		let settler = acct(4);
		fund_dot(&settler, 0);
		mint_pusd(&settler, 6_000 * PUSD);
		assert_ok!(Redemptions::redeem(
			RuntimeOrigin::signed(settler.clone()),
			get_native_id(),
			get_pusd_id(),
			RedemptionTerms { max_stable_to_spend: 6_000 * PUSD, min_collateral_out: 2_600 * WND },
			settler.clone(),
			16,
		));
		assert_eq!(pusd_balance(&settler), 0);
		// collateral paid out in total = 4,000 WND.
		assert_eq!(native_balance(&settler) - get_native_ed(), 4_000 * WND - partial_out);
		// Insurance Fund burn = 1,000 pUSD.
		assert_eq!(pusd_balance(&insurance), 0);
		// A full settlement leaves an empty Dormant husk, not a deleted vault.
		let husk = vault(&parked_owner);
		assert_eq!(husk.debt.total(), 0);
		assert_eq!(husk.collateral, 0);
		assert_eq!(
			Vaults::vault_status(get_native_id(), get_pusd_id(), parked_owner.clone()),
			Some(VaultStatus::Dormant),
		);
	});
}
