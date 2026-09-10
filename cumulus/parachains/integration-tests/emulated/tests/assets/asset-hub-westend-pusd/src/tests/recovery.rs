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
use asset_hub_westend_runtime::{Redemptions, RuntimeEvent, System, Vaults};
use frame_support::assert_noop;
use pallet_redemptions::{RecoveryOffsetQuote, RecoveryRegime, RedemptionTerms};
use pusd_primitives::VaultStatus;

/// Opens a vault at a healthy price, halves the price, and puts the vault in the
/// FinalRecovery FIFO.
fn park_in_final_recovery(owner: &AccountId, collateral: Balance, debt: Balance) {
	open_vault(owner, collateral, debt, FixedU128::zero());
	feed_price(dot_price(2, 1));
	enter_final_recovery(owner);
}

/// At CR 120% the raw bonus is 120% − 100% − 1% = 19%. It caps at the 10%
/// redistribution penalty, so 2,000 pUSD buys 2,200 pUSD of collateral value.
#[test]
fn final_recovery_redemption_above_par() {
	AssetHubWestend::execute_with(|| {
		// A 50% price decrease sets the vault CR to 120%.
		feed_price(dot_price(4, 1));
		// MCR 125% makes the CR 120% vault eligible for final recovery. Zero keeper terms keep
		// the parked collateral round; the entry reward has its own test below.
		create_branch(&accounting_spec());
		// 6,000 WND = 12,000 pUSD value against 10,000 pUSD debt: CR 120%.
		let parked_owner = acct(1);
		park_in_final_recovery(&parked_owner, 6_000 * WND, 10_000 * PUSD);

		// collateral_out = 2,000 * 1.10 / 2 = 1,100 WND. Recovery redemptions
		// charge no fee.
		let collateral_out = redeem(
			&acct(3),
			RedemptionTerms { max_stable_to_spend: 2_000 * PUSD, min_collateral_out: 1_100 * WND },
		);
		assert_eq!(collateral_out, 1_100 * WND);

		// Vault after: 8,000 pUSD debt, 4,900 WND = 9,800 pUSD value,
		// CR 122.5%, still in the FIFO.
		let parked_vault = vault(&parked_owner);
		assert_eq!(parked_vault.debt.total(), 8_000 * PUSD);
		assert_eq!(parked_vault.collateral, 4_900 * WND);
		assert_eq!(vault_status(&parked_owner), Some(VaultStatus::FinalRecovery));
	});
}

/// Entering final recovery pays the keeper what liquidating the vault would have
/// paid, out of the vault: the 6.25 WND that `liquidations.rs` pays for an
/// identical vault, from the 2 pUSD flat plus 0.1% of the 5,250 WND seizure.
/// Settlement then prices the collateral the reward left.
#[test]
fn final_recovery_entry_pays_the_liquidation_keeper_reward() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&liquidation_spec());
		// 6,000 WND = 12,000 pUSD value against 10,000 pUSD debt at 2: CR 120%.
		let parked_owner = acct(1);
		open_vault(&parked_owner, 6_000 * WND, 10_000 * PUSD, FixedU128::zero());
		feed_price(dot_price(2, 1));
		let keeper = keeper();

		assert_ok!(Vaults::enter_final_recovery(
			RuntimeOrigin::signed(keeper.clone()),
			get_native_id(),
			PUSD_ID,
			parked_owner.clone(),
		));

		let reward = 6_250_000_000_000;
		assert_eq!(native_balance(&keeper) - get_native_ed(), reward);
		assert_eq!(vault(&parked_owner).collateral, 6_000 * WND - reward);
		assert_eq!(collateral_on_hold(&get_native_id(), &parked_owner), 6_000 * WND - reward);
		assert_eq!(branch_state().total_collateral, 6_000 * WND - reward);
		System::assert_has_event(RuntimeEvent::Vaults(
			pallet_vaults::Event::VaultEnteredFinalRecovery {
				collateral_id: get_native_id(),
				stable_id: PUSD_ID,
				owner: parked_owner.clone(),
				keeper: keeper.clone(),
				keeper_reward: reward,
			},
		));

		// CR 119.875% still caps the bonus at the 10% penalty, so 2,000 pUSD still buys
		// 1,100 WND.
		let collateral_out = redeem(
			&acct(3),
			RedemptionTerms { max_stable_to_spend: 2_000 * PUSD, min_collateral_out: 1_100 * WND },
		);
		assert_eq!(collateral_out, 1_100 * WND);
		assert_eq!(vault(&parked_owner).collateral, 4_900 * WND - reward);
	});
}

