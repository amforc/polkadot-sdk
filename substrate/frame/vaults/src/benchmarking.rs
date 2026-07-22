//! Benchmarks for `pallet-vaults`. Rate-index dispatchables feed the
//! linked-list a hint that is exactly `hint_repair_budget` steps stale, so
//! the worst-case repair walk is what gets measured.

#![cfg(feature = "runtime-benchmarks")]

use crate::{
	pallet::{
		AccountIdLookupOf, BalanceOf, BranchIdleCursor, Branches, CollateralIdOf, CollateralRisks,
		Config, HoldReason, IdleCursor, Pallet, StableIdOf, Vaults,
	},
	types::{BranchAdmins, BranchConfig, BranchConfigUpdate, VaultListId, VaultStatus},
	BenchmarkHelper as _,
};
use alloc::vec::Vec;
use frame::{
	arithmetic::{FixedU128, Permill},
	benchmarking::prelude::*,
	traits::{
		fungibles::{
			Balanced as FungiblesBalanced, Mutate as FungiblesMutate,
			MutateHold as FungiblesMutateHold,
		},
		EnsureOrigin, EnsureOriginWithArg, SaturatedConversion, Zero,
	},
};
use frame_system::RawOrigin;
use pallet_linked_list::{Position, SortedListInterface};
use pusd_primitives::{RedemptionSettlement, VaultInterface};

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

