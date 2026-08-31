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
use asset_hub_westend_runtime::{governance::TreasuryAccount, Redemptions, Vaults};
use pallet_redemptions::{RedemptionStates, RedemptionTerms};
use pusd_primitives::VaultStatus;

/// Funds `redeemer` with `terms.max_stable_to_spend` plus the stablecoin minimum
/// balance, redeems, and returns the collateral received.
///
/// Every redemption in this file is budgeted to the unit, so the helper asserts
/// that only the minimum balance remains.
fn redeem(redeemer: &AccountId, terms: RedemptionTerms<Balance>) -> Balance {
	fund_dot(redeemer, 0);
	mint_pusd(redeemer, terms.max_stable_to_spend + PUSD_MIN_BALANCE);
	let native_before = native_balance(redeemer);

	assert_ok!(Redemptions::redeem(
		RuntimeOrigin::signed(redeemer.clone()),
		get_native_id(),
		get_pusd_id(),
		terms,
		redeemer.clone(),
		16,
	));

	assert_eq!(pusd_balance(redeemer), PUSD_MIN_BALANCE);
	native_balance(redeemer) - native_before
}

fn vault_status(owner: &AccountId) -> Option<VaultStatus> {
	Vaults::vault_status(get_native_id(), get_pusd_id(), owner.clone())
}

/// 1,000 pUSD against 100,000 pUSD of market debt raises the 1.5% dynamic fee by
/// 1,000 / 100,000 / 2 = 0.5%. The redemption pays the 1.75% mean of the 1.5%
/// arrival and 2.0% terminal fees, plus the 0.5% base fee.
#[test]
fn ordinary_redemption_updates_the_dynamic_fee_and_charges_the_mean() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());

		// 4,000 WND = 8,000 pUSD value against 5,000 pUSD debt: CR 160%.
		let target_owner = acct(1);
		open_vault(&target_owner, 4_000 * WND, 5_000 * PUSD, FixedU128::zero());
		// The filler brings market debt to 100,000 pUSD. Its higher rate keeps the
		// target at the redeemable head.
		let filler_owner = acct(2);
		open_vault(&filler_owner, 100_000 * WND, 95_000 * PUSD, FixedU128::from_rational(1, 100));

		set_dynamic_fee(FixedU128::from_rational(15, 1_000)); // decayed dynamic_fee = 1.5%

		let treasury_pusd_before = pusd_balance(&TreasuryAccount::get());
		// fee = 1,000 * 2.25% = 22.5 pUSD.
		let fee = 45 * PUSD / 2;
		let collateral_out = redeem(
			&acct(3),
			RedemptionTerms {
				max_stable_to_spend: 1_000 * PUSD + fee,
				min_collateral_out: 500 * WND,
			},
		);

		// new dynamic_fee = 1.5% + 0.5% = 2.0%.
		let state = RedemptionStates::<Runtime>::get(get_pusd_id());
		assert_eq!(state.dynamic_fee, FixedU128::from_rational(2, 100));

		// collateral_out = 1,000 / 2 = 500 WND.
		assert_eq!(collateral_out, 500 * WND);
		// The fee goes to the fee handler.
		assert_eq!(pusd_balance(&TreasuryAccount::get()) - treasury_pusd_before, fee);

		// Vault after: 4,000 pUSD debt, 3,500 WND = 7,000 pUSD value, CR 175%.
		let target_vault = vault(&target_owner);
		assert_eq!(target_vault.debt.total(), 4_000 * PUSD);
		assert_eq!(target_vault.collateral, 3_500 * WND);
	});
}

/// Creates a 200 pUSD dormant continuation target before an ordinary rate-index vault.
///
/// Returns the continuation target and the rate-index vault.
fn park_dormant_continuation() -> (AccountId, AccountId) {
	feed_price(dot_price(2, 1));
	create_branch(&BranchSpec { minimum_debt: 2_000 * PUSD, ..Default::default() });

	// 2,000 WND = 4,000 pUSD value against 3,000 pUSD debt: CR 133%.
	let target_owner = acct(1);
	open_vault(&target_owner, 2_000 * WND, 3_000 * PUSD, FixedU128::zero());
	// The filler brings market debt to 10,000 pUSD so the fee is exact. Its
	// higher rate keeps it behind the target in the rate index.
	let filler_owner = acct(2);
	open_vault(&filler_owner, 10_000 * WND, 7_000 * PUSD, FixedU128::from_rational(1, 100));

	// The dynamic fee rises 2,800 / 10,000 / 2 = 14%. The redemption pays the 7%
	// mean plus the 0.5% base fee: 2,800 * 7.5% = 210 pUSD.
	// collateral_out = 2,800 / 2 = 1,400 WND.
	let collateral_out = redeem(
		&acct(3),
		RedemptionTerms {
			max_stable_to_spend: (2_800 + 210) * PUSD,
			min_collateral_out: 1_400 * WND,
		},
	);
	assert_eq!(collateral_out, 1_400 * WND);

	(target_owner, filler_owner)
}

