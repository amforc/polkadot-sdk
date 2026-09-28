//! Benchmarks for `pallet-vaults`. Rate-index dispatchables feed the
//! linked-list a hint that is exactly `hint_repair_budget` steps stale, so
//! the worst-case repair walk is what gets measured.

#![cfg(feature = "runtime-benchmarks")]

use crate::{
	pallet::{
		AccountIdLookupOf, BalanceOf, Branches, CollateralIdOf, Config, GlobalDebtCeilings, Pallet,
		RegistrationConfigOf, StableIdOf, Vaults,
	},
	types::{BranchAdmins, BranchConfig, BranchConfigUpdate, JitTerms, VaultListId, VaultStatus},
	BenchmarkHelper as _, HoldReason,
};
use alloc::vec::Vec;
use frame::{
	arithmetic::{FixedU128, Permill},
	benchmarking::prelude::*,
	traits::{
		fungibles::{
			Balanced as FungiblesBalanced, Inspect as FungiblesInspect,
			InspectHold as FungiblesInspectHold, Mutate as FungiblesMutate,
		},
		tokens::Precision,
		Consideration as _, EnsureOriginWithArg, SaturatedConversion, Time, Zero,
	},
};
use frame_system::RawOrigin;
use linked_list_interface::{Position, SortedListInterface};
use pusd_primitives::{
	BranchSnapshot, OnBranchLifecycle, RedemptionSettlement, RedemptionStepSnapshot,
	StabilityPoolInspect, StabilityPoolOffset, VaultInterface,
};

const ORACLE_PRICE: u128 = 10;
/// High stablecoin-wide ceiling so the systemic cap never binds in benches.
const GLOBAL_CEILING: u128 = 1_000_000_000_000_000;
const ACCOUNT_FUNDING: u128 = 10_000_000;
const SEED_COLL: u128 = 1_000_000;
/// Must exceed `default_branch_config::minimum_debt` (200).
const SEED_DEBT: u128 = 300;
/// Price drop that pushes a `collateral=200, debt=300` vault below the 110% MCR,
/// so `enter_final_recovery` accepts it.
const RECOVERY_TRIGGER_PRICE: u32 = 1;
/// One hour in milliseconds — enough for a vault refresh to accrue
/// non-zero interest at the default 5% vault rate.
const ONE_HOUR_MS: u64 = 60 * 60 * 1_000;
const RECOVERY_VAULT_COLL: u128 = 200;

/// Stability Pool entry delay of the `liquidate` benchmark. A day between the pool deposits and
/// the liquidation lets the liquidated vault and its market accrue non-zero interest.
const LIQUIDATION_ENTRY_DELAY_MS: u64 = 24 * ONE_HOUR_MS;
/// Stablecoin each non-redistribution leg of the `liquidate` waterfall absorbs: the active pool,
/// the keeper JIT, and the pending pool.
const LIQUIDATION_LEG_DEBT: u128 = 2_000_000;
/// Debt of the liquidated vault. It exceeds the three legs, so redistribution takes the rest.
const LIQUIDATION_VICTIM_DEBT: u128 = 10_000_000;
/// Debt of each other vault in the `liquidate` market.
const LIQUIDATION_PEER_DEBT: u128 = 1_000_000;
/// Collateral of the `liquidate` vaults in collateral units (see [`collateral_unit`]). At
/// [`LIQUIDATION_PRICE`] the victim and the sacrifice sit below the MCR and the holders far above
/// it, so they stay safe after absorbing both redistributions.
const LIQUIDATION_VICTIM_COLL_UNITS: u128 = 6_000_000;
const LIQUIDATION_SACRIFICE_COLL_UNITS: u128 = 600_000;
const LIQUIDATION_HOLDER_COLL_UNITS: u128 = 50_000_000;
/// Stablecoin value of one collateral unit once the `liquidate` benchmark crashes the price.
const LIQUIDATION_PRICE: u128 = 1;

fn stable<T: Config>() -> StableIdOf<T> {
	T::BenchmarkHelper::stable_asset_id()
}

fn balance<T: Config>(value: u128) -> BalanceOf<T> {
	value.saturated_into()
}

fn rate(numerator: u128, denominator: u128) -> FixedU128 {
	FixedU128::from_rational(numerator, denominator)
}

/// Collateral planck per benchmark collateral unit: the collateral's minimum balance.
///
/// The fixture amounts are sized for a unit minimum balance. A runtime with a production
/// existential deposit scales them by this unit, and the `liquidate` benchmark quotes its price
/// per unit, so every stablecoin-denominated amount stays the same across runtimes.
fn collateral_unit<T: Config>() -> BalanceOf<T> {
	let unit = T::CollateralAssets::minimum_balance(T::BenchmarkHelper::collateral_asset_id())
		.max(balance::<T>(1));
	assert!(!unit.is_zero());
	unit
}

/// `units` collateral units in planck.
fn collateral_units<T: Config>(units: u128) -> BalanceOf<T> {
	balance::<T>(units).saturating_mul(collateral_unit::<T>())
}

/// Oracle price quoting `value` stablecoin per collateral unit.
fn unit_price<T: Config>(value: u128) -> FixedU128 {
	let unit: u128 = collateral_unit::<T>().saturated_into();
	FixedU128::from_rational(value, unit)
}

