//! Benchmark fixtures shared by the pallet's own benchmarks and its
//! [`BenchmarkVaults`](stablecoin_primitives::BenchmarkVaults) implementation.
//!
//! Collateral is always counted in collateral units (see [`collateral_unit`]) and prices are
//! quoted per unit, so the fixtures run unchanged on a runtime with a production existential
//! deposit. Stablecoin amounts are raw.

use crate::{
	pallet::{
		AccountIdLookupOf, BalanceOf, BranchOf, Branches, CollateralIdOf, Config, Event, Pallet,
		RegistrationConfigOf, StableIdOf, VaultListIdOf, Vaults,
	},
	types::{BranchAdmins, BranchConfig, JitTerms, VaultListId, VaultStatus},
	BenchmarkConfig, BenchmarkHelper as _, HoldReason,
};
use alloc::vec::Vec;
use frame::{
	arithmetic::{FixedU128, Permill},
	benchmarking::prelude::*,
	deps::sp_runtime::{DispatchError, DispatchResult},
	traits::{
		fungibles::{
			Balanced as FungiblesBalanced, Create as FungiblesCreate, Inspect as FungiblesInspect,
			InspectHold as FungiblesInspectHold, Mutate as FungiblesMutate,
		},
		tokens::Precision,
		Consideration, EnsureOriginWithArg, Footprint, OriginTrait, SaturatedConversion, Time,
		Zero,
	},
};
use frame_system::RawOrigin;
use linked_list_interface::{Position, SortedListInterface};
use stablecoin_primitives::{
	BenchmarkVault, BranchSnapshot, CollateralRatio, OnBranchLifecycle as _, ProvidePrice,
	RecoveryHeadSeed, RedemptionSettlement, RedemptionTargetsSeed, StabilityPoolOffset,
	VaultInterface,
};

/// Avoids asset ids used by runtime genesis and test mocks.
pub(crate) const STABLE_ASSET_ID: u32 = 777;
/// Default price, in stablecoin per collateral unit.
pub(crate) const ORACLE_PRICE: u128 = 10;
/// High stablecoin-wide ceiling so the systemic cap never binds in benches.
pub(crate) const GLOBAL_CEILING: u128 = 1_000_000_000_000_000;
/// Collateral units every benchmark account is funded with.
pub(crate) const ACCOUNT_FUNDING: u128 = 10_000_000;
/// Collateral units of a default vault: far above the MCR at both benchmark prices.
pub(crate) const SEED_COLL: u128 = 1_000_000;
/// Exceeds minimum debt (200); a chain of these vaults yields whole interest units over an hour.
pub(crate) const SEED_DEBT: u128 = 3_000;
/// Price, per collateral unit, that drops a [`RECOVERY_VAULT_COLL`]-unit, [`SEED_DEBT`]-debt vault
/// below the 110% MCR while every [`SEED_COLL`]-unit vault stays safe.
pub(crate) const RECOVERY_TRIGGER_PRICE: u128 = 1;
/// Collateral units of a vault that [`RECOVERY_TRIGGER_PRICE`] makes unsafe.
pub(crate) const RECOVERY_VAULT_COLL: u128 = 2_000;
/// Enough to accrue non-zero interest at the default 5% rate.
pub(crate) const ONE_HOUR_MS: u64 = 60 * 60 * 1_000;
/// Shorter than the rate-adjustment cooldown, so changing rates after activation still pays a fee.
pub(crate) const POOL_ENTRY_DELAY_MS: u64 = ONE_HOUR_MS;
/// Stablecoin each cohort that [`queue_pool_cohorts`] queues deposits.
pub(crate) const POOL_COHORT_DEPOSIT: u128 = 1_000_000;
/// Debt of each redemption target. At [`ORACLE_PRICE`] a full step pays out 100 collateral
/// units, above any account's minimum balance.
pub(crate) const REDEMPTION_TARGET_DEBT: u128 = 1_000;
/// Collateral units of a staged recovery head: a power of ten, so the exact-par price divides
/// evenly on runtimes whose collateral unit is a power of ten.
pub(crate) const RECOVERY_HEAD_COLL: u128 = 1_000;
/// Debt of a staged recovery head: 333% collateralized at [`ORACLE_PRICE`], 33% at
/// [`RECOVERY_TRIGGER_PRICE`].
pub(crate) const RECOVERY_HEAD_DEBT: u128 = 3_000;

pub(crate) fn stable<T: BenchmarkConfig>() -> StableIdOf<T> {
	STABLE_ASSET_ID.into()
}