/// 200 pUSD of residual debt is below the 2,000 pUSD branch minimum. The vault
/// leaves the rate index as Dormant instead of closing, and the branch records
/// it as the continuation target.
#[test]
fn ordinary_redemption_parks_a_dormant_continuation_vault() {
	AssetHubWestend::execute_with(|| {
		let (target_owner, _) = park_dormant_continuation();

		// Vault after: 200 pUSD debt, 600 WND, status Dormant.
		let target_vault = vault(&target_owner);
		assert_eq!(target_vault.debt.total(), 200 * PUSD);
		assert_eq!(target_vault.collateral, 600 * WND);
		assert_eq!(vault_status(&target_owner), Some(VaultStatus::Dormant));
		assert_eq!(branch_state().dormant_redemption_target, Some(target_owner));
	});
}

/// Serves the continuation target before the rate index when `FinalRecovery` is empty.
///
/// The continuation takes 200 pUSD, and the rate index takes the other 88 pUSD.
#[test]
fn continuation_precedes_the_ordinary_rate_index() {
	AssetHubWestend::execute_with(|| {
		let (target_owner, filler_owner) = park_dormant_continuation();

		// Market debt is 7,200 pUSD and the dynamic fee is 14%. 288 pUSD raises
		// it by 288 / 7,200 / 2 = 2%, so the redemption pays the 15% mean plus
		// the 0.5% base fee: 288 * 15.5% = 44.64 pUSD. It buys 288 / 2 = 144 WND
		// across both targets.
		let fee = 4_464 * PUSD / 100;
		let collateral_out = redeem(
			&acct(4),
			RedemptionTerms {
				max_stable_to_spend: 288 * PUSD + fee,
				min_collateral_out: 144 * WND,
			},
		);
		assert_eq!(collateral_out, 144 * WND);

		// The continuation is served first and drains: 200 pUSD for 100 WND. The
		// owner keeps the rest of the collateral in a debt-free Dormant vault.
		let target_vault = vault(&target_owner);
		assert_eq!(target_vault.debt.total(), 0);
		assert_eq!(target_vault.collateral, 500 * WND);
		assert_eq!(vault_status(&target_owner), Some(VaultStatus::Dormant));
		// A debt-free vault releases the continuation slot.
		assert_eq!(branch_state().dormant_redemption_target, None);

		// Only the remaining 88 pUSD reaches the rate index, for 44 WND.
		let filler_vault = vault(&filler_owner);
		assert_eq!(filler_vault.debt.total(), 6_912 * PUSD);
		assert_eq!(filler_vault.collateral, 9_956 * WND);
	});
}

