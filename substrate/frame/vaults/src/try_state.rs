//! `try_state` invariant verification.
//!
//! Gated on `feature = "try-runtime"`. Run after every test by the mock's
//! `next_block` and end-to-end by the runtime's pre-upgrade hook.

use crate::{
	pallet::{
		AssetRoles, BalanceOf, BranchOf, Branches, CollateralIdOf, Config, GlobalDebtCeilings,
		HoldReason, Millis, Pallet, StableIdOf, StablecoinDebt, Vaults,
	},
	types::{AssetRole, AssetRoleUsage, InterestWeight, PendingInterest, VaultListId},
};
use alloc::collections::BTreeMap;
use frame::{
	arithmetic::{CheckedAdd, FixedPointNumber, FixedU128, Zero},
	traits::{
		fungibles::{Inspect as FungiblesInspect, InspectHold},
		Convert, Time,
	},
	try_runtime::TryRuntimeError,
};
use linked_list_interface::SortedListInterface;

pub fn do_try_state<T: Config>() -> Result<(), TryRuntimeError> {
	let now = T::TimeProvider::now();
	// Owner-hold invariant accumulator: filled per branch inside
	// `check_branch_identities`' single vault pass, then checked once below.
	let mut owner_collateral: BTreeMap<(CollateralIdOf<T>, T::AccountId), BalanceOf<T>> =
		BTreeMap::new();
	// Derived-index accumulators, recomputed in full from the authoritative registry.
	let mut roles: BTreeMap<CollateralIdOf<T>, AssetRoleUsage> = BTreeMap::new();
	let mut stablecoin_debt: BTreeMap<
		StableIdOf<T>,
		(BalanceOf<T>, InterestWeight<BalanceOf<T>>, PendingInterest<BalanceOf<T>>),
	> = BTreeMap::new();
	for (collateral_id, stable_id, branch) in Branches::<T>::iter() {
		claim_role::<T>(&mut roles, collateral_id.clone(), AssetRole::Collateral)?;
		claim_role::<T>(
			&mut roles,
			T::StableToCollateralId::convert(stable_id.clone()),
			AssetRole::Stable,
		)?;
		let branch_outstanding = branch.state.debt.outstanding();
		let stable_entry = stablecoin_debt.entry(stable_id.clone()).or_default();
		stable_entry.0 = stable_entry
			.0
			.checked_add(&branch_outstanding)
			.ok_or("stablecoin outstanding-debt sum overflow")?;
		if !branch.state.is_frozen() {
			stable_entry.1 = stable_entry
				.1
				.checked_add(&branch.state.debt.weighted_principal)
				.ok_or("stablecoin active weighted-principal sum overflow")?;
		}
		stable_entry.2 = stable_entry
			.2
			.checked_add(
				&Pallet::<T>::branch_pending_interest(&branch.state, now)
					.map_err(|_| "branch pending-interest numerator overflow")?,
			)
			.ok_or("stablecoin pending-interest numerator overflow")?;
		let (collateral_id, stable_id) = (&collateral_id, &stable_id);
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
		// `dormant_redemption_target`, when set, must point at a Dormant vault.
		if let Some(owner) = branch.state.dormant_redemption_target.clone() {
			if !Vaults::<T>::contains_key((collateral_id, stable_id, &owner)) {
				return Err("dormant_redemption_target points at missing vault".into());
			}
			if !Pallet::<T>::vault_status_of(collateral_id, stable_id, &owner).is_dormant() {
				return Err("dormant_redemption_target points at non-Dormant".into());
			}
		}
	}

	check_owner_holds::<T>(owner_collateral)?;
	check_asset_roles::<T>(roles)?;
	if GlobalDebtCeilings::<T>::iter_values().any(|ceiling| ceiling.is_zero()) {
		return Err("zero GlobalDebtCeilings record stored".into());
	}
	check_stablecoin_debt::<T>(stablecoin_debt, now)?;
	Ok(())
}