/// Must precede `create_origin`, which resolves the signer from the asset owner.
/// Also stages the runtime's costliest global debt ceiling.
pub(crate) fn ensure_stable_asset<T: BenchmarkConfig>() -> DispatchResult {
	let stable_id = stable::<T>();
	if !T::StableAssets::asset_exists(stable_id.clone()) {
		let owner: T::AccountId = account("stable_owner", 0, 0);
		ensure_provider::<T>(&owner);
		T::StableAssets::create(stable_id.clone(), owner, true, balance::<T>(1))?;
	}
	assert!(T::StableAssets::asset_exists(stable_id.clone()));
	T::BenchmarkHelper::stage_debt_ceiling(stable_id);
	Ok(())
}

/// Gives `who` a provider reference unless it already has one, so it can hold balances.
pub(crate) fn ensure_provider<T: Config>(who: &T::AccountId) {
	if frame_system::Pallet::<T>::providers(who) == 0 {
		frame_system::Pallet::<T>::inc_providers(who);
	}
}

pub(crate) fn collateral<T: BenchmarkConfig>() -> CollateralIdOf<T> {
	T::BenchmarkHelper::collateral_asset_id()
}

pub(crate) fn balance<T: BenchmarkConfig>(value: u128) -> BalanceOf<T> {
	value.saturated_into()
}

/// Minimum balance in planck; scaling collateral and prices preserves stablecoin amounts
/// across runtimes.
pub(crate) fn collateral_unit<T: BenchmarkConfig>() -> BalanceOf<T> {
	let unit = T::CollateralAssets::minimum_balance(collateral::<T>()).max(balance::<T>(1));
	assert!(!unit.is_zero());
	unit
}

/// `units` collateral units in planck.
pub(crate) fn collateral_units<T: BenchmarkConfig>(units: u128) -> BalanceOf<T> {
	balance::<T>(units).saturating_mul(collateral_unit::<T>())
}

/// Oracle price quoting `value` stablecoin per collateral unit.
pub(crate) fn unit_price<T: BenchmarkConfig>(value: u128) -> FixedU128 {
	let unit: u128 = collateral_unit::<T>().saturated_into();
	FixedU128::from_rational(value, unit)
}

/// Quotes `value` stablecoin per collateral unit.
pub(crate) fn set_unit_price<T: BenchmarkConfig>(value: u128) {
	T::BenchmarkHelper::set_oracle_price(collateral::<T>(), unit_price::<T>(value));
}

/// Refreshes an existing quote to prevent oracle staleness; preserves a missing quote.
pub(crate) fn advance_time<T: BenchmarkConfig>(ms: u64) {
	let before = T::TimeProvider::now();
	T::BenchmarkHelper::advance_time(ms);
	assert_eq!(T::TimeProvider::now(), before.saturating_add(ms));
	if let Ok(price) = T::Oracle::provide_price(&collateral::<T>()) {
		T::BenchmarkHelper::set_oracle_price(collateral::<T>(), price);
	}
}

/// Moves the clock to `deadline`, if it is still ahead, and re-quotes the price.
pub(crate) fn advance_to<T: BenchmarkConfig>(deadline: u64) {
	advance_time::<T>(deadline.saturating_sub(T::TimeProvider::now()));
	assert!(T::TimeProvider::now() >= deadline);
}

pub(crate) fn rate_list<T: BenchmarkConfig>() -> VaultListIdOf<T> {
	VaultListId::Rate(collateral::<T>(), stable::<T>())
}

pub(crate) fn recovery_list<T: BenchmarkConfig>() -> VaultListIdOf<T> {
	VaultListId::FinalRecovery(collateral::<T>(), stable::<T>())
}

pub(crate) fn status_of<T: BenchmarkConfig>(owner: &T::AccountId) -> VaultStatus {
	Pallet::<T>::vault_status_of(&collateral::<T>(), &stable::<T>(), owner)
}

