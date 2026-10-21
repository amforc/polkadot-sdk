//! Benchmarks for `pallet-stability`.
//!
//! Every benchmark stages the costliest path of its call on real vault-engine state, seeded
//! through [`BenchmarkVaults`]:
//!
//! - The depositor row settles three windows that each read every row they can. Its active snapshot
//!   and its checkpointed pending tranche sit [`math::SCALE_SPAN`] scales behind a depletion of
//!   their leg, and the tranche's cohort activated into an active epoch that crossed the span and
//!   was depleted in turn. The checkpoint has a second member, so the settlement rewrites it.
//! - A payout needs principal, which no depleted leg keeps. Successful withdrawals instead keep the
//!   pending tranche alive across the full span in both its pending and activated phases. Separate
//!   benchmarks measure exhausted withdrawals and Safety-mode requests, including their interest
//!   accrual. The charge takes the maximum of the complete alternative paths.
//! - Two more cohorts with members are due at the call, so it activates both and writes their
//!   checkpoints.
//! - Calls that issue interest find a vault with elapsed interest and a small active pool, so the
//!   yield route distributes into the pool.
//! - Payouts go to accounts that hold none of the paid asset yet.
//!
//! Collateral is counted in collateral units and stablecoin amounts are raw, as in
//! [`BenchmarkVaults`], so the benchmarks run on runtimes with a production existential deposit.

use crate::{
	math,
	pallet::{
		BalanceOf, Call, CohortCheckpoints, CollateralIdOf, Config, DepositOf, Deposits, Error,
		Event, Pallet, PoolSumsStore, Pools, StabilityPoolOf, StableIdOf,
	},
	types::{Accumulators, CohortId, DepositSnapshot, Leg, WithdrawalRequest},
	weights::WeightInfo,
};
use frame::{
	arithmetic::{FixedU128, Permill},
	benchmarking::prelude::*,
	deps::{frame_support::dispatch::WithPostDispatchInfo, sp_runtime::traits::Bounded},
	traits::{
		fungibles::{Balanced as _, Inspect as _, Mutate as _},
		EnsureOriginWithArg, Time,
	},
};
use frame_system::RawOrigin;
use stablecoin_primitives::{
	BenchmarkVault, BenchmarkVaults, BranchMode, BranchSnapshot, Millis, OffsetLegs,
	StabilityPoolInspect, StabilityPoolOffset,
};

/// Entry delay of every staged cohort.
const ENTRY_DELAY_MS: Millis = 60 * 60 * 1_000;
/// Stablecoin of each deposit that a scale-crossing offset later shrinks. Leaving
/// [`LEG_REMAINDER`] of two such deposits crosses [`math::SCALE_SPAN`] scales at the benchmark
/// precision (`p_min` 1e-9, `scale_factor` 1e9), and each deposit keeps a non-zero value.
const BULK_DEPOSIT: u128 = 100_000_000_000_000_000_000;
/// Stablecoin a scale-crossing offset leaves on each leg.
const LEG_REMAINDER: u128 = 100;
/// Each pending member needs this much capital to survive two full-span reductions at the
/// benchmark precision. This is a reachable arithmetic bound, not an expected on-chain deposit.
const PAYOUT_DEPOSIT: u128 = 100_000_000_000_000_000_000_000_000_000_000_000_000;
/// The first reduction leaves 1.5e20 of 2e38; the second leaves 100 of 1.5e20. Both ratios are
/// below 1e-18, so both phases cross two scales, while each member still receives principal.
const PAYOUT_REMAINDER: u128 = 150_000_000_000_000_000_000;
/// Stablecoin each benchmarked deposit carries.
const COHORT_DEPOSIT: u128 = 1_000_000;
/// Collateral units each leg of a staged offset gains.
const OFFSET_COLLATERAL_UNITS: u128 = 1_000;
/// Active capital that a first offset reduces to [`NEAR_FLOOR_REMAINDER`], leaving `P` just above
/// `p_min`.
const NEAR_FLOOR_DEPOSIT: u128 = 10_000_000_000_000;
/// `P = 10_001 / 1e13`, one ten-thousandth above `p_min`, so a later offset of more than that
/// fraction of the active pool crosses a scale.
const NEAR_FLOOR_REMAINDER: u128 = 10_001;
/// Collateral units of the vault whose interest feeds the yield route.
const ACCRUAL_VAULT_COLLATERAL_UNITS: u128 = 20_000_000;
/// Interest must survive division by `scale_factor²` when a deposit reads yield two scales
/// behind. Half an hour at `ACCRUAL_VAULT_RATE` over `LEG_REMAINDER` clears that rounding floor.
const ACCRUAL_VAULT_DEBT: u128 = 40_000_000;
/// Annual rate of the vault whose interest feeds the yield route.
const ACCRUAL_VAULT_RATE: FixedU128 = FixedU128::from_rational(50, 100);
/// The head must be the only vault; its small debt needs weeks to yield whole interest units.
const RECOVERY_ACCRUAL_MS: Millis = 30 * 24 * 60 * 60 * 1_000;

