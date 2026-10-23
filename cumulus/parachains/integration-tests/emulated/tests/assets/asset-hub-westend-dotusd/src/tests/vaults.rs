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
	dotusd_config::VaultsBranchCreationDeposit, governance, Assets, Balances, Redemptions,
	RuntimeHoldReason, Stability, TrustBackedAssetsInstance, Vaults,
};
use frame_support::{
	assert_noop, hypothetically_ok,
	traits::{
		fungible::InspectHold as FungibleInspectHold,
		fungibles::{roles::Inspect as RolesInspect, Refund},
	},
};
use pallet_vaults::types::BranchAdmins;
use sp_runtime::{DispatchError, TokenError};

/// Creates a native-collateral branch as the asset owner, then transfers control to governance.
///
/// A stablecoin minimum balance above one unit requires the fee account's
/// stablecoin account before the first branch registers. The fee account owns
/// that deposit, because it outlives any single branch.
#[test]
fn signed_user_registers_a_native_collateral_market() {
	AssetHubWestend::execute_with(|| {
		create_dotusd();
		feed_price(dot_price(2, 1));

		// Asset ownership authorizes branch creation and redemption configuration.
		// The vault system uses fungibles traits to mint and burn.
		let creator = admin();
		// The administrators take no part in the creation.
		let full_admin = acct(7);
		let emergency_admin = acct(8);
		assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(&creator, 1_000 * WND));
		mint_dotusd(&creator, 2 * DOTUSD_MIN_BALANCE);
		let creator_free_before = native_balance(&creator);
		let creator_dotusd_before = dotusd_balance(&creator);
		let fee_account = governance::TreasuryAccount::get();
		assert!(DOTUSD_MIN_BALANCE > 1, "a one-unit minimum would skip the touch entirely");
		assert!(
			<Assets as Refund<AccountId>>::deposit_held(DOTUSD_ID, fee_account.clone()).is_none()
		);

		assert_ok!(Vaults::create_branch(
			RuntimeOrigin::signed(creator.clone()),
			get_native_id(),
			DOTUSD_ID,
			pallet_vaults::types::BranchAdmins {
				full_admin: MultiAddress::Id(full_admin.clone()),
				emergency_admin: MultiAddress::Id(emergency_admin.clone()),
			},
			branch_config(&get_native_id(), &BranchSpec::default()),
			registration_config(),
		));

		let branch = pallet_vaults::Branches::<Runtime>::get(get_native_id(), DOTUSD_ID)
			.expect("the signed creation registered the branch");
		// The deposit is held from the creator, so a removal refunds the creator.
		assert_eq!(branch.deposit.map(|(who, _)| who), Some(creator.clone()));
		assert_eq!(
			<Balances as FungibleInspectHold<AccountId>>::balance_on_hold(
				&RuntimeHoldReason::Vaults(pallet_vaults::HoldReason::BranchCreationDeposit),
				&creator,
			),
			VaultsBranchCreationDeposit::get(),
		);
		let custody = Vaults::redistribution_account(&get_native_id(), &DOTUSD_ID);
		assert_eq!(native_balance(&custody), get_native_ed());
		// The creator also parked the pool's custody float: one ED and one dotUSD minimum.
		assert_eq!(native_balance(&pool_account()), get_native_ed());
		assert_eq!(dotusd_balance(&pool_account()), DOTUSD_MIN_BALANCE);
		assert_eq!(
			creator_free_before - native_balance(&creator),
			VaultsBranchCreationDeposit::get() + 2 * get_native_ed(),
		);
		assert_eq!(creator_dotusd_before - dotusd_balance(&creator), DOTUSD_MIN_BALANCE);
		let (depositor, deposit) =
			<Assets as Refund<AccountId>>::deposit_held(DOTUSD_ID, fee_account.clone())
				.expect("registration touched the fee account");
		assert_eq!(depositor, fee_account, "the stablecoin-wide account owns its deposit");
		assert!(deposit > 0);
		// The account exists but is empty. It can now receive a credit of any size.
		assert_eq!(dotusd_balance(&fee_account), 0);

		// The full admin takes all asset roles and ownership. Only Root can reassign
		// them through `force_asset_status`.
		let custodian = full_admin.clone();
		assert_ok!(Assets::set_team(
			RuntimeOrigin::signed(creator.clone()),
			DOTUSD_ID.into(),
			MultiAddress::Id(custodian.clone()),
			MultiAddress::Id(custodian.clone()),
			MultiAddress::Id(custodian.clone()),
		));
		assert_ok!(Assets::transfer_ownership(
			RuntimeOrigin::signed(creator.clone()),
			DOTUSD_ID.into(),
			MultiAddress::Id(custodian.clone()),
		));
		assert_eq!(<Assets as RolesInspect<AccountId>>::owner(DOTUSD_ID), Some(custodian));
		// The former owner cannot register another branch.
		assert_noop!(
			Vaults::create_branch(
				RuntimeOrigin::signed(creator.clone()),
				get_native_id(),
				DOTUSD_ID,
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
				DOTUSD_ID,
				SCENARIO_GLOBAL_CEILING,
			),
			sp_runtime::DispatchError::BadOrigin,
		);
		assert_ok!(Vaults::set_global_debt_ceiling(
			RuntimeOrigin::signed(full_admin.clone()),
			DOTUSD_ID,
			SCENARIO_GLOBAL_CEILING,
		));
		let owner = acct(9);
		// 10,000 WND at 2 dotUSD against 10,000 dotUSD debt: CR 200%.
		open_vault(&owner, 10_000 * WND, 10_000 * DOTUSD, FixedU128::zero());
		assert_eq!(dotusd_balance(&owner), 10_000 * DOTUSD);
		assert_eq!(collateral_on_hold(&get_native_id(), &owner), 10_000 * WND);
	});
}

