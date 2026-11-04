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

//! Shared setup for a six-decimal stablecoin, runtime asset branches, and a Root-fed oracle.

use crate::imports::*;
use asset_hub_westend_runtime::{
	dotusd_config::{
		StabilityCollateral, StableInsuranceAccount, VaultsCollateral, VaultsDepositPolicy,
		VaultsNativeCollateralId,
	},
	governance, Assets, AuraExt, Balances, MockOracle, Redemptions, Runtime, RuntimeHoldReason,
	Stability, Timestamp, Vaults,
};
use frame_support::{assert_noop, assert_storage_noop};
use pallet_redemptions::RedemptionTerms;
use pallet_vaults::JitTerms;
use stablecoin_primitives::VaultStatus;

pub(crate) const WND: Balance = 1_000_000_000_000;
/// The stablecoin's metadata decimals.
pub(crate) const DOTUSD_DECIMALS: u8 = 6;
/// One whole stablecoin, at [`DOTUSD_DECIMALS`].
pub(crate) const DOTUSD: Balance = 10u128.pow(DOTUSD_DECIMALS as u32);
/// 0.01 dotUSD. The stablecoin registers with this minimum balance.
pub(crate) const DOTUSD_MIN_BALANCE: Balance = DOTUSD / 100;

/// Trust-backed asset id for the stablecoin: 7873, the id dotUSD carries on
/// Polkadot Asset Hub, where it is live. The emulated tests use the same id.
pub(crate) const DOTUSD_ID: u32 = 7873;

/// Stablecoin-wide supply cap that exceeds all scenario debt.
pub(crate) const SCENARIO_GLOBAL_CEILING: Balance = 1_000_000_000 * DOTUSD;

/// The Insurance Fund account of dotUSD, derived as the runtime derives it. A
/// change to that mapping then fails here instead of funding a wrong account.
pub(crate) fn insurance_account() -> AccountId {
	<StableInsuranceAccount as sp_runtime::traits::Convert<u32, AccountId>>::convert(DOTUSD_ID)
}

pub(crate) fn get_native_id() -> VaultsCollateralId {
	VaultsNativeCollateralId::get()
}

pub(crate) fn get_native_ed() -> Balance {
	<Balances as FungibleInspect<AccountId>>::minimum_balance()
}

pub(crate) fn acct(seed: u8) -> AccountId {
	AccountId::from([seed; 32])
}

pub(crate) fn admin() -> AccountId {
	acct(0xAD)
}

/// Returns a dotUSD/WND price as dotUSD base units per WND planck.
pub(crate) fn dot_price(dotusd_num: u128, dotusd_den: u128) -> FixedU128 {
	FixedU128::from_rational(dotusd_num * DOTUSD, dotusd_den * WND)
}

pub(crate) fn feed_price(price: FixedU128) {
	feed_price_for(get_native_id(), price);
}

pub(crate) fn feed_price_for(collateral_id: VaultsCollateralId, price: FixedU128) {
	advance_time(0);
	assert_ok!(MockOracle::set_price(RuntimeOrigin::root(), collateral_id, price));
}

/// Settings that vary between scenarios. [`branch_config`] supplies the other settings.
pub(crate) struct BranchSpec {
	pub mcr: FixedU128,
	pub icr: FixedU128,
	pub scr: FixedU128,
	pub minimum_debt: Balance,
	pub upfront_fee_period_ms: u64,
	pub keeper_flat_compensation_value: Balance,
	pub keeper_percent_compensation: Permill,
}

impl Default for BranchSpec {
	fn default() -> Self {
		Self {
			mcr: FixedU128::from_rational(110, 100),
			icr: FixedU128::from_rational(120, 100),
			scr: FixedU128::from_rational(120, 100),
			minimum_debt: 50 * DOTUSD,
			// Zero disables the upfront fee.
			upfront_fee_period_ms: 0,
			keeper_flat_compensation_value: 2 * DOTUSD,
			keeper_percent_compensation: Permill::from_rational(1u32, 1_000u32),
		}
	}
}

