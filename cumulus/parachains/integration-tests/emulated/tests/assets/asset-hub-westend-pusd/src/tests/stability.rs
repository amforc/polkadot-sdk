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
use asset_hub_westend_runtime::{Stability, Vaults};
use frame_support::assert_err;
use pallet_vaults::JitTerms;
use pusd_primitives::VaultStatus;

/// Liquidates with no JIT allowance, using a throwaway keeper.
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

fn poke_row(who: &AccountId) {
	assert_ok!(Stability::poke_deposit(
		RuntimeOrigin::signed(who.clone()),
		who.clone(),
		get_native_id(),
		get_pusd_id(),
	));
}

fn active_sums() -> pallet_stability::types::PoolSums {
	let coords = pool_state().coords;
	sums_at(coords.epoch, coords.scale)
}

fn sums_at(epoch: u32, scale: u32) -> pallet_stability::types::PoolSums {
	pallet_stability::PoolSumsStore::<Runtime>::get((get_native_id(), get_pusd_id(), epoch, scale))
}

fn park_in_final_recovery(owner: &AccountId, collateral: Balance, debt: Balance) {
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
	let parked = vault(owner);
	assert_eq!(parked.collateral, collateral);
	assert_eq!(parked.debt.total(), debt);
}

fn deposit_row(who: &AccountId) -> pallet_stability::types::Deposit<Balance> {
	pallet_stability::Deposits::<Runtime>::get((get_native_id(), get_pusd_id(), who.clone()))
		.expect("deposit row exists")
}

/// With a 120%-CR vault at the FinalRecovery head, an incoming 1,000 pUSD
/// deposit is consumed for recovery at the 10% bonus: the depositor gets
/// 550 WND claimable, nothing activates, and P/S/G stay untouched.
#[test]
fn example_05_1_incoming_deposit_recovery_offset_accepted() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&liquidation_spec());
		// 3,000 WND = 6,000 pUSD value against 5,000 pUSD debt at 2: CR 120%.
		let parked_owner = acct(1);
		open_vault(&parked_owner, 3_000 * WND, 5_000 * PUSD, FixedU128::zero());
		feed_price(dot_price(2, 1));
		park_in_final_recovery(&parked_owner, 3_000 * WND, 5_000 * PUSD);

		let depositor = acct(2);
		sp_deposit_pending(&depositor, 1_000 * PUSD);

		// used_for_recovery = 1,000; collateral_out = 1,000 × 1.10 / 2
		// = 550 WND, credited as claimable, not as a deposit.
		let row = deposit_row(&depositor);
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.pending_deposit, None);
		assert_eq!(row.claimable_collateral, 550 * WND);
		assert_eq!(native_balance(&pool_account()), 550 * WND);

		// P, S, G unchanged for this incoming-deposit offset.
		let state = pool_state();
		assert_eq!(state.coords.p, FixedU128::one());
		let sums =
			pallet_stability::PoolSumsStore::<Runtime>::get((get_native_id(), get_pusd_id(), 0, 0));
		assert_eq!(sums.s_collateral, FixedU128::zero());
		assert_eq!(sums.g_yield, FixedU128::zero());

		// Vault after: 4,000 pUSD debt, 2,450 WND.
		let parked = vault(&parked_owner);
		assert_eq!(parked.debt.total(), 4_000 * PUSD);
		assert_eq!(parked.collateral, 2_450 * WND);
	});
}