pub(crate) fn default_branch_config<T: BenchmarkConfig>() -> BranchConfig<BalanceOf<T>> {
	const DAY_MS: u64 = 24 * 3_600 * 1_000;
	// Vault floors must also clear production asset minimum balances.
	let minimum_debt = balance::<T>(200).max(T::StableAssets::minimum_balance(stable::<T>()));
	BranchConfig {
		minimum_collateralization_ratio: FixedU128::from_rational(110, 100),
		initial_collateralization_ratio: FixedU128::from_rational(120, 100),
		safety_collateralization_ratio: FixedU128::from_rational(130, 100),
		debt_ceiling: balance::<T>(100_000_000),
		minimum_debt,
		minimum_collateral: collateral_unit::<T>(),
		minimum_borrow_rate: FixedU128::from_rational(1, 1_000),
		maximum_borrow_rate: FixedU128::from_rational(100, 100),
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

/// One branch per stablecoin makes this the first lifecycle registration.
pub(crate) fn first_market_lifecycle_config<T: BenchmarkConfig>() -> RegistrationConfigOf<T> {
	T::OnBranchLifecycle::benchmark_registration_config(1)
}

pub(crate) fn branch_admin_accounts<T: BenchmarkConfig>() -> (T::AccountId, T::AccountId) {
	(account("full_admin", 0, 0), account("emergency_admin", 0, 0))
}

pub(crate) fn branch_admins<T: BenchmarkConfig>() -> BranchAdmins<AccountIdLookupOf<T>> {
	let (full_admin, emergency_admin) = branch_admin_accounts::<T>();
	BranchAdmins {
		full_admin: T::Lookup::unlookup(full_admin),
		emergency_admin: T::Lookup::unlookup(emergency_admin),
	}
}

/// The successful `CreateOrigin` for the default stablecoin. A stablecoin owner's signed origin
/// pays the creation deposit, the costlier path; the stablecoin must already exist.
pub(crate) fn create_origin<T: BenchmarkConfig>() -> Result<T::RuntimeOrigin, DispatchError> {
	T::CreateOrigin::try_successful_origin(&stable::<T>())
		.map_err(|()| DispatchError::Other("create origin unavailable"))
}

pub(crate) fn full_admin_origin<T: BenchmarkConfig>() -> T::RuntimeOrigin {
	RawOrigin::Signed(branch_admin_accounts::<T>().0).into()
}

/// A branch creation ready to dispatch: its collateral, origin, and the account charged its
/// deposit and custody seeds.
pub(crate) struct BranchRegistration<T: Config> {
	pub asset: CollateralIdOf<T>,
	pub origin: T::RuntimeOrigin,
	/// The depositor of a signed creation, otherwise the full admin.
	pub funder: T::AccountId,
	/// Whether the creation charges a deposit.
	pub signed: bool,
}

/// Funds creation deposits, custody seeds and the lifecycle handler's stablecoin float.
pub(crate) fn prepare_branch_registration<T: BenchmarkConfig>(
) -> Result<BranchRegistration<T>, DispatchError> {
	let asset = collateral::<T>();
	ensure_stable_asset::<T>()?;
	T::BenchmarkHelper::ensure_collateral_asset(asset.clone());
	set_unit_price::<T>(ORACLE_PRICE);
	T::BenchmarkHelper::fund_native(&T::FeeAccount::convert(stable::<T>()));
	let origin = create_origin::<T>()?;
	let depositor = origin.as_signer().cloned();
	let signed = depositor.is_some();
	let funder = depositor.unwrap_or_else(|| branch_admin_accounts::<T>().0);
	if signed {
		let footprint = Footprint::from_mel::<(BranchOf<T>, u32)>();
		T::BranchConsideration::ensure_successful(&funder, footprint);
	}
	fund_collateral::<T>(&funder, collateral_units::<T>(ACCOUNT_FUNDING))?;
	let float = T::StableAssets::minimum_balance(stable::<T>()).saturating_mul(balance::<T>(2));
	T::StableAssets::mint_into(stable::<T>(), &funder, float)?;
	Ok(BranchRegistration { asset, origin, funder, signed })
}

/// Registers the default branch and lifts the global debt ceiling out of the way.
pub(crate) fn register_default_branch<T: BenchmarkConfig>(
) -> Result<CollateralIdOf<T>, DispatchError> {
	let BranchRegistration { asset, origin, .. } = prepare_branch_registration::<T>()?;
	Pallet::<T>::create_branch(
		origin,
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
	assert!(Branches::<T>::contains_key(&asset, stable::<T>()));
	Ok(asset)
}

/// Deposits `amount` collateral planck to `who`, plus what opening a vault deposits.
pub(crate) fn fund_collateral<T: BenchmarkConfig>(
	who: &T::AccountId,
	amount: BalanceOf<T>,
) -> DispatchResult {
	ensure_provider::<T>(who);
	let debt = <T::CollateralAssets as FungiblesBalanced<T::AccountId>>::deposit(
		collateral::<T>(),
		who,
		amount,
		Precision::Exact,
	)?;
	drop(debt);
	T::VaultConsideration::ensure_successful(
		who,
		Pallet::<T>::vault_footprint(&collateral::<T>(), &stable::<T>(), who),
	);
	Ok(())
}

pub(crate) fn funded_caller<T: BenchmarkConfig>() -> Result<T::AccountId, DispatchError> {
	let who: T::AccountId = account("caller", 0, 0);
	fund_collateral::<T>(&who, collateral_units::<T>(ACCOUNT_FUNDING))?;
	Ok(who)
}

pub(crate) fn default_vault<T: BenchmarkConfig>() -> BenchmarkVault<BalanceOf<T>> {
	BenchmarkVault {
		collateral_units: SEED_COLL,
		debt: balance::<T>(SEED_DEBT),
		annual_rate: FixedU128::from_rational(5, 100),
	}
}

/// Funds `who` with twice `vault`'s collateral, so free balance stays above the minimum, and
/// opens `vault` at its exact rate-list position.
pub(crate) fn open_funded_vault<T: BenchmarkConfig>(
	who: &T::AccountId,
	vault: BenchmarkVault<BalanceOf<T>>,
) -> DispatchResult {
	let amount = collateral_units::<T>(vault.collateral_units);
	fund_collateral::<T>(who, amount.saturating_mul(balance::<T>(2)))?;
	let hint = T::VaultLists::find_position(&rate_list::<T>(), vault.annual_rate);
	Pallet::<T>::open_vault(
		RawOrigin::Signed(who.clone()).into(),
		collateral::<T>(),
		stable::<T>(),
		amount,
		vault.debt,
		vault.annual_rate,
		hint,
	)
	.map_err(|error| error.error)?;
	assert!(Vaults::<T>::contains_key((collateral::<T>(), stable::<T>(), who)));
	Ok(())
}

/// Seed rates must fit the runtime's borrow-rate bounds.
pub(crate) struct RateBounds {
	/// Highest seed-chain rate.
	pub base: FixedU128,
	/// Gap between consecutive seed rates.
	pub step: FixedU128,
	/// The rate of the seed in the middle of the chain. A vault opened at it lands right
	/// behind that seed, with seeds on both sides.
	pub middle: FixedU128,
}

impl RateBounds {
	/// A rate between the first two seeds, used for "land between two neighbours" worst cases.
	pub fn interior(&self) -> FixedU128 {
		let half_step = FixedU128::from_inner(self.step.into_inner() / 2);
		self.base.saturating_sub(half_step)
	}
}

/// The repair walk needs both endpoints to exercise an interior insertion.
pub(crate) fn worst_case_chain_len<T: BenchmarkConfig>() -> u32 {
	T::VaultLists::repair_budget().saturating_add(2)
}

pub(crate) fn rate_bounds<T: BenchmarkConfig>() -> Result<RateBounds, DispatchError> {
	let config = Pallet::<T>::branch_of(&collateral::<T>(), &stable::<T>())?.config;
	let count = worst_case_chain_len::<T>();
	let safety_floor = config
		.minimum_borrow_rate
		.saturating_mul(FixedU128::saturating_from_integer(2u32));
	// Half of `maximum_borrow_rate` keeps every seed rate well inside `validate_rate`'s range.
	let safety_ceiling = config
		.maximum_borrow_rate
		.const_checked_div(FixedU128::saturating_from_integer(2u32))
		.ok_or("maximum_borrow_rate halving overflowed")?;
	ensure!(safety_ceiling > safety_floor, "borrow-rate range too narrow for seeding");
	let span = safety_ceiling.saturating_sub(safety_floor);
	let divisor = FixedU128::saturating_from_integer(count.saturating_add(2));
	let step = span.const_checked_div(divisor).ok_or("rate step computation failed")?;
	ensure!(!step.is_zero(), "borrow-rate span too narrow for repair_budget");
	let base = safety_ceiling.saturating_sub(step);
	let middle_offset = step.saturating_mul(FixedU128::saturating_from_integer(count / 2));
	let middle = base.saturating_sub(middle_offset);
	Ok(RateBounds { base, step, middle })
}

/// A rate index seeded by [`seed_worst_case_chain`].
pub(crate) struct WorstCaseChain<T: Config> {
	/// Seed owners in head→tail order.
	pub seeds: Vec<T::AccountId>,
	/// The rate bounds the seeds were seeded from.
	pub bounds: RateBounds,
	/// A hint exactly `hint_repair_budget` steps stale for [`RateBounds::interior`], whose valid
	/// position has a neighbour on both sides, so the insert relinks two nodes.
	pub interior_hint: Position<T::AccountId>,
}

/// Seeds the rate index with [`worst_case_chain_len`] vaults, each insert hinted via
/// `find_position` to keep seeding O(count) — independent of the hint-repair budget.
pub(crate) fn seed_worst_case_chain<T: BenchmarkConfig>() -> Result<WorstCaseChain<T>, DispatchError>
{
	let budget: usize = T::VaultLists::repair_budget().saturated_into();
	let count = worst_case_chain_len::<T>();
	let mut owners = Vec::with_capacity(count.saturated_into());
	let bounds = rate_bounds::<T>()?;
	for i in 0..count {
		let offset = bounds.step.saturating_mul(FixedU128::saturating_from_integer(i));
		let who: T::AccountId = account("seed", i, 0);
		let annual_rate = bounds.base.saturating_sub(offset);
		open_funded_vault::<T>(&who, BenchmarkVault { annual_rate, ..default_vault::<T>() })?;
		owners.push(who);
	}
	assert_eq!(owners.len(), count.saturated_into::<usize>());
	assert_eq!(owners.len(), budget + 2);
	assert_eq!(T::VaultLists::head(&rate_list::<T>()).as_ref(), owners.first());
	let interior_hint = seed_gap(&owners, T::VaultLists::repair_budget());
	Ok(WorstCaseChain { seeds: owners, bounds, interior_hint })
}

/// The rate-index gap between seed `index` and the seed after it.
pub(crate) fn seed_gap<AccountId: Clone>(seeds: &[AccountId], index: u32) -> Position<AccountId> {
	let index: usize = index.saturated_into();
	Position::between(seeds[index].clone(), seeds[index.saturating_add(1)].clone())
}

/// A caller vault opened mid-chain in a worst-case seeded rate index.
pub(crate) struct MidChainCaller<T: Config> {
	pub asset: CollateralIdOf<T>,
	pub caller: T::AccountId,
	pub seeds: Vec<T::AccountId>,
	/// A rate between the two highest seeds.
	pub interior_rate: FixedU128,
}

/// Seeds the worst-case chain and opens the caller's vault in its middle. Leaves every vault a
/// pending redistribution share and two due pool cohorts, so no vault operation may follow.
pub(crate) fn seed_mid_chain_caller<T: BenchmarkConfig>() -> Result<MidChainCaller<T>, DispatchError>
{
	let asset = register_default_branch::<T>()?;
	let WorstCaseChain { seeds, bounds, .. } = seed_worst_case_chain::<T>()?;
	let caller: T::AccountId = account("caller", 0, 0);
	let vault = BenchmarkVault {
		collateral_units: SEED_COLL * 10,
		annual_rate: bounds.middle,
		..default_vault::<T>()
	};
	open_funded_vault::<T>(&caller, vault)?;
	let neighbours = T::VaultLists::neighbors(&rate_list::<T>(), &caller)
		.ok_or("caller not in the rate index")?;
	assert!(neighbours.prev.is_some());
	assert!(neighbours.next.is_some());
	seed_pending_redistribution::<T>()?;
	make_cohorts_due::<T>()?;
	Ok(MidChainCaller { asset, caller, seeds, interior_rate: bounds.interior() })
}

/// Redeems all of `owner`'s projected debt without paying out collateral, leaving a Dormant vault.
pub(crate) fn redeem_debt_only<T: BenchmarkConfig>(owner: &T::AccountId) -> DispatchResult {
	let recipient: T::AccountId = whitelisted_caller();
	let debt_payment = <T::StableAssets as FungiblesBalanced<T::AccountId>>::issue(
		stable::<T>(),
		projected_debt::<T>(owner)?,
	);
	<Pallet<T> as VaultInterface>::redeem_step(
		&collateral::<T>(),
		&stable::<T>(),
		owner,
		&recipient,
		RedemptionSettlement { debt_payment, collateral_to_recipient: BalanceOf::<T>::zero() },
	)?;
	Ok(())
}

/// Opens `vault` for `owner`, crashes the price to [`RECOVERY_TRIGGER_PRICE`] so `keeper` can push
/// it into the FinalRecovery FIFO, and leaves the price crashed.
///
/// The vault must be the branch's only stake bearer when it enters.
fn enter_recovery<T: BenchmarkConfig>(
	owner: &T::AccountId,
	vault: BenchmarkVault<BalanceOf<T>>,
	keeper: T::AccountId,
) -> DispatchResult {
	open_funded_vault::<T>(owner, vault)?;
	set_unit_price::<T>(RECOVERY_TRIGGER_PRICE);
	Pallet::<T>::enter_final_recovery(
		RawOrigin::Signed(keeper).into(),
		collateral::<T>(),
		stable::<T>(),
		owner.clone(),
	)
	.map_err(|error| error.error)?;
	ensure!(
		status_of::<T>(owner) == VaultStatus::FinalRecovery,
		"vault did not enter final recovery"
	);
	Ok(())
}

/// Opens a fresh vault at `annual_rate` that [`RECOVERY_TRIGGER_PRICE`] makes unsafe, pushes it
/// into the FinalRecovery FIFO, and restores the default price.
///
/// The vault must be the branch's only stake bearer when it enters.
pub(crate) fn recovery_cycle<T: BenchmarkConfig>(
	seed_index: u32,
	annual_rate: FixedU128,
) -> Result<T::AccountId, DispatchError> {
	let owner: T::AccountId = account("rec", seed_index, 0);
	let vault = BenchmarkVault {
		collateral_units: RECOVERY_VAULT_COLL,
		annual_rate,
		..default_vault::<T>()
	};
	enter_recovery::<T>(&owner, vault, whitelisted_caller())?;
	set_unit_price::<T>(ORACLE_PRICE);
	Ok(owner)
}

/// A vault opened only to be liquidated into the empty pool, so its debt and collateral are
/// redistributed over every other stake-bearing vault.
pub(crate) struct Sacrifice<Balance> {
	pub vault: BenchmarkVault<Balance>,
	/// Price, per collateral unit, at which the sacrifice is liquidated.
	pub liquidation_price: u128,
	/// Price, per collateral unit, restored after the liquidation.
	pub restored_price: u128,
}

/// Opens `sacrifice` and liquidates it at its price with no JIT, then restores the price. The
/// pool must be empty, so redistribution takes the whole vault.
pub(crate) fn redistribute<T: BenchmarkConfig>(
	sacrifice: Sacrifice<BalanceOf<T>>,
) -> DispatchResult {
	let owner: T::AccountId = account("sacrifice", 0, 0);
	open_funded_vault::<T>(&owner, sacrifice.vault)?;
	let held = redistribution_held::<T>();
	set_unit_price::<T>(sacrifice.liquidation_price);
	// A keeper distinct from every benchmark caller, so no caller's accounts are touched.
	let keeper: T::AccountId = account("sacrifice_keeper", 0, 0);
	Pallet::<T>::liquidate(
		RawOrigin::Signed(keeper).into(),
		collateral::<T>(),
		stable::<T>(),
		owner.clone(),
		JitTerms { max_stable: Zero::zero(), min_collateral_out: Zero::zero() },
	)?;
	set_unit_price::<T>(sacrifice.restored_price);
	assert!(!Vaults::<T>::contains_key((collateral::<T>(), stable::<T>(), &owner)));
	assert!(redistribution_held::<T>() > held, "the liquidation redistributed nothing");
	Ok(())
}

/// Leaves every stake-bearing vault a pending redistribution share, which the next touch of each
/// vault absorbs through a collateral transfer. The pool must be empty.
pub(crate) fn seed_pending_redistribution<T: BenchmarkConfig>() -> DispatchResult {
	redistribute::<T>(Sacrifice {
		vault: BenchmarkVault {
			collateral_units: RECOVERY_VAULT_COLL,
			annual_rate: FixedU128::from_rational(1, 100),
			..default_vault::<T>()
		},
		liquidation_price: RECOVERY_TRIGGER_PRICE,
		restored_price: ORACLE_PRICE,
	})
}

/// Queues two pool deposits of `deposit` with `entry_delay`, the second just before the first is
/// due, so they open two cohorts. Returns both deadlines; the clock is left one millisecond before
/// the first.
fn queue_two_cohorts<T: BenchmarkConfig>(
	entry_delay: u64,
	deposit: BalanceOf<T>,
) -> Result<(u64, u64), DispatchError> {
	let queue = |index| {
		T::StabilityPool::benchmark_queue_deposit(
			&collateral::<T>(),
			&stable::<T>(),
			index,
			entry_delay,
			deposit,
		)
	};
	let start = T::TimeProvider::now();
	let first = queue(0)?;
	assert!(first > start);
	advance_time::<T>(first.saturating_sub(start).saturating_sub(1));
	let second = queue(1)?;
	assert!(second > first);
	assert!(T::TimeProvider::now() < first);
	Ok((first, second))
}

/// Queues two pool cohorts, so the first yield distribution after they are due activates both
/// and finds active capital to distribute to. Returns the time from which both are due; the clock
/// is left one millisecond before the first deadline, so no operation before that rolls them.
pub(crate) fn queue_pool_cohorts<T: BenchmarkConfig>() -> Result<u64, DispatchError> {
	let deposit = balance::<T>(POOL_COHORT_DEPOSIT);
	queue_two_cohorts::<T>(POOL_ENTRY_DELAY_MS, deposit).map(|(_, second)| second)
}

/// Queues two pool cohorts and moves the clock to where both are due. No vault operation may
/// follow, since the first one would activate them. Precede any recovery head, which would route
/// the deposits into a recovery offset.
pub(crate) fn make_cohorts_due<T: BenchmarkConfig>() -> DispatchResult {
	advance_to::<T>(queue_pool_cohorts::<T>()?);
	Ok(())
}

/// Seed the Stability Pool so the next offset rolls a due cohort into the active leg and still
/// finds pending capital behind it. Leaves the clock one millisecond before the pending deadline,
/// a full entry delay after the last branch accrual.
pub(crate) fn seed_liquidation_pool<T: BenchmarkConfig>(
	entry_delay: u64,
	deposit: BalanceOf<T>,
) -> DispatchResult {
	let (active_deadline, pending_deadline) = queue_two_cohorts::<T>(entry_delay, deposit)?;
	advance_to::<T>(pending_deadline.saturating_sub(1));
	assert!(active_deadline <= T::TimeProvider::now());
	assert!(T::TimeProvider::now() < pending_deadline);
	Ok(())
}

/// The branch as the engine hands it to the pool, read the way `liquidate` reads it.
pub(crate) fn branch_snapshot<T: BenchmarkConfig>() -> Result<BranchSnapshot, DispatchError> {
	let mode = Pallet::<T>::current_mode(&collateral::<T>(), &stable::<T>())?;
	Ok(BranchSnapshot { mode, now: T::TimeProvider::now() })
}

pub(crate) fn redistribution_held<T: BenchmarkConfig>() -> BalanceOf<T> {
	T::CollateralAssets::balance_on_hold(
		collateral::<T>(),
		&HoldReason::VaultCollateral.into(),
		&Pallet::<T>::redistribution_account(&collateral::<T>(), &stable::<T>()),
	)
}

/// Asserts the measured call absorbed the pending redistribution share and issued interest,
/// the two lazy settlements every touching benchmark stages.
pub(crate) fn assert_worst_case_touch<T: BenchmarkConfig>(held_before: BalanceOf<T>) {
	assert!(redistribution_held::<T>() < held_before);
	assert!(interest_issued::<T>());
}

/// Mints `amount` stablecoin to `owner` and repays `owner`'s whole debt with it.
pub(crate) fn repay_in_full<T: BenchmarkConfig>(
	owner: &T::AccountId,
	amount: BalanceOf<T>,
) -> DispatchResult {
	T::StableAssets::mint_into(stable::<T>(), owner, amount)?;
	Pallet::<T>::repay_for(
		RawOrigin::Signed(owner.clone()).into(),
		collateral::<T>(),
		stable::<T>(),
		owner.clone(),
		None,
	)
}

pub(crate) fn fee_balance<T: BenchmarkConfig>() -> BalanceOf<T> {
	T::StableAssets::balance(stable::<T>(), &T::FeeAccount::convert(stable::<T>()))
}

/// Whether an `InterestIssued` event with a non-zero amount was deposited for the branch since
/// the events were last reset.
pub(crate) fn interest_issued<T: BenchmarkConfig>() -> bool {
	frame_system::Pallet::<T>::read_events_for_pallet::<Event<T>>()
		.into_iter()
		.any(|event| {
			matches!(
				event,
				Event::<T>::InterestIssued { collateral_id, stable_id, amount }
					if collateral_id == collateral::<T>() && stable_id == stable::<T>() &&
						!amount.is_zero()
			)
		})
}

/// `owner`'s debt with interest and any pending redistribution share applied.
pub(crate) fn projected_debt<T: BenchmarkConfig>(
	owner: &T::AccountId,
) -> Result<BalanceOf<T>, DispatchError> {
	let snapshot = <Pallet<T> as VaultInterface>::project_redemption_snapshot(
		&collateral::<T>(),
		&stable::<T>(),
		owner,
	)?;
	Ok(snapshot.debt)
}

/// Stages an exact-par `FinalRecovery` head, as documented on
/// [`stablecoin_primitives::BenchmarkVaults::stage_recovery_head`].
pub(crate) fn stage_recovery_head<T: BenchmarkConfig>(
	elapsed: u64,
) -> Result<RecoveryHeadSeed<T::AccountId, BalanceOf<T>>, DispatchError> {
	let asset = collateral::<T>();
	let owner: T::AccountId = account("recovery_head", 0, 0);
	let collateral = collateral_units::<T>(RECOVERY_HEAD_COLL);
	let vault = BenchmarkVault {
		collateral_units: RECOVERY_HEAD_COLL,
		debt: balance::<T>(RECOVERY_HEAD_DEBT),
		..default_vault::<T>()
	};
	enter_recovery::<T>(&owner, vault, account("recovery_keeper", 0, 0))?;
	// Top the keeper reward back up: the exact-par price below needs a round collateral amount.
	let reward = collateral
		.saturating_sub(Pallet::<T>::vault_of(&asset, &stable::<T>(), &owner)?.collateral);
	if !reward.is_zero() {
		Pallet::<T>::deposit_collateral_for(
			RawOrigin::Signed(owner.clone()).into(),
			asset.clone(),
			stable::<T>(),
			owner.clone(),
			reward,
		)?;
	}
	T::BenchmarkHelper::advance_time(elapsed);

	let snapshot =
		<Pallet<T> as VaultInterface>::project_redemption_snapshot(&asset, &stable::<T>(), &owner)?;
	assert_eq!(snapshot.collateral, collateral);
	let price = FixedU128::checked_from_rational(snapshot.debt, snapshot.collateral)
		.ok_or("recovery head price overflowed")?;
	T::BenchmarkHelper::set_oracle_price(asset, price);

	let staged = stablecoin_primitives::collateralization_ratio(&snapshot.position(), price)
		.map_err(|_| "recovery head ratio overflowed")?;
	let CollateralRatio::Ratio(staged) = staged else {
		return Err("recovery head carries no debt".into());
	};
	// Exact par: the debt buys back every planck of collateral, so a full settlement closes.
	let payout = stablecoin_primitives::math::collateral_for_value_floor(snapshot.debt, price);
	ensure!(staged == FixedU128::one(), "collateral unit does not admit an exact par price");
	ensure!(
		payout == Some(snapshot.collateral),
		"collateral unit does not admit an exact par price"
	);
	Ok(RecoveryHeadSeed { owner, debt: snapshot.debt, collateral: snapshot.collateral, price })
}

/// Seeds redemption targets, as documented on
/// [`stablecoin_primitives::BenchmarkVaults::seed_redemption_targets`].
pub(crate) fn seed_redemption_targets<T: BenchmarkConfig>(
	count: u32,
	elapsed: u64,
) -> Result<RedemptionTargetsSeed<T::AccountId, BalanceOf<T>>, DispatchError> {
	let config = Pallet::<T>::branch_of(&collateral::<T>(), &stable::<T>())?.config;
	let mut owners = Vec::with_capacity(count.saturated_into());
	for i in 0..count {
		let owner: T::AccountId = account("redemption_target", i, 0);
		// Ascending rates make redemption visit targets in index order.
		let annual_rate = config
			.minimum_borrow_rate
			.saturating_mul(FixedU128::saturating_from_integer(i.saturating_add(2)));
		ensure!(
			annual_rate <= config.maximum_borrow_rate,
			"too many redemption targets for the rate range"
		);
		let vault = BenchmarkVault {
			debt: balance::<T>(REDEMPTION_TARGET_DEBT),
			annual_rate,
			..default_vault::<T>()
		};
		open_funded_vault::<T>(&owner, vault)?;
		owners.push(owner);
	}
	seed_pending_redistribution::<T>()?;
	make_cohorts_due::<T>()?;
	advance_time::<T>(elapsed);

	let first = <Pallet<T> as VaultInterface>::next_redemption_target(
		&collateral::<T>(),
		&stable::<T>(),
		None,
	);
	assert_eq!(first.map(|(owner, _)| owner).as_ref(), owners.first());
	let mut total_debt = BalanceOf::<T>::zero();
	for owner in &owners {
		let snapshot = <Pallet<T> as VaultInterface>::project_redemption_snapshot(
			&collateral::<T>(),
			&stable::<T>(),
			owner,
		)?;
		assert_eq!(snapshot.status, VaultStatus::Active);
		// The worst case settles a redistribution share the stored row has not applied yet.
		let stored = Pallet::<T>::vault_of(&collateral::<T>(), &stable::<T>(), owner)?;
		assert!(snapshot.debt > stored.debt.total());
		total_debt = total_debt.saturating_add(snapshot.debt);
	}
	Ok(RedemptionTargetsSeed { owners, total_debt })
}
