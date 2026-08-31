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
use asset_hub_westend_runtime::{Stability, Vaults};
use frame_support::assert_err;
use pallet_stability::types::Leg;
use pallet_vaults::JitTerms;
use pusd_primitives::VaultStatus;

/// Liquidates without JIT, with a throwaway keeper.
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

fn settle_row(who: &AccountId) {
	assert_ok!(Stability::settle_deposit(
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
	sums_at_leg(Leg::Active, epoch, scale)
}

fn sums_at_leg(leg: Leg, epoch: u32, scale: u32) -> pallet_stability::types::PoolSums {
	pallet_stability::PoolSumsStore::<Runtime>::get((
		get_native_id(),
		get_pusd_id(),
		leg,
		epoch,
		scale,
	))
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

/// A CR 120% vault holds the FinalRecovery head. An incoming 1,000 pUSD deposit
/// is used for recovery at the 10% bonus. The depositor gets 550 WND claimable,
/// nothing activates, and P, S, G do not change.
#[test]
fn incoming_deposit_recovery_offset_accepted() {
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

		// collateral_out = 1,000 × 1.10 / 2 = 550 WND, credited as claimable, not
		// as a deposit.
		let row = deposit_row(&depositor);
		assert_eq!(row.active_deposit, 0);
		assert!(row.pending_deposit.is_none());
		assert_eq!(row.claimable_collateral, 550 * WND);
		assert_eq!(native_balance(&pool_account()), 550 * WND);

		// P, S, G do not change.
		let state = pool_state();
		assert_eq!(state.coords.p, FixedU128::one());
		let sums = sums_at_leg(Leg::Active, 0, 0);
		assert_eq!(sums.s_collateral, FixedU128::zero());
		assert_eq!(sums.g_yield, FixedU128::zero());

		// Vault after: 4,000 pUSD debt, 2,450 WND.
		let parked = vault(&parked_owner);
		assert_eq!(parked.debt.total(), 4_000 * PUSD);
		assert_eq!(parked.collateral, 2_450 * WND);
	});
}

/// The FinalRecovery head is below par, so the incoming deposit is rejected.
#[test]
fn incoming_deposit_rejected_below_par() {
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

/// A 2,000 pUSD active offset against the CR 120% FinalRecovery head yields
/// 1,100 WND at the 10% bonus. P goes from 1.0 to 0.8 and S rises by
/// 1,100 WND / 10,000 pUSD.
#[test]
fn active_pool_recovery_offset_and_realization() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&liquidation_spec());
		// 3,000 WND = 6,000 pUSD value against 5,000 pUSD debt at 2: CR 120%.
		let parked_owner = acct(1);
		open_vault(&parked_owner, 3_000 * WND, 5_000 * PUSD, FixedU128::zero());

		// Deposits activate before the vault enters the FIFO. Otherwise the pool
		// would use them as incoming recovery offsets.
		let big_depositor = acct(2);
		sp_deposit_matured(&big_depositor, 9_000 * PUSD);
		let small_depositor = acct(3);
		sp_deposit_matured(&small_depositor, 1_000 * PUSD);

		feed_price(dot_price(2, 1));
		park_in_final_recovery(&parked_owner, 3_000 * WND, 5_000 * PUSD);

		assert_ok!(Stability::offset_recovery(
			RuntimeOrigin::signed(acct(0xFE)),
			get_native_id(),
			get_pusd_id(),
			2_000 * PUSD,
		));

		// collateral_gain = 2,000 × 1.10 / 2 = 1,100 WND. P = 8,000 / 10,000 = 0.8.
		// S += 1,100 WND / 10,000 pUSD.
		let state = pool_state();
		assert_eq!(state.total_active_deposits, 8_000 * PUSD);
		assert_eq!(state.coords.p, FixedU128::from_rational(8, 10));
		let sums = sums_at_leg(Leg::Active, 0, 0);
		assert_eq!(sums.s_collateral, FixedU128::from_rational(1_100 * WND, 10_000 * PUSD));

		// Vault after: 3,000 pUSD debt, 1,900 WND.
		let parked = vault(&parked_owner);
		assert_eq!(parked.debt.total(), 3_000 * PUSD);
		assert_eq!(parked.collateral, 1_900 * WND);

		// The 1,000 pUSD depositor realizes 800 pUSD and 1,000 × 0.11 = 110 WND.
		assert_ok!(Stability::settle_deposit(
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

/// A liquidation uses 1,000 of 1,400 pUSD pending deposits for 500 WND. The
/// pending accumulators move once: P to 2/7 and S by 500 WND / 1,400 pUSD. Each
/// row later rounds down to pUSD base units and planck. The remainders stay in the pool totals.
#[test]
fn pending_deposit_pro_rata_offset() {
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

		// At 2.1 pUSD/WND the 1.05-weighted seizure for 1,000 pUSD is exactly 500 WND.
		feed_price(dot_price(21, 10));
		liquidate(&liquidated_owner);

		let state = pool_state();
		assert_eq!(state.total_pending_deposits, 400 * PUSD);
		// pending P = 1.0 × 400 / 1,400 = 2/7, floored at 18 decimals.
		assert_eq!(state.pending_coords.p, FixedU128::from_inner(285_714_285_714_285_714));
		// pending S += 500 WND / 1,400 pUSD, stored as planck per pUSD base unit.
		let pending_sums =
			sums_at_leg(Leg::Pending, state.pending_coords.epoch, state.pending_coords.scale);
		assert_eq!(
			pending_sums.s_collateral,
			FixedU128::from_inner(357_142_857_142_857_142_857_142),
		);

		// After maturity, `settle_deposit` activates the cohort and settles each row
		// through its checkpoint.
		advance_time(6_000);
		let mut active_sum: Balance = 0;
		let mut claimable_sum: Balance = 0;
		for (who, pending_left, collateral) in [
			// floor(300e6 × 2/7) = 85,714,285 base units. floor(300e6 × S) gives planck.
			(&alice, 85_714_285, 107_142_857_142_857),
			// floor(600e6 × 2/7) = 171,428,571; floor(600e6 × S).
			(&bob, 171_428_571, 214_285_714_285_714),
			// floor(500e6 × 2/7) = 142,857,142; floor(500e6 × S).
			(&cara, 142_857_142, 178_571_428_571_428),
		] {
			settle_row(who);
			let row = deposit_row(who);
			assert!(row.pending_deposit.is_none());
			assert_eq!(row.active_deposit, pending_left);
			assert_eq!(row.claimable_collateral, collateral);
			active_sum += row.active_deposit;
			claimable_sum += row.claimable_collateral;
		}

		// Each row rounds down. The pool totals retain 2 pUSD base units and 1 planck
		// without an owner.
		let state = pool_state();
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.total_active_deposits, 400 * PUSD);
		assert_eq!(state.total_active_deposits - active_sum, 2);
		assert_eq!(state.total_collateral_gains_unclaimed, 500 * WND);
		assert_eq!(state.total_collateral_gains_unclaimed - claimable_sum, 1);
		assert_eq!(pusd_balance(&pool_account()), 400 * PUSD);
		assert_eq!(native_balance(&pool_account()), 500 * WND);
	});
}