fn default_branch_config<T: Config>() -> BranchConfig<BalanceOf<T>> {
	const DAY_MS: u64 = 24 * 3_600 * 1_000;
	// Both vault floors have to clear their asset's minimum balance, which a runtime with a
	// production existential deposit puts far above the raw units the mock works in.
	let minimum_debt = balance::<T>(200).max(T::StableAssets::minimum_balance(stable::<T>()));
	let minimum_collateral = balance::<T>(1)
		.max(T::CollateralAssets::minimum_balance(T::BenchmarkHelper::collateral_asset_id()));
	BranchConfig {
		minimum_collateralization_ratio: rate(110, 100),
		initial_collateralization_ratio: rate(120, 100),
		safety_collateralization_ratio: rate(130, 100),
		debt_ceiling: balance::<T>(100_000_000),
		minimum_debt,
		minimum_collateral,
		minimum_borrow_rate: rate(1, 1_000),
		maximum_borrow_rate: rate(100, 100),
		upfront_fee_period: 7 * DAY_MS,
		rate_adjustment_cooldown: DAY_MS,
		final_recovery_reward_cooldown: ONE_HOUR_MS,
		liquidation: crate::LiquidationConfig {
			offset_penalty: Permill::from_percent(5),
			keeper_flat_compensation_value: balance::<T>(10),
			keeper_percent_compensation: Permill::from_rational(1u32, 1_000u32),
			keeper_compensation_cap_value: balance::<T>(10_000),
			minimum_jit_contribution: balance::<T>(100),
			redistribution_penalty: Permill::from_percent(5),
		},
	}
}

/// Every benchmark registers one market per stablecoin, so the lifecycle
/// handlers always see the first one.
fn first_market_lifecycle_config<T: Config>() -> RegistrationConfigOf<T> {
	T::OnBranchLifecycle::benchmark_registration_config(1)
}

/// The accounts acting as full and emergency admin of every benchmarked market.
fn branch_admin_accounts<T: Config>() -> (T::AccountId, T::AccountId) {
	(account("full_admin", 0, 0), account("emergency_admin", 0, 0))
}

/// The admin bundle every benchmarked market is created with.
fn branch_admins<T: Config>() -> BranchAdmins<AccountIdLookupOf<T>> {
	let (full_admin, emergency_admin) = branch_admin_accounts::<T>();
	BranchAdmins {
		full_admin: T::Lookup::unlookup(full_admin),
		emergency_admin: T::Lookup::unlookup(emergency_admin),
	}
}

/// A successful `CreateOrigin` for the default stablecoin — Root (deposit-free)
/// in both the mock and the node runtime.
fn create_origin<T: Config>() -> Result<T::RuntimeOrigin, BenchmarkError> {
	T::CreateOrigin::try_successful_origin(&stable::<T>())
		.map_err(|_| BenchmarkError::Stop("create origin unavailable"))
}

/// Signed origin of a benchmarked market's full admin.
fn full_admin_origin<T: Config>() -> T::RuntimeOrigin {
	RawOrigin::Signed(branch_admin_accounts::<T>().0).into()
}

fn register_default_branch<T: Config>() -> Result<CollateralIdOf<T>, BenchmarkError> {
	let asset = T::BenchmarkHelper::collateral_asset_id();
	T::BenchmarkHelper::ensure_stable_asset(stable::<T>());
	// `create_branch` validates the oracle price, so set it first.
	T::BenchmarkHelper::set_oracle_price(
		asset.clone(),
		FixedU128::saturating_from_integer(ORACLE_PRICE),
	);
	// The fee account needs native funds to pay its asset-account deposit.
	let fee_account = T::FeeAccount::convert(stable::<T>());
	fund_collateral::<T>(&asset, &fee_account, balance::<T>(1_000_000_000_000))?;
	// A Root-created market charges the custody seed to its full admin.
	fund_collateral::<T>(
		&asset,
		&branch_admin_accounts::<T>().0,
		collateral_units::<T>(ACCOUNT_FUNDING),
	)?;
	Pallet::<T>::create_branch(
		create_origin::<T>()?,
		asset.clone(),
		stable::<T>(),
		branch_admins::<T>(),
		default_branch_config::<T>(),
		first_market_lifecycle_config::<T>(),
	)?;
	Pallet::<T>::set_global_debt_ceiling(
		create_origin::<T>()?,
		stable::<T>(),
		balance::<T>(GLOBAL_CEILING),
	)?;
	Ok(asset)
}

fn fund_collateral<T: Config>(
	asset: &CollateralIdOf<T>,
	who: &T::AccountId,
	amount: BalanceOf<T>,
) -> Result<(), BenchmarkError> {
	if frame_system::Pallet::<T>::providers(who) == 0 {
		frame_system::Pallet::<T>::inc_providers(who);
	}
	let debt = <T::CollateralAssets as FungiblesBalanced<T::AccountId>>::deposit(
		asset.clone(),
		who,
		amount,
		Precision::Exact,
	)
	.map_err(|_| BenchmarkError::Stop("collateral funding failed"))?;
	drop(debt);
	T::VaultConsideration::ensure_successful(
		who,
		Pallet::<T>::vault_footprint(asset, &stable::<T>(), who),
	);
	Ok(())
}

fn funded_account<T: Config>(
	seed: &'static str,
	asset: &CollateralIdOf<T>,
) -> Result<T::AccountId, BenchmarkError> {
	let who: T::AccountId = account(seed, 0, 0);
	fund_collateral::<T>(asset, &who, balance::<T>(ACCOUNT_FUNDING))?;
	Ok(who)
}

