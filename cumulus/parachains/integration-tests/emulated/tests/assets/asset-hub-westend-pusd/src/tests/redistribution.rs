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

fn liquidate(owner: &AccountId) {
	let keeper = acct(0xEE);
	fund_dot(&keeper, 0);
	assert_ok!(Vaults::liquidate(
		RuntimeOrigin::signed(keeper),
		get_native_id(),
		get_pusd_id(),
		owner.clone(),
		JitTerms { max_stable: 0, min_collateral_out: 0 },
	));
}

fn poke(owner: &AccountId) {
	assert_ok!(Vaults::poke(
		RuntimeOrigin::signed(acct(0xFE)),
		get_native_id(),
		get_pusd_id(),
		owner.clone(),
	));
}

/// The first redistribution (1,000 pUSD / 550 WND) splits 60/40 over the
/// 6,000 + 4,000 WND stakes. For the second (1,555 pUSD / 855.25 WND) the
/// earlier recipients count with their still-pending gains — effective stakes
/// 6,330 / 4,220 / 5,000 — even though nothing was poked in between.
#[test]
fn example_13_redistribution_with_vault_joining_between_events() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());

		// Recipients: 6,000 and 4,000 WND of stake. Redistribution prices
		// pending debt at the recipient-average rate, so rates are non-zero.
		let owner_a = acct(1);
		open_vault(&owner_a, 6_000 * WND, 3_000 * PUSD, FixedU128::from_rational(1, 100));
		let owner_b = acct(2);
		open_vault(&owner_b, 4_000 * WND, 2_000 * PUSD, FixedU128::from_rational(1, 100));

		// First casualty: 590 WND / 1,000 pUSD, CR 118% at 2. With an empty
		// stability pool the whole debt redistributes; its collateral share
		// is 1,000 × 1.10 / 2 = 550 WND.
		let first_casualty = acct(3);
		open_vault(&first_casualty, 590 * WND, 1_000 * PUSD, FixedU128::zero());
		feed_price(dot_price(2, 1));
		liquidate(&first_casualty);

		// A new vault joins after the first redistribution, at the healthy
		// price so its 5,000 WND enter at a valid CR; the second casualty
		// (900 WND / 1,555 pUSD, CR 115.8% at 2) also opens now so it takes
		// no part in the first round.
		feed_price(dot_price(4, 1));
		let owner_c = acct(4);
		open_vault(&owner_c, 5_000 * WND, 2_500 * PUSD, FixedU128::from_rational(1, 100));
		let second_casualty = acct(5);
		open_vault(&second_casualty, 900 * WND, 1_555 * PUSD, FixedU128::zero());

		// Second redistribution: 1,555 pUSD and 1,555 × 1.10 / 2
		// = 855.25 WND over effective stakes 6,330 / 4,220 / 5,000 (each
		// stake unit gains exactly 0.1 pUSD and 0.055 WND).
		feed_price(dot_price(2, 1));
		liquidate(&second_casualty);

		poke(&owner_a);
		poke(&owner_b);
		poke(&owner_c);

		// A: 3,000 + 600 + 633; 6,000 + 330 + 348.15.
		let vault_a = vault(&owner_a);
		assert_eq!(vault_a.debt.total(), 4_233 * PUSD);
		assert_eq!(vault_a.collateral, 6_678 * WND + 150_000_000_000);
		// B: 2,000 + 400 + 422; 4,000 + 220 + 232.10.
		let vault_b = vault(&owner_b);
		assert_eq!(vault_b.debt.total(), 2_822 * PUSD);
		assert_eq!(vault_b.collateral, 4_452 * WND + 100_000_000_000);
		// C: 2,500 + 500; 5,000 + 275 — each a smallest-unit short of the
		// nominal share: per-stake indexes floor per recipient and the
		// remainder stays in branch accounting.
		let vault_c = vault(&owner_c);
		assert_eq!(vault_c.debt.total(), 3_000 * PUSD - 1);
		assert_eq!(vault_c.collateral, 5_275 * WND - 1);
	});
}

