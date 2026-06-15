use super::*;
use frame::deps::frame_support::traits::fungibles::Inspect as _;

/// Mode is `Frozen` if persisted, otherwise derived from live TCR.
pub fn current_mode<T: Config>(collateral_id: &T::AssetId) -> Result<BranchMode, DispatchError> {
	let bs = branch_state_of::<T>(collateral_id)?;
	if bs.is_frozen() {
		return Ok(BranchMode::Frozen);
	}
	// A failing oracle is what `refresh_branch` would persist as
	// `Frozen { OracleFailure }`; report `Frozen` to observers even before
	// that poke lands, rather than defaulting to the most permissive mode
	// while prices are unknowable.
	let price = match T::Oracle::provide_price(collateral_id) {
		Ok(feed) => feed.price,
		Err(_) => return Ok(BranchMode::Frozen),
	};
	let cfg = branch_cfg_of::<T>(collateral_id)?;
	let now = T::TimeProvider::now();
	let tcr = compute_tcr::<T>(&bs, price, now)?;
	if tcr < cfg.safety_collateralization_ratio {
		Ok(BranchMode::Safety)
	} else {
		Ok(BranchMode::Normal)
	}
}

/// Validate the rate is within branch bounds.
pub fn validate_rate<T: Config>(
	cfg: &BranchConfig<BalanceOf<T>, MomentOf<T>>,
	rate: FixedU128,
) -> Result<(), DispatchError> {
	if rate < cfg.minimum_borrow_rate || rate > cfg.maximum_borrow_rate {
		return Err(Error::<T>::RateOutOfBounds.into());
	}
	Ok(())
}

#[require_transactional]
pub fn register_branch<T: Config>(
	collateral_id: T::AssetId,
	config: BranchConfig<BalanceOf<T>, MomentOf<T>>,
) -> Result<(), DispatchError> {
	ensure!(!BranchConfigs::<T>::contains_key(&collateral_id), Error::<T>::BranchAlreadyRegistered);
	ensure!(
		T::CollateralAssets::asset_exists(collateral_id.clone()),
		Error::<T>::UnknownCollateral
	);
	ensure!(
		BranchConfigs::<T>::count() < <T::MaxBranches as Get<u32>>::get(),
		Error::<T>::TooManyBranches
	);
	BranchConfigs::<T>::insert(&collateral_id, config);
	let now = T::TimeProvider::now();
	BranchStates::<T>::insert(
		&collateral_id,
		BranchState {
			total_collateral: BalanceOf::<T>::zero(),
			debt: BranchDebt {
				principal: BalanceOf::<T>::zero(),
				minted_interest: BalanceOf::<T>::zero(),
				pending_redist_principal: BalanceOf::<T>::zero(),
				bad_debt: BalanceOf::<T>::zero(),
				weighted_principal_sum: BalanceOf::<T>::zero(),
				// Interest time is 0 at the epoch base (`now`); see `interest_time`.
				last_interest_time: Zero::zero(),
			},
			stakes: BranchStakes {
				total: BalanceOf::<T>::zero(),
				weighted_sum: BalanceOf::<T>::zero(),
			},
			rounding: crate::types::BranchRounding::default(),
			redist: RedistSnapshot::default(),
			interest_clock: InterestClock { epoch_base: now, frozen_elapsed: Zero::zero() },
			next_final_recovery_nonce: 0,
			dormant_redemption_target: None,
			idle_cursor: None,
			frozen: None,
		},
	);
	T::OnBranchRegistered::on_branch_registered(&collateral_id)?;
	Pallet::<T>::deposit_event(Event::BranchRegistered { collateral_id });
	Ok(())
}

/// Apply `update` to the branch config and emit `ParameterUpdated`. Caller is
/// responsible for any defensive-action / authorization gating.
#[require_transactional]
pub fn update_branch_config<T: Config>(
	collateral_id: &T::AssetId,
	update: crate::types::BranchConfigUpdate<BalanceOf<T>, MomentOf<T>>,
) -> Result<(), DispatchError> {
	let parameter = update.parameter_id();
	BranchConfigs::<T>::try_mutate(collateral_id, |maybe| -> Result<_, DispatchError> {
		let cfg = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
		update.apply_to(cfg);
		Ok(())
	})?;
	Pallet::<T>::deposit_event(Event::ParameterUpdated {
		collateral_id: collateral_id.clone(),
		parameter,
	});
	Ok(())
}

#[require_transactional]
pub fn enable_frozen_mode<T: Config>(collateral_id: &T::AssetId) -> Result<(), DispatchError> {
	if branch_state_of::<T>(collateral_id)?.is_frozen() {
		return Ok(());
	}
	enter_frozen::<T>(collateral_id, FrozenReason::Governance)
}