/// Opens a `SEED_DEBT` vault for `who` at the default 5% rate, hinted at the list endpoints.
fn open_default_vault<T: Config>(
	who: &T::AccountId,
	asset: &CollateralIdOf<T>,
	collateral: BalanceOf<T>,
) -> Result<(), BenchmarkError> {
	Pallet::<T>::open_vault(
		RawOrigin::Signed(who.clone()).into(),
		asset.clone(),
		stable::<T>(),
		collateral,
		balance::<T>(SEED_DEBT),
		rate(5, 100),
		Position::endpoints_only(),
	)?;
	Ok(())
}

/// Register the default market and open one vault in it. Returns the market's collateral id and the
/// vault owner.
fn seed_idle_market<T: Config>() -> Result<(CollateralIdOf<T>, T::AccountId), BenchmarkError> {
	let asset = register_default_branch::<T>()?;
	let owner = funded_account::<T>("owner", &asset)?;
	open_default_vault::<T>(&owner, &asset, balance::<T>(SEED_COLL))?;
	Ok((asset, owner))
}

/// Runtime-adaptive rate fixture derived from the live branch's borrow-rate
/// bounds.
struct RateBounds {
	/// Highest seed-chain rate.
	base: FixedU128,
	/// Gap between consecutive seed rates.
	step: FixedU128,
	/// A rate strictly above every seed-chain rate, used for "land at head"
	/// insert worst cases. Stays below `maximum_borrow_rate`.
	above: FixedU128,
	/// A rate inside the seed-chain range, used by `close_vault`'s
	/// middle-of-list removal case.
	middle: FixedU128,
}

fn rate_bounds<T: Config>(asset: &CollateralIdOf<T>) -> Result<RateBounds, BenchmarkError> {
	let config = Pallet::<T>::branch_of(asset, &stable::<T>())
		.map_err(|_| BenchmarkError::Stop("missing branch config"))?
		.config;
	let count = T::VaultLists::repair_budget().saturating_add(2);
	let safety_floor = config
		.minimum_borrow_rate
		.saturating_mul(FixedU128::saturating_from_integer(2u32));
	// Use half of `maximum_borrow_rate` as the ceiling so `above = safety_ceiling`
	// always satisfies `validate_rate`.
	let safety_ceiling = config
		.maximum_borrow_rate
		.const_checked_div(FixedU128::saturating_from_integer(2u32))
		.ok_or(BenchmarkError::Stop("maximum_borrow_rate halving overflowed"))?;
	if safety_ceiling <= safety_floor {
		return Err(BenchmarkError::Stop("borrow-rate range too narrow for seeding"));
	}
	let span = safety_ceiling.saturating_sub(safety_floor);
	let divisor = FixedU128::saturating_from_integer(count.saturating_add(2));
	let step = span
		.const_checked_div(divisor)
		.ok_or(BenchmarkError::Stop("rate step computation failed"))?;
	if step.is_zero() {
		return Err(BenchmarkError::Stop("borrow-rate span too narrow for repair_budget"));
	}
	let base = safety_ceiling.saturating_sub(step);
	let above = safety_ceiling;
	let middle_offset = step.saturating_mul(FixedU128::saturating_from_integer(count / 2));
	let middle = base.saturating_sub(middle_offset);
	Ok(RateBounds { base, step, above, middle })
}

/// Seed the rate index with the smallest chain that admits a worst-case
/// stale hint (`hint_repair_budget + 2`), each insert hinted via
/// `find_position` to keep seeding O(count) — independent of the
/// hint-repair budget. Returns owners in head→tail order.
fn seed_worst_case_chain<T: Config>(
	asset: &CollateralIdOf<T>,
) -> Result<Vec<T::AccountId>, BenchmarkError> {
	let count = T::VaultLists::repair_budget().saturating_add(2);
	let mut owners = Vec::with_capacity(count as usize);
	let bounds = rate_bounds::<T>(asset)?;
	for i in 0..count {
		let offset = bounds.step.saturating_mul(FixedU128::saturating_from_integer(i));
		let r = bounds.base.saturating_sub(offset);
		let who: T::AccountId = account("seed", i, 0);
		fund_collateral::<T>(asset, &who, balance::<T>(ACCOUNT_FUNDING))?;
		let hint =
			T::VaultLists::find_position(&VaultListId::Rate(asset.clone(), stable::<T>()), r);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(who.clone()).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(SEED_COLL),
			balance::<T>(SEED_DEBT),
			r,
			hint,
		)?;
		owners.push(who);
	}
	Ok(owners)
}

/// Returns a head-of-list hint that is exactly `hint_repair_budget` steps
/// stale, forcing the full repair walk on insert. Pair with a priority
/// above every seeded rate to land at the new head. Errors out for
/// `repair_budget == 0` (no walk to construct) or short seed chains.
fn worst_case_head_hint<T: Config>(
	seeds: &[T::AccountId],
) -> Result<Position<T::AccountId>, BenchmarkError> {
	let s = T::VaultLists::repair_budget() as usize;
	if s == 0 || seeds.len() <= s {
		return Err(BenchmarkError::Stop("repair_budget too small for worst-case hint"));
	}
	Ok(Position::between(seeds[s - 1].clone(), seeds[s].clone()))
}

