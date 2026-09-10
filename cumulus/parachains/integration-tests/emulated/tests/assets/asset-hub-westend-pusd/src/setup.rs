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

//! Shared setup for a six-decimal stablecoin, runtime asset markets, and a Root-fed oracle.

use crate::imports::*;
use asset_hub_westend_runtime::{
	governance,
	pusd_config::{
		StabilityCollateral, StableInsuranceAccount, VaultsCollateral, VaultsDepositPolicy,
		VaultsNativeCollateralId,
	},
	Assets, AuraExt, Balances, MockOracle, Redemptions, Runtime, RuntimeHoldReason, Stability,
	Timestamp, Vaults,
};
use frame_support::{assert_noop, assert_storage_noop};
use pallet_redemptions::RedemptionTerms;
use pallet_vaults::JitTerms;
use pusd_primitives::VaultStatus;
use sp_runtime::DispatchResult;

pub(crate) const WND: Balance = 1_000_000_000_000;
/// The stablecoin's metadata decimals.
pub(crate) const PUSD_DECIMALS: u8 = 6;
/// One whole stablecoin, at [`PUSD_DECIMALS`].
pub(crate) const PUSD: Balance = 10u128.pow(PUSD_DECIMALS as u32);
/// 0.01 pUSD. The stablecoin registers with this minimum balance.
pub(crate) const PUSD_MIN_BALANCE: Balance = PUSD / 100;

/// Trust-backed asset id for the stablecoin: 7873, the id the dotUSD launch
/// referendum creates on Polkadot Asset Hub.
pub(crate) const PUSD_ID: u32 = 7873;

/// Stablecoin-wide supply cap that exceeds all scenario debt.
pub(crate) const SCENARIO_GLOBAL_CEILING: Balance = 1_000_000_000 * PUSD;

/// The Insurance Fund account of pUSD, derived as the runtime derives it. A
/// change to that mapping then fails here instead of funding a wrong account.
pub(crate) fn insurance_account() -> AccountId {
	<StableInsuranceAccount as sp_runtime::traits::Convert<u32, AccountId>>::convert(PUSD_ID)
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

/// Returns a pUSD/WND price as pUSD base units per WND planck.
pub(crate) fn dot_price(pusd_num: u128, pusd_den: u128) -> FixedU128 {
	FixedU128::from_rational(pusd_num * PUSD, pusd_den * WND)
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
			minimum_debt: 50 * PUSD,
			// Zero disables the upfront fee.
			upfront_fee_period_ms: 0,
			keeper_flat_compensation_value: 2 * PUSD,
			keeper_percent_compensation: Permill::from_rational(1u32, 1_000u32),
		}
	}
}

/// Sets MCR above the scenario vaults' 120% CR.
pub(crate) fn liquidation_spec() -> BranchSpec {
	BranchSpec {
		mcr: FixedU128::from_rational(125, 100),
		icr: FixedU128::from_rational(130, 100),
		scr: FixedU128::from_rational(130, 100),
		..Default::default()
	}
}

/// Disables keeper compensation in [`liquidation_spec`] to keep amounts round.
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
		PUSD_ID,
		branch_admins(),
		branch_config(&get_native_id(), spec),
		registration_config(),
	));
	lift_global_ceiling(SCENARIO_GLOBAL_CEILING);
}

/// Creates the stablecoin asset, owned by [`admin`]. It is sufficient, so holders
/// need no native provider reference, and it has a real minimum balance.
///
/// With a minimum balance above one unit, the first market registration must
/// create the fee account's stablecoin account. That account owns its deposit,
/// so it needs native balance. A live treasury holds some. The emulated genesis
/// does not, so the helper funds it.
pub(crate) fn create_pusd() {
	advance_time(0);
	assert_ok!(Assets::force_create(
		RuntimeOrigin::root(),
		PUSD_ID.into(),
		MultiAddress::Id(admin()),
		true,
		PUSD_MIN_BALANCE,
	));
	assert_ok!(Assets::force_set_metadata(
		RuntimeOrigin::root(),
		PUSD_ID.into(),
		b"dotUSD".to_vec(),
		b"dotUSD".to_vec(),
		PUSD_DECIMALS,
		false,
	));
	assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(
		&governance::TreasuryAccount::get(),
		WND,
	));
}

/// Creates a market from the stablecoin owner instead of Root, and lifts the
/// global debt ceiling. The creator then pays every refundable registration
/// cost, not the full admin.
pub(crate) fn create_market_signed(collateral_id: VaultsCollateralId, spec: &BranchSpec) {
	// The creator pays the creation deposit, the pool account's asset deposit,
	// and the custody seed.
	assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(&admin(), 1_000 * WND));
	fund_collateral(&collateral_id, &admin(), collateral_min_balance(&collateral_id));
	assert_ok!(Vaults::create_branch(
		RuntimeOrigin::signed(admin()),
		collateral_id.clone(),
		PUSD_ID,
		branch_admins(),
		branch_config(&collateral_id, spec),
		registration_config(),
	));
	lift_global_ceiling(SCENARIO_GLOBAL_CEILING);
}