/// `StablecoinDebt` must equal its full recomputation from `Branches`.
fn check_stablecoin_debt<T: Config>(
	mut expected: BTreeMap<
		StableIdOf<T>,
		(BalanceOf<T>, InterestWeight<BalanceOf<T>>, PendingInterest<BalanceOf<T>>),
	>,
	now: Millis,
) -> Result<(), TryRuntimeError> {
	for (stable_id, stored) in StablecoinDebt::<T>::iter() {
		if stored.is_empty() {
			return Err("empty StablecoinDebt record stored".into());
		}
		if stored.last_update > now {
			return Err("StablecoinDebt last_update is ahead of now".into());
		}
		let elapsed = now.saturating_sub(stored.last_update);
		let projected = stored
			.pending_interest
			.checked_add(
				&PendingInterest::from_interest_weight(stored.active_weighted_principal, elapsed)
					.ok_or("StablecoinDebt projection overflow")?,
			)
			.ok_or("StablecoinDebt projection overflow")?;
		let (outstanding, active, pending) = expected.remove(&stable_id).unwrap_or_default();
		if stored.outstanding != outstanding {
			return Err("StablecoinDebt outstanding diverges from Branches".into());
		}
		if stored.active_weighted_principal != active {
			return Err("StablecoinDebt active weight diverges from Branches".into());
		}
		if projected != pending {
			return Err("StablecoinDebt numerator diverges from Branches".into());
		}
	}
	if expected.into_values().any(|(outstanding, active, pending)| {
		!outstanding.is_zero() || !active.is_zero() || !pending.is_zero()
	}) {
		return Err("stablecoin with debt lacks a StablecoinDebt record".into());
	}
	Ok(())
}

/// Accumulate one market reference for `asset` in `role`, rejecting an asset
/// that appears on both sides of the registry.
fn claim_role<T: Config>(
	roles: &mut BTreeMap<CollateralIdOf<T>, AssetRoleUsage>,
	asset: CollateralIdOf<T>,
	role: AssetRole,
) -> Result<(), TryRuntimeError> {
	let usage = roles.entry(asset).or_insert(AssetRoleUsage { role, markets: 0 });
	if usage.role != role {
		return Err("asset used as both collateral and stablecoin across markets".into());
	}
	usage.markets = usage.markets.checked_add(1).ok_or("asset role reference count overflow")?;
	Ok(())
}

/// `AssetRoles` must equal its full recomputation from `Branches` — same
/// entries, same roles, same reference counts, nothing extra.
fn check_asset_roles<T: Config>(
	roles: BTreeMap<CollateralIdOf<T>, AssetRoleUsage>,
) -> Result<(), TryRuntimeError> {
	let stored: BTreeMap<CollateralIdOf<T>, AssetRoleUsage> = AssetRoles::<T>::iter().collect();
	if stored != roles {
		return Err("AssetRoles diverges from its recomputation over Branches".into());
	}
	Ok(())
}

