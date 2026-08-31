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
use asset_hub_westend_runtime::{
	governance, pusd_config::VaultsBranchCreationDeposit, Assets, Balances, RuntimeHoldReason,
	TrustBackedAssetsInstance, Vaults,
};
use frame_support::{
	assert_err, assert_noop,
	traits::{
		fungible::InspectHold as FungibleInspectHold,
		fungibles::{roles::Inspect as RolesInspect, Refund},
	},
};

/// A stablecoin minimum balance above one unit requires the fee account's
/// stablecoin account before the first market registers.
///
/// The fee account owns that deposit, because it outlives any single market. So
/// it must hold native balance first. No stablecoin exists yet at that point.
#[test]
fn registration_creates_the_fee_account_stablecoin_account() {
	AssetHubWestend::execute_with(|| {
		let fee_account = governance::TreasuryAccount::get();
		assert!(PUSD_MIN_BALANCE > 1, "a one-unit minimum would skip the touch entirely");
		assert!(<Assets as Refund<AccountId>>::deposit_held(get_pusd_id(), fee_account.clone())
			.is_none());

		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());

		let (depositor, deposit) =
			<Assets as Refund<AccountId>>::deposit_held(get_pusd_id(), fee_account.clone())
				.expect("registration touched the fee account");
		assert_eq!(depositor, fee_account, "the stablecoin-wide account owns its deposit");
		assert!(deposit > 0);
		// The account exists but is empty. It can now receive a credit of any size.
		assert_eq!(pusd_balance(&fee_account), 0);
	});
}

/// Creates a native-collateral market as the asset owner, then transfers control to governance.
#[test]
fn signed_user_registers_a_native_collateral_market() {
	AssetHubWestend::execute_with(|| {
		create_pusd();
		feed_price(dot_price(2, 1));

		// Asset ownership authorizes market creation and redemption configuration.
		// The vault system uses fungibles traits to mint and burn.
		let creator = admin();
		// The administrators take no part in the creation.
		let full_admin = acct(7);
		let emergency_admin = acct(8);
		assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(&creator, 1_000 * WND));
		let creator_free_before = native_balance(&creator);

		assert_ok!(Vaults::create_branch(
			RuntimeOrigin::signed(creator.clone()),
			get_native_id(),
			get_pusd_id(),
			pallet_vaults::types::BranchAdmins {
				full_admin: MultiAddress::Id(full_admin.clone()),
				emergency_admin: MultiAddress::Id(emergency_admin.clone()),
			},
			branch_config(&get_native_id(), &BranchSpec::default()),
			registration_config(),
		));

		let branch = pallet_vaults::Branches::<Runtime>::get(get_native_id(), get_pusd_id())
			.expect("the signed creation registered the market");
		// The deposit is held from the creator, so a removal refunds the creator.
		assert_eq!(branch.deposit.map(|(who, _)| who), Some(creator.clone()));
		assert_eq!(
			<Balances as FungibleInspectHold<AccountId>>::balance_on_hold(
				&RuntimeHoldReason::Vaults(pallet_vaults::HoldReason::BranchCreationDeposit),
				&creator,
			),
			VaultsBranchCreationDeposit::get(),
		);
		let custody = Vaults::redistribution_account(&get_native_id(), &get_pusd_id());
		assert_eq!(native_balance(&custody), get_native_ed());
		assert_eq!(
			creator_free_before - native_balance(&creator),
			VaultsBranchCreationDeposit::get() + get_native_ed(),
		);

		assert_ok!(Vaults::set_param(
			RuntimeOrigin::signed(full_admin.clone()),
			get_native_id(),
			get_pusd_id(),
			pallet_vaults::BranchConfigUpdate::MinimumDebt(100 * PUSD),
		));
		// The full admin controls only the branch ceiling.
		assert_ok!(Vaults::set_param(
			RuntimeOrigin::signed(full_admin.clone()),
			get_native_id(),
			get_pusd_id(),
			pallet_vaults::BranchConfigUpdate::DebtCeiling(50_000_000 * PUSD),
		));

		// The treasury takes all asset roles and ownership. Only Root can reassign them
		// through `force_asset_status`.
		let custodian = governance::TreasuryAccount::get();
		assert_ok!(Assets::set_team(
			RuntimeOrigin::signed(creator.clone()),
			get_pusd_id().into(),
			MultiAddress::Id(custodian.clone()),
			MultiAddress::Id(custodian.clone()),
			MultiAddress::Id(custodian.clone()),
		));
		assert_ok!(Assets::transfer_ownership(
			RuntimeOrigin::signed(creator.clone()),
			get_pusd_id().into(),
			MultiAddress::Id(custodian.clone()),
		));
		assert_eq!(<Assets as RolesInspect<AccountId>>::owner(get_pusd_id()), Some(custodian));
		// The former owner cannot freeze the asset or register another market.
		assert_noop!(
			Assets::freeze_asset(RuntimeOrigin::signed(creator.clone()), get_pusd_id().into()),
			pallet_assets::Error::<Runtime, TrustBackedAssetsInstance>::NoPermission,
		);
		assert_noop!(
			Vaults::create_branch(
				RuntimeOrigin::signed(creator.clone()),
				get_native_id(),
				get_pusd_id(),
				branch_admins(),
				branch_config(&get_native_id(), &BranchSpec::default()),
				registration_config(),
			),
			sp_runtime::DispatchError::BadOrigin,
		);

		// The stablecoin-wide ceiling uses governance `ForceOrigin`. Root lifts it before
		// users can borrow.
		lift_global_ceiling(1_000_000_000 * PUSD);
		let owner = acct(9);
		// 10,000 WND at 2 pUSD against 10,000 pUSD debt: CR 200%.
		open_vault(&owner, 10_000 * WND, 10_000 * PUSD, FixedU128::zero());
		assert_eq!(pusd_balance(&owner), 10_000 * PUSD);
		assert_eq!(collateral_on_hold(&get_native_id(), &owner), 10_000 * WND);

		// A second borrower: 3,000 WND = 6,000 pUSD value against 2,500 pUSD debt at 5%, CR 240%.
		let other_owner = acct(10);
		open_vault(&other_owner, 3_000 * WND, 2_500 * PUSD, FixedU128::from_rational(5, 100));
		assert_eq!(pusd_balance(&other_owner), 2_500 * PUSD);
		assert_eq!(collateral_on_hold(&get_native_id(), &other_owner), 3_000 * WND);
		// Each vault keeps its own rate.
		assert_eq!(vault(&owner).annual_rate, FixedU128::zero());
		assert_eq!(vault(&other_owner).annual_rate, FixedU128::from_rational(5, 100));

		// The market aggregates both.
		let state = pallet_vaults::Branches::<Runtime>::get(get_native_id(), get_pusd_id())
			.expect("market still registered")
			.state;
		assert_eq!(state.vault_count, 2);
		assert_eq!(state.total_collateral, 13_000 * WND);
		assert_eq!(state.debt.principal, 12_500 * PUSD);
	});
}

