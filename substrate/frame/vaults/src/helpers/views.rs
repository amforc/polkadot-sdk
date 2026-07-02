use super::*;
use pusd_primitives::RedemptionTargetKind;

/// Fully-accrued total branch debt (principal + minted interest + pending
/// aggregate interest + pending redistribution + bad debt + ownerless debt).
/// Mirrors the numerator-side of [`compute_tcr`]; used to size the redemption
/// fee's redeemed fraction. Zero for an unregistered branch.
pub fn view_branch_debt<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	now: Millis,
) -> BalanceOf<T> {
	let Some(bs) = BranchStates::<T>::get(collateral_id, stable_id) else {
		return BalanceOf::<T>::zero();
	};
	accrued_branch_debt::<T>(&bs, now)
}

pub fn view_vault_status<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	owner: &T::AccountId,
) -> Option<VaultStatus> {
	let vault = Vaults::<T>::get((collateral_id, stable_id, owner))?;
	Some(vault.status::<T>(collateral_id, stable_id, owner))
}
pub fn view_vault_cr<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	owner: &T::AccountId,
) -> Option<FixedU128> {
	let vault = Vaults::<T>::get((collateral_id, stable_id, owner))?;
	let state = BranchStates::<T>::get(collateral_id, stable_id)?;
	let now = T::TimeProvider::now();
	let price = T::Oracle::provide_price(collateral_id).ok()?.price;
	let pending = pending_touch_for::<T>(&vault, &state, now);
	let total_coll = vault.collateral.saturating_add(pending.collateral);
	let total_debt = vault
		.debt
		.total()
		.saturating_add(pending.principal)
		.saturating_add(pending.interest);
	pusd_primitives::collateralization_ratio::<BalanceOf<T>>(total_coll, total_debt, price)
}

pub fn view_branch_tcr<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> Option<FixedU128> {
	let state = BranchStates::<T>::get(collateral_id, stable_id)?;
	let price = T::Oracle::provide_price(collateral_id).ok()?.price;
	let now = T::TimeProvider::now();
	compute_tcr::<T>(&state, price, now).ok()
}

/// Lazily walk a vault list from its tail, following `prev` pointers — the same
/// order as [`SortedListInterface::iter_from_tail`], but every storage read is
/// deferred until the iterator advances, so a caller taking only the head pays
/// for only the tail read.
fn list_from_tail<T: Config>(
	list: VaultListId<T::CollateralAssetId, T::StableAssetId>,
) -> impl Iterator<Item = T::AccountId> {
	let mut started = false;
	let mut cursor: Option<T::AccountId> = None;
	core::iter::from_fn(move || {
		if !started {
			started = true;
			cursor = T::VaultLists::tail(&list);
		} else if let Some(current) = &cursor {
			cursor = T::VaultLists::neighbors(&list, current).and_then(|p| p.prev);
		}
		cursor.clone()
	})
}

/// A branch's redemption targets, each tagged with its pricing regime, in
/// priority order: if the `FinalRecovery` FIFO is
/// non-empty, yield only its head; else if `dormant_redemption_target` is set,
/// yield only that; otherwise yield the rate index tail-first (all `Ordinary`).
/// Lazy and allocation-free: `.next()` gives the next target and `take(n)` the
/// queue view, reading only the tiers they reach.
pub(crate) fn redemption_targets<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> impl Iterator<Item = (T::AccountId, RedemptionTargetKind)> {
	let priority = recovery::next_target::<T>(collateral_id, stable_id)
		.map(|owner| (owner, RedemptionTargetKind::FinalRecovery))
		.or_else(|| {
			BranchStates::<T>::get(collateral_id, stable_id)
				.and_then(|bs| bs.dormant_redemption_target)
				.map(|owner| (owner, RedemptionTargetKind::Dormant))
		});
	// The rate index is walked only when no FinalRecovery/Dormant target gates it.
	let rate = priority
		.is_none()
		.then(|| list_from_tail::<T>(VaultListId::Rate(collateral_id.clone(), stable_id.clone())))
		.into_iter()
		.flatten()
		.map(|owner| (owner, RedemptionTargetKind::Ordinary));
	priority.into_iter().chain(rate)
}

