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

use crate::imports::*;
use asset_hub_westend_runtime::{RuntimeEvent, System};
use pallet_redemptions::{RecoveryRegime, RedemptionTerms};

/// Opens a vault at a healthy price, halves the price, and puts the vault in the
/// FinalRecovery FIFO.
fn park_in_final_recovery(owner: &AccountId, collateral: Balance, debt: Balance) {
	open_vault(owner, collateral, debt, FixedU128::zero());
	feed_price(dot_price(2, 1));
	enter_final_recovery(owner);
}
#[test]
fn final_recovery_redemption_below_par_with_full_insurance_cover() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());
		// 4,000 WND = 8,000 dotUSD value against 10,000 dotUSD debt: CR 80%.
		let parked_owner = acct(1);
		park_in_final_recovery(&parked_owner, 4_000 * WND, 10_000 * DOTUSD);

		// Insurance Fund balance = 3,000 dotUSD, above the 2,000 dotUSD shortfall.
		let insurance = insurance_account();
		mint_dotusd(&insurance, 3_000 * DOTUSD);

		// Full settlement: 8,000 dotUSD from the market plus 2,000 dotUSD of cover
		// cancels the 10,000 dotUSD of debt and takes all 4,000 WND.
		let settler = acct(4);
		let settled_out = redeem(
			&settler,
			RedemptionTerms {
				max_stable_to_spend: 8_000 * DOTUSD,
				min_collateral_out: 4_000 * WND,
			},
		);
		assert_eq!(settled_out, 4_000 * WND);
		// The fund burns only the shortfall and keeps the surplus.
		assert_eq!(dotusd_balance(&insurance), 1_000 * DOTUSD);
		// Redemptions reports who paid and what the fund covered.
		System::assert_has_event(RuntimeEvent::Redemptions(
			pallet_redemptions::Event::RecoveryRedemptionExecuted {
				collateral_id: get_native_id(),
				stable_id: DOTUSD_ID,
				redeemer: settler.clone(),
				stable_burned: 8_000 * DOTUSD,
				insurance_cover: 2_000 * DOTUSD,
				collateral_out: 4_000 * WND,
				regime: RecoveryRegime::InsuranceAdjusted,
			},
		));
		assert_eq!(vault_status(&parked_owner), None);
		assert_eq!(collateral_on_hold(&get_native_id(), &parked_owner), 0);
		assert_eq!(branch_state().vault_count, 0);
	});
}