/// Benchmark-only vault seeding bound.
pub(crate) trait BenchmarkConfig:
	Config<
	BranchInterface: BenchmarkVaults<
		CollateralIdOf<Self>,
		StableIdOf<Self>,
		Self::AccountId,
		BalanceOf<Self>,
	>,
>
{
}

impl<T> BenchmarkConfig for T where
	T: Config<
		BranchInterface: BenchmarkVaults<
			CollateralIdOf<T>,
			StableIdOf<T>,
			T::AccountId,
			BalanceOf<T>,
		>,
	>
{
}

type Seeder<T> = <T as Config>::BranchInterface;

struct Market<T: Config> {
	collateral_id: CollateralIdOf<T>,
	stable_id: StableIdOf<T>,
}

/// The capital a staged row keeps to pay out, see the module docs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Principal {
	/// Depletions closed every window behind the row, which keeps gains only.
	None,
	/// The pending leg survives, so the tranche activates with capital a withdrawal pays.
	Pending,
}

impl Principal {
	/// The stablecoin each pending member deposits, and what a scale-crossing offset leaves of
	/// the pending leg.
	const fn pending_leg(self) -> (u128, u128) {
		match self {
			Self::None => (BULK_DEPOSIT, LEG_REMAINDER),
			Self::Pending => (PAYOUT_DEPOSIT, PAYOUT_REMAINDER),
		}
	}
}

/// A depositor row staged for the costliest settlement, see the module docs.
struct RefreshSeed<T: Config> {
	market: Market<T>,
	owner: T::AccountId,
	principal: Principal,
	/// The activated cohort of the owner's pending tranche.
	checkpointed: CohortId,
	/// The two cohorts due at [`Self::due`].
	due_cohorts: [CohortId; 2],
	/// Time from which both cohorts are due. Moving the clock here is the last setup step.
	due: Millis,
}

fn balance<T: Config>(value: u128) -> BalanceOf<T> {
	BalanceOf::<T>::try_from(value)
		.ok()
		.expect("every benchmark amount fits the balance type; qed")
}

impl<T: Config> Market<T> {
	fn stable_balance(&self, who: &T::AccountId) -> BalanceOf<T> {
		T::StableAssets::balance(self.stable_id.clone(), who)
	}

	fn collateral_balance(&self, who: &T::AccountId) -> BalanceOf<T> {
		T::CollateralAssets::balance(self.collateral_id.clone(), who)
	}

	fn deposit_row(&self, who: &T::AccountId) -> Result<DepositOf<T>, BenchmarkError> {
		Ok(Deposits::<T>::get((&self.collateral_id, &self.stable_id, who))
			.ok_or("deposit row missing")?)
	}

	fn pool(&self) -> Result<StabilityPoolOf<T>, BenchmarkError> {
		Ok(Pools::<T>::get(&self.collateral_id, &self.stable_id).ok_or("pool missing")?)
	}

	/// The row the next settlement of `who` stores, before the call issues any interest.
	fn settled_row(&self, who: &T::AccountId) -> Result<DepositOf<T>, BenchmarkError> {
		Ok(Pallet::<T>::settled_deposit(who, &self.collateral_id, &self.stable_id)?)
	}

	/// The yield sum at the live active coordinates, which only a yield distribution raises.
	fn live_yield_sum(&self) -> Result<FixedU128, BenchmarkError> {
		let pool = self.pool()?;
		let sums = Pallet::<T>::sums_at(
			&self.collateral_id,
			&self.stable_id,
			Leg::Active,
			&pool.state.coords,
		)?;
		Ok(sums.g_yield)
	}

	/// An account with a provider and none of either asset, so a payout creates its asset
	/// account.
	fn fresh_recipient(&self) -> T::AccountId {
		let who: T::AccountId = account("recipient", 0, 0);
		frame_system::Pallet::<T>::inc_providers(&who);
		assert!(self.stable_balance(&who).is_zero());
		assert!(self.collateral_balance(&who).is_zero());
		who
	}

	/// The cohort of `who`'s pending tranche and its deadline.
	fn pending_cohort(&self, who: &T::AccountId) -> Result<(CohortId, Millis), BenchmarkError> {
		let cohort = self.deposit_row(who)?.pending_deposit.ok_or("deposit did not queue")?.cohort;
		let deadline = self.pool()?.state.cohort(cohort).ok_or("cohort not open")?.deadline;
		Ok((cohort, deadline))
	}