/// Sets up a market with both a FinalRecovery FIFO head and a parked
/// continuation target. Returns `(continuation_owner, head_owner)`.
///
/// No direct path reaches this state. Final recovery admits only the last
/// stake-bearing vault, and a continuation vault bears stake, so the
/// continuation cannot park first. A redemption cannot park it either while a
/// FinalRecovery head exists, because that head stops the ordinary walk. So two
/// vaults enter final recovery in turn. The FIFO head is then redeemed below
/// `minimum_debt` and exited, which parks it behind the vault still in the
/// FIFO.
fn park_continuation_behind_final_recovery_head() -> (AccountId, AccountId) {
	feed_price(dot_price(8, 1));
	// MCR 125% makes the vaults below eligible for final recovery.
	create_branch(&BranchSpec {
		mcr: FixedU128::from_rational(125, 100),
		icr: FixedU128::from_rational(130, 100),
		scr: FixedU128::from_rational(130, 100),
		minimum_debt: 2_000 * PUSD,
		..Default::default()
	});

	// 1,200 WND = 2,400 pUSD value against 2,000 pUSD debt at 2: CR 120%.
	let continuation_owner = acct(1);
	open_vault(&continuation_owner, 1_200 * WND, 2_000 * PUSD, FixedU128::zero());
	feed_price(dot_price(2, 1));
	assert_ok!(Vaults::enter_final_recovery(
		RuntimeOrigin::signed(acct(0xFE)),
		get_native_id(),
		get_pusd_id(),
		continuation_owner.clone(),
	));

	// 1,150 WND = 2,300 pUSD value against 2,000 pUSD debt at 2: CR 115%.
	// It opens at the healthy price. It is admitted because the first vault no
	// longer bears stake.
	feed_price(dot_price(8, 1));
	let head_owner = acct(2);
	open_vault(&head_owner, 1_150 * WND, 2_000 * PUSD, FixedU128::zero());
	feed_price(dot_price(2, 1));
	assert_ok!(Vaults::enter_final_recovery(
		RuntimeOrigin::signed(acct(0xFE)),
		get_native_id(),
		get_pusd_id(),
		head_owner.clone(),
	));

	// The FIFO head is the first vault. At CR 120% the bonus caps at 10%, so
	// 1,800 pUSD takes 1,800 * 1.10 / 2 = 990 WND. 200 pUSD against 210 WND
	// remains: CR 210%, above the 125% exit ratio and below the 2,000 pUSD
	// minimum.
	let collateral_out = redeem(
		&acct(3),
		RedemptionTerms { max_stable_to_spend: 1_800 * PUSD, min_collateral_out: 990 * WND },
	);
	assert_eq!(collateral_out, 990 * WND);
	let continuation_vault = vault(&continuation_owner);
	assert_eq!(continuation_vault.debt.total(), 200 * PUSD);
	assert_eq!(continuation_vault.collateral, 210 * WND);

	// An exit below the minimum debt parks the vault as the continuation target.
	assert_ok!(Vaults::exit_final_recovery(
		RuntimeOrigin::signed(acct(0xFE)),
		get_native_id(),
		get_pusd_id(),
		continuation_owner.clone(),
		pallet_linked_list::Position::endpoints_only(),
	));
	assert_eq!(vault_status(&continuation_owner), Some(VaultStatus::Dormant));
	assert_eq!(branch_state().dormant_redemption_target, Some(continuation_owner.clone()));

	(continuation_owner, head_owner)
}

/// Serves the `FinalRecovery` FIFO before the continuation target.
#[test]
fn final_recovery_head_precedes_the_continuation() {
	AssetHubWestend::execute_with(|| {
		let (continuation_owner, head_owner) = park_continuation_behind_final_recovery_head();

		// Tier one: the FinalRecovery head is served. At CR 115% the 14% raw bonus
		// caps at 10%, so 1,000 pUSD takes 1,000 * 1.10 / 2 = 550 WND.
		let collateral_out = redeem(
			&acct(4),
			RedemptionTerms { max_stable_to_spend: 1_000 * PUSD, min_collateral_out: 550 * WND },
		);
		assert_eq!(collateral_out, 550 * WND);
		let head_vault = vault(&head_owner);
		assert_eq!(head_vault.debt.total(), 1_000 * PUSD);
		assert_eq!(head_vault.collateral, 600 * WND);
		// The continuation target behind the head is untouched and keeps the slot.
		let continuation_vault = vault(&continuation_owner);
		assert_eq!(continuation_vault.debt.total(), 200 * PUSD);
		assert_eq!(continuation_vault.collateral, 210 * WND);
		assert_eq!(branch_state().dormant_redemption_target, Some(continuation_owner.clone()));

		// Settle the head to empty the FIFO. Its CR is 120% now, so 1,000 pUSD
		// takes 550 WND at the same capped bonus. The head becomes a debt-free
		// Dormant vault and does not take the continuation slot.
		let collateral_out = redeem(
			&acct(5),
			RedemptionTerms { max_stable_to_spend: 1_000 * PUSD, min_collateral_out: 550 * WND },
		);
		assert_eq!(collateral_out, 550 * WND);
		let husk = vault(&head_owner);
		assert_eq!(husk.debt.total(), 0);
		assert_eq!(husk.collateral, 50 * WND);
		assert_eq!(vault_status(&head_owner), Some(VaultStatus::Dormant));
		assert_eq!(branch_state().dormant_redemption_target, Some(continuation_owner.clone()));

		// Tier two: the FIFO is empty, so the continuation is served. Market debt
		// is 200 pUSD, and recovery redemptions charge no fee, so the dynamic fee
		// is still zero. 100 pUSD raises it by 100 / 200 / 2 = 25%. The redemption
		// pays the 12.5% mean plus the 0.5% base fee: 13 pUSD, for 100 / 2 = 50 WND.
		let collateral_out = redeem(
			&acct(6),
			RedemptionTerms { max_stable_to_spend: 113 * PUSD, min_collateral_out: 50 * WND },
		);
		assert_eq!(collateral_out, 50 * WND);
		let continuation_vault = vault(&continuation_owner);
		assert_eq!(continuation_vault.debt.total(), 100 * PUSD);
		assert_eq!(continuation_vault.collateral, 160 * WND);
		assert_eq!(branch_state().dormant_redemption_target, Some(continuation_owner));
	});
}