/// Between the redistribution and a vault's touch, the *branch* accrues the
/// pending debt at the recipient-average rate
/// (6,000 × 4% + 4,000 × 10%) / 10,000 = 6.4%. The touch reconciles each
/// vault's share to its own rate — the doc's "actual-rate contribution"
/// (A: 600 × 4% = 24, B: 400 × 10% = 40) — so vault-level interest for the
/// whole period prices at the vault's own rate, and the avg-vs-actual
/// difference nets to zero across recipients (−14.4 + 14.4).
///
/// `touch_order` picks which recipient reconciles first. Each share is a
/// function of that vault's own stake and rate, so both orders have to land on
/// the same rows and the same market totals.
fn run_example_15(touch_order: [usize; 2]) {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());

		let owner_a = acct(1); // 6,000 WND stake at 4%
		open_vault(&owner_a, 6_000 * WND, 3_000 * PUSD, FixedU128::from_rational(4, 100));
		let owner_b = acct(2); // 4,000 WND stake at 10%
		open_vault(&owner_b, 4_000 * WND, 2_000 * PUSD, FixedU128::from_rational(10, 100));

		// 1,000 pUSD / 550 WND redistribution.
		let casualty = acct(3);
		open_vault(&casualty, 590 * WND, 1_000 * PUSD, FixedU128::zero());
		feed_price(dot_price(2, 1));
		liquidate(&casualty);

		// One year passes before the touches reconcile each share to its
		// owner's rate.
		let recipients = [owner_a.clone(), owner_b.clone()];
		advance_time(31_557_600_000);
		poke(&recipients[touch_order[0]]);
		poke(&recipients[touch_order[1]]);

		// A: 3,000 + 600 principal; interest (3,000 + 600) × 4% = 144.
		let vault_a = vault(&owner_a);
		assert_eq!(vault_a.debt.principal, 3_600 * PUSD);
		assert_eq!(vault_a.debt.interest, 144 * PUSD);
		assert_eq!(vault_a.collateral, 6_330 * WND);
		// B: 2,000 + 400 principal; interest (2,000 + 400) × 10% = 240.
		let vault_b = vault(&owner_b);
		assert_eq!(vault_b.debt.principal, 2_400 * PUSD);
		assert_eq!(vault_b.debt.interest, 240 * PUSD);
		assert_eq!(vault_b.collateral, 4_220 * WND);

		// A second year accrues on the reconciled principals at the same
		// rates: A adds another 144, B another 240.
		advance_time(31_557_600_000);
		poke(&recipients[touch_order[0]]);
		poke(&recipients[touch_order[1]]);
		let vault_a = vault(&owner_a);
		let vault_b = vault(&owner_b);
		assert_eq!(vault_a.debt.interest, 2 * 144 * PUSD);
		assert_eq!(vault_b.debt.interest, 2 * 240 * PUSD);

		// Both touches materialized their share, so the market totals are
		// exactly the two rows: nothing is left pending in redistribution or
		// in interest attribution.
		let state = branch_state();
		assert_eq!(state.debt.pending_redistribution_principal, 0);
		assert_eq!(state.pending_redistribution_collateral, 0);
		assert_eq!(state.debt.pending_interest_attribution, 0);
		assert_eq!(state.debt.principal, vault_a.debt.principal + vault_b.debt.principal);
		assert_eq!(state.debt.minted_interest, vault_a.debt.interest + vault_b.debt.interest);
		assert_eq!(state.total_collateral, vault_a.collateral + vault_b.collateral);
		// The casualty's 1,000 pUSD moved to the recipients rather than being
		// burned, so the 6,000 pUSD drawn across the three vaults is still
		// outstanding, and every unit of interest charged was issued.
		assert_eq!(state.debt.outstanding(), 6_000 * PUSD + 2 * 384 * PUSD);
		assert_eq!(pusd_issuance(), state.debt.outstanding());
	});
}

#[test]
fn example_15_weight_per_stake_reconciliation() {
	run_example_15([0, 1]);
}

/// The reconciliation must not turn on which recipient touches first.
#[test]
fn example_15_reconciliation_is_touch_order_independent() {
	run_example_15([1, 0]);
}