	/// Require a closed epoch with the full row span to measure every settlement read.
	fn assert_closed_window(&self, leg: Leg, snapshot: &DepositSnapshot, end: &Accumulators) {
		assert!(snapshot.coords.epoch < end.epoch);
		for offset in 1..=math::SCALE_SPAN {
			let scale = snapshot.coords.scale.saturating_add(offset);
			assert!(PoolSumsStore::<T>::contains_key((
				&self.collateral_id,
				&self.stable_id,
				leg,
				snapshot.coords.epoch,
				scale,
			)));
		}
	}
}

impl<T: BenchmarkConfig> Market<T> {
	/// Registers the benchmark market and gives its pool a non-zero entry delay.
	fn register() -> Result<Self, BenchmarkError> {
		let (collateral_id, stable_id) = Seeder::<T>::register_market()?;
		Pools::<T>::try_mutate(&collateral_id, &stable_id, |pool| {
			let pool = pool.as_mut().ok_or("pool missing")?;
			pool.config.entry_delay = ENTRY_DELAY_MS;
			Ok::<_, BenchmarkError>(())
		})?;
		Ok(Self { collateral_id, stable_id })
	}

	fn deposit_from(&self, who: &T::AccountId, amount: BalanceOf<T>) -> Result<(), BenchmarkError> {
		if frame_system::Pallet::<T>::providers(who) == 0 {
			frame_system::Pallet::<T>::inc_providers(who);
		}
		T::StableAssets::mint_into(self.stable_id.clone(), who, amount)?;
		Pallet::<T>::deposit(
			RawOrigin::Signed(who.clone()).into(),
			self.collateral_id.clone(),
			self.stable_id.clone(),
			amount,
		)?;
		Ok(())
	}

	/// Settles the row of `who`, which activates every due cohort.
	fn settle(&self, who: &T::AccountId) -> Result<(), BenchmarkError> {
		Pallet::<T>::settle_deposit(
			RawOrigin::Signed(who.clone()).into(),
			who.clone(),
			self.collateral_id.clone(),
			self.stable_id.clone(),
		)?;
		Ok(())
	}

	/// Burns `active` and `pending` debt against the pool's legs, as a liquidation does, after
	/// checking that the pool quotes both in full.
	fn offset_legs(
		&self,
		active: BalanceOf<T>,
		pending: BalanceOf<T>,
	) -> Result<(), BenchmarkError> {
		let (collateral_id, stable_id) = (&self.collateral_id, &self.stable_id);
		let mode = Pallet::<T>::branch_mode_or_frozen(collateral_id, stable_id);
		let branch = BranchSnapshot { mode, now: T::TimeProvider::now() };
		let quote = Pallet::<T>::quote(collateral_id, stable_id, branch)
			.ok_or("pool has no offset capacity")?;
		assert_eq!(Pallet::<T>::quote_active(&quote, active), active);
		assert_eq!(Pallet::<T>::quote_pending(&quote, pending, active), pending);
		let collateral = |debt: BalanceOf<T>| {
			let units = if debt.is_zero() { 0 } else { OFFSET_COLLATERAL_UNITS };
			T::CollateralAssets::issue(collateral_id.clone(), Seeder::<T>::collateral_units(units))
		};
		let collateral = OffsetLegs { active: collateral(active), pending: collateral(pending) };
		let burned = Pallet::<T>::offset(
			collateral_id,
			stable_id,
			branch,
			OffsetLegs { active, pending },
			collateral,
		)?;
		assert_eq!(burned.map(|credit| credit.peek()), Some(active.saturating_add(pending)));
		Ok(())
	}

	fn queue_cohorts(&self) -> Result<([CohortId; 2], Millis), BenchmarkError> {
		let due = Seeder::<T>::queue_pool_cohorts()?;
		let pool = self.pool()?;
		let [first, second] = pool.state.open_cohorts.as_slice() else {
			return Err("expected two open cohorts".into());
		};
		assert_ne!(first.id, second.id);
		assert!(T::TimeProvider::now() < first.deadline);
		assert!(first.deadline < second.deadline);
		assert_eq!(second.deadline, due);
		Ok(([first.id, second.id], due))
	}

	/// Depletes the pending leg behind cohort A, after both legs crossed the span, then lets half
	/// an entry delay of interest land on the active remainder, where the owner's share survives
	/// the full-span division. The deposit of `epoch_one` opens cohort B behind the depleted leg.
	fn deplete_pending_leg(
		&self,
		epoch_one: &T::AccountId,
		deposit: BalanceOf<T>,
	) -> Result<(), BenchmarkError> {
		self.offset_legs(BalanceOf::<T>::zero(), balance::<T>(LEG_REMAINDER))?;
		Seeder::<T>::advance_time(ENTRY_DELAY_MS / 2);
		self.deposit_from(epoch_one, deposit)?;
		Ok(())
	}