/// Sets MCR above the scenario vaults' 120% CR, and disables keeper compensation
/// to keep amounts round.
pub(crate) fn accounting_spec() -> BranchSpec {
	BranchSpec {
		mcr: FixedU128::from_rational(125, 100),
		icr: FixedU128::from_rational(130, 100),
		scr: FixedU128::from_rational(130, 100),
		keeper_flat_compensation_value: 0,
		keeper_percent_compensation: Permill::zero(),
		..Default::default()
	}
}

/// Creates the stablecoin and a native-WND branch for it, and lifts the global
/// debt ceiling.
pub(crate) fn create_branch(spec: &BranchSpec) {
	create_dotusd();
	// Root creation charges the full admin two ED under `Preserve` (the Vaults
	// redistribution seed and the pool custody float), so the admin needs three
	// ED, plus two stablecoin minimums for the pool's stablecoin float. `admin()`
	// has no genesis balance.
	assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(
		&admin(),
		get_native_ed().saturating_mul(3),
	));
	mint_dotusd(&admin(), DOTUSD_MIN_BALANCE.saturating_mul(2));
	assert_ok!(Vaults::create_branch(
		RuntimeOrigin::root(),
		get_native_id(),
		DOTUSD_ID,
		branch_admins(),
		branch_config(&get_native_id(), spec),
		registration_config(),
	));
	lift_global_ceiling(SCENARIO_GLOBAL_CEILING);
}

/// Creates the stablecoin asset, owned by [`admin`]. It is sufficient, so holders
/// need no native provider reference, and it has a real minimum balance.
///
/// With a minimum balance above one unit, the first branch registration must
/// create the fee account's stablecoin account. That account owns its deposit,
/// so it needs native balance. A live treasury holds some. The emulated genesis
/// does not, so the helper funds it.
pub(crate) fn create_dotusd() {
	advance_time(0);
	assert_ok!(Assets::force_create(
		RuntimeOrigin::root(),
		DOTUSD_ID.into(),
		MultiAddress::Id(admin()),
		true,
		DOTUSD_MIN_BALANCE,
	));
	assert_ok!(Assets::force_set_metadata(
		RuntimeOrigin::root(),
		DOTUSD_ID.into(),
		b"dotUSD".to_vec(),
		b"dotUSD".to_vec(),
		DOTUSD_DECIMALS,
		false,
	));
	assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(
		&governance::TreasuryAccount::get(),
		WND,
	));
}

/// Creates a branch from the stablecoin owner instead of Root, and lifts the
/// global debt ceiling. The creator then pays every refundable registration
/// cost, not the full admin.
pub(crate) fn create_market_signed(collateral_id: VaultsCollateralId, spec: &BranchSpec) {
	// The creator pays the creation deposit and both custody seeds: collateral
	// for the redistribution account and the pool, stablecoin for the pool.
	assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(&admin(), 1_000 * WND));
	fund_collateral(
		&collateral_id,
		&admin(),
		collateral_min_balance(&collateral_id).saturating_mul(2),
	);
	mint_dotusd(&admin(), DOTUSD_MIN_BALANCE.saturating_mul(2));
	assert_ok!(Vaults::create_branch(
		RuntimeOrigin::signed(admin()),
		collateral_id.clone(),
		DOTUSD_ID,
		branch_admins(),
		branch_config(&collateral_id, spec),
		registration_config(),
	));
	lift_global_ceiling(SCENARIO_GLOBAL_CEILING);
}

/// Registration payload `(redemptions, stability)` for a dotUSD branch. The
/// redemption policy is stablecoin-wide, so only the first branch carries it.
pub(crate) fn registration_config() -> (
	Option<pallet_redemptions::RedemptionConfig<Balance>>,
	pallet_stability::types::StabilityPoolConfig<Balance>,
) {
	let redemption_config = (!pallet_redemptions::Stablecoins::<Runtime>::contains_key(DOTUSD_ID))
		.then(test_redemption_config);
	(redemption_config, test_stability_config())
}