/// An offset of 2,000 pUSD for 1,200 WND moves S by 0.12 and P to 0.8. 400 pUSD
/// of yield then moves G by 400 × 0.8 / 8,000 = 0.04.
#[test]
fn offset_yield_and_depositor_realization() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());
		// Full yield routing keeps the figures round.
		mutate_pool_config(|config| config.yield_share = Permill::one());

		// 1,300 WND against 2,000 pUSD debt: CR 113.75% at 1.75.
		let liquidated_owner = acct(1);
		open_vault(&liquidated_owner, 1_300 * WND, 2_000 * PUSD, FixedU128::zero());

		let big_depositor = acct(2);
		sp_deposit_matured(&big_depositor, 9_000 * PUSD);
		let small_depositor = acct(3);
		sp_deposit_matured(&small_depositor, 1_000 * PUSD);

		// The yield source: 10,000 pUSD at 4% accrues 400 pUSD in one 365.25-day
		// year. It opens after the deposit activations, so accrual starts here.
		let yield_owner = acct(4);
		open_vault(&yield_owner, 20_000 * WND, 10_000 * PUSD, FixedU128::from_rational(4, 100));

		// At 1.75 pUSD/WND the 1.05-weighted seizure for 2,000 pUSD is
		// exactly 1,200 WND.
		feed_price(dot_price(7, 4));
		liquidate(&liquidated_owner);

		let state = pool_state();
		assert_eq!(state.total_active_deposits, 8_000 * PUSD);
		assert_eq!(state.coords.p, FixedU128::from_rational(8, 10));
		// S += 1,200 WND / 10,000 pUSD.
		assert_eq!(
			active_sums().s_collateral,
			FixedU128::from_rational(1_200 * WND, 10_000 * PUSD),
		);

		// One year of interest mints 400 pUSD of yield: G += 400 × 0.8 / 8,000 = 0.04.
		advance_time(31_557_600_000);
		assert_ok!(Vaults::poke(
			RuntimeOrigin::signed(acct(0xFE)),
			get_native_id(),
			get_pusd_id(),
			yield_owner.clone(),
		));
		assert_eq!(active_sums().g_yield, FixedU128::from_rational(4, 100));

		// The 1,000 pUSD depositor realizes 800 pUSD, 120 WND, and 40 pUSD of yield.
		settle_row(&small_depositor);
		let row = deposit_row(&small_depositor);
		assert_eq!(row.active_deposit, 800 * PUSD);
		assert_eq!(row.claimable_collateral, 120 * WND);
		assert_eq!(row.claimable_yield, 40 * PUSD);
	});
}