	/// Activates cohort B into the second active epoch, which then crosses the span and is
	/// depleted in turn, so the owner's activated phase spans a closed epoch as well. Leaves a
	/// small active pool behind, so the interest of the call distributes into it.
	fn deplete_second_epoch(
		&self,
		bulk_depositor: &T::AccountId,
		epoch_one: &T::AccountId,
	) -> Result<(), BenchmarkError> {
		advance_to::<T>(self.pending_cohort(epoch_one)?.1);
		self.settle(bulk_depositor)?;
		let remainder = balance::<T>(LEG_REMAINDER);
		let active = self.pool()?.state.total_active_deposits;
		self.offset_legs(active.saturating_sub(remainder), BalanceOf::<T>::zero())?;
		self.offset_legs(remainder, BalanceOf::<T>::zero())?;
		let epoch_two: T::AccountId = account("epoch_two", 0, 0);
		self.deposit_from(&epoch_two, remainder)?;
		advance_to::<T>(self.pending_cohort(&epoch_two)?.1);
		self.settle(bulk_depositor)?;
		Ok(())
	}

	/// Prices the accrual vault below the Safety threshold without freezing the branch.
	fn enter_safety(&self) {
		Seeder::<T>::set_unit_price(2);
		assert_eq!(
			Pallet::<T>::branch_mode_or_frozen(&self.collateral_id, &self.stable_id),
			BranchMode::Safety
		);
	}
}

fn advance_to<T: BenchmarkConfig>(deadline: Millis) {
	Seeder::<T>::advance_time(deadline.saturating_sub(T::TimeProvider::now()));
	assert!(T::TimeProvider::now() >= deadline);
}

/// [`stage_refresh`] with a vault whose interest feeds the yield route, then moves the clock to
/// [`RefreshSeed::due`] as the last setup step and checks the mode the call runs in.
fn seed_refresh<T: BenchmarkConfig>(
	principal: Principal,
) -> Result<RefreshSeed<T>, BenchmarkError> {
	let seed = stage_refresh::<T>(true, principal)?;
	advance_to::<T>(seed.due);
	assert_refresh_staged::<T>(&seed)?;
	let market = &seed.market;
	let mode = Pallet::<T>::branch_mode_or_frozen(&market.collateral_id, &market.stable_id);
	assert_eq!(mode, BranchMode::Normal);
	Ok(seed)
}

/// Stages the costliest settlement of a depositor row, see the module docs. The clock is left
/// before [`RefreshSeed::due`].
///
/// `accrual_vault` opens a vault whose interest feeds the yield route at every later operation.
/// Leave it out to stage a recovery head afterwards, which must be the only vault.
fn stage_refresh<T: BenchmarkConfig>(
	accrual_vault: bool,
	principal: Principal,
) -> Result<RefreshSeed<T>, BenchmarkError> {
	let market = Market::<T>::register()?;
	if accrual_vault {
		let vault = BenchmarkVault {
			collateral_units: ACCRUAL_VAULT_COLLATERAL_UNITS,
			debt: balance::<T>(ACCRUAL_VAULT_DEBT),
			annual_rate: ACCRUAL_VAULT_RATE,
		};
		Seeder::<T>::open_vault(&account("accrual_vault", 0, 0), vault)?;
	}
	let owner: T::AccountId = account("owner", 0, 0);
	let member: T::AccountId = account("member", 0, 0);
	// Allows cohort activation without refreshing the owner's row.
	let bulk_depositor: T::AccountId = account("bulk", 0, 0);
	let bulk = balance::<T>(BULK_DEPOSIT);

	// Keep the owner's active snapshot and a shared pending checkpoint for settlement.
	market.deposit_from(&bulk_depositor, bulk)?;
	market.deposit_from(&owner, bulk)?;
	advance_to::<T>(market.pending_cohort(&owner)?.1);
	let (pending, pending_remainder) = principal.pending_leg();
	let (pending, pending_remainder) = (balance::<T>(pending), balance::<T>(pending_remainder));
	market.deposit_from(&owner, pending)?;
	market.deposit_from(&member, pending)?;
	let (checkpointed, checkpoint_due) = market.pending_cohort(&owner)?;
	assert_eq!(market.pending_cohort(&member)?.0, checkpointed);
	assert_eq!(market.deposit_row(&owner)?.active_deposit, bulk);

	// Both legs cross the full settlement span while cohort A is pending.
	let remainder = balance::<T>(LEG_REMAINDER);
	let doubled = bulk.saturating_add(bulk);
	market.offset_legs(
		doubled.saturating_sub(remainder),
		pending.saturating_add(pending).saturating_sub(pending_remainder),
	)?;
	let epoch_one: T::AccountId = account("epoch_one", 0, 0);
	if principal == Principal::None {
		market.deplete_pending_leg(&epoch_one, doubled)?;
	}
	market.offset_legs(remainder, BalanceOf::<T>::zero())?;

	advance_to::<T>(checkpoint_due);
	market.settle(&bulk_depositor)?;
	if principal == Principal::None {
		market.deplete_second_epoch(&bulk_depositor, &epoch_one)?;
	} else {
		market.offset_legs(pending_remainder.saturating_sub(remainder), BalanceOf::<T>::zero())?;
	}

	// Both cohorts must remain pending until the measured call.
	let (due_cohorts, due) = market.queue_cohorts()?;

	let seed = RefreshSeed { market, owner, principal, checkpointed, due_cohorts, due };
	assert_refresh_staged::<T>(&seed)?;
	Ok(seed)
}