/// Registration payload `(redemptions, stability)` for a pUSD market. The
/// redemption policy is stablecoin-wide, so only the first market carries it.
pub(crate) fn registration_config() -> (
	Option<pallet_redemptions::RedemptionConfig<Balance>>,
	pallet_stability::types::StabilityPoolConfig<Balance>,
) {
	let redemption_config =
		(!pallet_redemptions::RedemptionConfigs::<Runtime>::contains_key(PUSD_ID))
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
		entry_delay: u64::from(RELAY_CHAIN_SLOT_DURATION_MILLIS),
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
	assert_ok!(Vaults::set_global_debt_ceiling(RuntimeOrigin::root(), PUSD_ID, amount));
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
		final_recovery_reward_cooldown: 60 * 60 * 1_000,
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

/// The asset and amount the runtime charges as storage deposit for a vault on `collateral_id`.
pub(crate) fn expected_vault_deposit(
	collateral_id: &VaultsCollateralId,
	owner: &AccountId,
) -> (VaultsCollateralId, Balance) {
	let priced;
	// Pricing a deposit is a quote and must not write.
	assert_storage_noop!(
		priced = <VaultsDepositPolicy as sp_runtime::traits::Convert<_, _>>::convert(
			Vaults::vault_footprint(collateral_id, &PUSD_ID, owner),
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

pub(crate) fn mint_pusd(who: &AccountId, amount: Balance) {
	assert_ok!(<Assets as Mutate<AccountId>>::mint_into(PUSD_ID, who, amount));
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
		PUSD_ID,
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
	pallet_vaults::Vaults::<Runtime>::get((collateral_id, PUSD_ID, owner))
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
	let status;
	// Reading a status must not write.
	assert_storage_noop!(
		status = Vaults::vault_status(collateral_id.clone(), PUSD_ID, owner.clone())
	);
	status
}

/// Touches the vault through a throwaway origin, materializing pending
/// redistribution and interest.
pub(crate) fn poke(owner: &AccountId) {
	assert_ok!(Vaults::poke(
		RuntimeOrigin::signed(acct(0xFE)),
		get_native_id(),
		PUSD_ID,
		owner.clone(),
	));
}

/// Puts `owner`'s vault in the FinalRecovery FIFO through a throwaway keeper.
/// The keeper's reward, if any, has its own test.
pub(crate) fn enter_final_recovery(owner: &AccountId) {
	assert_ok!(Vaults::enter_final_recovery(
		RuntimeOrigin::signed(acct(0xFE)),
		get_native_id(),
		PUSD_ID,
		owner.clone(),
	));
	assert_eq!(vault_status(owner), Some(VaultStatus::FinalRecovery));
}

/// A fresh native-funded account for keeper duties. Its balance starts at the ED,
/// so a reward reads as `native_balance(&keeper) - get_native_ed()`.
pub(crate) fn keeper() -> AccountId {
	let keeper = acct(0xEE);
	assert_eq!(native_balance(&keeper), 0, "the keeper is funded once");
	fund_dot(&keeper, 0);
	keeper
}

/// Liquidates without JIT, with a throwaway keeper.
pub(crate) fn liquidate(owner: &AccountId) {
	liquidate_on(get_native_id(), owner);
}

pub(crate) fn liquidate_on(collateral_id: VaultsCollateralId, owner: &AccountId) {
	let keeper = acct(0xEE);
	fund_collateral(&collateral_id, &keeper, 0);
	assert_ok!(Vaults::liquidate(
		RuntimeOrigin::signed(keeper),
		collateral_id,
		PUSD_ID,
		owner.clone(),
		JitTerms { max_stable: 0, min_collateral_out: 0 },
	));
}

/// Repaid vaults retain stake. With an empty pool, liquidation assigns each 100 pUSD of debt
/// and 100 WND lazily, without putting either recipient in the redemption slot.
pub(crate) fn redistributed_husks() -> [AccountId; 2] {
	feed_price(dot_price(10, 1));
	create_branch(&BranchSpec {
		minimum_debt: 200 * PUSD,
		keeper_flat_compensation_value: 0,
		keeper_percent_compensation: Permill::zero(),
		..Default::default()
	});
	let owners = [acct(1), acct(2)];
	for owner in &owners {
		// The existing redistribution ledger requires non-zero rate-weighted recipient stake.
		open_vault(owner, 1_000 * WND, 500 * PUSD, FixedU128::from_rational(1, 100));
		assert_ok!(Vaults::repay_for(
			RuntimeOrigin::signed(owner.clone()),
			get_native_id(),
			PUSD_ID,
			owner.clone(),
			None,
		));
	}
	let victim = acct(3);
	open_vault(&victim, 200 * WND, 200 * PUSD, FixedU128::zero());
	feed_price(dot_price(1, 1));
	liquidate(&victim);
	assert_eq!(branch_state().dormant_redemption_target, None);
	assert_eq!(branch_state().debt.pending_redistribution_principal, 200 * PUSD);
	owners
}

/// Nominates `owner`'s dust vault for the Dormant slot through a throwaway signer.
pub(crate) fn nominate_dormant(owner: &AccountId) -> DispatchResult {
	Vaults::nominate_dormant(
		RuntimeOrigin::signed(acct(0xF0)),
		get_native_id(),
		PUSD_ID,
		owner.clone(),
	)
}

/// Funds a fresh `redeemer` with `terms.max_stable_to_spend` plus the stablecoin
/// minimum balance, redeems on the native market, and returns the collateral
/// received.
///
/// Every scenario redemption is budgeted to the unit, so the helper asserts
/// that only the minimum balance remains.
pub(crate) fn redeem(redeemer: &AccountId, terms: RedemptionTerms<Balance>) -> Balance {
	fund_dot(redeemer, 0);
	mint_pusd(redeemer, terms.max_stable_to_spend + PUSD_MIN_BALANCE);
	let native_before = native_balance(redeemer);

	assert_ok!(Redemptions::redeem(
		RuntimeOrigin::signed(redeemer.clone()),
		get_native_id(),
		PUSD_ID,
		terms,
		redeemer.clone(),
		16,
	));

	assert_eq!(pusd_balance(redeemer), PUSD_MIN_BALANCE);
	native_balance(redeemer) - native_before
}

pub(crate) fn pusd_balance(who: &AccountId) -> Balance {
	<Assets as Inspect<AccountId>>::balance(PUSD_ID, who)
}

pub(crate) fn pusd_issuance() -> Balance {
	<Assets as Inspect<AccountId>>::total_issuance(PUSD_ID)
}

/// Market-level accounting for the native-WND market.
pub(crate) fn branch_state() -> pallet_vaults::types::BranchState<AccountId, Balance> {
	pallet_vaults::Branches::<Runtime>::get(get_native_id(), PUSD_ID)
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
	Stability::pool_account(collateral_id, &PUSD_ID)
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
	let pending =
		pallet_stability::Deposits::<Runtime>::get((collateral_id.clone(), PUSD_ID, who.clone()))?
			.pending_deposit?;
	pallet_stability::Pools::<Runtime>::get(collateral_id.clone(), PUSD_ID)?
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
		PUSD_ID,
		amount,
	));
}

pub(crate) fn pool_state() -> pallet_stability::types::PoolState<Balance> {
	pallet_stability::Pools::<Runtime>::get(get_native_id(), PUSD_ID)
		.expect("stability pool registered")
		.state
}

pub(crate) fn deposit_row(who: &AccountId) -> pallet_stability::types::Deposit<Balance> {
	deposit_row_on(&get_native_id(), who)
}

pub(crate) fn deposit_row_on(
	collateral_id: &VaultsCollateralId,
	who: &AccountId,
) -> pallet_stability::types::Deposit<Balance> {
	pallet_stability::Deposits::<Runtime>::get((collateral_id, PUSD_ID, who.clone()))
		.expect("deposit row exists")
}

/// Settled rows that hold nothing are pruned, so a missing row is a valid end state.
pub(crate) fn deposit_row_exists(collateral_id: &VaultsCollateralId, who: &AccountId) -> bool {
	pallet_stability::Deposits::<Runtime>::contains_key((collateral_id, PUSD_ID, who.clone()))
}

/// Claims yield on the native market and checks the stablecoin paid out.
pub(crate) fn claim_yield_out(who: &AccountId, expected: Balance) {
	let pusd_before = pusd_balance(who);
	assert_ok!(Stability::claim_yield(
		RuntimeOrigin::signed(who.clone()),
		get_native_id(),
		PUSD_ID,
		None,
	));
	assert_eq!(pusd_balance(who) - pusd_before, expected);
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
		PUSD_ID,
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
			PUSD_ID,
			None,
		),
		expected_error,
	);
}

/// Changes the pool configuration through its full-admin `UpdateOrigin`.
pub(crate) fn mutate_pool_config(
	tweak: impl FnOnce(&mut pallet_stability::types::StabilityPoolConfig<Balance>),
) {
	let mut config = pallet_stability::Pools::<Runtime>::get(get_native_id(), PUSD_ID)
		.expect("stability pool registered")
		.config;
	tweak(&mut config);
	assert_ok!(Stability::set_stability_pool_config(
		RuntimeOrigin::signed(admin()),
		get_native_id(),
		PUSD_ID,
		config,
	));
}

/// Sets the redemption dynamic fee directly, in place of the redemption history
/// that would produce it. `last_fee_operation = now` means the value is already
/// decayed.
pub(crate) fn set_dynamic_fee(rate: FixedU128) {
	pallet_redemptions::RedemptionStates::<Runtime>::insert(
		PUSD_ID,
		pallet_redemptions::RedemptionState {
			dynamic_fee: rate,
			last_fee_operation: Timestamp::get(),
		},
	);
}
