//! `try_state` invariant verification.
//!
//! Gated on `feature = "try-runtime"`. Run after every test by the mock's
//! `next_block` and end-to-end by the runtime's pre-upgrade hook.

use crate::{
	pallet::{
		AssetRoles, BalanceOf, BranchOf, Branches, CollateralIdOf, CollateralRisks, Config,
		HoldReason, Millis, Pallet, StableIdOf, StablecoinDebt, Vaults,
	},
	types::{AssetRole, AssetRoleUsage, PendingInterest, VaultListId},
};
use alloc::collections::BTreeMap;
use frame::{
	arithmetic::{
		CheckedAdd, FixedPointNumber, FixedU128, One, Saturating, UniqueSaturatedInto, Zero,
	},
	traits::{fungibles::InspectHold, Convert, Time},
	try_runtime::TryRuntimeError,
};
use linked_list_interface::SortedListInterface;

pub fn do_try_state<T: Config>() -> Result<(), TryRuntimeError> {
	let now = T::TimeProvider::now();
	// Owner-hold invariant accumulator: filled per branch inside
	// `check_branch_identities`' single vault pass, then checked once below.
	let mut owner_collateral: BTreeMap<(CollateralIdOf<T>, T::AccountId), BalanceOf<T>> =
		BTreeMap::new();
	// Derived-index accumulators, recomputed in full from the authoritative
	// registry and compared against `AssetRoles`/`CollateralRisks` below.
	let mut roles: BTreeMap<CollateralIdOf<T>, AssetRoleUsage> = BTreeMap::new();
	let mut outstanding: BTreeMap<CollateralIdOf<T>, BalanceOf<T>> = BTreeMap::new();
	let mut stablecoin_debt: BTreeMap<
		StableIdOf<T>,
		(BalanceOf<T>, BalanceOf<T>, PendingInterest<BalanceOf<T>>),
	> = BTreeMap::new();
	for (collateral_id, stable_id, branch) in Branches::<T>::iter() {
		claim_role::<T>(&mut roles, collateral_id.clone(), AssetRole::Collateral)?;
		claim_role::<T>(
			&mut roles,
			T::StableToCollateralId::convert(stable_id.clone()),
			AssetRole::Stable,
		)?;
		let branch_outstanding = branch
			.state
			.debt
			.outstanding()
			.checked_add(&branch.state.ownerless_debt)
			.ok_or("branch outstanding debt overflow")?;
		let debt_entry = outstanding.entry(collateral_id.clone()).or_default();
		*debt_entry = debt_entry
			.checked_add(&branch_outstanding)
			.ok_or("collateral outstanding-debt sum overflow")?;
		let stable_entry = stablecoin_debt.entry(stable_id.clone()).or_default();
		stable_entry.0 = stable_entry
			.0
			.checked_add(&branch_outstanding)
			.ok_or("stablecoin outstanding-debt sum overflow")?;
		if !branch.state.is_frozen() {
			stable_entry.1 = stable_entry
				.1
				.checked_add(&branch.state.debt.weighted_principal_sum)
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
	check_collateral_risks::<T>(outstanding)?;
	check_stablecoin_debt::<T>(stablecoin_debt, now)?;
	Ok(())
}

/// `StablecoinDebt` must equal its full recomputation from `Branches`.
fn check_stablecoin_debt<T: Config>(
	mut expected: BTreeMap<
		StableIdOf<T>,
		(BalanceOf<T>, BalanceOf<T>, PendingInterest<BalanceOf<T>>),
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
				&PendingInterest::from_weight_millis(stored.active_weighted_principal_sum, elapsed)
					.ok_or("StablecoinDebt projection overflow")?,
			)
			.ok_or("StablecoinDebt projection overflow")?;
		let (outstanding, active, pending) = expected.remove(&stable_id).unwrap_or_default();
		if stored.outstanding != outstanding {
			return Err("StablecoinDebt outstanding diverges from Branches".into());
		}
		if stored.active_weighted_principal_sum != active {
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

/// Every `CollateralRisks` record must carry the recomputed outstanding total
/// for its collateral, no default records may be stored (the write paths
/// remove those), and no collateral with outstanding debt may lack a record.
/// The `debt_ceiling` side is a governance input with no recomputation.
fn check_collateral_risks<T: Config>(
	mut outstanding: BTreeMap<CollateralIdOf<T>, BalanceOf<T>>,
) -> Result<(), TryRuntimeError> {
	for (collateral_id, risk) in CollateralRisks::<T>::iter() {
		if risk.is_empty() {
			return Err("default CollateralRisk record stored".into());
		}
		let recomputed = outstanding.remove(&collateral_id).unwrap_or_default();
		if risk.outstanding != recomputed {
			return Err("CollateralRisks diverges from its recomputation over Branches".into());
		}
	}
	if outstanding.into_values().any(|total| !total.is_zero()) {
		return Err("collateral with outstanding debt lacks a CollateralRisks record".into());
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
	let cumul_debt_ps = state.redistribution.debt_per_stake;
	let cumul_collat_ps = state.redistribution.collateral_per_stake;

	let mut sum_stake = BalanceOf::<T>::zero();
	let mut sum_market_collateral = BalanceOf::<T>::zero();
	let mut sum_pending_debt_share = BalanceOf::<T>::zero();
	let mut sum_pending_collat_share = BalanceOf::<T>::zero();
	let mut sum_principal = BalanceOf::<T>::zero();
	let mut sum_weighted_principal = BalanceOf::<T>::zero();
	let mut sum_weighted_stake = BalanceOf::<T>::zero();
	let mut n_live_vaults: u128 = 0;

	for (owner, vault) in Vaults::<T>::iter_prefix((collateral_id, stable_id)) {
		if vault.last_interest_time > tau {
			return Err("vault last_interest_time ahead of interest_time(now)".into());
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
		sum_weighted_principal = sum_weighted_principal
			.checked_add(
				&vault
					.annual_rate
					.checked_mul_int(vault.debt.principal)
					.ok_or("weighted principal term overflow")?,
			)
			.ok_or("weighted principal sum overflow")?;
		sum_weighted_stake = sum_weighted_stake
			.checked_add(
				&vault
					.annual_rate
					.checked_mul_int(vault.redistribution_stake)
					.ok_or("weighted stake term overflow")?,
			)
			.ok_or("weighted stake sum overflow")?;
		if in_recovery {
			if !vault.redistribution_stake.is_zero() {
				return Err("FinalRecovery vault has non-zero redistribution_stake".into());
			}
			continue;
		}
		if vault.redistribution_stake != vault.collateral {
			return Err("vault.redistribution_stake != vault.collateral".into());
		}
		sum_stake = sum_stake
			.checked_add(&vault.redistribution_stake)
			.ok_or("branch stake sum overflow")?;
		let snap = vault.redistribution_checkpoint;
		let delta_debt = cumul_debt_ps.saturating_sub(snap.debt_per_stake);
		sum_pending_debt_share = sum_pending_debt_share
			.saturating_add(delta_debt.saturating_mul_int(vault.redistribution_stake));
		let delta_collat = cumul_collat_ps.saturating_sub(snap.collateral_per_stake);
		sum_pending_collat_share = sum_pending_collat_share
			.saturating_add(delta_collat.saturating_mul_int(vault.redistribution_stake));
		n_live_vaults = n_live_vaults.saturating_add(1);
	}

	if state.stakes.total != sum_stake {
		return Err("total_stakes != Σ active+dormant vault.redistribution_stake".into());
	}
	// Every writer moves principal on the branch and the vault by the same
	// amount, so this identity is exact (the prepare→finalize liquidation gap
	// is intra-extrinsic and invisible at block end).
	if state.debt.principal != sum_principal {
		return Err("branch principal != Σ vault principal".into());
	}
	// Every stake mutation swaps full `floor(rate · stake)` contributions, so
	// this identity is exact as well.
	if state.stakes.weighted_sum != sum_weighted_stake {
		return Err("stakes.weighted_sum != Σ floor(rate · stake)".into());
	}
	check_weighted_principal_sum::<T>(
		&branch.config,
		state.debt.weighted_principal_sum,
		state.debt.pending_redistribution_principal.saturating_add(state.ownerless_debt),
		sum_weighted_principal,
		n_live_vaults,
	)?;

	let tolerance: BalanceOf<T> = n_live_vaults.unique_saturated_into();

	let debt_drift = if state.debt.pending_redistribution_principal >= sum_pending_debt_share {
		state
			.debt
			.pending_redistribution_principal
			.saturating_sub(sum_pending_debt_share)
	} else {
		sum_pending_debt_share.saturating_sub(state.debt.pending_redistribution_principal)
	};
	if debt_drift > tolerance {
		return Err("pending redistribution principal drift exceeds rounding tolerance".into());
	}

	let held_redistribution = T::CollateralAssets::balance_on_hold(
		collateral_id.clone(),
		&HoldReason::VaultCollateral.into(),
		&Pallet::<T>::redistribution_account(collateral_id, stable_id),
	);
	let physical = sum_market_collateral
		.checked_add(&held_redistribution)
		.ok_or("physical collateral sum overflow")?;
	if state.total_collateral != physical {
		return Err("total_collateral != Σ owner-held + redistribution-account hold".into());
	}

	// The redistribution account's hold = Σ pending collateral shares vaults
	// will pick up on next touch + ownerless collateral surplus. Per-vault
	// flooring may leave shares slightly below the held amount; treat the gap
	// as tolerance plus the explicit ownerless bucket.
	let claimed_plus_surplus = sum_pending_collat_share.saturating_add(state.ownerless_collateral);
	let collateral_drift = if held_redistribution >= claimed_plus_surplus {
		held_redistribution.saturating_sub(claimed_plus_surplus)
	} else {
		claimed_plus_surplus.saturating_sub(held_redistribution)
	};
	if collateral_drift > tolerance {
		return Err("pending collateral share drift exceeds rounding tolerance".into());
	}
	Ok(())
}

fn check_weighted_principal_sum<T: Config>(
	config: &crate::types::BranchConfig<BalanceOf<T>>,
	weighted_principal_sum: BalanceOf<T>,
	pending_pool: BalanceOf<T>,
	sum_weighted_principal: BalanceOf<T>,
	n_live_vaults: u128,
) -> Result<(), TryRuntimeError> {
	if weighted_principal_sum < sum_weighted_principal {
		return Err("weighted_principal_sum below Σ floor(rate · principal)".into());
	}
	let rate_bound = config.maximum_borrow_rate.max(FixedU128::one());
	let w_pending = rate_bound.saturating_mul_int(pending_pool);
	let slack: BalanceOf<T> = n_live_vaults.saturating_add(1).unique_saturated_into();
	let upper = sum_weighted_principal.saturating_add(w_pending).saturating_add(slack);
	if weighted_principal_sum > upper {
		return Err("weighted_principal_sum exceeds Σ floor(rate · principal) + allowance".into());
	}
	Ok(())
}