/// A caller vault opened mid-chain in a worst-case seeded rate index.
struct MidChainCaller<T: Config> {
	asset: CollateralIdOf<T>,
	caller: T::AccountId,
	/// A rate above every seeded rate, landing the caller at the new head.
	head_rate: FixedU128,
	/// A head hint exactly `hint_repair_budget` steps stale.
	head_hint: Position<T::AccountId>,
}

/// Seeds the worst-case chain, opens the caller's vault in its middle, and advances time so the
/// measured call accrues interest before moving the caller to the head.
fn seed_mid_chain_caller<T: Config>() -> Result<MidChainCaller<T>, BenchmarkError> {
	let asset = register_default_branch::<T>()?;
	let seeds = seed_worst_case_chain::<T>(&asset)?;
	let bounds = rate_bounds::<T>(&asset)?;
	let caller = funded_account::<T>("caller", &asset)?;
	let caller_rate = bounds.middle;
	let caller_hint =
		T::VaultLists::find_position(&VaultListId::Rate(asset.clone(), stable::<T>()), caller_rate);
	Pallet::<T>::open_vault(
		RawOrigin::Signed(caller.clone()).into(),
		asset.clone(),
		stable::<T>(),
		balance::<T>(SEED_COLL * 10),
		balance::<T>(SEED_DEBT),
		caller_rate,
		caller_hint,
	)?;
	let head_hint = worst_case_head_hint::<T>(&seeds)?;
	T::BenchmarkHelper::advance_time(ONE_HOUR_MS);
	Ok(MidChainCaller { asset, caller, head_rate: bounds.above, head_hint })
}

/// Redeems `owner`'s debt without paying out collateral, leaving a Dormant vault.
///
/// `payment` sizes the redeemed debt from the step's fresh projection.
fn redeem_debt_only<T: Config>(
	asset: &CollateralIdOf<T>,
	owner: &T::AccountId,
	payment: impl FnOnce(&RedemptionStepSnapshot<BalanceOf<T>>) -> BalanceOf<T>,
) -> Result<(), BenchmarkError> {
	let recipient: T::AccountId = whitelisted_caller();
	let snapshot =
		<Pallet<T> as VaultInterface>::project_redemption_snapshot(asset, &stable::<T>(), owner)?;
	let debt_payment = <T::StableAssets as FungiblesBalanced<T::AccountId>>::issue(
		stable::<T>(),
		payment(&snapshot),
	);
	<Pallet<T> as VaultInterface>::redeem_step(
		asset,
		&stable::<T>(),
		owner,
		&recipient,
		RedemptionSettlement { debt_payment, collateral_to_recipient: BalanceOf::<T>::zero() },
	)?;
	Ok(())
}

/// Open a fresh "only-eligible" vault, drop the oracle so it qualifies for
/// recovery, push it into the FinalRecovery FIFO via `enter_final_recovery`,
/// then restore the oracle.
fn recovery_cycle<T: Config>(
	seed_index: u32,
	asset: &CollateralIdOf<T>,
) -> Result<T::AccountId, BenchmarkError> {
	let owner: T::AccountId = account("rec", seed_index, 0);
	fund_collateral::<T>(asset, &owner, balance::<T>(ACCOUNT_FUNDING))?;
	open_default_vault::<T>(&owner, asset, balance::<T>(RECOVERY_VAULT_COLL))?;
	T::BenchmarkHelper::set_oracle_price(
		asset.clone(),
		FixedU128::saturating_from_integer(RECOVERY_TRIGGER_PRICE),
	);
	let keeper: T::AccountId = whitelisted_caller();
	Pallet::<T>::enter_final_recovery(
		RawOrigin::Signed(keeper).into(),
		asset.clone(),
		stable::<T>(),
		owner.clone(),
	)?;
	T::BenchmarkHelper::set_oracle_price(
		asset.clone(),
		FixedU128::saturating_from_integer(ORACLE_PRICE),
	);
	Ok(owner)
}

/// Open a vault for a fresh account funded with twice its collateral, at its rate-list position.
fn open_liquidation_vault<T: Config>(
	seed: &'static str,
	asset: &CollateralIdOf<T>,
	collateral_units_count: u128,
	debt: u128,
	annual_rate: FixedU128,
) -> Result<T::AccountId, BenchmarkError> {
	let who: T::AccountId = account(seed, 0, 0);
	let collateral = collateral_units::<T>(collateral_units_count);
	fund_collateral::<T>(asset, &who, collateral.saturating_mul(balance::<T>(2)))?;
	let hint =
		T::VaultLists::find_position(&VaultListId::Rate(asset.clone(), stable::<T>()), annual_rate);
	Pallet::<T>::open_vault(
		RawOrigin::Signed(who.clone()).into(),
		asset.clone(),
		stable::<T>(),
		collateral,
		balance::<T>(debt),
		annual_rate,
		hint,
	)?;
	assert!(Vaults::<T>::contains_key((asset, stable::<T>(), &who)));
	Ok(who)
}

