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

//! Shared setup helpers. They apply the Shared Assumptions of the
//! numerical-examples document. Call them inside `AssetHubWestend::execute_with`.

use crate::imports::*;
use asset_hub_westend_runtime::{
	governance,
	pusd_config::{
		StabilityCollateral, StableInsuranceAccount, VaultsCollateral, VaultsNativeCollateralId,
	},
	Assets, Balances, MockOracle, Runtime, RuntimeHoldReason, Stability, Timestamp, Vaults,
};
pub(crate) const WND: Balance = 1_000_000_000_000;
/// The stablecoin has 6 decimals, the same as the runtime's PSM asset.
pub(crate) const PUSD: Balance = 1_000_000;
/// 0.01 pUSD. The stablecoin registers with this minimum balance.
pub(crate) const PUSD_MIN_BALANCE: Balance = PUSD / 100;

/// Trust-backed asset id for pUSD. The runtime names no stablecoin, so the tests
/// choose one. It must not collide with the assets the emulated genesis creates.
pub(crate) const PUSD_ID: u32 = 50_000_342;

pub(crate) fn get_pusd_id() -> u32 {
	PUSD_ID
}

/// The Insurance Fund account of pUSD, derived as the runtime derives it. A
/// change to that mapping then fails here instead of funding a wrong account.
pub(crate) fn insurance_account() -> AccountId {
	<StableInsuranceAccount as sp_runtime::traits::Convert<u32, AccountId>>::convert(get_pusd_id())
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

/// Price of one WND in pUSD, as the runtime's planck-to-micro-pUSD rate.
pub(crate) fn dot_price(pusd_num: u128, pusd_den: u128) -> FixedU128 {
	FixedU128::from_rational(pusd_num * PUSD, pusd_den * WND)
}

pub(crate) fn feed_price(price: FixedU128) {
	feed_price_for(get_native_id(), price);
}

pub(crate) fn feed_price_for(collateral_id: VaultsCollateralId, price: FixedU128) {
	assert_ok!(MockOracle::set_price(RuntimeOrigin::root(), collateral_id, price));
}

/// Branch settings the examples vary. All other settings follow the Shared
/// Assumptions.
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
			minimum_debt: 50 * PUSD,
			// Zero keeps the opened debt equal to the drawn amount. Example 16 sets
			// the period itself.
			upfront_fee_period_ms: 0,
			keeper_flat_compensation_value: 2 * PUSD,
			keeper_percent_compensation: Permill::from_rational(1u32, 1_000u32),
		}
	}
}

/// Spec for the liquidation examples. Their vaults sit at CR 120% after the
/// price halves, so the MCR must be above 120%.
pub(crate) fn liquidation_spec() -> BranchSpec {
	BranchSpec {
		mcr: FixedU128::from_rational(125, 100),
		icr: FixedU128::from_rational(130, 100),
		scr: FixedU128::from_rational(130, 100),
		..Default::default()
	}
}

/// [`liquidation_spec`] without keeper compensation, for the examples that
/// omit it.
pub(crate) fn accounting_spec() -> BranchSpec {
	BranchSpec {
		keeper_flat_compensation_value: 0,
		keeper_percent_compensation: Permill::zero(),
		..liquidation_spec()
	}
}

/// Creates the stablecoin and a native-WND market for it, and lifts the global
/// debt ceiling.
pub(crate) fn create_branch(spec: &BranchSpec) {
	create_pusd();
	// Root creation charges the full admin one ED under `Preserve`, so the admin
	// needs two ED. `admin()` has no genesis balance.
	assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(
		&admin(),
		get_native_ed().saturating_mul(2),
	));
	assert_ok!(Vaults::create_branch(
		RuntimeOrigin::root(),
		get_native_id(),
		get_pusd_id(),
		branch_admins(),
		branch_config(&get_native_id(), spec),
		registration_config(),
	));
	// A billion pUSD is above every example's debt.
	lift_global_ceiling(1_000_000_000 * PUSD);
}