/// Incoming deposit rejected when the FinalRecovery head is below par.
#[test]
fn example_05_2_incoming_deposit_rejected_below_par() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&liquidation_spec());
		// 2,000 WND = 4,000 pUSD value against 5,000 pUSD debt at 2: CR 80%.
		let parked_owner = acct(1);
		open_vault(&parked_owner, 2_000 * WND, 5_000 * PUSD, FixedU128::zero());
		feed_price(dot_price(2, 1));
		park_in_final_recovery(&parked_owner, 2_000 * WND, 5_000 * PUSD);

		let depositor = acct(2);
		mint_pusd(&depositor, 1_000 * PUSD);
		assert_err!(
			Stability::deposit(
				RuntimeOrigin::signed(depositor.clone()),
				get_native_id(),
				get_pusd_id(),
				1_000 * PUSD,
			),
			pallet_stability::Error::<Runtime>::RecoveryOffsetBelowPar,
		);

		// No pUSD burned, no pending deposit created.
		assert_eq!(pusd_balance(&depositor), 1_000 * PUSD);
		assert!(pallet_stability::Deposits::<Runtime>::get((
			get_native_id(),
			get_pusd_id(),
			depositor.clone(),
		))
		.is_none());
	});
}

/// A 2,000 pUSD active offset against the 120%-CR FinalRecovery head at the
/// 10% bonus yields 1,100 WND: P compounds 1.0 → 0.8 and S rises by
/// 1,100 WND / 10,000 pUSD.
#[test]
fn example_06_active_pool_recovery_offset_and_realization() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&liquidation_spec());
		// 3,000 WND = 6,000 pUSD value against 5,000 pUSD debt at 2: CR 120%.
		let parked_owner = acct(1);
		open_vault(&parked_owner, 3_000 * WND, 5_000 * PUSD, FixedU128::zero());

		// Deposits activate before the vault parks, otherwise they would be
		// consumed as incoming recovery offsets.
		let big_depositor = acct(2);
		sp_deposit_active(&big_depositor, 9_000 * PUSD);
		let small_depositor = acct(3);
		sp_deposit_active(&small_depositor, 1_000 * PUSD);

		feed_price(dot_price(2, 1));
		park_in_final_recovery(&parked_owner, 3_000 * WND, 5_000 * PUSD);

		assert_ok!(Stability::offset_recovery(
			RuntimeOrigin::signed(acct(0xFE)),
			get_native_id(),
			get_pusd_id(),
			2_000 * PUSD,
		));

		// collateral_gain = 2,000 × 1.10 / 2 = 1,100 WND;
		// P = 1.0 × 8,000 / 10,000 = 0.8;
		// S += 1,100 WND / 10,000 pUSD = 110,000 planck per micro-pUSD.
		let state = pool_state();
		assert_eq!(state.total_active_deposits, 8_000 * PUSD);
		assert_eq!(state.coords.p, FixedU128::from_rational(8, 10));
		let sums =
			pallet_stability::PoolSumsStore::<Runtime>::get((get_native_id(), get_pusd_id(), 0, 0));
		assert_eq!(sums.s_collateral, FixedU128::from_rational(1_100 * WND, 10_000 * PUSD));

		// Vault after: 3,000 pUSD debt, 1,900 WND.
		let parked = vault(&parked_owner);
		assert_eq!(parked.debt.total(), 3_000 * PUSD);
		assert_eq!(parked.collateral, 1_900 * WND);

		// The 1,000 pUSD depositor realizes 800 pUSD compounded and
		// 1,000 × 0.11 = 110 WND.
		assert_ok!(Stability::poke_deposit(
			RuntimeOrigin::signed(small_depositor.clone()),
			small_depositor.clone(),
			get_native_id(),
			get_pusd_id(),
		));
		let row = deposit_row(&small_depositor);
		assert_eq!(row.active_deposit, 800 * PUSD);
		assert_eq!(row.claimable_collateral, 110 * WND);
	});
}