/// Seed the Stability Pool so the next offset rolls a due cohort into the active leg and still
/// finds pending capital behind it. Leaves the clock one millisecond before the pending deadline,
/// a full entry delay after the last market accrual.
fn seed_liquidation_pool<T: Config>(asset: &CollateralIdOf<T>) -> Result<(), BenchmarkError> {
	let now = T::TimeProvider::now();
	let active_deadline = T::StabilityPool::benchmark_queue_deposit(
		asset,
		&stable::<T>(),
		0,
		LIQUIDATION_ENTRY_DELAY_MS,
		balance::<T>(LIQUIDATION_LEG_DEBT),
	)?;
	assert!(active_deadline > now);
	// The second deposit lands just before the first deadline, so it opens a later cohort.
	T::BenchmarkHelper::advance_time(active_deadline.saturating_sub(now).saturating_sub(1));
	let pending_deadline = T::StabilityPool::benchmark_queue_deposit(
		asset,
		&stable::<T>(),
		1,
		LIQUIDATION_ENTRY_DELAY_MS,
		balance::<T>(LIQUIDATION_LEG_DEBT),
	)?;
	assert!(pending_deadline > active_deadline);
	let now = T::TimeProvider::now();
	T::BenchmarkHelper::advance_time(pending_deadline.saturating_sub(now).saturating_sub(1));
	let now = T::TimeProvider::now();
	assert!(active_deadline <= now);
	assert!(now < pending_deadline);
	Ok(())
}

/// Collateral the redistribution account holds for vaults that have not yet absorbed it.
/// The market as the engine hands it to the pool, read the way `liquidate` reads it.
fn branch_snapshot<T: Config>(asset: &CollateralIdOf<T>) -> Result<BranchSnapshot, BenchmarkError> {
	let mode = Pallet::<T>::current_mode(asset, &stable::<T>())
		.map_err(|_| BenchmarkError::Stop("branch mode unavailable"))?;
	Ok(BranchSnapshot { mode, now: T::TimeProvider::now() })
}

fn redistribution_held<T: Config>(asset: &CollateralIdOf<T>) -> BalanceOf<T> {
	T::CollateralAssets::balance_on_hold(
		asset.clone(),
		&HoldReason::VaultCollateral.into(),
		&Pallet::<T>::redistribution_account(asset, &stable::<T>()),
	)
}

#[benchmarks]
mod benchmarks {
	use super::*;
	use crate::pallet::Call;

	#[benchmark]
	fn open_vault() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let seeds = seed_worst_case_chain::<T>(&asset)?;
		let caller = funded_account::<T>("caller", &asset)?;
		let collateral = balance::<T>(SEED_COLL);
		let debt = balance::<T>(SEED_DEBT);
		let hint = worst_case_head_hint::<T>(&seeds)?;
		let new_rate = rate_bounds::<T>(&asset)?.above;

		#[extrinsic_call]
		_(
			RawOrigin::Signed(caller.clone()),
			asset.clone(),
			stable::<T>(),
			collateral,
			debt,
			new_rate,
			hint,
		);