/// The runtime routes each update origin: branch freezes take Root through
/// `ForceOrigin`, stablecoin-wide redemption settings take the asset owner, and
/// the pool configuration takes the branch full admin or Root but not the
/// emergency admin.
#[test]
fn runtime_update_origins_route_to_owner_admin_or_root() {
	AssetHubWestend::execute_with(|| {
		feed_price(dot_price(2, 1));
		create_branch(&BranchSpec::default());
		let full_admin = acct(7);
		let emergency_admin = acct(8);
		assert_ok!(Vaults::set_branch_admins(
			RuntimeOrigin::signed(admin()),
			get_native_id(),
			DOTUSD_ID,
			BranchAdmins {
				full_admin: MultiAddress::Id(full_admin.clone()),
				emergency_admin: MultiAddress::Id(emergency_admin.clone()),
			},
		));
		// Root holds no admin role, so only `ForceOrigin` can admit it.
		hypothetically_ok!(Vaults::set_governance_frozen(
			RuntimeOrigin::root(),
			get_native_id(),
			DOTUSD_ID,
			true,
		));

		// The redemption policy takes the stablecoin owner or Root.
		let redemption_config = pallet_redemptions::Stablecoins::<Runtime>::get(DOTUSD_ID)
			.expect("registration stored the redemption policy")
			.config;
		assert_noop!(
			Redemptions::set_redemption_config(
				RuntimeOrigin::signed(full_admin.clone()),
				DOTUSD_ID,
				redemption_config.clone(),
			),
			DispatchError::BadOrigin,
		);
		hypothetically_ok!(Redemptions::set_redemption_config(
			RuntimeOrigin::signed(admin()),
			DOTUSD_ID,
			redemption_config,
		));
		// The pool configuration takes the branch full admin or Root.
		let pool_config = pallet_stability::Pools::<Runtime>::get(get_native_id(), DOTUSD_ID)
			.expect("stability pool registered")
			.config;
		let set_pool_config = |origin: RuntimeOrigin| {
			Stability::set_stability_pool_config(
				origin,
				get_native_id(),
				DOTUSD_ID,
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
		open_vault(&owner, 10_000 * WND, 5_000 * DOTUSD, FixedU128::zero());
		let depositor = acct(2);
		mint_dotusd(&depositor, 1_000 * DOTUSD);

		assert_ok!(Assets::freeze_asset(RuntimeOrigin::signed(admin()), DOTUSD_ID.into()));
		let borrow = || {
			Vaults::borrow(
				RuntimeOrigin::signed(owner.clone()),
				get_native_id(),
				DOTUSD_ID,
				1_000 * DOTUSD,
				None,
				None,
				pallet_linked_list::Position::endpoints_only(),
			)
		};
		let repay = || {
			Vaults::repay_for(
				RuntimeOrigin::signed(owner.clone()),
				get_native_id(),
				DOTUSD_ID,
				owner.clone(),
				Some(1_000 * DOTUSD),
			)
		};
		let deposit = || {
			Stability::deposit(
				RuntimeOrigin::signed(depositor.clone()),
				get_native_id(),
				DOTUSD_ID,
				1_000 * DOTUSD,
			)
		};
		let not_live = || pallet_assets::Error::<Runtime, TrustBackedAssetsInstance>::AssetNotLive;
		assert_noop!(borrow(), not_live());
		// A frozen asset reports no reducible balance, so the repayment is short
		// of funds before the burn ever reaches the status check.
		assert_noop!(repay(), TokenError::FundsUnavailable);
		assert_noop!(deposit(), not_live());

		assert_ok!(Assets::thaw_asset(RuntimeOrigin::signed(admin()), DOTUSD_ID.into()));
		hypothetically_ok!(borrow());
		hypothetically_ok!(repay());
		assert_ok!(deposit());
	});
}
