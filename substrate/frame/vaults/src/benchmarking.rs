//! Benchmarks for `pallet-vaults`. Rate-index dispatchables feed the
//! linked-list a hint that is exactly `hint_repair_budget` steps stale, so
//! the worst-case repair walk is what gets measured.

#![cfg(feature = "runtime-benchmarks")]

use crate::{
	pallet::{
		BalanceOf, BranchConfigs, BranchStates, Config, HoldReason, Pallet, PalletsOriginOf, Vaults,
	},
	types::{BranchAdmins, BranchConfig, BranchConfigUpdate, VaultListId, VaultStatus},
	BenchmarkHelper as _,
};
use alloc::vec::Vec;
use frame::{
	benchmarking::prelude::*,
	deps::{
		frame_support::traits::{
			fungibles::{Mutate as FungiblesMutate, MutateHold as FungiblesMutateHold},
			EnsureOrigin, EnsureOriginWithArg,
		},
		sp_runtime::{
			traits::{SaturatedConversion, Zero},
			FixedU128, Permill,
		},
	},
};
use frame_system::RawOrigin;
use pallet_linked_list::{Position, SortedListInterface};
use pusd_primitives::{RedemptionAllocation, VaultRedemptionInterface};

const ORACLE_PRICE: u128 = 10;
/// High per-collateral global ceiling so the systemic cap never binds in benches.
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
const REDIST_PER_STAKE_NUM: u128 = 1;
const REDIST_PER_STAKE_DEN: u128 = 100;
const REDIST_WEIGHT_PER_STAKE_NUM: u128 = 1;
const REDIST_WEIGHT_PER_STAKE_DEN: u128 = 10_000;
const REDIST_PRESTAGE_COLL: u128 = 10_000_000;

fn stable<T: Config>() -> T::StableAssetId {
	T::BenchmarkHelper::stable_asset_id()
}

fn balance<T: Config>(value: u128) -> BalanceOf<T> {
	value.saturated_into()
}

fn rate(numerator: u128, denominator: u128) -> FixedU128 {
	FixedU128::from_rational(numerator, denominator)
}

fn default_branch_config<T: Config>() -> BranchConfig<BalanceOf<T>> {
	const DAY_MS: u64 = 24 * 3_600 * 1_000;
	BranchConfig {
		minimum_collateralization_ratio: rate(110, 100),
		initial_collateralization_ratio: rate(120, 100),
		safety_collateralization_ratio: rate(130, 100),
		debt_ceiling: balance::<T>(100_000_000),
		minimum_debt: balance::<T>(200),
		minimum_collateral: balance::<T>(1),
		minimum_borrow_rate: rate(1, 1_000),
		maximum_borrow_rate: rate(100, 100),
		upfront_fee_period: 7 * DAY_MS,
		rate_adjustment_cooldown: DAY_MS,
		redistribution_penalty: Permill::from_percent(5),
		ceiling_gap: balance::<T>(0),
		ceiling_ttl: 0,
	}
}

/// The accounts acting as full and emergency admin of every benchmarked market.
fn branch_admin_accounts<T: Config>() -> (T::AccountId, T::AccountId) {
	(account("full_admin", 0, 0), account("emergency_admin", 0, 0))
}