/// Two offsets, 600 pUSD / 300 WND then 900 pUSD / 450 WND, and two yield
/// payments, 150 then 180 pUSD, with a 900 pUSD deposit between them. Each
/// cohort realizes against its own snapshot and the totals reconcile.
#[test]
fn multiple_depositor_cohorts() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());
		// Instant activation excludes the entry delay from interest. The full yield share
		// keeps the amounts round.
		mutate_pool_config(|config| {
			config.entry_delay = 0;
			config.yield_share = Permill::one();
		});

		// The yield source: 15,000 pUSD at 1% accrues 150 pUSD in year one and
		// 180 pUSD in the next 1.2 years.
		let yield_owner = acct(1);
		open_vault(&yield_owner, 40_000 * WND, 15_000 * PUSD, FixedU128::from_rational(1, 100));
		// First casualty: 340 WND / 600 pUSD, CR 119% at 2.1.
		let first_owner = acct(2);
		open_vault(&first_owner, 340 * WND, 600 * PUSD, FixedU128::zero());
		// Second casualty: 500 WND / 900 pUSD, CR 116.7% at 2.1.
		let second_owner = acct(3);
		open_vault(&second_owner, 500 * WND, 900 * PUSD, FixedU128::zero());

		let depositor_1 = acct(4);
		sp_deposit_pending(&depositor_1, 1_000 * PUSD);
		let depositor_2 = acct(5);
		sp_deposit_pending(&depositor_2, 500 * PUSD);
		assert_eq!(pool_state().total_active_deposits, 1_500 * PUSD);

		// Year one: 150 pUSD yield, G = 150 / 1,500 = 0.1.
		advance_time(31_557_600_000);
		assert_ok!(Vaults::poke(
			RuntimeOrigin::signed(acct(0xFE)),
			get_native_id(),
			get_pusd_id(),
			yield_owner.clone(),
		));
		assert_eq!(active_sums().g_yield, FixedU128::from_rational(1, 10));

		// First offset: 600 pUSD / 300 WND. S = 0.2, P = 0.6, total 900.
		feed_price(dot_price(21, 10));
		liquidate(&first_owner);
		assert_eq!(pool_state().coords.p, FixedU128::from_rational(6, 10));
		assert_eq!(active_sums().s_collateral, FixedU128::from_rational(300 * WND, 1_500 * PUSD),);

		// The third cohort joins at P = 0.6, S = 0.2, G = 0.1.
		let depositor_3 = acct(6);
		sp_deposit_pending(&depositor_3, 900 * PUSD);
		assert_eq!(pool_state().total_active_deposits, 1_800 * PUSD);

		// 1.2 more years: 180 pUSD yield, G += 180 × 0.6 / 1,800 = 0.06.
		advance_time(37_869_120_000);
		assert_ok!(Vaults::poke(
			RuntimeOrigin::signed(acct(0xFE)),
			get_native_id(),
			get_pusd_id(),
			yield_owner.clone(),
		));
		assert_eq!(active_sums().g_yield, FixedU128::from_rational(16, 100));

		// Second offset: 900 pUSD / 450 WND. S += 450 × 0.6 / 1,800 = 0.15,
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
			settle_row(who);
			let row = deposit_row(who);
			assert_eq!(row.active_deposit, compounded);
			assert_eq!(row.claimable_collateral, collateral);
			assert_eq!(row.claimable_yield, yield_gain);
		}

		// The pool holds 900 pUSD of deposits, 330 pUSD of yield, and 750 WND of gains.
		let state = pool_state();
		assert_eq!(state.total_active_deposits, 900 * PUSD);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.total_collateral_gains_unclaimed, 750 * WND);
		assert_eq!(state.total_yield_unclaimed, 330 * PUSD);
		assert_eq!(native_balance(&pool_account()), 750 * WND);
		assert_eq!(pusd_balance(&pool_account()), 1_230 * PUSD);
	});
}