/// A liquidation consuming 1,000 of 1,400 pUSD pending deposits (500 WND of
/// collateral) moves the pending accumulators once: pending P → 2/7 and
/// pending S += 500 WND / 1,400 pUSD, and each row realizes lazily with
/// flooring; the 2 pUSD / 1 WND aggregate difference stays in pool
/// accounting.
///
/// The document floors at whole-unit scale for illustration; the chain floors
/// at its native precision (micro-pUSD, planck), so the realized figures
/// below carry the full fractional part.
#[test]
fn example_09_pending_deposit_pro_rata_offset() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());

		// 590 WND against 1,000 pUSD debt: CR 123.9% at 2.1.
		let liquidated_owner = acct(1);
		open_vault(&liquidated_owner, 590 * WND, 1_000 * PUSD, FixedU128::zero());
		let filler_owner = acct(2);
		open_vault(&filler_owner, 100_000 * WND, 1_000 * PUSD, FixedU128::zero());

		// Alice 300, Bob 600, Cara 500 pUSD, all still pending.
		let alice = acct(3);
		sp_deposit_pending(&alice, 300 * PUSD);
		let bob = acct(4);
		sp_deposit_pending(&bob, 600 * PUSD);
		let cara = acct(5);
		sp_deposit_pending(&cara, 500 * PUSD);

		// At 2.1 pUSD/WND the 1.05-weighted seizure for 1,000 pUSD is
		// exactly 500 WND, matching the example's premise.
		feed_price(dot_price(21, 10));
		liquidate(&liquidated_owner);

		let state = pool_state();
		assert_eq!(state.total_pending_deposits, 400 * PUSD);
		// pending P = 1.0 × 400 / 1,400 = 2/7, floored at 18 decimals.
		assert_eq!(state.pending_coords.p, FixedU128::from_inner(285_714_285_714_285_714));
		// pending S += 500 WND × 1.0 / 1,400 pUSD, in planck per micro-pUSD.
		let pending_sums = pallet_stability::PendingSumsStore::<Runtime>::get((
			get_native_id(),
			get_pusd_id(),
			state.pending_coords.epoch,
			state.pending_coords.scale,
		));
		assert_eq!(
			pending_sums.s_collateral,
			FixedU128::from_inner(357_142_857_142_857_142_857_142),
		);

		// Rows realize lazily, flooring at chain precision; the matured
		// remainder folds into the active deposit.
		advance_time(6_000);
		for (who, pending_left, collateral) in [
			// floor(300e6 × 2/7) = 85,714,285 µpUSD;
			// floor(300e6 × 357,142.857…) = 107_142_857_142_857 planck.
			(&alice, 85_714_285, 107_142_857_142_857),
			// floor(600e6 × 2/7) = 171,428,571; floor(600e6 × S).
			(&bob, 171_428_571, 214_285_714_285_714),
			// floor(500e6 × 2/7) = 142,857,142; floor(500e6 × S).
			(&cara, 142_857_142, 178_571_428_571_428),
		] {
			poke_row(who);
			let row = deposit_row(who);
			assert_eq!(row.pending_deposit, None);
			assert_eq!(row.active_deposit, pending_left);
			assert_eq!(row.claimable_collateral, collateral);
		}
	});
}

