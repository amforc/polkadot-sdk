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

//! Multi-step vault operations submitted as one `Utility::batch_all`.
//!
//! Every vault extrinsic checks its own post-state, so a batch passes when each
//! leg passes in order. A failing leg reverts the whole batch.

use crate::imports::*;
use asset_hub_westend_runtime::{RuntimeCall, Vaults};
use frame_support::{assert_noop, dispatch::DispatchResult};
use sp_runtime::traits::Dispatchable;
use stablecoin_primitives::{CollateralRatio, VaultStatus};

fn batch_all(who: &AccountId, calls: Vec<RuntimeCall>) -> DispatchResult {
	RuntimeCall::Utility(pallet_utility::Call::batch_all { calls })
		.dispatch(RuntimeOrigin::signed(who.clone()))
		.map(|_| ())
		.map_err(|error| error.error)
}

fn deposit_call(owner: &AccountId, amount: Balance) -> RuntimeCall {
	RuntimeCall::Vaults(pallet_vaults::Call::deposit_collateral_for {
		collateral_id: get_native_id(),
		stable_id: DOTUSD_ID,
		owner: owner.clone(),
		amount,
	})
}

fn borrow_call(amount: Balance) -> RuntimeCall {
	RuntimeCall::Vaults(pallet_vaults::Call::borrow {
		collateral_id: get_native_id(),
		stable_id: DOTUSD_ID,
		amount,
		maybe_new_rate: None,
		recipient: None,
		hint: pallet_linked_list::Position::endpoints_only(),
	})
}

fn branch_tcr() -> FixedU128 {
	match Vaults::branch_tcr(get_native_id(), DOTUSD_ID).expect("branch priced") {
		CollateralRatio::Ratio(ratio) => ratio,
		CollateralRatio::DebtFree => panic!("the scenario branch carries debt"),
	}
}

/// A borrow that alone would leave the vault below ICR passes when a collateral
/// deposit precedes it in the batch. The reverse order fails on the borrow leg
/// and keeps nothing of the batch.
#[test]
fn batch_all_deposit_then_borrow_meets_icr_together() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());
		// The other vault keeps the branch far from Safety mode, so only ICR binds.
		open_vault(&acct(2), 100_000 * WND, 10_000 * DOTUSD, FixedU128::zero());
		let owner = acct(1);
		// 1,000 WND × 2 = 2,000 dotUSD against 1,000 dotUSD: CR 200%.
		open_vault(&owner, 1_000 * WND, 1_000 * DOTUSD, FixedU128::zero());
		fund_dot(&owner, 100 * WND);

		// 2,000 / (1,000 + 700) ≈ 117.6% < ICR 120%.
		let unsafe_ratio = || pallet_vaults::Error::<Runtime>::UnsafeCollateralizationRatio;
		assert_noop!(batch_all(&owner, vec![borrow_call(700 * DOTUSD)]), unsafe_ratio());
		assert_noop!(
			batch_all(&owner, vec![borrow_call(700 * DOTUSD), deposit_call(&owner, 100 * WND)]),
			unsafe_ratio(),
		);

		// (1,000 + 100) × 2 / (1,000 + 700) ≈ 129.4% ≥ ICR 120%.
		assert_ok!(batch_all(
			&owner,
			vec![deposit_call(&owner, 100 * WND), borrow_call(700 * DOTUSD)],
		));
		assert_eq!(vault(&owner).collateral, 1_100 * WND);
		assert_eq!(vault(&owner).debt.total(), 1_700 * DOTUSD);
		assert_eq!(collateral_on_hold(&get_native_id(), &owner), 1_100 * WND);
		assert_eq!(dotusd_balance(&owner), 1_700 * DOTUSD);
	});
}

/// In Safety mode, a borrow that lowers TCR fails alone. Batched after a deposit
/// that lifts TCR to the safety ratio, it passes under the Normal-mode rule and
/// the batch ends with a higher TCR than it started from.
#[test]
fn batch_all_deposit_lifting_branch_out_of_safety_allows_borrow() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());
		// 20,000 dotUSD of collateral value against 15,000 dotUSD: CR 133%.
		open_vault(&acct(2), 10_000 * WND, 15_000 * DOTUSD, FixedU128::zero());
		let owner = acct(1);
		open_vault(&owner, 1_000 * WND, 500 * DOTUSD, FixedU128::zero());
		fund_dot(&owner, 1_000 * WND);

		// TCR = 11,000 WND × 1.68 / 15,500 dotUSD = 18,480 / 15,500 ≈ 119.2% < SCR 120%.
		feed_price(dot_price(168, 100));
		let tcr_before = branch_tcr();
		assert_eq!(tcr_before, FixedU128::from_rational(18_480, 15_500));
		assert!(tcr_before < BranchSpec::default().scr, "the branch is in Safety mode");

		// The owner stays above ICR (1,680 / 1,000 = 168%), but TCR would fall to
		// 18,480 / 16,000 = 115.5%.
		assert_noop!(
			batch_all(&owner, vec![borrow_call(500 * DOTUSD)]),
			pallet_vaults::Error::<Runtime>::SafetyModeTcrWorsening,
		);

		// The deposit lifts TCR to 12,000 × 1.68 / 15,500 = 20,160 / 15,500 ≈ 130.1%,
		// so the borrow only has to stay at SCR: 20,160 / 16,000 = 126%.
		assert_ok!(batch_all(
			&owner,
			vec![deposit_call(&owner, 1_000 * WND), borrow_call(500 * DOTUSD)],
		));
		assert_eq!(branch_tcr(), FixedU128::from_rational(126, 100));
		assert!(branch_tcr() > tcr_before);
		assert_eq!(vault(&owner).collateral, 2_000 * WND);
		assert_eq!(vault(&owner).debt.total(), 1_000 * DOTUSD);
	});
}

/// A Dormant vault takes no deposit, so a revive-and-top-up batch must borrow
/// first: the borrow above minimum debt makes the vault Active, after which the
/// deposit leg is accepted.
#[test]
fn batch_all_revives_dormant_vault_borrow_first() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());
		let owner = acct(1);
		open_vault(&owner, 1_000 * WND, 500 * DOTUSD, FixedU128::zero());
		fund_dot(&owner, 100 * WND);

		// A full payoff leaves the collateral in a debt-free Dormant vault.
		assert_ok!(Vaults::repay_for(
			RuntimeOrigin::signed(owner.clone()),
			get_native_id(),
			DOTUSD_ID,
			owner.clone(),
			None,
		));
		assert_eq!(vault_status(&owner), Some(VaultStatus::Dormant));
		assert_eq!(vault(&owner).collateral, 1_000 * WND);

		assert_noop!(
			batch_all(&owner, vec![deposit_call(&owner, 100 * WND), borrow_call(500 * DOTUSD)]),
			pallet_vaults::Error::<Runtime>::InvalidVaultStatus,
		);

		assert_ok!(batch_all(
			&owner,
			vec![borrow_call(500 * DOTUSD), deposit_call(&owner, 100 * WND)],
		));
		assert_eq!(vault_status(&owner), Some(VaultStatus::Active));
		assert_eq!(vault(&owner).collateral, 1_100 * WND);
		assert_eq!(vault(&owner).debt.total(), 500 * DOTUSD);
		assert_eq!(dotusd_balance(&owner), 500 * DOTUSD);
	});
}
