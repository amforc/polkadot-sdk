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
use pallet_redemptions::{RecoveryRegime, RedemptionTerms};
use pusd_primitives::VaultStatus;

/// Opens a vault at a healthy price, halves the price, and puts the vault in the
/// FinalRecovery FIFO.
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

		let redeemer = acct(3);
		fund_dot(&redeemer, 0);
		mint_pusd(&redeemer, 2_000 * PUSD);

		// collateral_out = 2,000 * 1.10 / 2 = 1,100 WND. Recovery redemptions
		// charge no fee.
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
		// CR 122.5%, still in the FIFO.
		let parked_vault = vault(&parked_owner);
		assert_eq!(parked_vault.debt.total(), 8_000 * PUSD);
		assert_eq!(parked_vault.collateral, 4_900 * WND);
		assert_eq!(
			Vaults::vault_status(get_native_id(), get_pusd_id(), parked_owner.clone()),
			Some(VaultStatus::FinalRecovery),
		);
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
		let keeper = acct(0xFE);
		fund_dot(&keeper, 0);

		assert_ok!(Vaults::enter_final_recovery(
			RuntimeOrigin::signed(keeper.clone()),
			get_native_id(),
			get_pusd_id(),
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
				stable_id: get_pusd_id(),
				owner: parked_owner.clone(),
				keeper: keeper.clone(),
				keeper_reward: reward,
			},
		));

		// CR 119.875% still caps the bonus at the 10% penalty, so 2,000 pUSD still buys
		// 1,100 WND.
		let redeemer = acct(3);
		fund_dot(&redeemer, 0);
		mint_pusd(&redeemer, 2_000 * PUSD);
		assert_ok!(Redemptions::redeem(
			RuntimeOrigin::signed(redeemer.clone()),
			get_native_id(),
			get_pusd_id(),
			RedemptionTerms { max_stable_to_spend: 2_000 * PUSD, min_collateral_out: 1_100 * WND },
			redeemer.clone(),
			16,
		));
		assert_eq!(native_balance(&redeemer) - get_native_ed(), 1_100 * WND);
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
		// value = 1,333.33 WND.
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
		// 3,000 of the 9,000 pUSD market debt buys a third of the 4,000 WND,
		// floored to the planck.
		let partial_out = native_balance(&redeemer) - get_native_ed();
		assert_eq!(partial_out, 1_333_333_333_333_333);

		// Full settlement: the remaining 6,000 pUSD takes all collateral and burns the cover.
		let deposit_held_before = vault_deposit_on_hold(&get_native_id(), &parked_owner);
		let owner_free_before = native_balance(&parked_owner);
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
		// Total collateral paid out = 4,000 WND.
		assert_eq!(native_balance(&settler) - get_native_ed(), 4_000 * WND - partial_out);
		// Insurance Fund burn = 1,000 pUSD.
		assert_eq!(pusd_balance(&insurance), 0);
		// Full settlement removes the vault, releases its holds, and refunds its storage deposit.
		assert!(pallet_vaults::Vaults::<Runtime>::get((
			get_native_id(),
			get_pusd_id(),
			parked_owner.clone(),
		))
		.is_none());
		assert_eq!(
			Vaults::vault_status(get_native_id(), get_pusd_id(), parked_owner.clone()),
			None
		);
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
			stable_id: get_pusd_id(),
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
		let redeemer = acct(3);
		fund_dot(&redeemer, 0);
		mint_pusd(&redeemer, 3_000 * PUSD);
		assert_ok!(Redemptions::redeem(
			RuntimeOrigin::signed(redeemer.clone()),
			get_native_id(),
			get_pusd_id(),
			RedemptionTerms { max_stable_to_spend: 3_000 * PUSD, min_collateral_out: 1_500 * WND },
			redeemer.clone(),
			16,
		));
		assert_eq!(pusd_balance(&redeemer), 0);
		assert_eq!(native_balance(&redeemer) - get_native_ed(), 1_500 * WND);
		assert_eq!(pusd_balance(&insurance), 3_000 * PUSD, "a partial fill draws no cover");
		let parked_vault = vault(&parked_owner);
		assert_eq!(parked_vault.debt.total(), 7_000 * PUSD);
		assert_eq!(parked_vault.collateral, 2_500 * WND);
		assert_eq!(
			Vaults::vault_status(get_native_id(), get_pusd_id(), parked_owner.clone()),
			Some(VaultStatus::FinalRecovery),
		);

		// Full settlement: 5,000 pUSD from the market plus 2,000 pUSD of cover
		// cancels the remaining 7,000 pUSD of debt and takes the last 2,500 WND.
		let settler = acct(4);
		fund_dot(&settler, 0);
		mint_pusd(&settler, 5_000 * PUSD);
		assert_ok!(Redemptions::redeem(
			RuntimeOrigin::signed(settler.clone()),
			get_native_id(),
			get_pusd_id(),
			RedemptionTerms { max_stable_to_spend: 5_000 * PUSD, min_collateral_out: 2_500 * WND },
			settler.clone(),
			16,
		));
		assert_eq!(pusd_balance(&settler), 0);
		assert_eq!(native_balance(&settler) - get_native_ed(), 2_500 * WND);
		// The fund burns only the shortfall and keeps the surplus.
		assert_eq!(pusd_balance(&insurance), 1_000 * PUSD);
		System::assert_has_event(RuntimeEvent::Redemptions(
			pallet_redemptions::Event::RecoveryRedemptionExecuted {
				collateral_id: get_native_id(),
				stable_id: get_pusd_id(),
				redeemer: settler.clone(),
				recipient: settler.clone(),
				vault_owner: parked_owner.clone(),
				stable_burned: 5_000 * PUSD,
				insurance_cover: 2_000 * PUSD,
				collateral_out: 2_500 * WND,
				regime: RecoveryRegime::InsuranceAdjusted,
			},
		));
		assert!(pallet_vaults::Vaults::<Runtime>::get((
			get_native_id(),
			get_pusd_id(),
			parked_owner.clone(),
		))
		.is_none());
		assert_eq!(collateral_on_hold(&get_native_id(), &parked_owner), 0);
		assert_eq!(branch_state().vault_count, 0);
	});
}
