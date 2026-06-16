use super::*;

pub fn view_vault_status<T: Config>(
	collateral_id: &T::AssetId,
	owner: &T::AccountId,
) -> Option<VaultStatus> {
	let vault = Vaults::<T>::get(collateral_id, owner)?;
	Some(vault.status::<T>(collateral_id, owner))
}
pub fn view_vault_cr<T: Config>(
	collateral_id: &T::AssetId,
	owner: &T::AccountId,
) -> Option<FixedU128> {
	let vault = Vaults::<T>::get(collateral_id, owner)?;
	let state = BranchStates::<T>::get(collateral_id)?;
	let now = T::TimeProvider::now();
	let collateral = T::CollateralAssets::balance_on_hold(
		collateral_id.clone(),
		&HoldReason::VaultCollateral.into(),
		owner,
	);
	let price = T::Oracle::provide_price(collateral_id).ok()?.price;
	let pending = pending_touch_for::<T>(&vault, &state, now);
	let total_coll = collateral.saturating_add(pending.collateral);
	let total_debt = vault
		.debt
		.total()
		.saturating_add(pending.principal)
		.saturating_add(pending.interest);
	math::collateralization_ratio::<BalanceOf<T>>(total_coll, total_debt, price)
}

pub fn view_branch_tcr<T: Config>(collateral_id: &T::AssetId) -> Option<FixedU128> {
	let state = BranchStates::<T>::get(collateral_id)?;
	let price = T::Oracle::provide_price(collateral_id).ok()?.price;
	let now = T::TimeProvider::now();
	compute_tcr::<T>(&state, price, now).ok()
}

/// Lazily walk a vault list from its tail, following `prev` pointers — the same
/// order as [`SortedListInterface::iter_from_tail`], but every storage read is
/// deferred until the iterator advances, so a caller taking only the head pays
/// for only the tail read.
fn list_from_tail<T: Config>(list: VaultListId<T::AssetId>) -> impl Iterator<Item = T::AccountId> {
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

/// A branch's redemption targets, in priority order with the SPEC §6.1 cutoff:
/// if the `FinalRecovery` FIFO is non-empty, yield only its head; else if
/// `dormant_redemption_target` is set, yield only that; otherwise yield the rate
/// index tail-first. Lazy and allocation-free: `.next()` gives the next target
/// and `take(n)` the queue view, reading only the tiers they reach.
pub(crate) fn redemption_targets<T: Config>(
	collateral_id: &T::AssetId,
) -> impl Iterator<Item = T::AccountId> {
	let priority = recovery::next_target::<T>(collateral_id).or_else(|| {
		BranchStates::<T>::get(collateral_id).and_then(|state| state.dormant_redemption_target)
	});
	// The rate index is walked only when no FinalRecovery/Dormant target gates it.
	let rate = priority
		.is_none()
		.then(|| list_from_tail::<T>(VaultListId::Rate(collateral_id.clone())))
		.into_iter()
		.flatten();
	priority.into_iter().chain(rate)
}

/// Walk the rate index tail-first, summing active-vault principal while the
/// stored priority is strictly below `rate`, visiting at most `max_steps`
/// vaults. Returns the partial sum when the cap stops the walk early.
pub fn view_debt_in_front<T: Config>(
	collateral_id: &T::AssetId,
	rate: FixedU128,
	max_steps: u32,
) -> BalanceOf<T> {
	let mut total = BalanceOf::<T>::zero();
	let rate_list = VaultListId::Rate(collateral_id.clone());
	let mut cursor = T::VaultLists::tail(&rate_list);
	for _ in 0..max_steps {
		let Some(o) = cursor else { break };
		let Some((priority, neighbors)) = T::VaultLists::node(&rate_list, &o) else { break };
		if priority >= rate {
			break;
		}
		if let Some(v) = Vaults::<T>::get(collateral_id, &o) {
			total = total.saturating_add(v.debt.principal);
		}
		cursor = neighbors.prev;
	}
	total
}

pub fn predict_upfront_fee_open<T: Config>(
	collateral_id: &T::AssetId,
	initial_debt: BalanceOf<T>,
	annual_rate: FixedU128,
) -> BalanceOf<T> {
	match (BranchConfigs::<T>::get(collateral_id), BranchStates::<T>::get(collateral_id)) {
		(Some(config), Some(state)) => {
			open_upfront_fee::<T>(&state, &config, initial_debt, annual_rate)
		},
		_ => BalanceOf::<T>::zero(),
	}
}

pub fn predict_upfront_fee_borrow<T: Config>(
	collateral_id: &T::AssetId,
	owner: &T::AccountId,
	debt_increase: BalanceOf<T>,
	maybe_new_rate: Option<FixedU128>,
) -> BalanceOf<T> {
	let Some((config, state, vault)) = predict_inputs::<T>(collateral_id, owner) else {
		return BalanceOf::<T>::zero();
	};
	let new_rate = maybe_new_rate.unwrap_or(vault.annual_rate);
	let now = T::TimeProvider::now();
	let cooldown_elapsed =
		now.saturating_sub(vault.last_rate_update) >= config.rate_adjustment_cooldown;
	let rate_change_fee_base = if maybe_new_rate.is_some() && !cooldown_elapsed {
		vault.debt.principal
	} else {
		BalanceOf::<T>::zero()
	};
	simulate_borrow::<T>(&state, &config, &vault, debt_increase, new_rate, rate_change_fee_base).1
}

pub fn predict_upfront_fee_rate_change<T: Config>(
	collateral_id: &T::AssetId,
	owner: &T::AccountId,
	new_rate: FixedU128,
) -> BalanceOf<T> {
	let Some((config, state, vault)) = predict_inputs::<T>(collateral_id, owner) else {
		return BalanceOf::<T>::zero();
	};
	let now = T::TimeProvider::now();
	let cooldown_elapsed =
		now.saturating_sub(vault.last_rate_update) >= config.rate_adjustment_cooldown;
	simulate_change_rate::<T>(&state, &config, &vault, new_rate, cooldown_elapsed).1
}

/// Read the `(config, branch state, vault)` triple for a `predict_*` view.
/// Returns `None` if any row is missing — the predict APIs treat that as
/// "no fee" rather than an error.
fn predict_inputs<T: Config>(
	collateral_id: &T::AssetId,
	owner: &T::AccountId,
) -> Option<(
	BranchConfig<BalanceOf<T>>,
	BranchState<T::AccountId, BalanceOf<T>>,
	Vault<BalanceOf<T>>,
)> {
	Some((
		BranchConfigs::<T>::get(collateral_id)?,
		BranchStates::<T>::get(collateral_id)?,
		Vaults::<T>::get(collateral_id, owner)?,
	))
}
