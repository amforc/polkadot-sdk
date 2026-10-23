//! `try_state` invariant verification.
//!
//! Gated on `feature = "try-runtime"` or `test`. Run after every test by the mock's
//! `build_and_execute` and end-to-end by the runtime's pre-upgrade hook.

use crate::{
	math,
	pallet::{
		BalanceOf, BranchOf, Branches, CollateralIdOf, Config, GlobalDebtCeilings, HoldReason,
		Millis, Pallet, StableIdOf, StablecoinDebt, StablecoinMarkets, VaultListIdOf, Vaults,
	},
	types::{AssetMinimums, VaultListId},
	utility_impls::BranchContribution,
};
use alloc::collections::{BTreeMap, BTreeSet};
use frame::{
	arithmetic::{CheckedAdd, CheckedSub, Zero},
	deps::{sp_core::U256, sp_runtime::TryRuntimeError},
	prelude::ensure,
	traits::{
		fungibles::{Inspect as FungiblesInspect, InspectHold},
		Convert, Get, Time,
	},
};
use linked_list_interface::SortedListInterface;

pub fn do_try_state<T: Config>() -> Result<(), TryRuntimeError> {
	let now = T::TimeProvider::now();
	// Owner-hold invariant accumulator: filled per branch inside
	// `check_branch_identities`' single vault pass, then checked once below.
	let mut owner_collateral: BTreeMap<(CollateralIdOf<T>, T::AccountId), BalanceOf<T>> =
		BTreeMap::new();
	// Derived-index accumulators, recomputed in full from the authoritative registry.
	let mut collaterals: BTreeSet<CollateralIdOf<T>> = BTreeSet::new();
	let mut stablecoin_markets: BTreeMap<CollateralIdOf<T>, u32> = BTreeMap::new();
	let mut stablecoin_debt: BTreeMap<StableIdOf<T>, BranchContribution<BalanceOf<T>>> =
		BTreeMap::new();
	for (collateral_id, stable_id, branch) in Branches::<T>::iter() {
		collaterals.insert(collateral_id.clone());
		let markets = stablecoin_markets
			.entry(T::StableToCollateralId::convert(stable_id.clone()))
			.or_default();
		*markets = markets.checked_add(1).ok_or("stablecoin branch count overflow")?;
		let branch_outstanding = branch.state.debt.outstanding();
		let stable_entry = stablecoin_debt.entry(stable_id.clone()).or_default();
		stable_entry.outstanding = stable_entry
			.outstanding
			.checked_add(&branch_outstanding)
			.ok_or("stablecoin outstanding-debt sum overflow")?;
		if !branch.state.is_frozen() {
			stable_entry.active_accrual_rate = stable_entry
				.active_accrual_rate
				.checked_add(branch.state.debt.accrual_rate)
				.ok_or("stablecoin active accrual rate sum overflow")?;
		}
		stable_entry.pending_interest = stable_entry
			.pending_interest
			.checked_add(
				Pallet::<T>::branch_pending_interest(&branch.state, now)
					.map_err(|_| "branch pending-interest numerator overflow")?,
			)
			.ok_or("stablecoin pending-interest numerator overflow")?;
		let (collateral_id, stable_id) = (&collateral_id, &stable_id);
		check_branch_config::<T>(collateral_id, stable_id, &branch)?;
		let rate_list = VaultListId::Rate(collateral_id.clone(), stable_id.clone());
		let recovery_list = VaultListId::FinalRecovery(collateral_id.clone(), stable_id.clone());
		check_branch_identities::<T>(
			collateral_id,
			stable_id,
			&branch,
			&rate_list,
			&recovery_list,
			now,
			&mut owner_collateral,
		)?;
		// The slot exposes redemption dust, so a debt-free Dormant vault cannot occupy it.
		if let Some(owner) = branch.state.dormant_redemption_target.clone() {
			let vault = Pallet::<T>::vault_of(collateral_id, stable_id, &owner)
				.map_err(|_| "dormant_redemption_target points at missing vault")?;
			ensure!(
				Pallet::<T>::vault_status_of(collateral_id, stable_id, &owner).is_dormant(),
				"dormant_redemption_target points at non-Dormant"
			);
			ensure!(
				!vault.debt.total().is_zero(),
				"dormant_redemption_target points at a debt-free vault"
			);
		}
	}

	check_owner_holds::<T>(owner_collateral)?;
	check_stablecoin_markets::<T>(&collaterals, stablecoin_markets)?;
	// No `StablecoinMarkets` entry is required per ceiling: governance sets a stablecoin's
	// ceiling independently of its branches, before the first registers and after the last is
	// removed, so an orphan ceiling is ordinary state.
	ensure!(
		GlobalDebtCeilings::<T>::iter_values().all(|ceiling| !ceiling.is_zero()),
		"zero GlobalDebtCeilings record stored"
	);
	check_stablecoin_debt::<T>(stablecoin_debt, now)?;
	Ok(())
}

