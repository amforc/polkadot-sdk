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
use pallet_vaults::{types::InterestWeight, JitTerms};

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

/// A whole-unit rate-weighted principal, `principal × annual_rate` in stablecoin
/// units.
fn weight(whole: Balance) -> InterestWeight<Balance> {
	InterestWeight { whole, remainder: 0 }
}

fn poke(owner: &AccountId) {
	assert_ok!(Vaults::poke(
		RuntimeOrigin::signed(acct(0xFE)),
		get_native_id(),
		get_pusd_id(),
		owner.clone(),
	));
}

/// The first redistribution, 1,000 pUSD / 550 WND, splits 60/40 over the 6,000
/// and 4,000 WND stakes. The second, 1,555 pUSD / 855.25 WND, counts the earlier
/// recipients with their pending gains: effective stakes 6,330 / 4,220 / 5,000.
/// No poke happens between the two.
#[test]
fn redistribution_with_a_vault_joining_between_events() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());

		// Recipients with 6,000 and 4,000 WND of stake. Their rates are non-zero,
		// because redistribution prices pending debt at the recipient-average rate.
		let owner_a = acct(1);
		open_vault(&owner_a, 6_000 * WND, 3_000 * PUSD, FixedU128::from_rational(1, 100));
		let owner_b = acct(2);
		open_vault(&owner_b, 4_000 * WND, 2_000 * PUSD, FixedU128::from_rational(1, 100));

		// First casualty: 590 WND / 1,000 pUSD, CR 118% at 2. The stability pool
		// is empty, so the whole debt redistributes with 1,000 × 1.10 / 2 = 550 WND.
		let first_casualty = acct(3);
		open_vault(&first_casualty, 590 * WND, 1_000 * PUSD, FixedU128::zero());
		feed_price(dot_price(2, 1));
		liquidate(&first_casualty);

		// A new vault joins after the first redistribution, at the healthy price
		// so its CR is valid. The second casualty, 900 WND / 1,555 pUSD, CR 115.8%
		// at 2, also opens now so it takes no part in the first round.
		feed_price(dot_price(4, 1));
		let owner_c = acct(4);
		open_vault(&owner_c, 5_000 * WND, 2_500 * PUSD, FixedU128::from_rational(1, 100));
		let second_casualty = acct(5);
		open_vault(&second_casualty, 900 * WND, 1_555 * PUSD, FixedU128::zero());

		// Second redistribution: 1,555 pUSD and 1,555 × 1.10 / 2 = 855.25 WND over
		// effective stakes 6,330 / 4,220 / 5,000. Each stake unit gains 0.1 pUSD
		// and 0.055 WND.
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
		// C receives one unit less of debt and collateral because its stake and shares round down.
		let vault_c = vault(&owner_c);
		assert_eq!(vault_c.debt.total(), 3_000 * PUSD - 1);
		assert_eq!(vault_c.collateral, 5_275 * WND - 1);

		// The units remain in pending redistribution custody and totals until a sole
		// stake bearer sweeps them.
		let state = branch_state();
		assert_eq!(state.debt.pending_redistribution_principal, 1);
		assert_eq!(state.pending_redistribution_collateral, 1);
		assert_eq!(
			state.debt.outstanding(),
			vault_a.debt.total() + vault_b.debt.total() + vault_c.debt.total() + 1,
		);
		assert_eq!(
			state.total_collateral,
			vault_a.collateral + vault_b.collateral + vault_c.collateral + 1,
		);
	});
}

/// Adds 1,000 × 6.4% = 64 pUSD of pending weight at the recipient-average rate.
///
/// A touch moves A's 600 × 4% = 24 or B's 400 × 10% = 40 to its principal weight.
/// The total remains 64 pUSD and does not depend on `touch_order`.
fn run_weight_per_stake_reconciliation(touch_order: [usize; 2]) {
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

		// The redistributed 1,000 pUSD posts 64 of pending weight on top of the
		// recipients' own 3,000 × 4% + 2,000 × 10% = 320.
		let debt = branch_state().debt;
		assert_eq!(debt.pending_redistribution_weight, weight(64 * PUSD));
		assert_eq!(debt.weighted_principal, weight(384 * PUSD));

		// One year passes before the touches.
		let recipients = [owner_a.clone(), owner_b.clone()];
		// A's share of the pending weight is 600 × 4% = 24, B's 400 × 10% = 40.
		let shares = [24 * PUSD, 40 * PUSD];
		advance_time(31_557_600_000);
		poke(&recipients[touch_order[0]]);
		// The first touch moves its share from pending weight to vault principal.
		// The aggregate does not change.
		let debt = branch_state().debt;
		assert_eq!(debt.pending_redistribution_weight, weight(64 * PUSD - shares[touch_order[0]]));
		assert_eq!(debt.weighted_principal, weight(384 * PUSD));
		poke(&recipients[touch_order[1]]);
		// The second touch drains the pending weight.
		let debt = branch_state().debt;
		assert_eq!(debt.pending_redistribution_weight, weight(0));
		assert_eq!(debt.weighted_principal, weight(384 * PUSD));

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

		// A second year accrues on the reconciled principals: A adds 144, B adds 240.
		advance_time(31_557_600_000);
		poke(&recipients[touch_order[0]]);
		poke(&recipients[touch_order[1]]);
		let vault_a = vault(&owner_a);
		let vault_b = vault(&owner_b);
		assert_eq!(vault_a.debt.interest, 2 * 144 * PUSD);
		assert_eq!(vault_b.debt.interest, 2 * 240 * PUSD);

		// Both shares are materialized, so the market totals equal the two rows.
		// Nothing is pending in redistribution or interest attribution.
		let state = branch_state();
		assert_eq!(state.debt.pending_redistribution_principal, 0);
		assert_eq!(state.pending_redistribution_collateral, 0);
		assert_eq!(state.debt.pending_interest_attribution, 0);
		assert_eq!(state.debt.principal, vault_a.debt.principal + vault_b.debt.principal);
		assert_eq!(state.debt.minted_interest, vault_a.debt.interest + vault_b.debt.interest);
		assert_eq!(state.total_collateral, vault_a.collateral + vault_b.collateral);
		// The casualty's 1,000 pUSD moved to the recipients, so all 6,000 pUSD
		// drawn is still outstanding. Every unit of interest charged was issued.
		assert_eq!(state.debt.outstanding(), 6_000 * PUSD + 2 * 384 * PUSD);
		assert_eq!(pusd_issuance(), state.debt.outstanding());
	});
}

#[test]
fn weight_per_stake_reconciliation() {
	run_weight_per_stake_reconciliation([0, 1]);
}

/// The result must not depend on which recipient touches first.
#[test]
fn reconciliation_is_touch_order_independent() {
	run_weight_per_stake_reconciliation([1, 0]);
}