/// An offset of 2,000 pUSD debt for 1,200 WND moves S by 0.12 WND and P to
/// 0.8; 400 pUSD of yield afterwards moves G by 400 × 0.8 / 8,000 = 0.04.
#[test]
fn example_10_offset_yield_and_depositor_realization() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());
		// The example routes the full yield to the pool.
		mutate_pool_config(|config| config.yield_share = Permill::one());

		// 1,300 WND against 2,000 pUSD debt: CR 113.75% at 1.75.
		let liquidated_owner = acct(1);
		open_vault(&liquidated_owner, 1_300 * WND, 2_000 * PUSD, FixedU128::zero());

		let big_depositor = acct(2);
		sp_deposit_active(&big_depositor, 9_000 * PUSD);
		let small_depositor = acct(3);
		sp_deposit_active(&small_depositor, 1_000 * PUSD);

		// The yield source: 10,000 pUSD principal at 4% accrues exactly
		// 400 pUSD over one (365.25-day) year. Opened after the deposit
		// activations so the accrual window starts here.
		let yield_owner = acct(4);
		open_vault(&yield_owner, 20_000 * WND, 10_000 * PUSD, FixedU128::from_rational(4, 100));

		// At 1.75 pUSD/WND the 1.05-weighted seizure for 2,000 pUSD is
		// exactly 1,200 WND.
		feed_price(dot_price(7, 4));
		liquidate(&liquidated_owner);

		let state = pool_state();
		assert_eq!(state.total_active_deposits, 8_000 * PUSD);
		assert_eq!(state.coords.p, FixedU128::from_rational(8, 10));
		// S += 1,200 WND × 1.0 / 10,000 pUSD.
		assert_eq!(
			active_sums().s_collateral,
			FixedU128::from_rational(1_200 * WND, 10_000 * PUSD),
		);

		// One year of interest on the yield vault mints 400 pUSD of yield:
		// G += 400 × 0.8 / 8,000 = 0.04.
		advance_time(31_557_600_000);
		assert_ok!(Vaults::poke(
			RuntimeOrigin::signed(acct(0xFE)),
			get_native_id(),
			get_pusd_id(),
			yield_owner.clone(),
		));
		assert_eq!(active_sums().g_yield, FixedU128::from_rational(4, 100));

		// The 1,000 pUSD depositor realizes 800 compounded, 120 WND,
		// 40 pUSD yield.
		poke_row(&small_depositor);
		let row = deposit_row(&small_depositor);
		assert_eq!(row.active_deposit, 800 * PUSD);
		assert_eq!(row.claimable_collateral, 120 * WND);
		assert_eq!(row.claimable_yield, 40 * PUSD);
	});
}

/// Two offsets (600 pUSD / 300 WND, then 900 pUSD / 450 WND) and two yield
/// payments (150 then 180 pUSD) around a mid-sequence 900 pUSD deposit; each
/// cohort realizes exactly against its own snapshot and the totals reconcile.
#[test]
fn example_11_multiple_depositor_cohorts() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());
		// Instant activation keeps the interest clock free of entry-delay
		// jumps; full yield routing matches the example.
		mutate_pool_config(|config| {
			config.entry_delay = 0;
			config.yield_share = Permill::one();
		});

		// The yield source: 15,000 pUSD at 1% accrues 150 pUSD over the
		// first year and 180 pUSD over the next 1.2 years.
		let yield_owner = acct(1);
		open_vault(&yield_owner, 40_000 * WND, 15_000 * PUSD, FixedU128::from_rational(1, 100));
		// First liquidation target: 340 WND / 600 pUSD, CR 119% at 2.1.
		let first_owner = acct(2);
		open_vault(&first_owner, 340 * WND, 600 * PUSD, FixedU128::zero());
		// Second target: 500 WND / 900 pUSD, CR 116.7% at 2.1.
		let second_owner = acct(3);
		open_vault(&second_owner, 500 * WND, 900 * PUSD, FixedU128::zero());

		let depositor_1 = acct(4);
		sp_deposit_pending(&depositor_1, 1_000 * PUSD);
		poke_row(&depositor_1);
		let depositor_2 = acct(5);
		sp_deposit_pending(&depositor_2, 500 * PUSD);
		poke_row(&depositor_2);
		assert_eq!(pool_state().total_active_deposits, 1_500 * PUSD);

		// Year one: 150 pUSD yield → G = 150 × 1.0 / 1,500 = 0.1.
		advance_time(31_557_600_000);
		assert_ok!(Vaults::poke(
			RuntimeOrigin::signed(acct(0xFE)),
			get_native_id(),
			get_pusd_id(),
			yield_owner.clone(),
		));
		assert_eq!(active_sums().g_yield, FixedU128::from_rational(1, 10));

		// First offset: 600 pUSD / 300 WND → S = 0.2 WND per pUSD,
		// P = 0.6, total 900.
		feed_price(dot_price(21, 10));
		liquidate(&first_owner);
		assert_eq!(pool_state().coords.p, FixedU128::from_rational(6, 10));
		assert_eq!(active_sums().s_collateral, FixedU128::from_rational(300 * WND, 1_500 * PUSD),);

		// The third cohort joins at P = 0.6, S = 0.2, G = 0.1.
		let depositor_3 = acct(6);
		sp_deposit_pending(&depositor_3, 900 * PUSD);
		poke_row(&depositor_3);
		assert_eq!(pool_state().total_active_deposits, 1_800 * PUSD);

		// 1.2 more years: 180 pUSD yield → G += 180 × 0.6 / 1,800 = 0.06.
		advance_time(37_869_120_000);
		assert_ok!(Vaults::poke(
			RuntimeOrigin::signed(acct(0xFE)),
			get_native_id(),
			get_pusd_id(),
			yield_owner.clone(),
		));
		assert_eq!(active_sums().g_yield, FixedU128::from_rational(16, 100));

		// Second offset: 900 pUSD / 450 WND → S += 450 × 0.6 / 1,800 = 0.15,
		// P = 0.3, total 900.
		liquidate(&second_owner);
		assert_eq!(pool_state().coords.p, FixedU128::from_rational(3, 10));
		assert_eq!(
			active_sums().s_collateral,
			FixedU128::from_inner(350_000_000_000_000_000_000_000),
		);

		// Cohort realizations: (compounded, collateral, yield).
		for (who, compounded, collateral, yield_gain) in [
			(&depositor_1, 300 * PUSD, 350 * WND, 160 * PUSD),
			(&depositor_2, 150 * PUSD, 175 * WND, 80 * PUSD),
			(&depositor_3, 450 * PUSD, 225 * WND, 90 * PUSD),
		] {
			poke_row(who);
			let row = deposit_row(who);
			assert_eq!(row.active_deposit, compounded);
			assert_eq!(row.claimable_collateral, collateral);
			assert_eq!(row.claimable_yield, yield_gain);
		}

		// The totals reconcile: 750 WND of gains, 330 pUSD of yield,
		// 900 pUSD still deposited.
		assert_eq!(pool_state().total_active_deposits, 900 * PUSD);
	});
}

