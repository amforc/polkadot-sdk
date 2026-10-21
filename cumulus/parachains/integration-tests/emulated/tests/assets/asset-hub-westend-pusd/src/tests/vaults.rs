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
	governance, pusd_config::VaultsBranchCreationDeposit, Assets, Balances, Redemptions,
	RuntimeHoldReason, Stability, TrustBackedAssetsInstance, Vaults,
};
use frame_support::{
	assert_noop, hypothetically_ok,
	traits::{
		fungible::InspectHold as FungibleInspectHold,
		fungibles::{roles::Inspect as RolesInspect, Refund},
	},
};
use pallet_vaults::{types::BranchAdmins, BranchConfigUpdate};
use sp_runtime::{DispatchError, TokenError};

/// Opens a 10,000 WND / 10,000 pUSD vault for `owner` without any helper funding.
fn open_native_vault(owner: &AccountId) -> sp_runtime::DispatchResult {
	Vaults::open_vault(
		RuntimeOrigin::signed(owner.clone()),
		get_native_id(),
		PUSD_ID,
		10_000 * WND,
		10_000 * PUSD,
		FixedU128::zero(),
		pallet_linked_list::Position::endpoints_only(),
	)
	.map(|_| ())
	.map_err(|error| error.error)
}

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
		assert!(<Assets as Refund<AccountId>>::deposit_held(PUSD_ID, fee_account.clone()).is_none());

		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());

		let (depositor, deposit) =
			<Assets as Refund<AccountId>>::deposit_held(PUSD_ID, fee_account.clone())
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
		mint_pusd(&creator, 2 * PUSD_MIN_BALANCE);
		let creator_free_before = native_balance(&creator);
		let creator_pusd_before = pusd_balance(&creator);

		assert_ok!(Vaults::create_branch(
			RuntimeOrigin::signed(creator.clone()),
			get_native_id(),
			PUSD_ID,
			pallet_vaults::types::BranchAdmins {
				full_admin: MultiAddress::Id(full_admin.clone()),
				emergency_admin: MultiAddress::Id(emergency_admin.clone()),
			},
			branch_config(&get_native_id(), &BranchSpec::default()),
			registration_config(),
		));

		let branch = pallet_vaults::Branches::<Runtime>::get(get_native_id(), PUSD_ID)
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
		let custody = Vaults::redistribution_account(&get_native_id(), &PUSD_ID);
		assert_eq!(native_balance(&custody), get_native_ed());
		// The creator also parked the pool's custody float: one ED and one pUSD minimum.
		assert_eq!(native_balance(&pool_account()), get_native_ed());
		assert_eq!(pusd_balance(&pool_account()), PUSD_MIN_BALANCE);
		assert_eq!(
			creator_free_before - native_balance(&creator),
			VaultsBranchCreationDeposit::get() + 2 * get_native_ed(),
		);
		assert_eq!(creator_pusd_before - pusd_balance(&creator), PUSD_MIN_BALANCE);

		assert_ok!(Vaults::set_param(
			RuntimeOrigin::signed(full_admin.clone()),
			get_native_id(),
			PUSD_ID,
			pallet_vaults::BranchConfigUpdate::MinimumDebt(100 * PUSD),
		));
		// The full admin controls every market parameter. Lowering the branch
		// ceiling reduces risk, so the emergency admin may do it too. Raising it
		// does not, so only the full admin can.
		assert_ok!(Vaults::set_param(
			RuntimeOrigin::signed(emergency_admin.clone()),
			get_native_id(),
			PUSD_ID,
			pallet_vaults::BranchConfigUpdate::DebtCeiling(50_000_000 * PUSD),
		));
		assert_noop!(
			Vaults::set_param(
				RuntimeOrigin::signed(emergency_admin.clone()),
				get_native_id(),
				PUSD_ID,
				pallet_vaults::BranchConfigUpdate::DebtCeiling(200_000_000 * PUSD),
			),
			pallet_vaults::Error::<Runtime>::DefensiveActionNotDefensive,
		);
		assert_ok!(Vaults::set_param(
			RuntimeOrigin::signed(full_admin.clone()),
			get_native_id(),
			PUSD_ID,
			pallet_vaults::BranchConfigUpdate::DebtCeiling(200_000_000 * PUSD),
		));

		// The full admin takes all asset roles and ownership. Only Root can reassign
		// them through `force_asset_status`.
		let custodian = full_admin.clone();
		assert_ok!(Assets::set_team(
			RuntimeOrigin::signed(creator.clone()),
			PUSD_ID.into(),
			MultiAddress::Id(custodian.clone()),
			MultiAddress::Id(custodian.clone()),
			MultiAddress::Id(custodian.clone()),
		));
		assert_ok!(Assets::transfer_ownership(
			RuntimeOrigin::signed(creator.clone()),
			PUSD_ID.into(),
			MultiAddress::Id(custodian.clone()),
		));
		assert_eq!(<Assets as RolesInspect<AccountId>>::owner(PUSD_ID), Some(custodian));
		// The former owner cannot freeze the asset or register another market.
		assert_noop!(
			Assets::freeze_asset(RuntimeOrigin::signed(creator.clone()), PUSD_ID.into()),
			pallet_assets::Error::<Runtime, TrustBackedAssetsInstance>::NoPermission,
		);
		assert_noop!(
			Vaults::create_branch(
				RuntimeOrigin::signed(creator.clone()),
				get_native_id(),
				PUSD_ID,
				branch_admins(),
				branch_config(&get_native_id(), &BranchSpec::default()),
				registration_config(),
			),
			sp_runtime::DispatchError::BadOrigin,
		);

		// The stablecoin-wide ceiling belongs to the stablecoin owner, now the `custodian`,
		// through `CreateOrigin`.
		assert_noop!(
			Vaults::set_global_debt_ceiling(
				RuntimeOrigin::signed(creator.clone()),
				PUSD_ID,
				SCENARIO_GLOBAL_CEILING,
			),
			sp_runtime::DispatchError::BadOrigin,
		);
		assert_ok!(Vaults::set_global_debt_ceiling(
			RuntimeOrigin::signed(full_admin.clone()),
			PUSD_ID,
			SCENARIO_GLOBAL_CEILING,
		));
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
		let state = pallet_vaults::Branches::<Runtime>::get(get_native_id(), PUSD_ID)
			.expect("market still registered")
			.state;
		assert_eq!(state.vault_count, 2);
		assert_eq!(state.total_collateral, 13_000 * WND);
		assert_eq!(state.debt.principal, 12_500 * PUSD);
	});
}