/// Creates the stablecoin asset, owned by [`admin`]. It is sufficient, so holders
/// need no native provider reference, and it has a real minimum balance.
///
/// With a minimum balance above one unit, the first market registration must
/// create the fee account's stablecoin account. That account owns its deposit,
/// so it needs native balance. A live treasury holds some. The emulated genesis
/// does not, so the helper funds it.
pub(crate) fn create_pusd() {
	assert_ok!(Assets::force_create(
		RuntimeOrigin::root(),
		get_pusd_id().into(),
		MultiAddress::Id(admin()),
		true,
		PUSD_MIN_BALANCE,
	));
	assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(
		&governance::TreasuryAccount::get(),
		WND,
	));
}

/// Creates a market from the stablecoin owner instead of Root. The creator then
/// pays every refundable registration cost, not the full admin.
pub(crate) fn create_market_signed(collateral_id: VaultsCollateralId, spec: &BranchSpec) {
	// The creator pays the creation deposit, the pool account's asset deposit,
	// and the custody seed.
	assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(&admin(), 1_000 * WND));
	fund_collateral(&collateral_id, &admin(), collateral_min_balance(&collateral_id));
	assert_ok!(Vaults::create_branch(
		RuntimeOrigin::signed(admin()),
		collateral_id.clone(),
		get_pusd_id(),
		branch_admins(),
		branch_config(&collateral_id, spec),
		registration_config(),
	));
}

/// Registration payload `(redemptions, stability)` for a pUSD market. The
/// redemption policy is stablecoin-wide, so only the first market carries it.
pub(crate) fn registration_config() -> (
	Option<pallet_redemptions::RedemptionConfig<Balance>>,
	pallet_stability::types::StabilityPoolConfig<Balance>,
) {
	let redemption_config =
		(!pallet_redemptions::RedemptionConfigs::<Runtime>::contains_key(get_pusd_id()))
			.then(test_redemption_config);
	(redemption_config, test_stability_config())
}

fn test_redemption_config() -> pallet_redemptions::RedemptionConfig<Balance> {
	pallet_redemptions::RedemptionConfig {
		minimum_redemption_amount: 100 * PUSD,
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
		minimum_deposit: 100 * PUSD,
		minimum_active_pool_balance: 100 * PUSD,
		entry_delay: 5_000,
		safety_withdrawal_delay: 10 * 60 * 1_000,
		precision: pallet_stability::types::PoolPrecision {
			p_min: FixedU128::from_inner(1_000_000_000),
			scale_factor: 1_000_000_000,
		},
		yield_share: Permill::from_percent(75),
	}
}