/// The document's premise is a pool that has long left its fresh coordinates:
/// P = 0.42 at epoch 7, scale 3. The realized ratios are scale-free, but the
/// storage coordinates are not, so the pool is walked off (0, 0) first — one
/// full depletion opens epoch 1, then one offset that shrinks P past the
/// 1e-9 floor crosses into scale 1. Only then does §12 itself run: offsetting
/// the pool's entire 1,500 pUSD for 900 WND closes the (1, 1) row, opens
/// epoch 2 at P = 1, and leaves the 600 pUSD depositor with 0 compounded and
/// 600 × 0.6 = 360 WND.
#[test]
fn example_12_full_depletion_and_epoch_transition() {
	AssetHubWestend::execute_with(|| {
		// The pool that crosses a scale below. One rescale costs a 1e9 shrink
		// in P, so the pool has to be more than 1e9 times its smallest legal
		// residue — 0.01 pUSD, the stablecoin's own minimum balance.
		let scale_pool = 20_000_000 * PUSD;

		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());
		// The floor has to allow that residue; otherwise the offset
		// below rounds up into a full depletion and no scale is ever crossed.
		mutate_pool_config(|config| config.minimum_active_pool_balance = PUSD_MIN_BALANCE);

		// Every vault opens at the healthy price, each clearing the 130%
		// initial ratio. The filler is sized to hold the branch TCR above the
		// 130% safety ratio while the oversized casualty is under water.
		let filler_owner = acct(1);
		open_vault(&filler_owner, 20_000_000 * WND, 1_000 * PUSD, FixedU128::zero());
		// 340 WND against 1,000 pUSD: CR 136% at 4, 122.4% at 3.6.
		let epoch_casualty = acct(2);
		open_vault(&epoch_casualty, 340 * WND, 1_000 * PUSD, FixedU128::zero());
		// 6,600,000 WND against the whole scale pool bar its residue: CR 132%
		// at 4, 118.8% at 3.6.
		let scale_casualty = acct(3);
		open_vault(
			&scale_casualty,
			6_600_000 * WND,
			scale_pool - PUSD_MIN_BALANCE,
			FixedU128::zero(),
		);
		// 1,000 WND against 1,500 pUSD: CR 116.7% at 1.75.
		let final_casualty = acct(4);
		open_vault(&final_casualty, 1_000 * WND, 1_500 * PUSD, FixedU128::zero());

		// Epoch 0 → 1: the offset matches the pool exactly, so it depletes.
		let epoch_depositor = acct(5);
		sp_deposit_active(&epoch_depositor, 1_000 * PUSD);
		feed_price(dot_price(36, 10));
		liquidate(&epoch_casualty);
		let state = pool_state();
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.coords.epoch, 1);
		assert_eq!(state.coords.scale, 0);
		assert_eq!(state.coords.p, FixedU128::one());

		// Scale 0 → 1: leaving 0.01 pUSD of a 20,000,000 pUSD pool would put
		// P at 5e-10, under the 1e-9 floor, so the pallet folds one 1e9
		// rescale into the division and lands at P = 0.5 one scale along.
		let scale_depositor = acct(6);
		sp_deposit_active(&scale_depositor, scale_pool);
		liquidate(&scale_casualty);
		let state = pool_state();
		assert_eq!(state.total_active_deposits, PUSD_MIN_BALANCE);
		assert_eq!(state.coords.epoch, 1);
		assert_eq!(state.coords.scale, 1);
		assert_eq!(state.coords.p, FixedU128::from_rational(1, 2));

		// The residue leaves, so §12 runs against a pool holding exactly the
		// document's 1,500 pUSD — but at epoch 1, scale 1, P = 0.5.
		assert_ok!(Stability::withdraw(
			RuntimeOrigin::signed(scale_depositor.clone()),
			get_native_id(),
			get_pusd_id(),
			PUSD_MIN_BALANCE,
			None,
		));
		let watched_depositor = acct(7);
		sp_deposit_active(&watched_depositor, 600 * PUSD);
		let other_depositor = acct(8);
		sp_deposit_active(&other_depositor, 900 * PUSD);
		assert_eq!(pool_state().total_active_deposits, 1_500 * PUSD);

		// At 1.75 pUSD/WND the 1.05-weighted seizure for 1,500 pUSD is
		// exactly 900 WND, the whole pool's collateral gain.
		feed_price(dot_price(7, 4));
		liquidate(&final_casualty);

		// The pool empties: the epoch advances off 1, the scale resets off 1,
		// and P returns to 1.
		let state = pool_state();
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.coords.epoch, 2);
		assert_eq!(state.coords.scale, 0);
		assert_eq!(state.coords.p, FixedU128::one());
		// The closed row is the one the depositors snapshotted, (1, 1), and it
		// keeps S += 900 × P / 1,500 at the pre-offset P = 0.5.
		assert_eq!(sums_at(1, 1).s_collateral, FixedU128::from_rational(450 * WND, 1_500 * PUSD));
		// The new epoch opens on a zeroed row.
		assert_eq!(sums_at(2, 0), Default::default());

		// Realization crosses the epoch boundary: nothing compounds, and the
		// gain still prices off the snapshot row — 600 × 0.3 / 0.5 = 360 WND.
		poke_row(&watched_depositor);
		let row = deposit_row(&watched_depositor);
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.claimable_collateral, 360 * WND);
		// The rest of the seizure lands on the other depositor:
		// 900 × 0.3 / 0.5 = 540 WND.
		poke_row(&other_depositor);
		assert_eq!(deposit_row(&other_depositor).claimable_collateral, 540 * WND);
	});
}