fn test_redemption_config() -> pallet_redemptions::RedemptionConfig<Balance> {
	pallet_redemptions::RedemptionConfig {
		minimum_redemption_amount: 100 * DOTUSD,
		dynamic_fee_decay_period: 6 * 60 * 60 * 1_000,
		dynamic_fee_floor: FixedU128::from_rational(0u128, 1u128),
		dynamic_fee_ceiling: FixedU128::from_rational(1u128, 1u128),
		base_fee: Permill::from_rational(5u32, 1_000u32),
		fee_ceiling: Permill::one(),
		dynamic_fee_increase_divisor: FixedU128::from_rational(2u128, 1u128),
		final_recovery_bonus_buffer: Permill::from_percent(1),
	}
}

fn test_stability_config() -> pallet_stability::types::StabilityPoolConfig<Balance> {
	pallet_stability::types::StabilityPoolConfig {
		minimum_deposit: 100 * DOTUSD,
		minimum_active_pool_balance: 100 * DOTUSD,
		entry_delay: u64::from(RELAY_CHAIN_SLOT_DURATION_MILLIS),
		safety_withdrawal_delay: 10 * 60 * 1_000,
		precision: pallet_stability::types::PoolPrecision {
			p_min: FixedU128::from_inner(1_000_000_000),
			scale_factor: 1_000_000_000,
		},
		yield_share: Permill::from_percent(75),
	}
}

/// One call covers every branch of the coin, because the ceiling is
/// stablecoin-wide. `amount` is in stablecoin units.
pub(crate) fn lift_global_ceiling(amount: Balance) {
	assert_ok!(Vaults::set_global_debt_ceiling(RuntimeOrigin::root(), DOTUSD_ID, amount));
}

pub(crate) fn branch_admins() -> pallet_vaults::types::BranchAdmins<MultiAddress<AccountId, ()>> {
	pallet_vaults::types::BranchAdmins {
		full_admin: MultiAddress::Id(admin()),
		emergency_admin: MultiAddress::Id(admin()),
	}
}

/// The collateral floor comes from the collateral's own minimum balance, so one
/// spec suits every collateral.
pub(crate) fn branch_config(
	collateral_id: &VaultsCollateralId,
	spec: &BranchSpec,
) -> pallet_vaults::BranchConfig<Balance> {
	pallet_vaults::BranchConfig {
		minimum_collateralization_ratio: spec.mcr,
		initial_collateralization_ratio: spec.icr,
		safety_collateralization_ratio: spec.scr,
		debt_ceiling: 100_000_000 * DOTUSD,
		minimum_debt: spec.minimum_debt,
		minimum_collateral: collateral_min_balance(collateral_id),
		minimum_borrow_rate: FixedU128::zero(),
		maximum_borrow_rate: FixedU128::from_rational(400, 100),
		upfront_fee_period: spec.upfront_fee_period_ms,
		rate_adjustment_cooldown: 0,
		final_recovery_reward_cooldown: 60 * 60 * 1_000,
		liquidation: pallet_vaults::LiquidationConfig {
			offset_penalty: Permill::from_percent(5),
			keeper_flat_compensation_value: spec.keeper_flat_compensation_value,
			keeper_percent_compensation: spec.keeper_percent_compensation,
			keeper_compensation_cap_value: 100 * DOTUSD,
			minimum_jit_contribution: DOTUSD,
			redistribution_penalty: Permill::from_percent(10),
		},
	}
}

/// Native collateral the owner can pledge, plus the ED that keeps the account
/// alive under the vault hold.
pub(crate) fn fund_dot(who: &AccountId, amount: Balance) {
	fund_collateral(&get_native_id(), who, amount);
}

/// Collateral the owner can pledge, plus the minimum-balance float a hold must
/// leave free.
pub(crate) fn fund_collateral(
	collateral_id: &VaultsCollateralId,
	who: &AccountId,
	amount: Balance,
) {
	// A non-sufficient asset account needs a provider reference. Native funding
	// supplies one.
	if *collateral_id != get_native_id() {
		if <Balances as FungibleInspect<AccountId>>::balance(who) < get_native_ed() {
			assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(who, get_native_ed()));
		}
	}
	let float = collateral_min_balance(collateral_id);
	assert_ok!(<StabilityCollateral as Mutate<AccountId>>::mint_into(
		collateral_id.clone(),
		who,
		amount.saturating_add(float),
	));
}