fn assert_refresh_staged<T: Config>(seed: &RefreshSeed<T>) -> Result<(), BenchmarkError> {
	let market = &seed.market;
	let pool = market.pool()?;
	let checkpoint =
		CohortCheckpoints::<T>::get((&market.collateral_id, &market.stable_id, seed.checkpointed))
			.ok_or("cohort A has no checkpoint")?;
	assert_eq!(checkpoint.members, 2);
	let row = market.deposit_row(&seed.owner)?;
	let pending = row.pending_deposit.ok_or("owner tranche settled")?;
	assert_eq!(pending.cohort, seed.checkpointed);
	market.assert_closed_window(Leg::Active, &row.snapshot, &pool.state.coords);
	match seed.principal {
		Principal::None => {
			market.assert_closed_window(
				Leg::Pending,
				&pending.snapshot,
				&checkpoint.pending_end.coords,
			);
			market.assert_closed_window(Leg::Active, &checkpoint.active_start, &pool.state.coords);
		},
		Principal::Pending => {
			let pending_end = checkpoint.pending_end.coords;
			assert_eq!(pending.snapshot.coords.epoch, pending_end.epoch);
			assert_eq!(
				pending_end.scale.saturating_sub(pending.snapshot.coords.scale),
				math::SCALE_SPAN
			);
			assert_eq!(checkpoint.active_start.coords.epoch, pool.state.coords.epoch);
			assert_eq!(
				pool.state.coords.scale.saturating_sub(checkpoint.active_start.coords.scale),
				math::SCALE_SPAN
			);
			assert!(!market.settled_row(&seed.owner)?.active_deposit.is_zero());
		},
	}
	assert!(pool.state.open_cohorts.iter().map(|cohort| cohort.id).eq(seed.due_cohorts));
	assert!(pool.state.open_cohorts.iter().all(|cohort| cohort.deadline <= seed.due));
	assert!(pool.state.open_cohorts.iter().all(|cohort| cohort.members > 0));
	Ok(())
}

/// Stages a matured request made before the losses, without refreshing the depositor's history.
fn authorize_withdrawal<T: BenchmarkConfig>(seed: &RefreshSeed<T>) -> Result<(), BenchmarkError> {
	let market = &seed.market;
	market.enter_safety();
	Deposits::<T>::try_mutate((&market.collateral_id, &market.stable_id, &seed.owner), |row| {
		row.as_mut().ok_or("the seed stores the owner")?.withdrawal_request =
			Some(WithdrawalRequest {
				amount: BalanceOf::<T>::max_value(),
				executable_at: T::TimeProvider::now(),
			});
		Ok(())
	})
}

fn assert_refreshed<T: Config>(seed: &RefreshSeed<T>) -> Result<(), BenchmarkError> {
	let market = &seed.market;
	let (collateral_id, stable_id) = (&market.collateral_id, &market.stable_id);
	let checkpoint = CohortCheckpoints::<T>::get((collateral_id, stable_id, seed.checkpointed))
		.ok_or("checkpoint removed")?;
	assert_eq!(checkpoint.members, 1);
	for cohort in seed.due_cohorts {
		assert!(CohortCheckpoints::<T>::contains_key((collateral_id, stable_id, cohort)));
	}
	assert!(market
		.pool()?
		.state
		.open_cohorts
		.iter()
		.all(|cohort| !seed.due_cohorts.contains(&cohort.id)));
	Ok(())
}

#[benchmarks(where T: BenchmarkConfig)]
mod benchmarks {
	use super::*;