/// A 2,000 pUSD shortfall with 1,000 pUSD of insurance cover leaves 9,000 pUSD
/// to cancel externally, at recovery rate 8,000 / 9,000. The full settlement
/// pays out all collateral, burns the cover, and closes the vault.
#[test]
fn final_recovery_redemption_below_par_with_insurance_cover() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());
		// 4,000 WND = 8,000 pUSD value against 10,000 pUSD debt: CR 80%.
		let parked_owner = acct(1);
		park_in_final_recovery(&parked_owner, 4_000 * WND, 10_000 * PUSD);

		// Insurance Fund balance = 1,000 pUSD.
		let insurance = insurance_account();
		mint_pusd(&insurance, 1_000 * PUSD);

		// Partial settlement: 3,000 pUSD × 8/9 = 2,666.67 pUSD of collateral
		// value = 1,333.33 WND. 3,000 of the 9,000 pUSD market debt buys a third
		// of the 4,000 WND, floored to the planck.
		let partial_out = redeem(
			&acct(3),
			RedemptionTerms { max_stable_to_spend: 3_000 * PUSD, min_collateral_out: 1_333 * WND },
		);
		assert_eq!(partial_out, 1_333_333_333_333_333);

		// Full settlement: the remaining 6,000 pUSD takes all collateral and burns the cover.
		let deposit_held_before = vault_deposit_on_hold(&get_native_id(), &parked_owner);
		let owner_free_before = native_balance(&parked_owner);
		let settled_out = redeem(
			&acct(4),
			RedemptionTerms { max_stable_to_spend: 6_000 * PUSD, min_collateral_out: 2_600 * WND },
		);
		// Total collateral paid out = 4,000 WND.
		assert_eq!(settled_out, 4_000 * WND - partial_out);
		// Insurance Fund burn = 1,000 pUSD.
		assert_eq!(pusd_balance(&insurance), 0);
		// Full settlement removes the vault, releases its holds, and refunds its storage deposit.
		assert_eq!(vault_status(&parked_owner), None);
		assert_eq!(collateral_on_hold(&get_native_id(), &parked_owner), 0);
		assert_eq!(vault_deposit_on_hold(&get_native_id(), &parked_owner), 0);
		assert_eq!(
			native_balance(&parked_owner) - owner_free_before,
			deposit_held_before,
			"the close refunded the vault's storage deposit",
		);
		assert_eq!(branch_state().vault_count, 0);
		System::assert_has_event(RuntimeEvent::Vaults(pallet_vaults::Event::VaultClosed {
			collateral_id: get_native_id(),
			stable_id: PUSD_ID,
			owner: parked_owner.clone(),
			recipient: parked_owner.clone(),
			collateral: 0,
		}));
	});
}