/// The asset and amount the runtime charges as storage deposit for a vault on `collateral_id`.
pub(crate) fn expected_vault_deposit(
	collateral_id: &VaultsCollateralId,
	owner: &AccountId,
) -> (VaultsCollateralId, Balance) {
	let priced;
	// Pricing a deposit is a quote and must not write.
	assert_storage_noop!(
		priced = <VaultsDepositPolicy as sp_runtime::traits::Convert<_, _>>::convert(
			Vaults::vault_footprint(collateral_id, &DOTUSD_ID, owner),
		)
	);
	priced.expect("vault deposit priced")
}

/// Mints the owner's vault deposit to keep vault amounts round.
pub(crate) fn fund_vault_deposit(collateral_id: &VaultsCollateralId, who: &AccountId) {
	let (asset, amount) = expected_vault_deposit(collateral_id, who);
	assert_ok!(<StabilityCollateral as Mutate<AccountId>>::mint_into(asset, who, amount));
}

pub(crate) fn vault_deposit_on_hold(asset: &VaultsCollateralId, who: &AccountId) -> Balance {
	<VaultsCollateral as InspectHold<AccountId>>::balance_on_hold(
		asset.clone(),
		&RuntimeHoldReason::Vaults(pallet_vaults::HoldReason::VaultCreationDeposit),
		who,
	)
}

pub(crate) fn collateral_min_balance(collateral_id: &VaultsCollateralId) -> Balance {
	<StabilityCollateral as Inspect<AccountId>>::minimum_balance(collateral_id.clone())
}

/// Collateral outside any vault hold, the multi-asset form of
/// [`native_balance`]. It includes the minimum-balance float, which a held
/// account cannot spend.
pub(crate) fn collateral_free(collateral_id: &VaultsCollateralId, who: &AccountId) -> Balance {
	<VaultsCollateral as Inspect<AccountId>>::balance(collateral_id.clone(), who)
}

pub(crate) fn collateral_on_hold(collateral_id: &VaultsCollateralId, who: &AccountId) -> Balance {
	<VaultsCollateral as InspectHold<AccountId>>::balance_on_hold(
		collateral_id.clone(),
		&RuntimeHoldReason::Vaults(pallet_vaults::HoldReason::VaultCollateral),
		who,
	)
}

pub(crate) fn mint_dotusd(who: &AccountId, amount: Balance) {
	assert_ok!(<Assets as Mutate<AccountId>>::mint_into(DOTUSD_ID, who, amount));
}

pub(crate) fn open_vault(owner: &AccountId, collateral: Balance, debt: Balance, rate: FixedU128) {
	open_vault_on(get_native_id(), owner, collateral, debt, rate);
}

pub(crate) fn open_vault_on(
	collateral_id: VaultsCollateralId,
	owner: &AccountId,
	collateral: Balance,
	debt: Balance,
	rate: FixedU128,
) {
	fund_collateral(&collateral_id, owner, collateral);
	fund_vault_deposit(&collateral_id, owner);
	assert_ok!(Vaults::open_vault(
		RuntimeOrigin::signed(owner.clone()),
		collateral_id,
		DOTUSD_ID,
		collateral,
		debt,
		rate,
		pallet_linked_list::Position::endpoints_only(),
	));
}

pub(crate) fn vault(owner: &AccountId) -> pallet_vaults::types::Vault<Balance> {
	vault_on(&get_native_id(), owner)
}

pub(crate) fn vault_on(
	collateral_id: &VaultsCollateralId,
	owner: &AccountId,
) -> pallet_vaults::types::Vault<Balance> {
	pallet_vaults::Vaults::<Runtime>::get((collateral_id, DOTUSD_ID, owner))
		.expect("vault exists for owner")
		.vault
}

/// `None` once the vault is closed or liquidated.
pub(crate) fn vault_status(owner: &AccountId) -> Option<VaultStatus> {
	vault_status_on(&get_native_id(), owner)
}