	/// The deposit first routes the head's interest into the pool, then settles an above-par
	/// `FinalRecovery` head in full, which closes its vault, then settles the depositor's
	/// checkpointed tranche, activates two cohorts and queues the change. It spends the
	/// depositor's whole balance.
	#[benchmark]
	fn deposit() -> Result<(), BenchmarkError> {
		let seed = stage_refresh::<T>(false, Principal::None)?;
		let market = &seed.market;
		// Staging the head prices the market last, so it also carries the clock to the cohorts.
		let elapsed = seed.due.saturating_sub(T::TimeProvider::now()).max(RECOVERY_ACCRUAL_MS);
		let head = Seeder::<T>::stage_recovery_head(elapsed)?;
		assert!(T::TimeProvider::now() >= seed.due);
		assert_refresh_staged::<T>(&seed)?;
		let change = balance::<T>(COHORT_DEPOSIT);
		let amount = head.debt.saturating_add(change);
		T::StableAssets::mint_into(market.stable_id.clone(), &seed.owner, amount)?;
		assert_eq!(market.stable_balance(&seed.owner), amount);
		let claimable_before = market.settled_row(&seed.owner)?.claimable_collateral;
		let yield_before = market.live_yield_sum()?;

		#[extrinsic_call]
		_(
			RawOrigin::Signed(seed.owner.clone()),
			market.collateral_id.clone(),
			market.stable_id.clone(),
			amount,
		);

		assert_refreshed::<T>(&seed)?;
		assert!(market.live_yield_sum()? > yield_before);
		let row = market.deposit_row(&seed.owner)?;
		assert_eq!(row.claimable_collateral, claimable_before.saturating_add(head.collateral));
		assert_eq!(row.pending_deposit.map(|pending| pending.amount), Some(change));
		assert!(market.stable_balance(&seed.owner).is_zero());
		frame_system::Pallet::<T>::assert_has_event(
			Event::<T>::RecoveryOffsetApplied {
				collateral_id: market.collateral_id.clone(),
				stable_id: market.stable_id.clone(),
				source: crate::types::RecoveryOffsetSource::IncomingDeposit,
			}
			.into(),
		);
		Ok(())
	}

	/// [`deposit`](Pallet::deposit) with an empty `FinalRecovery` queue, which it refunds to.
	#[benchmark]
	fn deposit_no_recovery() -> Result<(), BenchmarkError> {
		let seed = seed_refresh::<T>(Principal::None)?;
		let market = &seed.market;
		let amount = balance::<T>(COHORT_DEPOSIT);
		assert!(market.stable_balance(&seed.owner).is_zero());
		T::StableAssets::mint_into(market.stable_id.clone(), &seed.owner, amount)?;
		let yield_before = market.live_yield_sum()?;

		#[extrinsic_call]
		deposit(
			RawOrigin::Signed(seed.owner.clone()),
			market.collateral_id.clone(),
			market.stable_id.clone(),
			amount,
		);

		assert_refreshed::<T>(&seed)?;
		let (cohort, _) = market.pending_cohort(&seed.owner)?;
		assert!(!seed.due_cohorts.contains(&cohort));
		let row = market.deposit_row(&seed.owner)?;
		assert_eq!(row.pending_deposit.map(|pending| pending.amount), Some(amount));
		assert!(market.stable_balance(&seed.owner).is_zero());
		assert!(market.live_yield_sum()? > yield_before);
		Ok(())
	}

	/// In Normal mode the request pays out at once, as a full withdrawal.
	#[benchmark]
	fn request_withdraw() -> Result<(), BenchmarkError> {
		let seed = seed_refresh::<T>(Principal::Pending)?;
		let market = &seed.market;
		assert!(market.stable_balance(&seed.owner).is_zero());
		let yield_before = market.live_yield_sum()?;

		#[extrinsic_call]
		_(
			RawOrigin::Signed(seed.owner.clone()),
			market.collateral_id.clone(),
			market.stable_id.clone(),
			BalanceOf::<T>::max_value(),
		);

		assert_refreshed::<T>(&seed)?;
		assert!(market.deposit_row(&seed.owner)?.active_deposit.is_zero());
		assert!(!market.stable_balance(&seed.owner).is_zero());
		assert!(market.live_yield_sum()? > yield_before);
		Ok(())
	}

	/// Safety mode also validates and reduces a matured request before paying principal.
	#[benchmark]
	fn withdraw() -> Result<(), BenchmarkError> {
		let seed = seed_refresh::<T>(Principal::Pending)?;
		let market = &seed.market;
		let recipient = market.fresh_recipient();
		let yield_before = market.live_yield_sum()?;
		authorize_withdrawal::<T>(&seed)?;

		#[extrinsic_call]
		_(
			RawOrigin::Signed(seed.owner.clone()),
			market.collateral_id.clone(),
			market.stable_id.clone(),
			BalanceOf::<T>::max_value(),
			Some(recipient.clone()),
		);

		assert_refreshed::<T>(&seed)?;
		assert!(market.deposit_row(&seed.owner)?.active_deposit.is_zero());
		assert!(!market.stable_balance(&recipient).is_zero());
		assert!(market.live_yield_sum()? > yield_before);
		Ok(())
	}