/// Flush interest up to `now`, then persist `Frozen { reason, entered_at: now }`
/// and emit `ModeChanged`. The pre-freeze flush pins `interest_time(now)` so the
/// frozen window itself accrues nothing.
#[require_transactional]
fn enter_frozen<T: Config>(
	collateral_id: &T::AssetId,
	reason: FrozenReason,
) -> Result<(), DispatchError> {
	let now = T::TimeProvider::now();
	update_aggregate_interest::<T>(collateral_id, now)?;
	BranchStates::<T>::try_mutate(collateral_id, |maybe| -> Result<_, DispatchError> {
		let bs = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
		bs.frozen = Some(FrozenState { reason, entered_at: now });
		Pallet::<T>::deposit_event(Event::ModeChanged {
			collateral_id: collateral_id.clone(),
			old_mode: BranchMode::Normal,
			new_mode: BranchMode::Frozen,
		});
		Ok(())
	})
}

pub(crate) fn ensure_not_frozen<T: Config>(
	collateral_id: &T::AssetId,
) -> Result<(), DispatchError> {
	let bs = branch_state_of::<T>(collateral_id)?;
	ensure!(!bs.is_frozen(), Error::<T>::BranchFrozen);
	Ok(())
}

/// Reconcile the branch's `Frozen { OracleFailure }` state with the live
/// oracle (DESIGN.md §8.4 / §10.1). Permissionless. Behaviour:
///
/// - oracle healthy + branch frozen for `OracleFailure` → fold the frozen window into
///   `interest_clock.frozen_elapsed`, then clear `frozen`, suspending accrual across the window.
/// - oracle failing + branch not frozen → persist `Frozen { OracleFailure }`.
/// - branch frozen for `Governance` → no-op (use `clear_governance_frozen_mode`).
/// - all other combinations → no-op `Ok`.
#[require_transactional]
pub fn refresh_branch<T: Config>(collateral_id: &T::AssetId) -> Result<(), DispatchError> {
	let bs = branch_state_of::<T>(collateral_id)?;
	let oracle_ok = T::Oracle::provide_price(collateral_id).is_ok();
	match (bs.frozen, oracle_ok) {
		(Some(state), true) if matches!(state.reason, FrozenReason::OracleFailure) => {
			clear_frozen::<T>(collateral_id, BranchMode::Frozen, BranchMode::Normal)
		},
		(None, false) => freeze_oracle::<T>(collateral_id),
		_ => Ok(()),
	}
}

fn freeze_oracle<T: Config>(collateral_id: &T::AssetId) -> Result<(), DispatchError> {
	enter_frozen::<T>(collateral_id, FrozenReason::OracleFailure)
}

fn clear_frozen<T: Config>(
	collateral_id: &T::AssetId,
	old_mode: BranchMode,
	new_mode: BranchMode,
) -> Result<(), DispatchError> {
	let now = T::TimeProvider::now();
	BranchStates::<T>::try_mutate(collateral_id, |maybe| -> Result<_, DispatchError> {
		let bs = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
		// Fold the frozen window into `frozen_elapsed` so that
		// `interest_time(now)` stays continuous: the next aggregate-interest
		// update charges only for time after the unfreeze.
		let entered_at = bs.frozen.as_ref().map(|state| state.entered_at);
		if let Some(entered_at) = entered_at {
			let frozen_window = now.saturating_sub(entered_at);
			bs.interest_clock.frozen_elapsed =
				bs.interest_clock.frozen_elapsed.saturating_add(frozen_window);
		}
		bs.frozen = None;
		Pallet::<T>::deposit_event(Event::ModeChanged {
			collateral_id: collateral_id.clone(),
			old_mode,
			new_mode,
		});
		Ok(())
	})
}

/// Clear a governance-induced Frozen state. No-op when not frozen, or when
/// frozen for a non-governance reason.
#[require_transactional]
pub fn clear_governance_frozen_mode<T: Config>(
	collateral_id: &T::AssetId,
) -> Result<(), DispatchError> {
	let bs = branch_state_of::<T>(collateral_id)?;
	match bs.frozen {
		Some(state) if matches!(state.reason, FrozenReason::Governance) => {
			clear_frozen::<T>(collateral_id, BranchMode::Frozen, BranchMode::Normal)
		},
		_ => Ok(()),
	}
}

/// Apply Normal/Safety mode-aware TCR rules.
///
/// `is_settlement` is true for `FinalRecovery` redemptions/recovery offsets,
/// which are explicit settlement exceptions to the Safety-mode non-worsening
/// rule.
pub fn enforce_mode_rules<T: Config>(
	cfg: &BranchConfig<BalanceOf<T>, MomentOf<T>>,
	bs_pre: &BranchState<T::AccountId, BalanceOf<T>, MomentOf<T>>,
	pre_tcr: FixedU128,
	post_tcr: FixedU128,
	is_settlement: bool,
) -> Result<(), DispatchError> {
	if bs_pre.is_frozen() {
		return Err(Error::<T>::BranchFrozen.into());
	}
	if pre_tcr < cfg.safety_collateralization_ratio {
		// Safety mode.
		if !is_settlement && post_tcr < pre_tcr {
			return Err(Error::<T>::SafetyModeTcrWorsening.into());
		}
	} else {
		// Normal mode.
		if !is_settlement && post_tcr < cfg.safety_collateralization_ratio {
			return Err(Error::<T>::WouldEnterSafetyMode.into());
		}
	}
	Ok(())
}