/// The emergency admin can pull the freeze but not clear it, a stranger can
/// change nothing, and Root can do everything. A market with a live vault
/// cannot be removed. Stablecoin-wide settings belong to the asset owner, not
/// to branch admins.
#[test]
fn branch_admin_levels_gate_freeze_config_and_removal() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());
		let full_admin = acct(7);
		let emergency_admin = acct(8);
		assert_ok!(Vaults::set_branch_admins(
			RuntimeOrigin::signed(admin()),
			get_native_id(),
			PUSD_ID,
			BranchAdmins {
				full_admin: MultiAddress::Id(full_admin.clone()),
				emergency_admin: MultiAddress::Id(emergency_admin.clone()),
			},
		));
		open_vault(&acct(1), 10_000 * WND, 10_000 * PUSD, FixedU128::zero());

		let stranger = acct(9);
		assert_noop!(
			Vaults::set_param(
				RuntimeOrigin::signed(stranger.clone()),
				get_native_id(),
				PUSD_ID,
				BranchConfigUpdate::MinimumDebt(100 * PUSD),
			),
			pallet_vaults::Error::<Runtime>::NotBranchAdmin,
		);
		assert_noop!(
			Vaults::set_branch_admins(
				RuntimeOrigin::signed(stranger.clone()),
				get_native_id(),
				PUSD_ID,
				branch_admins(),
			),
			pallet_vaults::Error::<Runtime>::NotBranchAdmin,
		);
		assert_noop!(
			Vaults::remove_branch(
				RuntimeOrigin::signed(full_admin.clone()),
				get_native_id(),
				PUSD_ID
			),
			pallet_vaults::Error::<Runtime>::BranchNotEmpty,
		);

		let set_frozen = |who: &AccountId, frozen: bool| {
			Vaults::set_governance_frozen(
				RuntimeOrigin::signed(who.clone()),
				get_native_id(),
				PUSD_ID,
				frozen,
			)
		};
		assert_noop!(set_frozen(&stranger, true), pallet_vaults::Error::<Runtime>::NotBranchAdmin);
		assert_ok!(set_frozen(&emergency_admin, true));
		// The freeze bites: the same open is refused frozen and accepted thawed.
		let late_owner = acct(2);
		fund_dot(&late_owner, 10_000 * WND);
		fund_vault_deposit(&get_native_id(), &late_owner);
		assert_noop!(open_native_vault(&late_owner), pallet_vaults::Error::<Runtime>::BranchFrozen);
		assert_noop!(
			set_frozen(&emergency_admin, false),
			pallet_vaults::Error::<Runtime>::NotBranchAdmin
		);
		hypothetically_ok!(Vaults::set_governance_frozen(
			RuntimeOrigin::root(),
			get_native_id(),
			PUSD_ID,
			false,
		));
		assert_ok!(set_frozen(&full_admin, false));
		assert_ok!(open_native_vault(&late_owner));

		// The redemption policy takes the stablecoin owner or Root.
		let redemption_config = pallet_redemptions::RedemptionConfigs::<Runtime>::get(PUSD_ID)
			.expect("registration stored the redemption policy");
		assert_noop!(
			Redemptions::set_redemption_config(
				RuntimeOrigin::signed(full_admin.clone()),
				PUSD_ID,
				redemption_config.clone(),
			),
			DispatchError::BadOrigin,
		);
		hypothetically_ok!(Redemptions::set_redemption_config(
			RuntimeOrigin::signed(admin()),
			PUSD_ID,
			redemption_config,
		));
		// The pool configuration takes the branch full admin or Root.
		let pool_config = pallet_stability::Pools::<Runtime>::get(get_native_id(), PUSD_ID)
			.expect("stability pool registered")
			.config;
		let set_pool_config = |origin: RuntimeOrigin| {
			Stability::set_stability_pool_config(
				origin,
				get_native_id(),
				PUSD_ID,
				pool_config.clone(),
			)
		};
		assert_noop!(
			set_pool_config(RuntimeOrigin::signed(emergency_admin.clone())),
			DispatchError::BadOrigin
		);
		hypothetically_ok!(set_pool_config(RuntimeOrigin::root()));
		assert_ok!(set_pool_config(RuntimeOrigin::signed(full_admin.clone())));
	});
}