	/// An exhausted row still accrues interest, settles all three closed windows and validates
	/// its Safety-mode request before rejecting the withdrawal. The dispatch rolls it all back.
	#[benchmark]
	fn withdraw_exhausted() -> Result<(), BenchmarkError> {
		let seed = seed_refresh::<T>(Principal::None)?;
		let market = &seed.market;
		authorize_withdrawal::<T>(&seed)?;
		assert!(market.settled_row(&seed.owner)?.active_deposit.is_zero());
		let yield_before = market.live_yield_sum()?;
		let recipient = market.fresh_recipient();

		#[block]
		{
			assert_eq!(
				Pallet::<T>::withdraw(
					RawOrigin::Signed(seed.owner.clone()).into(),
					market.collateral_id.clone(),
					market.stable_id.clone(),
					BalanceOf::<T>::max_value(),
					Some(recipient.clone()),
				),
				Err(Error::<T>::NoActiveDeposit.with_weight(T::WeightInfo::withdraw_exhausted()))
			);
		}

		assert_eq!(market.live_yield_sum()?, yield_before);
		assert_refresh_staged::<T>(&seed)?;
		assert!(market.stable_balance(&recipient).is_zero());
		Ok(())
	}

	/// Normal mode reaches the exhausted withdrawal through the request's own pool and mode
	/// checks, so measure the complete wrapper as well.
	#[benchmark]
	fn request_withdraw_exhausted() -> Result<(), BenchmarkError> {
		let seed = seed_refresh::<T>(Principal::None)?;
		let market = &seed.market;
		assert!(market.settled_row(&seed.owner)?.active_deposit.is_zero());
		let yield_before = market.live_yield_sum()?;

		#[block]
		{
			assert_eq!(
				Pallet::<T>::request_withdraw(
					RawOrigin::Signed(seed.owner.clone()).into(),
					market.collateral_id.clone(),
					market.stable_id.clone(),
					BalanceOf::<T>::max_value(),
				),
				Err(Error::<T>::NoActiveDeposit
					.with_weight(T::WeightInfo::request_withdraw_exhausted()))
			);
		}

		assert_eq!(market.live_yield_sum()?, yield_before);
		assert_refresh_staged::<T>(&seed)?;
		Ok(())
	}

	/// Safety mode commits the full settlement and records a request even without principal.
	#[benchmark]
	fn request_withdraw_safety() -> Result<(), BenchmarkError> {
		let seed = seed_refresh::<T>(Principal::None)?;
		let market = &seed.market;
		market.enter_safety();
		assert!(market.settled_row(&seed.owner)?.active_deposit.is_zero());
		let yield_before = market.live_yield_sum()?;
		let amount = BalanceOf::<T>::max_value();
		let executable_at =
			T::TimeProvider::now().saturating_add(market.pool()?.config.safety_withdrawal_delay);

		#[extrinsic_call]
		request_withdraw(
			RawOrigin::Signed(seed.owner.clone()),
			market.collateral_id.clone(),
			market.stable_id.clone(),
			amount,
		);

		assert_refreshed::<T>(&seed)?;
		let row = market.deposit_row(&seed.owner)?;
		assert!(row.active_deposit.is_zero());
		let request = row.withdrawal_request.ok_or("no request recorded")?;
		assert_eq!(request.amount, amount);
		assert_eq!(request.executable_at, executable_at);
		assert!(market.stable_balance(&seed.owner).is_zero());
		assert!(market.live_yield_sum()? > yield_before);
		Ok(())
	}

	/// The claim issues no interest, so the settlement itself activates both cohorts.
	#[benchmark]
	fn claim_collateral() -> Result<(), BenchmarkError> {
		let seed = seed_refresh::<T>(Principal::None)?;
		let market = &seed.market;
		let recipient = market.fresh_recipient();
		let claimable = market.settled_row(&seed.owner)?.claimable_collateral;
		assert!(!claimable.is_zero());

		#[extrinsic_call]
		_(
			RawOrigin::Signed(seed.owner.clone()),
			market.collateral_id.clone(),
			market.stable_id.clone(),
			Some(recipient.clone()),
		);

		assert_refreshed::<T>(&seed)?;
		assert!(market.deposit_row(&seed.owner)?.claimable_collateral.is_zero());
		assert_eq!(market.collateral_balance(&recipient), claimable);
		Ok(())
	}

	#[benchmark]
	fn claim_yield() -> Result<(), BenchmarkError> {
		let seed = seed_refresh::<T>(Principal::None)?;
		let market = &seed.market;
		let recipient = market.fresh_recipient();
		let claimable = market.settled_row(&seed.owner)?.claimable_yield;
		assert!(!claimable.is_zero());
		let yield_before = market.live_yield_sum()?;

		#[extrinsic_call]
		_(
			RawOrigin::Signed(seed.owner.clone()),
			market.collateral_id.clone(),
			market.stable_id.clone(),
			Some(recipient.clone()),
		);

		assert_refreshed::<T>(&seed)?;
		assert!(market.deposit_row(&seed.owner)?.claimable_yield.is_zero());
		// The call's own interest reaches the owner before the claim, on top of the staged yield.
		assert!(market.stable_balance(&recipient) >= claimable);
		assert!(market.live_yield_sum()? > yield_before);
		Ok(())
	}