/// Owner-hold invariant: per `(owner, collateral C)`, the owner's
/// `VaultCollateral` hold on `C` equals the sum of `vault.collateral` across
/// every stablecoin market on `C`. The sum is accumulated in `check_branch_identities`'
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
		if sum != held {
			return Err("Σ vault.collateral over a collateral's markets != owner hold".into());
		}
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
	rate_list: &VaultListId<CollateralIdOf<T>, StableIdOf<T>>,
	recovery_list: &VaultListId<CollateralIdOf<T>, StableIdOf<T>>,
	now: Millis,
	owner_collateral: &mut BTreeMap<(CollateralIdOf<T>, T::AccountId), BalanceOf<T>>,
) -> Result<(), TryRuntimeError> {
	let state = &branch.state;
	let tau = state.interest_time(now);
	if state.debt.last_interest_time > tau {
		return Err("branch last_interest_time ahead of interest_time(now)".into());
	}
	let interest_denominator = PendingInterest::<BalanceOf<T>>::DENOMINATOR;
	if state.debt.aggregate_interest_remainder >= interest_denominator {
		return Err("aggregate interest remainder is not normalized".into());
	}
	if state.debt.weighted_principal.remainder >= FixedU128::DIV {
		return Err("weighted principal remainder is not normalized".into());
	}

	let mut sum_stake = BalanceOf::<T>::zero();
	let mut sum_market_collateral = BalanceOf::<T>::zero();
	let mut sum_principal = BalanceOf::<T>::zero();
	let mut sum_interest = BalanceOf::<T>::zero();
	let mut sum_weighted_principal = InterestWeight::<BalanceOf<T>>::default();
	let mut sum_weighted_stake = InterestWeight::<BalanceOf<T>>::default();
	let mut sum_eligible_collateral = BalanceOf::<T>::zero();
	let mut vault_count: u32 = 0;

	for (owner, vault) in Vaults::<T>::iter_prefix((collateral_id, stable_id)) {
		vault_count = vault_count.checked_add(1).ok_or("branch vault count overflow")?;
		if vault.last_interest_time > tau {
			return Err("vault last_interest_time ahead of interest_time(now)".into());
		}
		if vault.interest_remainder >= interest_denominator {
			return Err("vault interest remainder is not normalized".into());
		}
		if vault.redistribution_checkpoint.principal_per_stake >
			state.redistribution.principal_per_stake ||
			vault.redistribution_checkpoint.collateral_per_stake >
				state.redistribution.collateral_per_stake ||
			vault.redistribution_checkpoint.weight_per_weighted_stake >
				state.redistribution.weight_per_weighted_stake ||
			vault.redistribution_checkpoint.weight_time_per_weighted_stake.to_wide() >
				state.redistribution.weight_time_per_weighted_stake.to_wide()
		{
			return Err("vault redistribution checkpoint is ahead of branch totals".into());
		}
		// Debt-free vaults must not retain fractional interest.
		if vault.debt.total().is_zero() && vault.interest_remainder != 0 {
			return Err("debt-free vault carries an interest fraction".into());
		}
		let in_rate_index = T::VaultLists::contains(rate_list, &owner);
		let in_recovery = T::VaultLists::contains(recovery_list, &owner);
		if in_rate_index && in_recovery {
			return Err("vault in both rate index and recovery FIFO".into());
		}
		// The row carries this market's share of the owner's hold; the cross-market
		// owner-hold invariant (`Σ_stablecoins vault.collateral == hold`) is
		// accumulated here and checked once, globally, in `do_try_state`.
		let owner_entry =
			owner_collateral.entry((collateral_id.clone(), owner.clone())).or_default();
		*owner_entry = owner_entry
			.checked_add(&vault.collateral)
			.ok_or("owner collateral sum overflow")?;
		sum_market_collateral = sum_market_collateral
			.checked_add(&vault.collateral)
			.ok_or("market collateral sum overflow")?;
		// Every row — FinalRecovery included — keeps its debt attached to the
		// branch aggregates; only the stake is detached while in the FIFO.
		sum_principal = sum_principal
			.checked_add(&vault.debt.principal)
			.ok_or("branch principal sum overflow")?;
		sum_interest = sum_interest
			.checked_add(&vault.debt.interest)
			.ok_or("branch interest sum overflow")?;
		sum_weighted_principal = sum_weighted_principal
			.checked_add(
				&InterestWeight::from_principal_rate(vault.debt.principal, vault.annual_rate)
					.ok_or("weighted principal term overflow")?,
			)
			.ok_or("weighted principal sum overflow")?;
		if in_recovery {
			if !vault.redistribution_stake.is_zero() {
				return Err("FinalRecovery vault has non-zero redistribution_stake".into());
			}
			continue;
		}
		if in_rate_index && vault.debt.total().is_zero() {
			return Err("debt-free vault remains in the rate index".into());
		}
		if !in_rate_index &&
			!vault.debt.total().is_zero() &&
			state.dormant_redemption_target.as_ref() != Some(&owner)
		{
			return Err("debt-bearing Dormant vault is not the redemption target".into());
		}
		if vault.debt.total().is_zero() && !vault.redistribution_stake.is_zero() {
			return Err("debt-free vault has redistribution stake".into());
		}
		if !vault.debt.total().is_zero() && vault.redistribution_stake.is_zero() {
			return Err("eligible debt-bearing vault has zero redistribution stake".into());
		}
		sum_stake = sum_stake
			.checked_add(&vault.redistribution_stake)
			.ok_or("branch stake sum overflow")?;
		if !vault.redistribution_stake.is_zero() {
			sum_eligible_collateral = sum_eligible_collateral
				.checked_add(&vault.collateral)
				.ok_or("eligible collateral sum overflow")?;
			sum_weighted_stake = sum_weighted_stake
				.checked_add(
					&InterestWeight::from_principal_rate(
						vault.redistribution_stake,
						vault.annual_rate,
					)
					.ok_or("weighted stake term overflow")?,
				)
				.ok_or("weighted stake sum overflow")?;
		}
	}
	if state.vault_count != vault_count {
		return Err("branch vault_count != number of vault rows".into());
	}

	if state.stakes.total != sum_stake {
		return Err("total_stakes != Σ debt-bearing vault.redistribution_stake".into());
	}
	if state.stakes.weighted != sum_weighted_stake {
		return Err("weighted stakes != exact Σ rate · stake".into());
	}
	if state.debt.principal != sum_principal {
		return Err("branch principal != Σ vault principal".into());
	}
	let issued_interest = sum_interest
		.checked_add(&state.debt.pending_interest_attribution)
		.ok_or("issued interest sum overflow")?;
	if state.debt.minted_interest != issued_interest {
		return Err("minted interest != Σ vault interest + pending attribution".into());
	}
	let expected_weighted_principal = sum_weighted_principal
		.checked_add(&state.debt.pending_redistribution_weight)
		.ok_or("weighted principal plus pending sum overflow")?;
	if state.debt.weighted_principal != expected_weighted_principal {
		return Err("weighted principal != row weight + pending redistribution weight".into());
	}
	let expected_basis = sum_eligible_collateral
		.checked_add(&state.pending_redistribution_collateral)
		.ok_or("redistribution collateral basis overflow")?;
	if state.stakes.collateral_basis != expected_basis {
		return Err("stake collateral basis != eligible rows + pending collateral".into());
	}
	let maximum_weight_time = state
		.debt
		.pending_redistribution_weight
		.raw()
		.checked_mul(tau.into())
		.ok_or("pending redistribution weight-time bound overflow")?;
	if state.pending_redistribution_weight_time.to_wide() > maximum_weight_time {
		return Err("pending redistribution weight-time exceeds current-time bound".into());
	}

	let redistribution_account = Pallet::<T>::redistribution_account(collateral_id, stable_id);
	let held_redistribution = T::CollateralAssets::balance_on_hold(
		collateral_id.clone(),
		&HoldReason::VaultCollateral.into(),
		&redistribution_account,
	);
	if held_redistribution != state.pending_redistribution_collateral {
		return Err("redistribution account hold != pending collateral".into());
	}
	// A hold must leave the asset's minimum balance free, so the registration seed has to survive
	// every seizure for the next one to succeed.
	let custody_free = T::CollateralAssets::balance(collateral_id.clone(), &redistribution_account);
	if custody_free < T::CollateralAssets::minimum_balance(collateral_id.clone()) {
		return Err("redistribution account free balance below the collateral minimum".into());
	}
	let expected_total_collateral = sum_market_collateral
		.checked_add(&state.pending_redistribution_collateral)
		.ok_or("total collateral sum overflow")?;
	if state.total_collateral != expected_total_collateral {
		return Err("total_collateral != rows + pending redistribution collateral".into());
	}
	Ok(())
}