fn stable<T: Config>() -> StableIdOf<T> {
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

fn force_origin<T: Config>() -> Result<T::RuntimeOrigin, BenchmarkError> {
	T::ForceOrigin::try_successful_origin()
		.map_err(|_| BenchmarkError::Stop("force origin unavailable"))
}

fn register_default_branch<T: Config>() -> Result<CollateralIdOf<T>, BenchmarkError> {
	let asset = T::BenchmarkHelper::collateral_asset_id();
	// `create_branch` validates the oracle price, so set it first.
	T::BenchmarkHelper::set_oracle_price(
		asset.clone(),
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
		force_origin::<T>()?,
		asset.clone(),
		balance::<T>(GLOBAL_CEILING),
	)?;
	Ok(asset)
}

fn funded_account<T: Config>(seed: &'static str, asset: &CollateralIdOf<T>) -> T::AccountId {
	let who: T::AccountId = account(seed, 0, 0);
	T::BenchmarkHelper::mint_collateral(asset.clone(), &who, balance::<T>(ACCOUNT_FUNDING));
	who
}

/// Register the default market and open one vault in it. Returns the market's collateral id and the
/// vault owner.
fn seed_idle_market<T: Config>() -> Result<(CollateralIdOf<T>, T::AccountId), BenchmarkError> {
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
fn seed_pending_redistribution<T: Config>(asset: &CollateralIdOf<T>) -> Result<(), BenchmarkError> {
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

	// Seed through the audited boundary so `CollateralRisks` stays true.
	let mut branch = Pallet::<T>::branch_of(asset, &stable::<T>())
		.map_err(|_| BenchmarkError::Stop("branch missing"))?;
	let outstanding_before = branch.state.debt.outstanding();
	let state = &mut branch.state;
	state.redistribution.debt_per_stake = per_stake;
	state.redistribution.collateral_per_stake = per_stake;
	state.redistribution.weight_per_stake = weight_per_stake;
	state.redistribution.debt_time_per_stake = FixedU128::zero();
	state.debt.pending_redistribution_principal = per_stake.saturating_mul_int(state.stakes.total);
	Pallet::<T>::commit_branch(asset, &stable::<T>(), outstanding_before, branch)
		.map_err(|_| BenchmarkError::Stop("branch commit failed"))
}

/// Open a fresh "only-eligible" vault, drop the oracle so it qualifies for
/// recovery, push it into the FinalRecovery FIFO via `enter_final_recovery`,
/// then restore the oracle.
fn recovery_cycle<T: Config>(
	seed_index: u32,
	asset: &CollateralIdOf<T>,
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
		let recipient: T::AccountId = whitelisted_caller();
		<Pallet<T> as VaultInterface>::redeem_step(
			&asset,
			&stable::<T>(),
			&caller,
			&recipient,
			|snapshot| {
				// Freshly issued inside the closure per the credit contract.
				let debt_payment = <T::StableAssets as FungiblesBalanced<T::AccountId>>::issue(
					stable::<T>(),
					snapshot.debt,
				);
				Ok(Some(RedemptionSettlement {
					debt_payment,
					collateral_to_recipient: BalanceOf::<T>::zero(),
				}))
			},
		)?;

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
		let recipient: T::AccountId = whitelisted_caller();
		<Pallet<T> as VaultInterface>::redeem_step(
			&asset,
			&stable::<T>(),
			&owner,
			&recipient,
			|snapshot| {
				// Freshly issued inside the closure per the credit contract.
				let debt_payment = <T::StableAssets as FungiblesBalanced<T::AccountId>>::issue(
					stable::<T>(),
					snapshot.debt.saturating_sub(remaining),
				);
				Ok(Some(RedemptionSettlement {
					debt_payment,
					collateral_to_recipient: BalanceOf::<T>::zero(),
				}))
			},
		)?;
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
	fn create_branch() -> Result<(), BenchmarkError> {
		let asset = T::BenchmarkHelper::collateral_asset_id();
		let config = default_branch_config::<T>();
		let admins = branch_admins::<T>();
		T::BenchmarkHelper::set_oracle_price(
			asset.clone(),
			FixedU128::saturating_from_integer(ORACLE_PRICE),
		);
		let origin = create_origin::<T>()?;

		#[extrinsic_call]
		_(origin, asset.clone(), stable::<T>(), admins, config);

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
		_(origin, asset.clone(), stable::<T>(), admins.clone());

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
		let asset = T::BenchmarkHelper::collateral_asset_id();
		let origin = force_origin::<T>()?;
		let ceiling = balance::<T>(GLOBAL_CEILING);

		#[extrinsic_call]
		_(origin, asset.clone(), ceiling);

		assert_eq!(CollateralRisks::<T>::get(&asset).debt_ceiling, ceiling);
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
	fn poke_ceiling() -> Result<(), BenchmarkError> {
		// An autoline-enabled market derived from the guard envelope, so the
		// worst case — a ratcheting `Branches` write — is constructible on
		// any runtime. The gap stays below the line max: a gap at the line
		// max starts the ceiling there and leaves the ratchet nothing to do.
		let guard = T::BranchConfigGuard::get();
		let mut config = default_branch_config::<T>();
		config.ceiling_ttl = guard.min_ceiling_ttl;
		config.ceiling_gap = {
			let below_line = balance::<T>(50_000_000);
			guard.max_ceiling_gap.min(below_line)
		};
		if config.ceiling_gap.is_zero() {
			return Err(BenchmarkError::Stop("guard envelope disables the autoline"));
		}
		let asset = T::BenchmarkHelper::collateral_asset_id();
		T::BenchmarkHelper::set_oracle_price(
			asset.clone(),
			FixedU128::saturating_from_integer(ORACLE_PRICE),
		);
		Pallet::<T>::create_branch(
			create_origin::<T>()?,
			asset.clone(),
			stable::<T>(),
			branch_admins::<T>(),
			config,
		)?;
		Pallet::<T>::set_global_debt_ceiling(
			force_origin::<T>()?,
			asset.clone(),
			balance::<T>(GLOBAL_CEILING),
		)?;
		let owner = funded_account::<T>("owner", &asset);
		Pallet::<T>::open_vault(
			RawOrigin::Signed(owner).into(),
			asset.clone(),
			stable::<T>(),
			balance::<T>(SEED_COLL),
			balance::<T>(SEED_DEBT),
			rate(5, 100),
			Position::endpoints_only(),
		)?;
		// Raises are ttl-gated from registration; move past the gate so the
		// poke performs the write.
		T::BenchmarkHelper::advance_time(guard.min_ceiling_ttl.saturating_add(1));
		let before = Branches::<T>::get(&asset, &stable::<T>())
			.expect("branch registered above")
			.state
			.effective_ceiling;
		let caller: T::AccountId = whitelisted_caller();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>());

		let after = Branches::<T>::get(&asset, &stable::<T>())
			.expect("branch registered above")
			.state
			.effective_ceiling;
		assert!(after > before, "the poke ratcheted the ceiling up");
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

	#[benchmark]
	fn on_idle_base() -> Result<(), BenchmarkError> {
		let (asset, owner) = seed_idle_market::<T>()?;
		BranchIdleCursor::<T>::put((asset.clone(), stable::<T>()));
		IdleCursor::<T>::put((asset.clone(), stable::<T>(), owner));

		// The idle walk's flat cost: both cursors' read/write plus one
		// terminal `next_key` probe per walk — `idle_walk_pass`'s charging
		// contract.
		let branch_probe;
		let vault_probe;
		#[block]
		{
			let branch_cursor = BranchIdleCursor::<T>::get();
			let vault_cursor = IdleCursor::<T>::get();
			branch_probe = Branches::<T>::iter_keys().next();
			vault_probe = Vaults::<T>::iter_keys().next();
			BranchIdleCursor::<T>::set(branch_cursor);
			IdleCursor::<T>::set(vault_cursor);
		}

		assert!(branch_probe.is_some(), "the probe read the registered branch's key");
		assert!(vault_probe.is_some(), "the probe read the opened vault's key");
		assert!(BranchIdleCursor::<T>::get().is_some());
		Ok(())
	}

	#[benchmark]
	fn on_idle_one_branch() -> Result<(), BenchmarkError> {
		let (asset, _owner) = seed_idle_market::<T>()?;
		// A year of accrual, then a dead oracle: the refresh takes its
		// heaviest path — freeze, flush the aggregate interest, mint and
		// route the yield.
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS.saturating_mul(24 * 365));
		T::BenchmarkHelper::clear_oracle_price(asset.clone());

		// One `idle_branch_walk` step: the key pull plus the shared step fn.
		#[block]
		{
			let (collateral_id, stable_id) =
				Branches::<T>::iter_keys().next().expect("branch registered above");
			Pallet::<T>::idle_branch_step(&collateral_id, &stable_id);
		}

		let branch = Branches::<T>::get(&asset, &stable::<T>()).expect("branch registered above");
		assert!(branch.state.frozen.is_some(), "oracle failure froze the branch");
		assert!(
			!branch.state.debt.minted_interest.is_zero(),
			"the freeze flushed accrued aggregate interest"
		);
		Ok(())
	}

	#[benchmark]
	fn on_idle_one_vault() -> Result<(), BenchmarkError> {
		let (asset, owner) = seed_idle_market::<T>()?;
		seed_pending_redistribution::<T>(&asset)?;
		T::BenchmarkHelper::advance_time(ONE_HOUR_MS);

		// One `idle_vault_walk` step: the key pull plus the shared step fn.
		#[block]
		{
			let (collateral_id, stable_id, walked_owner) =
				Vaults::<T>::iter_keys().next().expect("vault opened above");
			Pallet::<T>::idle_vault_step(&collateral_id, &stable_id, &walked_owner);
		}

		let vault = Vaults::<T>::get((&asset, &stable::<T>(), &owner)).expect("vault opened above");
		let branch = Branches::<T>::get(&asset, &stable::<T>()).expect("branch registered above");
		assert_eq!(
			vault.redistribution_snapshot, branch.state.redistribution,
			"the refresh caught the vault up to the seeded redistribution"
		);
		Ok(())
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