/// One call covers every market of the coin, because the ceiling is
/// stablecoin-wide. `amount` is in stablecoin units.
pub(crate) fn lift_global_ceiling(amount: Balance) {
	assert_ok!(Vaults::set_global_debt_ceiling(RuntimeOrigin::root(), get_pusd_id(), amount));
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
		debt_ceiling: 100_000_000 * PUSD,
		minimum_debt: spec.minimum_debt,
		minimum_collateral: collateral_min_balance(collateral_id),
		minimum_borrow_rate: FixedU128::zero(),
		maximum_borrow_rate: FixedU128::from_rational(400, 100),
		upfront_fee_period: spec.upfront_fee_period_ms,
		rate_adjustment_cooldown: 0,
		liquidation: pallet_vaults::LiquidationConfig {
			offset_penalty: Permill::from_percent(5),
			keeper_flat_compensation_value: spec.keeper_flat_compensation_value,
			keeper_percent_compensation: spec.keeper_percent_compensation,
			keeper_compensation_cap_value: 100 * PUSD,
			minimum_jit_contribution: PUSD,
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

pub(crate) fn mint_pusd(who: &AccountId, amount: Balance) {
	assert_ok!(<Assets as Mutate<AccountId>>::mint_into(get_pusd_id(), who, amount));
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
	assert_ok!(Vaults::open_vault(
		RuntimeOrigin::signed(owner.clone()),
		collateral_id,
		get_pusd_id(),
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
	pallet_vaults::Vaults::<Runtime>::get((collateral_id, get_pusd_id(), owner))
		.expect("vault exists for owner")
}

pub(crate) fn pusd_balance(who: &AccountId) -> Balance {
	<Assets as Inspect<AccountId>>::balance(get_pusd_id(), who)
}

pub(crate) fn pusd_issuance() -> Balance {
	<Assets as Inspect<AccountId>>::total_issuance(get_pusd_id())
}

/// Market-level accounting for the native-WND market.
pub(crate) fn branch_state() -> pallet_vaults::types::BranchState<AccountId, Balance> {
	pallet_vaults::Branches::<Runtime>::get(get_native_id(), get_pusd_id())
		.expect("branch registered")
		.state
}

pub(crate) fn native_balance(who: &AccountId) -> Balance {
	<Balances as FungibleInspect<AccountId>>::balance(who)
}

/// Moves the emulated clock forward. Aura requires the timestamp to match its
/// slot, so the slot moves too.
pub(crate) fn advance_time(ms: u64) {
	let now = Timestamp::get() + ms;
	let slot = now / asset_hub_westend_runtime::Aura::slot_duration();
	pallet_aura::CurrentSlot::<Runtime>::put(sp_consensus_slots::Slot::from(slot));
	Timestamp::set_timestamp(now);
}

pub(crate) fn pool_account() -> AccountId {
	pool_account_on(&get_native_id())
}

pub(crate) fn pool_account_on(collateral_id: &VaultsCollateralId) -> AccountId {
	Stability::pool_account(collateral_id, &get_pusd_id())
}

/// Mints and deposits, then waits out the entry delay.
///
/// The deposit is left matured rather than active. Activation belongs to the next pool
/// operation the test performs, which is how it happens on chain.
pub(crate) fn sp_deposit_matured(who: &AccountId, amount: Balance) {
	sp_deposit_matured_on(get_native_id(), who, amount);
}

pub(crate) fn sp_deposit_matured_on(
	collateral_id: VaultsCollateralId,
	who: &AccountId,
	amount: Balance,
) {
	sp_deposit_pending_on(collateral_id.clone(), who, amount);
	// A deposit matures at its cohort's deadline, which lands anywhere in
	// `[entry_delay, 2 * entry_delay)`, so wait out the cohort it actually joined. A zero
	// entry delay activates on deposit and leaves no cohort to wait for.
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
		get_pusd_id(),
		who.clone(),
	))?
	.pending_deposit?;
	pallet_stability::Pools::<Runtime>::get(collateral_id.clone(), get_pusd_id())?
		.state
		.cohort(pending.cohort)
		.map(|cohort| cohort.deadline)
}

/// Mints and deposits. The funds stay pending behind the entry delay.
pub(crate) fn sp_deposit_pending(who: &AccountId, amount: Balance) {
	sp_deposit_pending_on(get_native_id(), who, amount);
}

pub(crate) fn sp_deposit_pending_on(
	collateral_id: VaultsCollateralId,
	who: &AccountId,
	amount: Balance,
) {
	mint_pusd(who, amount);
	assert_ok!(Stability::deposit(
		RuntimeOrigin::signed(who.clone()),
		collateral_id,
		get_pusd_id(),
		amount,
	));
}

pub(crate) fn pool_state() -> pallet_stability::types::PoolState<Balance> {
	pallet_stability::Pools::<Runtime>::get(get_native_id(), get_pusd_id())
		.expect("stability pool registered")
		.state
}

/// Changes the pool config through governance.
pub(crate) fn mutate_pool_config(
	tweak: impl FnOnce(&mut pallet_stability::types::StabilityPoolConfig<Balance>),
) {
	let mut config = pallet_stability::Pools::<Runtime>::get(get_native_id(), get_pusd_id())
		.expect("stability pool registered")
		.config;
	tweak(&mut config);
	assert_ok!(Stability::set_stability_pool_config(
		RuntimeOrigin::root(),
		get_native_id(),
		get_pusd_id(),
		config,
	));
}

/// Sets the redemption dynamic fee directly, in place of the redemption history
/// that would produce it. `last_fee_operation = now` means the value is already
/// decayed.
pub(crate) fn set_dynamic_fee(rate: FixedU128) {
	pallet_redemptions::RedemptionStates::<Runtime>::insert(
		get_pusd_id(),
		pallet_redemptions::RedemptionState {
			dynamic_fee: rate,
			last_fee_operation: Timestamp::get(),
		},
	);
}
