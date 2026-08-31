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
use asset_hub_westend_runtime::Vaults;
use pallet_vaults::JitTerms;

/// Seizure caps at debt × 1.05 in value. The keeper takes 2 pUSD flat plus 0.1%
/// of the seizure. The rest goes to the pool.
#[test]
fn liquidation_fully_covered_by_the_active_stability_pool() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&liquidation_spec());

		// 6,000 WND against 10,000 pUSD debt: CR 240% at 4, 120% at 2.
		let liquidated_owner = acct(1);
		open_vault(&liquidated_owner, 6_000 * WND, 10_000 * PUSD, FixedU128::zero());
		// A healthy vault keeps the branch populated after the liquidation.
		let filler_owner = acct(2);
		open_vault(&filler_owner, 100_000 * WND, 10_000 * PUSD, FixedU128::zero());

		let depositor = acct(3);
		sp_deposit_matured(&depositor, 20_000 * PUSD);

		feed_price(dot_price(2, 1)); // CR 120% < MCR 125%: liquidatable

		let keeper = acct(4);
		fund_dot(&keeper, 0);
		let owner_free_before = native_balance(&liquidated_owner);
		assert_ok!(Vaults::liquidate(
			RuntimeOrigin::signed(keeper.clone()),
			get_native_id(),
			get_pusd_id(),
			liquidated_owner.clone(),
			JitTerms { max_stable: 0, min_collateral_out: 0 },
		));

		// seized = min(6,000, 10,000 × 1.05 / 2) = 5,250 WND. The 750 WND
		// surplus returns to the owner, with the vault's storage deposit.
		let (_, deposit) = expected_vault_deposit(&get_native_id(), &liquidated_owner);
		assert_eq!(native_balance(&liquidated_owner) - owner_free_before, 750 * WND + deposit);
		// keeper = 2 pUSD flat / 2 + 5,250 × 0.1% = 1 + 5.25 = 6.25 WND.
		assert_eq!(native_balance(&keeper) - get_native_ed(), 6_250_000_000_000);
		// The pool burns the full 10,000 pUSD debt and receives 5,250 − 6.25 = 5,243.75 WND.
		assert_eq!(pusd_balance(&pool_account()), 10_000 * PUSD);
		assert_eq!(native_balance(&pool_account()), 5_243_750_000_000_000);
		assert_eq!(pool_state().total_active_deposits, 10_000 * PUSD);
		// The liquidated vault is removed.
		assert_eq!(
			Vaults::vault_status(get_native_id(), get_pusd_id(), liquidated_owner.clone()),
			None
		);
	});
}

/// The 1,000 pUSD debt splits into 500 active, 200 JIT, 100 pending, and 200
/// redistributed. Seizure weighs offsets at 1.05 and redistribution at 1.10.
/// Collateral uses the same weights. Zero keeper compensation keeps the split round.
#[test]
fn liquidation_splits_across_active_jit_pending_and_redistribution() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&BranchSpec {
			keeper_flat_compensation_value: 0,
			keeper_percent_compensation: Permill::zero(),
			..liquidation_spec()
		});
		// minimum_active_pool_balance = 1 pUSD.
		mutate_pool_config(|config| config.minimum_active_pool_balance = PUSD);

		// 600 WND against 1,000 pUSD debt: CR 240% at 4, 120% at 2.
		let liquidated_owner = acct(1);
		open_vault(&liquidated_owner, 600 * WND, 1_000 * PUSD, FixedU128::zero());

		let active_depositor = acct(3);
		sp_deposit_matured(&active_depositor, 500 * PUSD);
		let pending_depositor = acct(5);
		sp_deposit_pending(&pending_depositor, 100 * PUSD);

		// Redistribution pricing requires a nonzero recipient rate. The recipient opens
		// after the entry delay, which prevents interest before liquidation.
		let recipient_owner = acct(2);
		open_vault(&recipient_owner, 10_000 * WND, 1_000 * PUSD, FixedU128::from_rational(1, 100));

		feed_price(dot_price(2, 1));

		let keeper = acct(4);
		fund_dot(&keeper, 0);
		// JIT allowance = 200 pUSD, plus 1 pUSD so the burn does not empty the account.
		mint_pusd(&keeper, 201 * PUSD);
		assert_ok!(Vaults::liquidate(
			RuntimeOrigin::signed(keeper.clone()),
			get_native_id(),
			get_pusd_id(),
			liquidated_owner.clone(),
			JitTerms { max_stable: 200 * PUSD, min_collateral_out: 0 },
		));

		// total weight = 800 × 1.05 + 200 × 1.10 = 1,060 pUSD, so 530 WND is
		// seized. The 70 WND surplus returns to the owner, with the vault's storage deposit.
		let (_, deposit) = expected_vault_deposit(&get_native_id(), &liquidated_owner);
		assert_eq!(native_balance(&liquidated_owner), 70 * WND + get_native_ed() + deposit);
		// JIT: burns 200 pUSD, receives 210 / 2 = 105 WND.
		assert_eq!(pusd_balance(&keeper), PUSD);
		assert_eq!(native_balance(&keeper) - get_native_ed(), 105 * WND);
		// Pool: burns 500 active + 100 pending, receives 262.5 + 52.5 = 315 WND.
		assert_eq!(pusd_balance(&pool_account()), 0);
		assert_eq!(native_balance(&pool_account()), 315 * WND);
		assert_eq!(pool_state().total_active_deposits, 0);
		assert_eq!(pool_state().total_pending_deposits, 0);
		// Redistribution: 200 pUSD debt and 110 WND reach the recipient after a poke.
		assert_ok!(Vaults::poke(
			RuntimeOrigin::signed(keeper.clone()),
			get_native_id(),
			get_pusd_id(),
			recipient_owner.clone(),
		));
		let recipient_vault = vault(&recipient_owner);
		assert_eq!(recipient_vault.debt.total(), 1_200 * PUSD);
		assert_eq!(recipient_vault.collateral, 10_110 * WND);
	});
}