/// The admin bundle every benchmarked market is created with.
fn branch_admins<T: Config>() -> BranchAdmins<PalletsOriginOf<T>> {
	let (full_admin, emergency_admin) = branch_admin_accounts::<T>();
	BranchAdmins {
		full_admin: RawOrigin::Signed(full_admin).into(),
		emergency_admin: RawOrigin::Signed(emergency_admin).into(),
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

fn global_manager_origin<T: Config>() -> Result<T::RuntimeOrigin, BenchmarkError> {
	T::GlobalManagerOrigin::try_successful_origin()
		.map_err(|_| BenchmarkError::Stop("global manager origin unavailable"))
}

fn register_default_branch<T: Config>() -> Result<T::CollateralAssetId, BenchmarkError> {
	let asset = T::BenchmarkHelper::collateral_asset_id();
	// `create_branch` validates the oracle price, so set it first.
	T::BenchmarkHelper::set_oracle_price(
		asset.clone(),
		stable::<T>(),
		FixedU128::saturating_from_integer(ORACLE_PRICE),
	);
	Pallet::<T>::create_branch(
		create_origin::<T>()?,
		asset.clone(),
		stable::<T>(),
		branch_admins::<T>(),
		default_branch_config::<T>(),
	)?;
	Pallet::<T>::set_global_debt_ceiling(
		global_manager_origin::<T>()?,
		asset.clone(),
		balance::<T>(GLOBAL_CEILING),
	)?;
	Ok(asset)
}

fn funded_account<T: Config>(seed: &'static str, asset: &T::CollateralAssetId) -> T::AccountId {
	let who: T::AccountId = account(seed, 0, 0);
	T::BenchmarkHelper::mint_collateral(asset.clone(), &who, balance::<T>(ACCOUNT_FUNDING));
	who
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

fn rate_bounds<T: Config>(asset: &T::CollateralAssetId) -> Result<RateBounds, BenchmarkError> {
	let config = Pallet::<T>::branch_config_of(asset, &stable::<T>())
		.map_err(|_| BenchmarkError::Stop("missing branch config"))?;
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
	asset: &T::CollateralAssetId,
) -> Result<Vec<T::AccountId>, BenchmarkError> {
	let count = T::VaultLists::repair_budget().saturating_add(2);
	let mut owners = Vec::with_capacity(count as usize);
	let bounds = rate_bounds::<T>(asset)?;
	for i in 0..count {
		let offset = bounds.step.saturating_mul(FixedU128::saturating_from_integer(i));
		let r = bounds.base.saturating_sub(offset);
		let who: T::AccountId = account("seed", i, 0);
		T::BenchmarkHelper::mint_collateral(asset.clone(), &who, balance::<T>(ACCOUNT_FUNDING));
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

/// Plant a non-trivial branch redistribution snapshot so every vault touch
/// enters the `snap != redistribution` branch with non-zero `redistribution_collateral` and
/// `redistribution_debt_principal`.
fn seed_pending_redistribution<T: Config>(
	asset: &T::CollateralAssetId,
) -> Result<(), BenchmarkError> {
	let per_stake = rate(REDIST_PER_STAKE_NUM, REDIST_PER_STAKE_DEN);
	let weight_per_stake = rate(REDIST_WEIGHT_PER_STAKE_NUM, REDIST_WEIGHT_PER_STAKE_DEN);

	let redistribution_account_id = Pallet::<T>::redistribution_account(asset, &stable::<T>());
	let pre_stage = balance::<T>(REDIST_PRESTAGE_COLL);
	T::BenchmarkHelper::mint_collateral(asset.clone(), &redistribution_account_id, pre_stage);
	T::CollateralAssets::hold(
		asset.clone(),
		&HoldReason::VaultCollateral.into(),
		&redistribution_account_id,
		pre_stage,
	)
	.map_err(|_| BenchmarkError::Stop("hold on redistribution account failed"))?;

	BranchStates::<T>::try_mutate(asset, &stable::<T>(), |maybe| -> Result<(), BenchmarkError> {
		let state = maybe.as_mut().ok_or(BenchmarkError::Stop("branch missing"))?;
		state.redistribution.debt_per_stake = per_stake;
		state.redistribution.collateral_per_stake = per_stake;
		state.redistribution.weight_per_stake = weight_per_stake;
		state.redistribution.debt_time_per_stake = FixedU128::zero();
		state.debt.pending_redistribution_principal =
			per_stake.saturating_mul_int(state.stakes.total);
		Ok(())
	})
}

/// Open a fresh "only-eligible" vault, drop the oracle so it qualifies for
/// recovery, push it into the FinalRecovery FIFO via `enter_final_recovery`,
/// then restore the oracle.
fn recovery_cycle<T: Config>(
	seed_index: u32,
	asset: &T::CollateralAssetId,
) -> Result<T::AccountId, BenchmarkError> {
	let owner: T::AccountId = account("rec", seed_index, 0);
	T::BenchmarkHelper::mint_collateral(asset.clone(), &owner, balance::<T>(ACCOUNT_FUNDING));
	Pallet::<T>::open_vault(
		RawOrigin::Signed(owner.clone()).into(),
		asset.clone(),
		stable::<T>(),
		balance::<T>(RECOVERY_VAULT_COLL),
		balance::<T>(SEED_DEBT),
		rate(5, 100),
		Position::endpoints_only(),
	)?;
	T::BenchmarkHelper::set_oracle_price(
		asset.clone(),
		stable::<T>(),
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
		stable::<T>(),
		FixedU128::saturating_from_integer(ORACLE_PRICE),
	);
	Ok(owner)
}

fn prefill_branches<T: Config>(count: u32) {
	// The registry is the `BranchConfigs` key set, and its counted variant tracks
	// the capacity gate, so seeding configs is enough to fill the registry.
	for seed in 0..count {
		let (collateral, stable) = T::BenchmarkHelper::synth_market(seed);
		BranchConfigs::<T>::insert((collateral, stable), default_branch_config::<T>());
	}
}

#[benchmarks]
mod benchmarks {
	use super::*;
	use crate::pallet::Call;

	#[benchmark]
	fn open_vault() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let seeds = seed_worst_case_chain::<T>(&asset)?;
		let caller = funded_account::<T>("caller", &asset);
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
		let owner = funded_account::<T>("owner", &asset);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(owner.clone()).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(SEED_COLL),
			balance::<T>(SEED_DEBT),
			rate(5, 100),
			Position::endpoints_only(),
		)?;
		let caller = funded_account::<T>("caller", &asset);
		let deposit = balance::<T>(SEED_COLL);
		seed_pending_redistribution::<T>(&asset)?;
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone(), deposit);

		assert!(Vaults::<T>::contains_key((&asset, &stable::<T>(), &owner)));
		Ok(())
	}

	#[benchmark]
	fn withdraw_collateral() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let caller = funded_account::<T>("caller", &asset);
		let initial_coll = balance::<T>(SEED_COLL * 10);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(caller.clone()).into(),
			asset.clone(),
			stable::<T>(),
			initial_coll,
			balance::<T>(SEED_DEBT),
			rate(5, 100),
			Position::endpoints_only(),
		)?;
		let withdraw = balance::<T>(SEED_COLL);
		seed_pending_redistribution::<T>(&asset)?;
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
		let asset = register_default_branch::<T>()?;
		let seeds = seed_worst_case_chain::<T>(&asset)?;
		let bounds = rate_bounds::<T>(&asset)?;
		let caller = funded_account::<T>("caller", &asset);
		let caller_rate = bounds.middle;
		let caller_hint = T::VaultLists::find_position(
			&VaultListId::Rate(asset.clone(), stable::<T>()),
			caller_rate,
		);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(caller.clone()).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(SEED_COLL * 10),
			balance::<T>(SEED_DEBT),
			caller_rate,
			caller_hint,
		)?;
		let extra_debt = balance::<T>(SEED_DEBT);
		let new_rate = Some(bounds.above);
		let hint = worst_case_head_hint::<T>(&seeds)?;
		seed_pending_redistribution::<T>(&asset)?;
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);

		#[extrinsic_call]
		_(
			RawOrigin::Signed(caller.clone()),
			asset.clone(),
			stable::<T>(),
			extra_debt,
			new_rate,
			Some(caller.clone()),
			hint,
		);

		assert!(Vaults::<T>::contains_key((&asset, &stable::<T>(), &caller)));
		Ok(())
	}

	#[benchmark]
	fn repay_for() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let owner = funded_account::<T>("owner", &asset);
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
		seed_pending_redistribution::<T>(&asset)?;
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone(), amount);

		assert!(Vaults::<T>::contains_key((&asset, &stable::<T>(), &owner)));
		Ok(())
	}

	#[benchmark]
	fn change_rate() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let seeds = seed_worst_case_chain::<T>(&asset)?;
		let bounds = rate_bounds::<T>(&asset)?;
		let caller = funded_account::<T>("caller", &asset);
		let caller_rate = bounds.middle;
		let caller_hint = T::VaultLists::find_position(
			&VaultListId::Rate(asset.clone(), stable::<T>()),
			caller_rate,
		);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(caller.clone()).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(SEED_COLL * 10),
			balance::<T>(SEED_DEBT),
			caller_rate,
			caller_hint,
		)?;
		let new_rate = bounds.above;
		let hint = worst_case_head_hint::<T>(&seeds)?;
		seed_pending_redistribution::<T>(&asset)?;
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), asset.clone(), stable::<T>(), new_rate, hint);

		assert!(Vaults::<T>::contains_key((&asset, &stable::<T>(), &caller)));
		Ok(())
	}

	#[benchmark]
	fn close_vault() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		// A second vault keeps the branch TCR healthy when the caller's
		// collateral leaves at close: a last-vault close trips the Safety-mode
		// gate on residual aggregate-interest drift.
		let background = funded_account::<T>("background", &asset);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(background.clone()).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(SEED_COLL),
			balance::<T>(SEED_DEBT),
			rate(5, 100),
			Position::endpoints_only(),
		)?;
		let caller = funded_account::<T>("caller", &asset);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(caller.clone()).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(SEED_COLL),
			balance::<T>(SEED_DEBT),
			rate(5, 100),
			Position::endpoints_only(),
		)?;
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);
		// `close_vault` requires zero debt. Clearing debt (via a full repayment or,
		// as here, a full redemption) leaves a Dormant husk — zero debt, row
		// intact, collateral still held, out of the rate index — which is the
		// state this extrinsic acts on.
		let redeemer: T::AccountId = whitelisted_caller();
		<Pallet<T> as VaultRedemptionInterface<
			T::CollateralAssetId,
			T::StableAssetId,
			T::AccountId,
			BalanceOf<T>,
		>>::redeem_step(&asset, &stable::<T>(), &caller, |snapshot| {
			Ok(Some(RedemptionAllocation {
				redeemer,
				debt_to_cancel: snapshot.debt,
				collateral_to_redeemer: BalanceOf::<T>::zero(),
				fee_collateral_retained: BalanceOf::<T>::zero(),
			}))
		})?;

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), asset.clone(), stable::<T>(), None);

		assert!(!Vaults::<T>::contains_key((&asset, &stable::<T>(), &caller)));
		Ok(())
	}

	#[benchmark]
	fn poke() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let owner = funded_account::<T>("owner", &asset);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(owner.clone()).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(SEED_COLL),
			balance::<T>(SEED_DEBT),
			rate(5, 100),
			Position::endpoints_only(),
		)?;
		seed_pending_redistribution::<T>(&asset)?;
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
		let owner = funded_account::<T>("target", &asset);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(owner.clone()).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(RECOVERY_VAULT_COLL),
			balance::<T>(SEED_DEBT),
			rate(5, 100),
			Position::endpoints_only(),
		)?;
		seed_pending_redistribution::<T>(&asset)?;
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);
		T::BenchmarkHelper::set_oracle_price(
			asset.clone(),
			stable::<T>(),
			FixedU128::saturating_from_integer(RECOVERY_TRIGGER_PRICE),
		);
		let caller: T::AccountId = whitelisted_caller();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone());

		assert_eq!(
			Pallet::<T>::vault_status(asset, stable::<T>(), owner),
			Some(VaultStatus::FinalRecovery)
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
		seed_pending_redistribution::<T>(&asset)?;
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
		let background = funded_account::<T>("background", &asset);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(background).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(SEED_COLL),
			balance::<T>(SEED_DEBT),
			rate(5, 100),
			Position::endpoints_only(),
		)?;
		let owner = funded_account::<T>("target", &asset);
		let owner_rate = rate(5, 100);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(owner.clone()).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(SEED_COLL),
			balance::<T>(SEED_DEBT),
			owner_rate,
			Position::endpoints_only(),
		)?;
		// Redeem the target to just below `minimum_debt`, leaving a debt-bearing
		// Dormant vault outside the rate index.
		let remaining = balance::<T>(199);
		let redeemer: T::AccountId = whitelisted_caller();
		<Pallet<T> as VaultRedemptionInterface<
			T::CollateralAssetId,
			T::StableAssetId,
			T::AccountId,
			BalanceOf<T>,
		>>::redeem_step(&asset, &stable::<T>(), &owner, |snapshot| {
			Ok(Some(RedemptionAllocation {
				redeemer,
				debt_to_cancel: snapshot.debt.saturating_sub(remaining),
				collateral_to_redeemer: BalanceOf::<T>::zero(),
				fee_collateral_retained: BalanceOf::<T>::zero(),
			}))
		})?;
		assert_eq!(
			Pallet::<T>::vault_status(asset.clone(), stable::<T>(), owner.clone()),
			Some(VaultStatus::Dormant)
		);
		// Accrue interest until the fully-accrued debt is back at/above
		// `minimum_debt`, so the vault is activation-eligible.
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS.saturating_mul(24 * 365 * 2));
		let hint = T::VaultLists::find_position(
			&VaultListId::Rate(asset.clone(), stable::<T>()),
			owner_rate,
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
	fn register_branch() -> Result<(), BenchmarkError> {
		let prefill = <T::MaxBranches as Get<u32>>::get().saturating_sub(1);
		prefill_branches::<T>(prefill);
		let asset = T::BenchmarkHelper::collateral_asset_id();
		let config = default_branch_config::<T>();
		let admins = branch_admins::<T>();
		T::BenchmarkHelper::set_oracle_price(
			asset.clone(),
			stable::<T>(),
			FixedU128::saturating_from_integer(ORACLE_PRICE),
		);
		let origin = create_origin::<T>()?;

		#[extrinsic_call]
		create_branch(origin, asset.clone(), stable::<T>(), admins, config);

		assert!(BranchStates::<T>::contains_key(&asset, &stable::<T>()));
		Ok(())
	}

	#[benchmark]
	fn set_param() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let origin = full_admin_origin::<T>();
		let new_value = rate(150, 100);

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
	fn enable_frozen_mode() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let origin = full_admin_origin::<T>();

		#[extrinsic_call]
		_(origin, asset.clone(), stable::<T>());

		let state = BranchStates::<T>::get(&asset, &stable::<T>())
			.expect("branch state present after register");
		assert!(state.frozen.is_some());
		Ok(())
	}

	#[benchmark]
	fn on_idle_one_vault() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let owner = funded_account::<T>("owner", &asset);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(owner.clone()).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(SEED_COLL),
			balance::<T>(SEED_DEBT),
			rate(5, 100),
			Position::endpoints_only(),
		)?;
		seed_pending_redistribution::<T>(&asset)?;
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);

		#[block]
		{
			if Vaults::<T>::contains_key((&asset, &stable::<T>(), &owner)) {
				let _ =
					crate::context::OpContext::<T>::refresh(asset.clone(), stable::<T>(), &owner);
				let _ = T::VaultLists::neighbors(
					&VaultListId::Rate(asset.clone(), stable::<T>()),
					&owner,
				);
			}
		}

		assert!(Vaults::<T>::contains_key((&asset, &stable::<T>(), &owner)));
		Ok(())
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