		assert!(Vaults::<T>::contains_key((&asset, &stable::<T>(), &caller)));
		Ok(())
	}

	#[benchmark]
	fn deposit_collateral_for() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let owner = funded_account::<T>("owner", &asset)?;
		open_default_vault::<T>(&owner, &asset, balance::<T>(SEED_COLL))?;
		let caller = funded_account::<T>("caller", &asset)?;
		let deposit = balance::<T>(SEED_COLL);
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone(), deposit);

		assert!(Vaults::<T>::contains_key((&asset, &stable::<T>(), &owner)));
		Ok(())
	}

	#[benchmark]
	fn withdraw_collateral() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let caller = funded_account::<T>("caller", &asset)?;
		let initial_coll = balance::<T>(SEED_COLL * 10);
		open_default_vault::<T>(&caller, &asset, initial_coll)?;
		let withdraw = balance::<T>(SEED_COLL);
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);

		#[extrinsic_call]
		_(
			RawOrigin::Signed(caller.clone()),
			asset.clone(),
			stable::<T>(),
			withdraw,
			Some(caller.clone()),
		);

		assert!(Vaults::<T>::contains_key((&asset, &stable::<T>(), &caller)));
		Ok(())
	}

	#[benchmark]
	fn borrow() -> Result<(), BenchmarkError> {
		let MidChainCaller { asset, caller, head_rate, head_hint } = seed_mid_chain_caller::<T>()?;
		let extra_debt = balance::<T>(SEED_DEBT);

		#[extrinsic_call]
		_(
			RawOrigin::Signed(caller.clone()),
			asset.clone(),
			stable::<T>(),
			extra_debt,
			Some(head_rate),
			Some(caller.clone()),
			head_hint,
		);

		assert!(Vaults::<T>::contains_key((&asset, &stable::<T>(), &caller)));
		Ok(())
	}

	#[benchmark]
	fn repay_for() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let owner = funded_account::<T>("owner", &asset)?;
		Pallet::<T>::open_vault(
			RawOrigin::Signed(owner.clone()).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(SEED_COLL),
			balance::<T>(SEED_DEBT * 10),
			rate(5, 100),
			Position::endpoints_only(),
		)?;
		let caller: T::AccountId = whitelisted_caller();
		T::StableAssets::mint_into(stable::<T>(), &caller, balance::<T>(SEED_DEBT * 100))
			.expect("mint pUSD to repay caller");
		let amount = balance::<T>(SEED_DEBT);
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone(), Some(amount));

		assert!(Vaults::<T>::contains_key((&asset, &stable::<T>(), &owner)));
		Ok(())
	}

	#[benchmark]
	fn change_rate() -> Result<(), BenchmarkError> {
		let MidChainCaller { asset, caller, head_rate, head_hint } = seed_mid_chain_caller::<T>()?;

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), asset.clone(), stable::<T>(), head_rate, head_hint);

		assert!(Vaults::<T>::contains_key((&asset, &stable::<T>(), &caller)));
		Ok(())
	}

	#[benchmark]
	fn close_vault() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		// A second vault keeps the branch TCR healthy when the caller's
		// collateral leaves at close: a last-vault close trips the Safety-mode
		// gate on residual aggregate-interest drift.
		let background = funded_account::<T>("background", &asset)?;
		open_default_vault::<T>(&background, &asset, balance::<T>(SEED_COLL))?;
		let caller = funded_account::<T>("caller", &asset)?;
		open_default_vault::<T>(&caller, &asset, balance::<T>(SEED_COLL))?;
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);
		// `close_vault` requires zero debt. Clearing debt while collateral remains
		// (via a full repayment or, as here, a redemption that pays out no
		// collateral) leaves a Dormant husk — zero debt, row intact, collateral
		// still held, out of the rate index — which is the state this extrinsic
		// acts on.
		redeem_debt_only::<T>(&asset, &caller, |snapshot| snapshot.debt)?;

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), asset.clone(), stable::<T>(), None);

		assert!(!Vaults::<T>::contains_key((&asset, &stable::<T>(), &caller)));
		Ok(())
	}

	#[benchmark]
	fn poke() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let owner = funded_account::<T>("owner", &asset)?;
		open_default_vault::<T>(&owner, &asset, balance::<T>(SEED_COLL))?;
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);
		let caller: T::AccountId = whitelisted_caller();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone());

		assert!(Vaults::<T>::contains_key((&asset, &stable::<T>(), &owner)));
		Ok(())
	}

	#[benchmark]
	fn enter_final_recovery() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let _prior = recovery_cycle::<T>(0, &asset)?;
		let owner = funded_account::<T>("target", &asset)?;
		open_default_vault::<T>(&owner, &asset, balance::<T>(RECOVERY_VAULT_COLL))?;
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);
		T::BenchmarkHelper::set_oracle_price(
			asset.clone(),
			FixedU128::saturating_from_integer(RECOVERY_TRIGGER_PRICE),
		);
		// A funded keeper can receive the entry reward, so the payout leg is measured.
		let caller = funded_account::<T>("keeper", &asset)?;
		let caller_before = <T::CollateralAssets as FungiblesInspect<T::AccountId>>::balance(
			asset.clone(),
			&caller,
		);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), asset.clone(), stable::<T>(), owner.clone());

		assert_eq!(
			Pallet::<T>::vault_status(asset.clone(), stable::<T>(), owner),
			Some(VaultStatus::FinalRecovery)
		);
		assert!(
			<T::CollateralAssets as FungiblesInspect<T::AccountId>>::balance(asset, &caller) >
				caller_before,
			"the entry paid its keeper"
		);
		Ok(())
	}

	#[benchmark]
	fn exit_final_recovery() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let _a = recovery_cycle::<T>(0, &asset)?;
		let owner = recovery_cycle::<T>(1, &asset)?;
		let _c = recovery_cycle::<T>(2, &asset)?;
		let seeds = seed_worst_case_chain::<T>(&asset)?;
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);
		let caller: T::AccountId = whitelisted_caller();
		let hint = worst_case_head_hint::<T>(&seeds)?;

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone(), hint);

		assert_eq!(
			Pallet::<T>::vault_status(asset, stable::<T>(), owner),
			Some(VaultStatus::Active)
		);
		Ok(())
	}

	#[benchmark]
	fn activate_dormant() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		// A background vault keeps the branch alive while the target is dormant.
		let background = funded_account::<T>("background", &asset)?;
		open_default_vault::<T>(&background, &asset, balance::<T>(SEED_COLL))?;
		let owner = funded_account::<T>("target", &asset)?;
		open_default_vault::<T>(&owner, &asset, balance::<T>(SEED_COLL))?;
		// Redeem the target to just below `minimum_debt`, leaving a debt-bearing
		// Dormant vault outside the rate index.
		let remaining = balance::<T>(199);
		redeem_debt_only::<T>(&asset, &owner, |snapshot| snapshot.debt.saturating_sub(remaining))?;
		assert_eq!(
			Pallet::<T>::vault_status(asset.clone(), stable::<T>(), owner.clone()),
			Some(VaultStatus::Dormant)
		);
		// Accrue interest until the fully-accrued debt is back at/above
		// `minimum_debt`, so the vault is activation-eligible.
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS.saturating_mul(24 * 365 * 2));
		let hint = T::VaultLists::find_position(
			&VaultListId::Rate(asset.clone(), stable::<T>()),
			rate(5, 100),
		);
		let caller: T::AccountId = whitelisted_caller();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone(), hint);

		assert_eq!(
			Pallet::<T>::vault_status(asset, stable::<T>(), owner),
			Some(VaultStatus::Active)
		);
		Ok(())
	}

	#[benchmark]
	fn nominate_dormant() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let owner: T::AccountId = account("husk", 0, 0);
		// Two repaid vaults split a liquidation into sub-minimum debt. Leave the allocation
		// pending so nomination measures the collateral transfer as well as interest minting.
		for who in [owner.clone(), account("husk", 1, 0)] {
			fund_collateral::<T>(&asset, &who, balance::<T>(ACCOUNT_FUNDING))?;
			open_default_vault::<T>(&who, &asset, balance::<T>(SEED_COLL))?;
			T::StableAssets::mint_into(stable::<T>(), &who, balance::<T>(SEED_DEBT))?;
			Pallet::<T>::repay_for(
				RawOrigin::Signed(who.clone()).into(),
				asset.clone(),
				stable::<T>(),
				who,
				None,
			)?;
		}
		let victim = funded_account::<T>("victim", &asset)?;
		open_default_vault::<T>(&victim, &asset, balance::<T>(RECOVERY_VAULT_COLL))?;
		T::BenchmarkHelper::set_oracle_price(
			asset.clone(),
			FixedU128::saturating_from_integer(RECOVERY_TRIGGER_PRICE),
		);
		let caller: T::AccountId = whitelisted_caller();
		Pallet::<T>::liquidate(
			RawOrigin::Signed(caller.clone()).into(),
			asset.clone(),
			stable::<T>(),
			victim,
			JitTerms {
				max_stable: BalanceOf::<T>::zero(),
				min_collateral_out: BalanceOf::<T>::zero(),
			},
		)?;
		T::BenchmarkHelper::advance_time(pusd_primitives::MILLIS_PER_YEAR);
		let before = Pallet::<T>::vault_of(&asset, &stable::<T>(), &owner)?;
		assert!(before.debt.total().is_zero());

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone());

		let branch = Pallet::<T>::branch_of(&asset, &stable::<T>())?;
		let after = Pallet::<T>::vault_of(&asset, &stable::<T>(), &owner)?;
		assert_eq!(branch.state.dormant_redemption_target, Some(owner));
		assert!(!after.debt.principal.is_zero());
		assert!(!after.debt.interest.is_zero());
		assert!(after.debt.total() < branch.config.minimum_debt);
		assert!(after.collateral > before.collateral);
		Ok(())
	}

	#[benchmark]
	fn create_branch() -> Result<(), BenchmarkError> {
		let asset = T::BenchmarkHelper::collateral_asset_id();
		let config = default_branch_config::<T>();
		let admins = branch_admins::<T>();
		T::BenchmarkHelper::set_oracle_price(
			asset.clone(),
			FixedU128::saturating_from_integer(ORACLE_PRICE),
		);
		let origin = create_origin::<T>()?;
		fund_collateral::<T>(
			&asset,
			&branch_admin_accounts::<T>().0,
			balance::<T>(ACCOUNT_FUNDING),
		)?;

		#[extrinsic_call]
		_(
			origin,
			asset.clone(),
			stable::<T>(),
			admins,
			config,
			first_market_lifecycle_config::<T>(),
		);

		assert!(Branches::<T>::contains_key(&asset, &stable::<T>()));
		Ok(())
	}

	#[benchmark]
	fn remove_branch() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let origin = full_admin_origin::<T>();

		#[extrinsic_call]
		_(origin, asset.clone(), stable::<T>());

		assert!(!Branches::<T>::contains_key(&asset, &stable::<T>()));
		Ok(())
	}

	#[benchmark]
	fn set_branch_admins() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let origin = full_admin_origin::<T>();
		let new_full: T::AccountId = account("new_full_admin", 0, 0);
		let new_emergency: T::AccountId = account("new_emergency_admin", 0, 0);
		let admins = BranchAdmins {
			full_admin: T::Lookup::unlookup(new_full.clone()),
			emergency_admin: T::Lookup::unlookup(new_emergency.clone()),
		};

		#[extrinsic_call]
		_(origin, asset.clone(), stable::<T>(), admins);

		let branch =
			Branches::<T>::get(&asset, &stable::<T>()).expect("branch present after register");
		assert_eq!(
			branch.admins,
			BranchAdmins { full_admin: new_full, emergency_admin: new_emergency }
		);
		Ok(())
	}

	#[benchmark]
	fn set_global_debt_ceiling() -> Result<(), BenchmarkError> {
		let stable_id = stable::<T>();
		let origin = create_origin::<T>()?;
		let ceiling = balance::<T>(GLOBAL_CEILING);

		#[extrinsic_call]
		_(origin, stable_id.clone(), ceiling);

		assert_eq!(GlobalDebtCeilings::<T>::get(&stable_id), ceiling);
		Ok(())
	}

	#[benchmark]
	fn refresh_branch() -> Result<(), BenchmarkError> {
		let (asset, _owner) = seed_idle_market::<T>()?;
		// A year of accrual, then a dead oracle: the refresh takes its
		// heaviest path — freeze, flush the aggregate interest, mint and
		// route the yield.
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS.saturating_mul(24 * 365));
		T::BenchmarkHelper::clear_oracle_price(asset.clone());
		let caller: T::AccountId = whitelisted_caller();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>());

		let branch = Branches::<T>::get(&asset, &stable::<T>()).expect("branch registered above");
		assert!(branch.state.frozen.is_some(), "oracle failure froze the branch");
		Ok(())
	}

	#[benchmark]
	fn set_param() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let origin = full_admin_origin::<T>();
		// A raise, but not past the market's 120% borrow ratio: a liquidation ratio above it
		// would open every new vault liquidatable, which the config check rejects.
		let new_value = rate(115, 100);

		#[extrinsic_call]
		set_param(
			origin,
			asset,
			stable::<T>(),
			BranchConfigUpdate::MinimumCollateralizationRatio(new_value),
		);

		Ok(())
	}

	#[benchmark]
	fn set_governance_frozen() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let origin = full_admin_origin::<T>();

		// Freezing is the heavier direction: it flushes pending aggregate
		// interest and mints/routes the yield.
		#[extrinsic_call]
		_(origin, asset.clone(), stable::<T>(), true);

		let branch =
			Branches::<T>::get(&asset, &stable::<T>()).expect("branch present after register");
		assert!(branch.state.frozen.is_some());
		Ok(())
	}

	/// Worst-case liquidation: every waterfall leg is non-zero and every lazy settlement is due.
	///
	/// - The active pool leg first rolls a due cohort, then is depleted into a new epoch.
	/// - The keeper burns JIT stablecoin and receives its share and the capped reward.
	/// - The pending pool leg is depleted behind it; redistribution takes the remainder.
	/// - The victim first absorbs a pending redistribution share and a day of interest, whose mint
	///   routes yield through the pool.
	/// - The victim sits mid-list, so its removal relinks both neighbours.
	#[benchmark]
	fn liquidate() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		T::BenchmarkHelper::set_oracle_price(asset.clone(), unit_price::<T>(ORACLE_PRICE));
		let peer_debt = LIQUIDATION_PEER_DEBT;
		let holder_units = LIQUIDATION_HOLDER_COLL_UNITS;
		open_liquidation_vault::<T>("holder_high", &asset, holder_units, peer_debt, rate(8, 100))?;
		let victim = open_liquidation_vault::<T>(
			"victim",
			&asset,
			LIQUIDATION_VICTIM_COLL_UNITS,
			LIQUIDATION_VICTIM_DEBT,
			rate(5, 100),
		)?;
		open_liquidation_vault::<T>("holder_low", &asset, holder_units, peer_debt, rate(2, 100))?;
		let sacrifice_units = LIQUIDATION_SACRIFICE_COLL_UNITS;
		let sacrifice = open_liquidation_vault::<T>(
			"sacrifice",
			&asset,
			sacrifice_units,
			peer_debt,
			rate(1, 100),
		)?;
		let keeper: T::AccountId = whitelisted_caller();
		fund_collateral::<T>(&asset, &keeper, collateral_units::<T>(ACCOUNT_FUNDING))?;
		let jit = balance::<T>(LIQUIDATION_LEG_DEBT);
		T::StableAssets::mint_into(stable::<T>(), &keeper, jit.saturating_mul(balance::<T>(2)))?;

		// With the pool still empty, the sacrifice redistributes in full, leaving the victim a
		// pending share to absorb when it is touched.
		T::BenchmarkHelper::set_oracle_price(asset.clone(), unit_price::<T>(LIQUIDATION_PRICE));
		let no_jit = JitTerms { max_stable: Zero::zero(), min_collateral_out: Zero::zero() };
		Pallet::<T>::liquidate(
			RawOrigin::Signed(keeper.clone()).into(),
			asset.clone(),
			stable::<T>(),
			sacrifice,
			no_jit,
		)?;
		assert!(!redistribution_held::<T>(&asset).is_zero());

		seed_liquidation_pool::<T>(&asset)?;
		// Re-quote after the clock moved, so a runtime with a staleness bound accepts the price.
		T::BenchmarkHelper::set_oracle_price(asset.clone(), unit_price::<T>(LIQUIDATION_PRICE));
		let branch = branch_snapshot::<T>(&asset)?;
		let active_quote = T::StabilityPool::reducible_active(&asset, &stable::<T>(), branch, jit);
		let pending_quote =
			T::StabilityPool::reducible_pending(&asset, &stable::<T>(), branch, jit, active_quote);
		assert_eq!(active_quote, jit);
		assert_eq!(pending_quote, jit);
		let victim_debt = Vaults::<T>::get((&asset, stable::<T>(), &victim))
			.ok_or(BenchmarkError::Stop("victim vault missing"))?
			.vault
			.debt
			.total();
		assert!(victim_debt > jit.saturating_mul(balance::<T>(3)), "redistribution leg is empty");
		let keeper_stable = T::StableAssets::balance(stable::<T>(), &keeper);
		let keeper_collateral = T::CollateralAssets::balance(asset.clone(), &keeper);
		let held = redistribution_held::<T>(&asset);

		#[extrinsic_call]
		_(
			RawOrigin::Signed(keeper.clone()),
			asset.clone(),
			stable::<T>(),
			victim.clone(),
			JitTerms { max_stable: jit, min_collateral_out: BalanceOf::<T>::zero() },
		);

		assert!(!Vaults::<T>::contains_key((&asset, stable::<T>(), &victim)));
		let max = balance::<T>(u128::MAX);
		let branch = branch_snapshot::<T>(&asset)?;
		let active_left = T::StabilityPool::reducible_active(&asset, &stable::<T>(), branch, max);
		assert!(active_left.is_zero());
		let pending_left =
			T::StabilityPool::reducible_pending(&asset, &stable::<T>(), branch, max, max);
		assert!(pending_left.is_zero());
		let stable_after = T::StableAssets::balance(stable::<T>(), &keeper);
		assert_eq!(keeper_stable.saturating_sub(stable_after), jit);
		assert!(T::CollateralAssets::balance(asset.clone(), &keeper) > keeper_collateral);
		assert!(redistribution_held::<T>(&asset) != held);
		Ok(())
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
