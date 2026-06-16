use super::*;
use frame::traits::fungibles::Inspect as _;

/// Mode is `Frozen` if persisted, otherwise derived from live TCR.
pub fn current_mode<T: Config>(collateral_id: &T::AssetId) -> Result<BranchMode, DispatchError> {
	let state = branch_state_of::<T>(collateral_id)?;
	if state.is_frozen() {
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
	let config = branch_config_of::<T>(collateral_id)?;
	let now = T::TimeProvider::now();
	let tcr = compute_tcr::<T>(&state, price, now)?;
	if tcr < config.safety_collateralization_ratio {
		Ok(BranchMode::Safety)
	} else {
		Ok(BranchMode::Normal)
	}
}

/// Validate the rate is within branch bounds.
pub fn validate_rate<T: Config>(
	config: &BranchConfig<BalanceOf<T>>,
	rate: FixedU128,
) -> Result<(), DispatchError> {
	if rate < config.minimum_borrow_rate || rate > config.maximum_borrow_rate {
		return Err(Error::<T>::RateOutOfBounds.into());
	}
	Ok(())
}

pub fn register_branch<T: Config>(
	collateral_id: T::AssetId,
	config: BranchConfig<BalanceOf<T>>,
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
				pending_redistribution_principal: BalanceOf::<T>::zero(),
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
			redistribution: RedistributionSnapshot::default(),
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

/// Apply a branch config update and emit `ParameterUpdated`.
pub fn update_branch_config<T: Config>(
	collateral_id: &T::AssetId,
	update: crate::types::BranchConfigUpdate<BalanceOf<T>>,
) -> Result<(), DispatchError> {
	BranchConfigs::<T>::try_mutate(collateral_id, |maybe| -> Result<_, DispatchError> {
		let config = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
		update.clone().apply_to(config);
		Ok(())
	})?;
	Pallet::<T>::deposit_event(Event::ParameterUpdated {
		collateral_id: collateral_id.clone(),
		update,
	});
	Ok(())
}

pub fn enable_frozen_mode<T: Config>(collateral_id: &T::AssetId) -> Result<(), DispatchError> {
	if branch_state_of::<T>(collateral_id)?.is_frozen() {
		return Ok(());
	}
	enter_frozen::<T>(collateral_id, FrozenReason::Governance)
}

fn enter_frozen<T: Config>(
	collateral_id: &T::AssetId,
	reason: FrozenReason,
) -> Result<(), DispatchError> {
	let now = T::TimeProvider::now();
	BranchStates::<T>::try_mutate(collateral_id, |maybe| -> Result<_, DispatchError> {
		let state = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
		// Flush before freezing so the frozen window accrues nothing.
		let minted = accounting::accrue_aggregate_interest::<T>(state, now);
		if !minted.is_zero() {
			accounting::mint_and_route_yield::<T>(
				collateral_id,
				minted,
				accounting::YieldSource::BranchInterest,
			);
		}
		state.frozen = Some(FrozenState { reason, entered_at: now });
		Ok(())
	})?;
	Pallet::<T>::deposit_event(Event::ModeChanged {
		collateral_id: collateral_id.clone(),
		old_mode: BranchMode::Normal,
		new_mode: BranchMode::Frozen,
	});
	Ok(())
}

/// Reconcile oracle-driven Frozen state with the live oracle.
pub fn refresh_branch<T: Config>(collateral_id: &T::AssetId) -> Result<(), DispatchError> {
	let state = branch_state_of::<T>(collateral_id)?;
	let oracle_ok = T::Oracle::provide_price(collateral_id).is_ok();
	match (state.frozen, oracle_ok) {
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
		let state = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
		// Keep `interest_time(now)` continuous across the frozen window.
		let entered_at = state.frozen.as_ref().map(|state| state.entered_at);
		if let Some(entered_at) = entered_at {
			let frozen_window = now.saturating_sub(entered_at);
			state.interest_clock.frozen_elapsed =
				state.interest_clock.frozen_elapsed.saturating_add(frozen_window);
		}
		state.frozen = None;
		Ok(())
	})?;
	Pallet::<T>::deposit_event(Event::ModeChanged {
		collateral_id: collateral_id.clone(),
		old_mode,
		new_mode,
	});
	Ok(())
}

/// Clear a governance-induced Frozen state. No-op when not frozen, or when
/// frozen for a non-governance reason.
pub fn clear_governance_frozen_mode<T: Config>(
	collateral_id: &T::AssetId,
) -> Result<(), DispatchError> {
	let state = branch_state_of::<T>(collateral_id)?;
	match state.frozen {
		Some(state) if matches!(state.reason, FrozenReason::Governance) => {
			clear_frozen::<T>(collateral_id, BranchMode::Frozen, BranchMode::Normal)
		},
		_ => Ok(()),
	}
}

/// Apply Normal/Safety mode TCR rules.
pub fn enforce_mode_rules<T: Config>(
	config: &BranchConfig<BalanceOf<T>>,
	branch_state_pre: &BranchState<T::AccountId, BalanceOf<T>>,
	pre_tcr: FixedU128,
	post_tcr: FixedU128,
	is_settlement: bool,
) -> Result<(), DispatchError> {
	if branch_state_pre.is_frozen() {
		return Err(Error::<T>::BranchFrozen.into());
	}
	if pre_tcr < config.safety_collateralization_ratio {
		if !is_settlement && post_tcr < pre_tcr {
			return Err(Error::<T>::SafetyModeTcrWorsening.into());
		}
	} else {
		if !is_settlement && post_tcr < config.safety_collateralization_ratio {
			return Err(Error::<T>::WouldEnterSafetyMode.into());
		}
	}
	Ok(())
}