/// The branch TCR is 125% and the safety threshold 120%. A withdrawal that moves
/// the TCR to 115% is rejected although the vault stays healthy. A debt
/// repayment improves the TCR and goes through.
#[test]
fn branch_safety_ratio_gates_withdrawals_not_repayments() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		// Lower vault ratios leave the vault healthy, so the branch TCR check is
		// what rejects the withdrawal.
		create_branch(&BranchSpec {
			mcr: FixedU128::from_rational(105, 100),
			icr: FixedU128::from_rational(110, 100),
			scr: FixedU128::from_rational(120, 100),
			..Default::default()
		});

		// Branch totals: 62,500 WND = 125,000 pUSD value over 100,000 pUSD debt, TCR 125%.
		let roomy_owner = acct(1); // 34,750 WND = 69,500 pUSD value, CR 139%
		open_vault(&roomy_owner, 34_750 * WND, 50_000 * PUSD, FixedU128::zero());
		let tight_owner = acct(2); // 27,750 WND = 55,500 pUSD value, CR 111%
		open_vault(&tight_owner, 27_750 * WND, 50_000 * PUSD, FixedU128::zero());

		// A 5,000 WND = 10,000 pUSD withdrawal leaves the vault at 119%, but the
		// branch at 115,000 / 100,000 = 115% < 120%.
		assert_err!(
			Vaults::withdraw_collateral(
				RuntimeOrigin::signed(roomy_owner.clone()),
				get_native_id(),
				get_pusd_id(),
				5_000 * WND,
				None,
			),
			pallet_vaults::Error::<Runtime>::WouldEnterSafetyMode,
		);

		// Repaying 10,000 pUSD improves TCR to 125,000 / 90,000 = 138.89%.
		assert_ok!(Vaults::repay_for(
			RuntimeOrigin::signed(roomy_owner.clone()),
			get_native_id(),
			get_pusd_id(),
			roomy_owner.clone(),
			Some(10_000 * PUSD),
		));
		// 125,000 / 90,000 = 1.38888…, floored at 18 decimals. `from_rational` would round up.
		assert_eq!(
			Vaults::branch_tcr(get_native_id(), get_pusd_id()),
			Ok(FixedU128::from_inner(1_388_888_888_888_888_888)),
		);
	});
}

/// The upfront fee is 7 days of interest on the newly drawn amount. It goes to
/// `debt.interest`, not `debt.principal`.
///
/// The year is 365.25 days (`MILLIS_PER_YEAR = 31,557,600,000`) and the fee
/// rounds up:
///   open fee = ceil(5,000e6 × 4% × 7 / 365.25) = ceil(3,832,991.10) = 3,832,992
///   draw fee = ceil(2,000e6 × 4% × 7 / 365.25) = ceil(1,533,196.44) = 1,533,197
#[test]
fn upfront_fee_is_charged_on_open_and_on_each_draw() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec {
			upfront_fee_period_ms: 7 * 24 * 60 * 60 * 1_000,
			..Default::default()
		});

		// 10,000 WND = 20,000 pUSD value, 5,000 pUSD drawn at 4%.
		let borrower = acct(1);
		open_vault(&borrower, 10_000 * WND, 5_000 * PUSD, FixedU128::from_rational(4, 100));

		// The borrower receives the full draw. The fee is added to the debt.
		assert_eq!(pusd_balance(&borrower), 5_000 * PUSD);
		let opened = vault(&borrower);
		assert_eq!(opened.debt.principal, 5_000 * PUSD);
		assert_eq!(opened.debt.interest, 3_832_992);

		// A further 2,000 pUSD draw is charged only on the increase.
		assert_ok!(Vaults::borrow(
			RuntimeOrigin::signed(borrower.clone()),
			get_native_id(),
			get_pusd_id(),
			2_000 * PUSD,
			None,
			None,
			pallet_linked_list::Position::endpoints_only(),
		));

		assert_eq!(pusd_balance(&borrower), 7_000 * PUSD);
		let increased = vault(&borrower);
		assert_eq!(increased.debt.principal, 7_000 * PUSD);
		assert_eq!(increased.debt.interest, 3_832_992 + 1_533_197);
	});
}