/// Tests depletion, a scale change, and realization from pool coordinates other than (0, 0).
///
/// Full depletion opens epoch 1. Three 1e-4 survival ratios move P to 1e-3 on scale 1.
/// The final depletion opens epoch 2 and gives the 600 pUSD depositor 360 WND.
#[test]
fn full_depletion_and_scale_crossing_then_realization() {
	AssetHubWestend::execute_with(|| {
		let scale_pool = 1_000_000 * PUSD;
		// Each partial offset must leave this minimum active balance.
		let floor = 100 * PUSD;
		let scale_debt = scale_pool - floor;

		feed_price(dot_price(4, 1));
		create_branch(&accounting_spec());

		// Zero rates prevent changes to G. The filler keeps TCR above 130%.
		let filler_owner = acct(1);
		open_vault(&filler_owner, 20_000_000 * WND, 1_000 * PUSD, FixedU128::zero());
		// 340 WND against 1,000 pUSD: CR 136% at 4, 122.4% at 3.6.
		let epoch_casualty = acct(2);
		open_vault(&epoch_casualty, 340 * WND, 1_000 * PUSD, FixedU128::zero());
		// Each casualty has debt equal to the pool minus its floor.
		let scale_casualties = [acct(3), acct(4), acct(5)];
		for casualty in &scale_casualties {
			open_vault(casualty, 339_966 * WND, scale_debt, FixedU128::zero());
		}
		// 1,000 WND against 1,500 pUSD: CR 116.7% at 1.75.
		let final_casualty = acct(6);
		open_vault(&final_casualty, 1_000 * WND, 1_500 * PUSD, FixedU128::zero());

		// Epoch 0 to 1: the offset equals the pool, so the pool depletes.
		let epoch_depositor = acct(7);
		sp_deposit_matured(&epoch_depositor, 1_000 * PUSD);
		feed_price(dot_price(36, 10));
		liquidate(&epoch_casualty);
		let state = pool_state();
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.coords.epoch, 1);
		assert_eq!(state.coords.scale, 0);
		assert_eq!(state.coords.p, FixedU128::one());

		// Each offset leaves the floor and multiplies P by 1e-4. The third offset
		// rescales 1e-12 to 1e-3 on scale 1.
		let scale_depositor = acct(8);
		let expected_coords = [
			(0, FixedU128::from_inner(100_000_000_000_000)),
			(0, FixedU128::from_inner(10_000_000_000)),
			(1, FixedU128::from_inner(1_000_000_000_000_000)),
		];
		for (index, casualty) in scale_casualties.iter().enumerate() {
			let refill = if index == 0 { scale_pool } else { scale_debt };
			sp_deposit_matured(&scale_depositor, refill);
			liquidate(casualty);
			let state = pool_state();
			assert_eq!(state.total_active_deposits, floor);
			assert_eq!(state.coords.epoch, 1);
			assert_eq!(state.coords.scale, expected_coords[index].0);
			assert_eq!(state.coords.p, expected_coords[index].1);
		}

		// The depositor realizes three seizures: 3 × 291,637.5 = 874,912.5 WND.
		claim_collateral_out(&get_native_id(), &scale_depositor, 874_912_500_000_000_000);
		assert_ok!(Stability::withdraw(
			RuntimeOrigin::signed(scale_depositor.clone()),
			get_native_id(),
			get_pusd_id(),
			floor,
			None,
		));
		assert_eq!(pusd_balance(&scale_depositor), floor);
		// Claim and withdrawal prune the empty row.
		assert!(pallet_stability::Deposits::<Runtime>::get((
			get_native_id(),
			get_pusd_id(),
			scale_depositor.clone(),
		))
		.is_none());
		assert_eq!(pool_state().total_active_deposits, 0);
		// The epoch depositor receives ceil(1,050 pUSD / 3.6) = 291.666666666667 WND.
		claim_collateral_out(&get_native_id(), &epoch_depositor, 291_666_666_666_667);
		assert_eq!(pool_state().total_collateral_gains_unclaimed, 0);

		// Realization starts at epoch 1, scale 1, P = 1e-3, and 1,500 pUSD.
		let watched_depositor = acct(9);
		sp_deposit_matured(&watched_depositor, 600 * PUSD);
		let other_depositor = acct(10);
		sp_deposit_matured(&other_depositor, 900 * PUSD);
		// The second deposit activates the first cohort. Liquidation activates the
		// matured second cohort and offsets 1,500 pUSD.
		let state = pool_state();
		assert_eq!(state.total_active_deposits, 600 * PUSD);
		assert_eq!(state.total_pending_deposits, 900 * PUSD);

		// At 1.75 pUSD/WND the 1.05-weighted seizure for 1,500 pUSD is exactly 900 WND.
		feed_price(dot_price(7, 4));
		liquidate(&final_casualty);

		// The pool empties: the epoch advances, the scale resets, and P returns to 1.
		let state = pool_state();
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.coords.epoch, 2);
		assert_eq!(state.coords.scale, 0);
		assert_eq!(state.coords.p, FixedU128::one());
		// The closed (1, 1) row keeps S += 900 WND × P / 1,500 pUSD = 600 planck
		// per pUSD base unit.
		assert_eq!(sums_at(1, 1).s_collateral, FixedU128::from_inner(600_000_000_000_000_000_000));
		// The new epoch opens on a zeroed row.
		assert_eq!(sums_at(2, 0).s_collateral, FixedU128::zero());
		assert_eq!(sums_at(2, 0).g_yield, FixedU128::zero());

		// Across the epoch boundary, 600e6 × 600 planck / P(1e-3) = 360 WND.
		settle_row(&watched_depositor);
		let row = deposit_row(&watched_depositor);
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.claimable_collateral, 360 * WND);
		// The other depositor receives 900e6 × 600 planck / P(1e-3) = 540 WND.
		settle_row(&other_depositor);
		assert_eq!(deposit_row(&other_depositor).claimable_collateral, 540 * WND);
		// Both claims drain all offset collateral.
		claim_collateral_out(&get_native_id(), &watched_depositor, 360 * WND);
		claim_collateral_out(&get_native_id(), &other_depositor, 540 * WND);
		assert_eq!(pool_state().total_collateral_gains_unclaimed, 0);
		assert_eq!(native_balance(&pool_account()), 0);
	});
}