/// The stored config must still pass the checks every config write passed.
///
/// Both are re-judged against today's inputs, not the ones at write time: a runtime upgrade that
/// tightens [`Config::BranchConfigBounds`], or an asset whose minimum balance rises, leaves stored
/// branches the dispatchables would now refuse, so such a change needs a migration of their
/// configs.
fn check_branch_config<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
	branch: &BranchOf<T>,
) -> Result<(), TryRuntimeError> {
	let minimums = AssetMinimums {
		collateral: T::CollateralAssets::minimum_balance(collateral_id.clone()),
		stable: T::StableAssets::minimum_balance(stable_id.clone()),
	};
	branch
		.config
		.ensure_consistent(&minimums)
		.map_err(|_| "stored branch config contradicts itself or its assets")?;
	T::BranchConfigBounds::get()
		.ensure_within(&branch.config)
		.map_err(|_| "stored branch config is outside BranchConfigBounds")?;
	Ok(())
}

/// `StablecoinDebt` must equal its full recomputation from `Branches`.
fn check_stablecoin_debt<T: Config>(
	mut expected: BTreeMap<StableIdOf<T>, BranchContribution<BalanceOf<T>>>,
	now: Millis,
) -> Result<(), TryRuntimeError> {
	for (stable_id, stored) in StablecoinDebt::<T>::iter() {
		ensure!(!stored.is_empty(), "empty StablecoinDebt record stored");
		ensure!(stored.last_update <= now, "StablecoinDebt last_update is ahead of now");
		let elapsed = now.saturating_sub(stored.last_update);
		let projected = stored
			.pending_interest
			.checked_add(
				math::interest_numerator(stored.active_accrual_rate, elapsed)
					.ok_or("StablecoinDebt projection overflow")?,
			)
			.ok_or("StablecoinDebt projection overflow")?;
		let BranchContribution {
			outstanding,
			pending_interest: pending,
			active_accrual_rate: active,
		} = expected.remove(&stable_id).unwrap_or_default();
		ensure!(
			stored.outstanding == outstanding,
			"StablecoinDebt outstanding diverges from Branches"
		);
		ensure!(
			stored.active_accrual_rate == active,
			"StablecoinDebt active accrual rate diverges from Branches"
		);
		ensure!(projected == pending, "StablecoinDebt numerator diverges from Branches");
	}
	ensure!(
		expected.into_values().all(|sums| {
			sums.outstanding.is_zero() &&
				sums.active_accrual_rate.is_zero() &&
				sums.pending_interest.is_zero()
		}),
		"stablecoin with debt lacks a StablecoinDebt record"
	);
	Ok(())
}

/// Role exclusivity and the stable-side index: no stablecoin key may appear
/// among `Branches`' collateral keys, and `StablecoinMarkets` must equal its
/// full recomputation from `Branches` — same entries, same counts, nothing
/// extra.
fn check_stablecoin_markets<T: Config>(
	collaterals: &BTreeSet<CollateralIdOf<T>>,
	counts: BTreeMap<CollateralIdOf<T>, u32>,
) -> Result<(), TryRuntimeError> {
	ensure!(
		!counts.keys().any(|stable_key| collaterals.contains(stable_key)),
		"asset used as both collateral and stablecoin across branches"
	);
	let stored: BTreeMap<CollateralIdOf<T>, u32> = StablecoinMarkets::<T>::iter().collect();
	ensure!(stored == counts, "StablecoinMarkets diverges from its recomputation over Branches");
	Ok(())
}

/// Owner-hold invariant: per `(owner, collateral C)`, the owner's
/// `VaultCollateral` hold on `C` equals the sum of `vault.collateral` across
/// every stablecoin branch on `C`. The sum is accumulated in `check_branch_identities`'
/// single vault pass; here we only compare it against the on-chain hold. With one
/// stablecoin per collateral this collapses to a single term.
fn check_owner_holds<T: Config>(
	owner_collateral: BTreeMap<(CollateralIdOf<T>, T::AccountId), BalanceOf<T>>,
) -> Result<(), TryRuntimeError> {
	for ((collateral_id, owner), sum) in owner_collateral {
		let held = T::CollateralAssets::balance_on_hold(
			collateral_id,
			&HoldReason::VaultCollateral.into(),
			&owner,
		);
		ensure!(sum == held, "Σ vault.collateral over a collateral's branches != owner hold");
	}
	Ok(())
}