pub(crate) fn vault_status_on(
	collateral_id: &VaultsCollateralId,
	owner: &AccountId,
) -> Option<VaultStatus> {
	let view;
	// Reading a status must not write.
	assert_storage_noop!(
		view = Vaults::vault_after_touch(collateral_id.clone(), DOTUSD_ID, owner.clone())
	);
	match view {
		Ok((_, status)) => Some(status),
		Err(error) => {
			assert_eq!(error, pallet_vaults::Error::<Runtime>::VaultNotFound.into());
			None
		},
	}
}

/// Puts `owner`'s vault in the FinalRecovery FIFO through a throwaway keeper.
/// The keeper's reward, if any, has its own test.
pub(crate) fn enter_final_recovery(owner: &AccountId) {
	assert_ok!(Vaults::enter_final_recovery(
		RuntimeOrigin::signed(acct(0xFE)),
		get_native_id(),
		DOTUSD_ID,
		owner.clone(),
	));
	assert_eq!(vault_status(owner), Some(VaultStatus::FinalRecovery));
}

pub(crate) fn liquidate_on(collateral_id: VaultsCollateralId, owner: &AccountId) {
	let keeper = acct(0xEE);
	fund_collateral(&collateral_id, &keeper, 0);
	assert_ok!(Vaults::liquidate(
		RuntimeOrigin::signed(keeper),
		collateral_id,
		DOTUSD_ID,
		owner.clone(),
		JitTerms { max_stable: 0, min_collateral_out: 0 },
	));
}

/// Funds a fresh `redeemer` with `terms.max_stable_to_spend` plus the stablecoin
/// minimum balance, redeems on the native branch, and returns the collateral
/// received.
///
/// Every scenario redemption is budgeted to the unit, so the helper asserts
/// that only the minimum balance remains.
pub(crate) fn redeem(redeemer: &AccountId, terms: RedemptionTerms<Balance>) -> Balance {
	fund_dot(redeemer, 0);
	mint_dotusd(redeemer, terms.max_stable_to_spend + DOTUSD_MIN_BALANCE);
	let native_before = native_balance(redeemer);

	assert_ok!(Redemptions::redeem(
		RuntimeOrigin::signed(redeemer.clone()),
		get_native_id(),
		DOTUSD_ID,
		terms,
		redeemer.clone(),
		16,
	));

	assert_eq!(dotusd_balance(redeemer), DOTUSD_MIN_BALANCE);
	native_balance(redeemer) - native_before
}

pub(crate) fn dotusd_balance(who: &AccountId) -> Balance {
	<Assets as Inspect<AccountId>>::balance(DOTUSD_ID, who)
}

/// Branch-level accounting for the native-WND branch.
pub(crate) fn branch_state() -> pallet_vaults::types::BranchState<AccountId, Balance> {
	pallet_vaults::Branches::<Runtime>::get(get_native_id(), DOTUSD_ID)
		.expect("branch registered")
		.state
}

pub(crate) fn native_balance(who: &AccountId) -> Balance {
	<Balances as FungibleInspect<AccountId>>::balance(who)
}

/// Moves the emulated clock forward by at least `ms`, landing on the next Aura slot
/// boundary. Also raises the relay-derived clock.
pub(crate) fn advance_time(ms: u64) {
	let slot_duration = asset_hub_westend_runtime::Aura::slot_duration();
	let slot = (Timestamp::get() + ms).div_ceil(slot_duration);
	pallet_aura::CurrentSlot::<Runtime>::put(sp_consensus_slots::Slot::from(slot));
	Timestamp::set_timestamp(slot * slot_duration);
	set_relay_slot_at_least(slot * slot_duration / RELAY_CHAIN_SLOT_DURATION_MILLIS);
}

/// Raises the stored relay chain slot to `relay_slot` if it is behind. The consensus
/// hook rejects backwards relay slots, so the bump must stay monotone.
fn set_relay_slot_at_least(relay_slot: u64) {
	let stored = AuraExt::relay_slot_info().map_or(0, |(slot, _)| *slot);
	if relay_slot > stored {
		AuraExt::set_relay_slot_info(sp_consensus_slots::Slot::from(relay_slot), 0);
	}
}