	#[benchmark]
	fn compound_yield() -> Result<(), BenchmarkError> {
		let seed = seed_refresh::<T>(Principal::None)?;
		let market = &seed.market;
		let before = market.settled_row(&seed.owner)?;
		assert!(!before.claimable_yield.is_zero());
		let yield_before = market.live_yield_sum()?;

		#[extrinsic_call]
		_(
			RawOrigin::Signed(seed.owner.clone()),
			market.collateral_id.clone(),
			market.stable_id.clone(),
			BalanceOf::<T>::max_value(),
		);

		assert_refreshed::<T>(&seed)?;
		let row = market.deposit_row(&seed.owner)?;
		assert!(row.claimable_yield.is_zero());
		let compounded = before.active_deposit.saturating_add(before.claimable_yield);
		assert!(row.active_deposit >= compounded);
		assert!(market.live_yield_sum()? > yield_before);
		Ok(())
	}

	/// The active pool settles an above-par `FinalRecovery` head in full, closing its vault and
	/// routing its interest into the pool, after activating two cohorts. `P` sits just above
	/// `p_min`, so the offset crosses a scale.
	#[benchmark]
	fn offset_recovery() -> Result<(), BenchmarkError> {
		let market = Market::<T>::register()?;
		let bulk: T::AccountId = account("bulk", 0, 0);
		market.deposit_from(&bulk, balance::<T>(NEAR_FLOOR_DEPOSIT))?;
		advance_to::<T>(market.pending_cohort(&bulk)?.1);
		let near_floor = NEAR_FLOOR_DEPOSIT.saturating_sub(NEAR_FLOOR_REMAINDER);
		market.offset_legs(balance::<T>(near_floor), BalanceOf::<T>::zero())?;
		let (collateral_id, stable_id) = (&market.collateral_id, &market.stable_id);
		let pool = market.pool()?;
		let p_min = pool.config.precision.p_min;
		assert!(pool.state.coords.p >= p_min);
		assert!(
			pool.state.coords.p < p_min.saturating_mul(FixedU128::from_rational(10_002, 10_000))
		);

		let (due_cohorts, due) = market.queue_cohorts()?;
		let head = Seeder::<T>::stage_recovery_head(
			due.saturating_sub(T::TimeProvider::now()).max(RECOVERY_ACCRUAL_MS),
		)?;
		assert!(T::TimeProvider::now() >= due);
		assert_eq!(market.pool()?.state.open_cohorts.len(), 2);
		let max_stable_in =
			Pallet::<T>::activated_pool_state(collateral_id, stable_id)?.total_active_deposits;
		assert!(max_stable_in > head.debt);
		let before = market.pool()?.state;
		let caller: T::AccountId = whitelisted_caller();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), collateral_id.clone(), stable_id.clone(), max_stable_in);

		for cohort in due_cohorts {
			assert!(CohortCheckpoints::<T>::contains_key((collateral_id, stable_id, cohort)));
		}
		let after = market.pool()?.state;
		assert_eq!(after.coords.epoch, 0);
		assert_eq!(after.coords.scale, 1);
		assert_eq!(
			after.total_collateral_gains_unclaimed,
			before.total_collateral_gains_unclaimed.saturating_add(head.collateral)
		);
		assert!(after.total_yield_unclaimed > before.total_yield_unclaimed);
		Ok(())
	}

	/// The settlement issues no interest and moves no value, so it activates both cohorts.
	#[benchmark]
	fn settle_deposit() -> Result<(), BenchmarkError> {
		let seed = seed_refresh::<T>(Principal::None)?;
		let market = &seed.market;
		let caller: T::AccountId = whitelisted_caller();

		#[extrinsic_call]
		_(
			RawOrigin::Signed(caller),
			seed.owner.clone(),
			market.collateral_id.clone(),
			market.stable_id.clone(),
		);

		assert_refreshed::<T>(&seed)?;
		assert!(market.deposit_row(&seed.owner)?.pending_deposit.is_none());
		Ok(())
	}

	/// Signed by the market's full administrator, whose check reads the market, where the
	/// runtime's `UpdateOrigin` admits one.
	#[benchmark]
	fn set_stability_pool_config() -> Result<(), BenchmarkError> {
		let market = Market::<T>::register()?;
		let (collateral_id, stable_id) = (&market.collateral_id, &market.stable_id);
		let origin =
			T::UpdateOrigin::try_successful_origin(&(collateral_id.clone(), stable_id.clone()))
				.map_err(|()| BenchmarkError::Weightless)?;
		let mut config = market.pool()?.config;
		config.yield_share = Permill::from_percent(50);
		assert!(config.is_valid());

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, collateral_id.clone(), stable_id.clone(), config.clone());

		let stored = market.pool()?.config;
		assert_eq!(stored.yield_share, Permill::from_percent(50));
		assert_eq!(stored.entry_delay, ENTRY_DELAY_MS);
		Ok(())
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