/// Single pass over `Vaults::<T>::iter_prefix(c)`: per-vault membership and
/// interest-clock invariants, and redistribution accounting sums. The sums
/// are checked, not saturating, so an overflow is diagnosed rather than
/// silently absorbed into a saturated comparison.
fn check_branch_identities<T: Config>(
	collateral_id: &CollateralIdOf<T>,
	stable_id: &StableIdOf<T>,
	branch: &BranchOf<T>,
	rate_list: &VaultListIdOf<T>,
	recovery_list: &VaultListIdOf<T>,
	now: Millis,
	owner_collateral: &mut BTreeMap<(CollateralIdOf<T>, T::AccountId), BalanceOf<T>>,
) -> Result<(), TryRuntimeError> {
	let state = &branch.state;
	let tau = state.interest_time(now);
	ensure!(
		state.debt.last_interest_time <= tau,
		"branch last_interest_time ahead of interest_time(now)"
	);
	ensure!(
		state.debt.interest_minted_ahead.is_zero() ||
			state.debt.pending_interest_attribution.is_zero(),
		"interest both minted ahead and pending attribution"
	);
	ensure!(
		state.debt.aggregate_interest_remainder < math::INTEREST_DENOMINATOR,
		"aggregate interest remainder is not normalized"
	);

	let mut sum_stake = BalanceOf::<T>::zero();
	let mut sum_market_collateral = BalanceOf::<T>::zero();
	let mut sum_principal = BalanceOf::<T>::zero();
	let mut sum_interest = BalanceOf::<T>::zero();
	let mut sum_accrual_rate = U256::zero();
	let mut sum_stake_accrual_rate = U256::zero();
	let mut sum_claimable_accrual_rate = U256::zero();
	let mut sum_eligible_collateral = BalanceOf::<T>::zero();
	let mut vault_count: u32 = 0;

	for (owner, record) in Vaults::<T>::iter_prefix((collateral_id, stable_id)) {
		let vault = record.vault;
		vault_count = vault_count.checked_add(1).ok_or("branch vault count overflow")?;
		ensure!(
			vault.last_interest_time <= tau,
			"vault last_interest_time ahead of interest_time(now)"
		);
		ensure!(
			vault.interest_prepaid < math::INTEREST_DENOMINATOR,
			"vault prepaid interest is not normalized"
		);
		let checkpoint = &vault.redistribution_checkpoint;
		let totals = &state.redistribution;
		ensure!(
			checkpoint.principal_per_stake <= totals.principal_per_stake,
			"vault redistribution checkpoint is ahead of branch totals"
		);
		ensure!(
			checkpoint.collateral_per_stake <= totals.collateral_per_stake,
			"vault redistribution checkpoint is ahead of branch totals"
		);
		ensure!(
			checkpoint.principal_time_per_stake <= totals.principal_time_per_stake,
			"vault redistribution checkpoint is ahead of branch totals"
		);
		if vault.debt.total().is_zero() {
			ensure!(vault.interest_prepaid == 0, "debt-free vault carries prepaid interest");
		}
		let in_rate_index = T::VaultLists::contains(rate_list, &owner);
		let in_recovery = T::VaultLists::contains(recovery_list, &owner);
		ensure!(!(in_rate_index && in_recovery), "vault in both rate index and recovery FIFO");
		// The row carries this branch's share of the owner's hold; the cross-branch
		// owner-hold invariant (`Σ_stablecoins vault.collateral == hold`) is
		// accumulated here and checked once, globally, in `do_try_state`.
		let owner_entry =
			owner_collateral.entry((collateral_id.clone(), owner.clone())).or_default();
		*owner_entry = owner_entry
			.checked_add(&vault.collateral)
			.ok_or("owner collateral sum overflow")?;
		sum_market_collateral = sum_market_collateral
			.checked_add(&vault.collateral)
			.ok_or("branch collateral sum overflow")?;
		// Every row — FinalRecovery included — keeps its debt attached to the
		// branch aggregates; only the stake is detached while in the FIFO.
		sum_principal = sum_principal
			.checked_add(&vault.debt.principal)
			.ok_or("branch principal sum overflow")?;
		sum_interest = sum_interest
			.checked_add(&vault.debt.interest)
			.ok_or("branch interest sum overflow")?;
		sum_accrual_rate = sum_accrual_rate
			.checked_add(math::accrual_rate(vault.debt.principal, vault.annual_rate))
			.ok_or("accrual rate sum overflow")?;
		if in_recovery {
			ensure!(
				vault.redistribution_stake.is_zero(),
				"FinalRecovery vault has non-zero redistribution_stake"
			);
			continue;
		}
		if in_rate_index {
			ensure!(!vault.debt.total().is_zero(), "debt-free vault remains in the rate index");
		}
		// Redistribution can give debt to Dormant vaults that ordinary redemption cannot target.
		if vault.collateral.is_zero() {
			ensure!(
				vault.redistribution_stake.is_zero(),
				"collateral-free vault has redistribution stake"
			);
		} else {
			ensure!(
				!vault.redistribution_stake.is_zero(),
				"eligible vault with collateral has zero redistribution stake"
			);
		}
		sum_stake = sum_stake
			.checked_add(&vault.redistribution_stake)
			.ok_or("branch stake sum overflow")?;
		if !vault.redistribution_stake.is_zero() {
			sum_eligible_collateral = sum_eligible_collateral
				.checked_add(&vault.collateral)
				.ok_or("eligible collateral sum overflow")?;
			let stake_accrual_rate =
				math::accrual_rate(vault.redistribution_stake, vault.annual_rate);
			sum_stake_accrual_rate = sum_stake_accrual_rate
				.checked_add(stake_accrual_rate)
				.ok_or("stake accrual rate sum overflow")?;
			let unclaimed = state
				.redistribution
				.principal_per_stake
				.checked_sub(&vault.redistribution_checkpoint.principal_per_stake)
				.ok_or("vault redistribution checkpoint is ahead of branch totals")?;
			let claimable = math::claimable_accrual_rate(stake_accrual_rate, unclaimed)
				.ok_or("claimable redistribution accrual rate overflow")?;
			sum_claimable_accrual_rate = sum_claimable_accrual_rate
				.checked_add(claimable)
				.ok_or("claimable redistribution accrual rate sum overflow")?;
		}
	}
	ensure!(state.vault_count == vault_count, "branch vault_count != number of vault rows");

	ensure!(
		state.stakes.total == sum_stake,
		"total_stakes != Σ eligible vault.redistribution_stake"
	);
	ensure!(
		state.stakes.accrual_rate == sum_stake_accrual_rate,
		"stake accrual rate != exact Σ rate · stake"
	);
	ensure!(state.debt.principal == sum_principal, "branch principal != Σ vault principal");
	let issued_interest = sum_interest
		.checked_add(&state.debt.pending_interest_attribution)
		.ok_or("issued interest sum overflow")?;
	ensure!(
		state.debt.minted_interest == issued_interest,
		"minted interest != Σ vault interest + pending attribution"
	);
	let expected_accrual_rate = sum_accrual_rate
		.checked_add(state.debt.pending_redistribution_accrual_rate)
		.ok_or("accrual rate plus pending sum overflow")?;
	ensure!(
		state.debt.accrual_rate == expected_accrual_rate,
		"accrual rate != Σ vault accrual rate + pending redistribution accrual rate"
	);
	let expected_basis = sum_eligible_collateral
		.checked_add(&state.pending_redistribution_collateral)
		.ok_or("redistribution collateral basis overflow")?;
	ensure!(
		state.stakes.collateral_basis == expected_basis,
		"stake collateral basis != eligible rows + pending collateral"
	);
	// The pool is posted rounded down and claimed rounded up, so the branch never projects
	// interest on an accrual rate that its vaults will not claim.
	ensure!(
		state.debt.pending_redistribution_accrual_rate <= sum_claimable_accrual_rate,
		"pending redistribution accrual rate exceeds what vaults can claim"
	);

	let redistribution_account = Pallet::<T>::redistribution_account(collateral_id, stable_id);
	let held_redistribution = T::CollateralAssets::balance_on_hold(
		collateral_id.clone(),
		&HoldReason::VaultCollateral.into(),
		&redistribution_account,
	);
	ensure!(
		held_redistribution == state.pending_redistribution_collateral,
		"redistribution account hold != pending collateral"
	);
	// A hold must leave the asset's minimum balance free, so the registration seed has to survive
	// every seizure for the next one to succeed.
	let custody_free = T::CollateralAssets::balance(collateral_id.clone(), &redistribution_account);
	ensure!(
		custody_free >= T::CollateralAssets::minimum_balance(collateral_id.clone()),
		"redistribution account free balance below the collateral minimum"
	);
	let expected_total_collateral = sum_market_collateral
		.checked_add(&state.pending_redistribution_collateral)
		.ok_or("total collateral sum overflow")?;
	ensure!(
		state.total_collateral == expected_total_collateral,
		"total_collateral != rows + pending redistribution collateral"
	);
	Ok(())
}