pub(crate) fn pool_account() -> AccountId {
	pool_account_on(&get_native_id())
}

pub(crate) fn pool_account_on(collateral_id: &VaultsCollateralId) -> AccountId {
	Stability::pool_account(collateral_id, &DOTUSD_ID)
}

pub(crate) fn sp_deposit_matured_on(
	collateral_id: VaultsCollateralId,
	who: &AccountId,
	amount: Balance,
) {
	sp_deposit_pending_on(collateral_id.clone(), who, amount);
	// A deposit matures at its cohort's deadline, which lands anywhere in
	// `[entry_delay, 2 * entry_delay)`, so wait out the cohort it actually joined. A zero
	// entry delay activates on deposit and leaves no cohort to wait for. The wait lands
	// on the first slot boundary at or after the deadline — the first instant a real
	// block can observe maturity — not on the deadline itself.
	let Some(deadline) = sp_pending_deadline(&collateral_id, who) else {
		return;
	};
	let now = Timestamp::get();
	if deadline > now {
		advance_time(deadline - now);
	}
}

/// The deadline of the cohort that the pending deposit of `who` waits out.
fn sp_pending_deadline(collateral_id: &VaultsCollateralId, who: &AccountId) -> Option<u64> {
	let pending = pallet_stability::Deposits::<Runtime>::get((
		collateral_id.clone(),
		DOTUSD_ID,
		who.clone(),
	))?
	.pending_deposit?;
	pallet_stability::Pools::<Runtime>::get(collateral_id.clone(), DOTUSD_ID)?
		.state
		.cohort(pending.cohort)
		.map(|cohort| cohort.deadline)
}

pub(crate) fn sp_deposit_pending_on(
	collateral_id: VaultsCollateralId,
	who: &AccountId,
	amount: Balance,
) {
	mint_dotusd(who, amount);
	assert_ok!(Stability::deposit(
		RuntimeOrigin::signed(who.clone()),
		collateral_id,
		DOTUSD_ID,
		amount,
	));
}

pub(crate) fn deposit_row_on(
	collateral_id: &VaultsCollateralId,
	who: &AccountId,
) -> pallet_stability::types::Deposit<Balance> {
	pallet_stability::Deposits::<Runtime>::get((collateral_id, DOTUSD_ID, who.clone()))
		.expect("deposit row exists")
}

/// Settled rows that hold nothing are pruned, so a missing row is a valid end state.
pub(crate) fn deposit_row_exists(collateral_id: &VaultsCollateralId, who: &AccountId) -> bool {
	pallet_stability::Deposits::<Runtime>::contains_key((collateral_id, DOTUSD_ID, who.clone()))
}

/// Claims realized collateral and checks transfers in the collateral asset pallet.
pub(crate) fn claim_collateral_out(
	collateral_id: &VaultsCollateralId,
	depositor: &AccountId,
	expected: Balance,
) {
	let pool = pool_account_on(collateral_id);
	let depositor_before = collateral_free(collateral_id, depositor);
	let pool_before = collateral_free(collateral_id, &pool);

	assert_ok!(Stability::claim_collateral(
		RuntimeOrigin::signed(depositor.clone()),
		collateral_id.clone(),
		DOTUSD_ID,
		None,
	));

	assert_eq!(collateral_free(collateral_id, depositor) - depositor_before, expected);
	assert_eq!(pool_before - collateral_free(collateral_id, &pool), expected);
	// A second claim fails because the row has no claimable collateral or no longer exists.
	let expected_error = if deposit_row_exists(collateral_id, depositor) {
		pallet_stability::Error::<Runtime>::NoClaimableCollateral
	} else {
		pallet_stability::Error::<Runtime>::DepositNotFound
	};
	assert_noop!(
		Stability::claim_collateral(
			RuntimeOrigin::signed(depositor.clone()),
			collateral_id.clone(),
			DOTUSD_ID,
			None,
		),
		expected_error,
	);
}