/// A 2,000 pUSD shortfall against 3,000 pUSD of insurance cover is covered in
/// full, so the market cancels the remaining 8,000 pUSD at recovery rate 1: the
/// redeemer settles at par. A partial settlement leaves the fund untouched; the
/// full settlement burns exactly the shortfall and leaves the surplus in the fund.
#[test]
fn final_recovery_redemption_below_par_with_full_insurance_cover() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());
		// 4,000 WND = 8,000 pUSD value against 10,000 pUSD debt: CR 80%.
		let parked_owner = acct(1);
		park_in_final_recovery(&parked_owner, 4_000 * WND, 10_000 * PUSD);

		// Insurance Fund balance = 3,000 pUSD, above the 2,000 pUSD shortfall.
		let insurance = insurance_account();
		mint_pusd(&insurance, 3_000 * PUSD);

		// Partial settlement at par: 3,000 pUSD buys 1,500 WND and draws no cover.
		let partial_out = redeem(
			&acct(3),
			RedemptionTerms { max_stable_to_spend: 3_000 * PUSD, min_collateral_out: 1_500 * WND },
		);
		assert_eq!(partial_out, 1_500 * WND);
		assert_eq!(pusd_balance(&insurance), 3_000 * PUSD, "a partial fill draws no cover");
		let parked_vault = vault(&parked_owner);
		assert_eq!(parked_vault.debt.total(), 7_000 * PUSD);
		assert_eq!(parked_vault.collateral, 2_500 * WND);
		assert_eq!(vault_status(&parked_owner), Some(VaultStatus::FinalRecovery));

		// Full settlement: 5,000 pUSD from the market plus 2,000 pUSD of cover
		// cancels the remaining 7,000 pUSD of debt and takes the last 2,500 WND.
		let settler = acct(4);
		let settled_out = redeem(
			&settler,
			RedemptionTerms { max_stable_to_spend: 5_000 * PUSD, min_collateral_out: 2_500 * WND },
		);
		assert_eq!(settled_out, 2_500 * WND);
		// The fund burns only the shortfall and keeps the surplus.
		assert_eq!(pusd_balance(&insurance), 1_000 * PUSD);
		System::assert_has_event(RuntimeEvent::Redemptions(
			pallet_redemptions::Event::RecoveryRedemptionExecuted {
				collateral_id: get_native_id(),
				stable_id: PUSD_ID,
				redeemer: settler.clone(),
				recipient: settler.clone(),
				vault_owner: parked_owner.clone(),
				stable_burned: 5_000 * PUSD,
				insurance_cover: 2_000 * PUSD,
				collateral_out: 2_500 * WND,
				regime: RecoveryRegime::InsuranceAdjusted,
			},
		));
		assert_eq!(vault_status(&parked_owner), None);
		assert_eq!(collateral_on_hold(&get_native_id(), &parked_owner), 0);
		assert_eq!(branch_state().vault_count, 0);
	});
}

#[test]
fn nominated_last_dormant_can_recover_and_settle_after_becoming_underwater() {
	AssetHubWestend::execute_with(|| {
		let [owner, other] = redistributed_husks();
		assert_ok!(nominate_dormant(&owner));
		// Repay and exit the other stake bearer while the TCR still permits collateral release.
		mint_pusd(&other, 100 * PUSD);
		assert_ok!(Vaults::repay_for(
			RuntimeOrigin::signed(other.clone()),
			get_native_id(),
			PUSD_ID,
			other.clone(),
			None,
		));
		assert_ok!(Vaults::close_vault(
			RuntimeOrigin::signed(other),
			get_native_id(),
			PUSD_ID,
			None
		));

		feed_price(dot_price(1, 20));
		assert_noop!(
			Redemptions::preview_redeem(get_native_id(), PUSD_ID, 1_000 * PUSD, 16),
			pallet_redemptions::Error::<Runtime>::NoRedeemableVault
		);
		assert_ok!(
			Redemptions::preview_recovery_offset(&get_native_id(), &PUSD_ID, 100 * PUSD),
			RecoveryOffsetQuote::NoTarget
		);
		enter_final_recovery(&owner);
		assert_eq!(branch_state().dormant_redemption_target, None);
		assert_eq!(branch_state().stakes.total, 0);

		// Recovery offers settlement, not a guarantee of willing demand: with an empty Insurance
		// Fund this redeemer deliberately pays 100 pUSD for collateral worth only 55 pUSD.
		assert_eq!(pusd_balance(&insurance_account()), 0);
		assert_eq!(
			redeem(
				&acct(4),
				RedemptionTerms {
					max_stable_to_spend: 100 * PUSD,
					min_collateral_out: 1_100 * WND,
				},
			),
			1_100 * WND
		);
		assert_eq!(vault_status(&owner), None);
		assert_eq!(branch_state().vault_count, 0);
		assert_eq!(branch_state().debt.outstanding(), 0);
	});
}