/// A frozen stablecoin asset stops every mint and burn: borrowing, repaying,
/// and pool deposits all fail, and thawing restores them.
#[test]
fn frozen_stablecoin_blocks_borrow_repay_and_pool_deposits() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());
		let owner = acct(1);
		open_vault(&owner, 10_000 * WND, 5_000 * PUSD, FixedU128::zero());
		let depositor = acct(2);
		mint_pusd(&depositor, 1_000 * PUSD);

		assert_ok!(Assets::freeze_asset(RuntimeOrigin::signed(admin()), PUSD_ID.into()));
		let borrow = || {
			Vaults::borrow(
				RuntimeOrigin::signed(owner.clone()),
				get_native_id(),
				PUSD_ID,
				1_000 * PUSD,
				None,
				None,
				pallet_linked_list::Position::endpoints_only(),
			)
		};
		let repay = || {
			Vaults::repay_for(
				RuntimeOrigin::signed(owner.clone()),
				get_native_id(),
				PUSD_ID,
				owner.clone(),
				Some(1_000 * PUSD),
			)
		};
		let deposit = || {
			Stability::deposit(
				RuntimeOrigin::signed(depositor.clone()),
				get_native_id(),
				PUSD_ID,
				1_000 * PUSD,
			)
		};
		let not_live = || pallet_assets::Error::<Runtime, TrustBackedAssetsInstance>::AssetNotLive;
		assert_noop!(borrow(), not_live());
		// A frozen asset reports no reducible balance, so the repayment is short
		// of funds before the burn ever reaches the status check.
		assert_noop!(repay(), TokenError::FundsUnavailable);
		assert_noop!(deposit(), not_live());

		assert_ok!(Assets::thaw_asset(RuntimeOrigin::signed(admin()), PUSD_ID.into()));
		hypothetically_ok!(borrow());
		hypothetically_ok!(repay());
		assert_ok!(deposit());
	});
}