/// The next ordinary redemption target after `owner` in the rate index: its
/// head-ward (`prev`) neighbor. Lets the orchestrator skip an underwater
/// ordinary head tail-first without mutating the index. `None` when `owner` is
/// the head (highest-rate) vault or is not a rate-index member — the latter is
/// an orchestrator contract violation, silent in release so a broken cursor
/// reads as an exhausted queue rather than corrupting the walk.
pub(crate) fn ordinary_target_after<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	owner: &T::AccountId,
) -> Option<T::AccountId> {
	let rate_list = VaultListId::Rate(collateral_id.clone(), stable_id.clone());
	debug_assert!(
		T::VaultLists::contains(&rate_list, owner),
		"redemption after-cursor must be a current rate-index member"
	);
	T::VaultLists::neighbors(&rate_list, owner).and_then(|p| p.prev)
}

/// Walk the rate index tail-first, summing active-vault principal while the
/// stored priority is strictly below `rate`, visiting at most `max_steps`
/// vaults. Returns the partial sum when the cap stops the walk early.
pub fn view_debt_in_front<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	rate: FixedU128,
	max_steps: u32,
) -> BalanceOf<T> {
	let mut total = BalanceOf::<T>::zero();
	let rate_list = VaultListId::Rate(collateral_id.clone(), stable_id.clone());
	let mut cursor = T::VaultLists::tail(&rate_list);
	for _ in 0..max_steps {
		let Some(o) = cursor else { break };
		let Some((priority, neighbors)) = T::VaultLists::node(&rate_list, &o) else { break };
		if priority >= rate {
			break;
		}
		if let Some(v) = Vaults::<T>::get((collateral_id, stable_id, &o)) {
			total = total.saturating_add(v.debt.principal);
		}
		cursor = neighbors.prev;
	}
	total
}

pub fn predict_upfront_fee_open<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	initial_debt: BalanceOf<T>,
	annual_rate: FixedU128,
) -> BalanceOf<T> {
	match (
		BranchConfigs::<T>::get((collateral_id, stable_id)),
		BranchStates::<T>::get(collateral_id, stable_id),
	) {
		(Some(config), Some(state)) => {
			open_upfront_fee::<T>(&state, &config, initial_debt, annual_rate)
		},
		_ => BalanceOf::<T>::zero(),
	}
}

pub fn predict_upfront_fee_borrow<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	owner: &T::AccountId,
	debt_increase: BalanceOf<T>,
	maybe_new_rate: Option<FixedU128>,
) -> BalanceOf<T> {
	let Some((config, state, vault)) = predict_inputs::<T>(collateral_id, stable_id, owner) else {
		return BalanceOf::<T>::zero();
	};
	let new_rate = maybe_new_rate.unwrap_or(vault.annual_rate);
	let now = T::TimeProvider::now();
	let cooldown_elapsed = vault.cooldown_elapsed(&config, now);
	let rate_change_fee_base = vault.rate_change_base(maybe_new_rate, cooldown_elapsed);
	simulate_borrow::<T>(&state, &config, &vault, debt_increase, new_rate, rate_change_fee_base).1
}

pub fn predict_upfront_fee_rate_change<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	owner: &T::AccountId,
	new_rate: FixedU128,
) -> BalanceOf<T> {
	let Some((config, state, vault)) = predict_inputs::<T>(collateral_id, stable_id, owner) else {
		return BalanceOf::<T>::zero();
	};
	let now = T::TimeProvider::now();
	let cooldown_elapsed = vault.cooldown_elapsed(&config, now);
	simulate_change_rate::<T>(&state, &config, &vault, new_rate, cooldown_elapsed).1
}

/// Read the `(config, branch state, vault)` triple for a `predict_*` view.
/// Returns `None` if any row is missing — the predict APIs treat that as
/// "no fee" rather than an error.
fn predict_inputs<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	owner: &T::AccountId,
) -> Option<(
	BranchConfig<BalanceOf<T>>,
	BranchState<T::AccountId, BalanceOf<T>>,
	Vault<BalanceOf<T>>,
)> {
	Some((
		BranchConfigs::<T>::get((collateral_id, stable_id))?,
		BranchStates::<T>::get(collateral_id, stable_id)?,
		Vaults::<T>::get((collateral_id, stable_id, owner))?,
	))
}
