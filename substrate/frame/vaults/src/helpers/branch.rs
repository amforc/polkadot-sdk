use super::*;
use frame::traits::fungibles::Inspect as _;

/// Mode is `Frozen` if persisted, otherwise derived from live TCR.
pub fn current_mode<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> Result<BranchMode, DispatchError> {
	let state = branch_state_of::<T>(collateral_id, stable_id)?;
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
	let config = branch_config_of::<T>(collateral_id, stable_id)?;
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
	collateral_id: T::CollateralAssetId,
	stable_id: T::StableAssetId,
	config: BranchConfig<BalanceOf<T>>,
) -> Result<(), DispatchError> {
	ensure!(
		!BranchConfigs::<T>::contains_key((&collateral_id, &stable_id)),
		Error::<T>::BranchAlreadyRegistered
	);
	ensure!(
		T::CollateralAssets::asset_exists(collateral_id.clone()),
		Error::<T>::UnknownCollateral
	);
	ensure!(T::StableAssets::asset_exists(stable_id.clone()), Error::<T>::UnknownStable);
	// The pallet mints a market's stablecoin permissionlessly, so that asset must
	// never be trusted as collateral — in this market or any sibling — else its
	// owner could mint unbacked collateral.
	ensure!(!T::is_same_asset(&collateral_id, &stable_id), Error::<T>::StableCollateralCollision);
	for (existing_collateral, existing_stable) in BranchConfigs::<T>::iter_keys() {
		ensure!(
			!T::is_same_asset(&existing_collateral, &stable_id),
			Error::<T>::StableCollateralCollision
		);
		ensure!(
			!T::is_same_asset(&collateral_id, &existing_stable),
			Error::<T>::StableCollateralCollision
		);
	}
	ensure!(
		BranchConfigs::<T>::count() < <T::MaxBranches as Get<u32>>::get(),
		Error::<T>::TooManyBranches
	);
	let redistribution_account = Pallet::<T>::redistribution_account(&collateral_id, &stable_id);
	if frame_system::Pallet::<T>::providers(&redistribution_account) == 0 {
		frame_system::Pallet::<T>::inc_providers(&redistribution_account);
	}
	let now = T::TimeProvider::now();
	let initial_ceiling = if config.ceiling_gap.is_zero() {
		config.debt_ceiling
	} else {
		config.ceiling_gap.min(config.debt_ceiling)
	};
	BranchConfigs::<T>::insert((&collateral_id, &stable_id), config);
	BranchStates::<T>::insert(
		&collateral_id,
		&stable_id,
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
			effective_ceiling: initial_ceiling,
			ceiling_last_inc: now,
		},
	);
	T::OnBranchLifecycle::on_registered(&collateral_id, &stable_id)?;
	Pallet::<T>::deposit_event(Event::BranchRegistered { collateral_id, stable_id });
	Ok(())
}

/// Authorize, validate, and apply a single-field config update, emitting
/// `ParameterUpdated`. The required admin tier, the `Emergency`-only "must be
/// defensive" rule, and the governance-envelope check are all derived from the
/// `update` itself ([`BranchConfigUpdate::required_level`] /
/// [`BranchConfigUpdate::is_defensive`] and [`BranchConfigGuard::permits`]), so
/// each `set_*` dispatchable is a thin wrapper over this one path. The whole
/// post-update config is re-validated through the same `permits` gate
/// `create_branch` applies, keeping envelope enforcement in a single place.
pub fn set_param<T: Config>(
	origin: OriginFor<T>,
	collateral_id: T::CollateralAssetId,
	stable_id: T::StableAssetId,
	update: crate::types::BranchConfigUpdate<BalanceOf<T>>,
) -> Result<(), DispatchError> {
	let level =
		ensure_branch_admin::<T>(origin, &collateral_id, &stable_id, update.required_level())?;
	let guard = T::BranchConfigGuard::get();
	BranchConfigs::<T>::try_mutate(
		(&collateral_id, &stable_id),
		|maybe| -> Result<_, DispatchError> {
			let config = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
			if matches!(level, AdminLevel::Emergency) {
				ensure!(update.is_defensive(config), Error::<T>::DefensiveActionNotDefensive);
			}
			update.clone().apply_to(config);
			ensure!(guard.permits(config), Error::<T>::ConfigOutsideEnvelope);
			Ok(())
		},
	)?;
	Pallet::<T>::deposit_event(Event::ParameterUpdated { collateral_id, stable_id, update });
	Ok(())
}

pub fn enable_frozen_mode<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> Result<(), DispatchError> {
	if branch_state_of::<T>(collateral_id, stable_id)?.is_frozen() {
		return Ok(());
	}
	enter_frozen::<T>(collateral_id, stable_id, FrozenReason::Governance)
}

fn enter_frozen<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	reason: FrozenReason,
) -> Result<(), DispatchError> {
	let now = T::TimeProvider::now();
	let old_mode = current_mode::<T>(collateral_id, stable_id).unwrap_or(BranchMode::Normal);
	BranchStates::<T>::try_mutate(collateral_id, stable_id, |maybe| -> Result<_, DispatchError> {
		let state = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
		// Flush before freezing so the frozen window accrues nothing.
		let minted = accounting::accrue_aggregate_interest::<T>(state, now);
		if !minted.is_zero() {
			accounting::mint_and_route_yield::<T>(
				collateral_id,
				stable_id,
				minted,
				accounting::YieldSource::BranchInterest,
			);
		}
		state.frozen = Some(FrozenState { reason, entered_at: now });
		Ok(())
	})?;
	Pallet::<T>::deposit_event(Event::ModeChanged {
		collateral_id: collateral_id.clone(),
		stable_id: stable_id.clone(),
		old_mode,
		new_mode: BranchMode::Frozen,
	});
	Ok(())
}

/// Reconcile oracle-driven Frozen state with the live oracle.
pub fn refresh_branch<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> Result<(), DispatchError> {
	let state = branch_state_of::<T>(collateral_id, stable_id)?;
	let oracle_ok = T::Oracle::provide_price(collateral_id).is_ok();
	match (state.frozen, oracle_ok) {
		(Some(state), true) if matches!(state.reason, FrozenReason::OracleFailure) => {
			clear_frozen::<T>(collateral_id, stable_id)
		},
		(None, false) => freeze_oracle::<T>(collateral_id, stable_id),
		_ => Ok(()),
	}
}

fn freeze_oracle<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> Result<(), DispatchError> {
	enter_frozen::<T>(collateral_id, stable_id, FrozenReason::OracleFailure)
}

fn clear_frozen<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> Result<(), DispatchError> {
	let now = T::TimeProvider::now();
	BranchStates::<T>::try_mutate(collateral_id, stable_id, |maybe| -> Result<_, DispatchError> {
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
	// `Frozen` is the only persisted mode, so the branch always leaves it for the
	// live TCR-derived mode.
	let new_mode = current_mode::<T>(collateral_id, stable_id).unwrap_or(BranchMode::Normal);
	Pallet::<T>::deposit_event(Event::ModeChanged {
		collateral_id: collateral_id.clone(),
		stable_id: stable_id.clone(),
		old_mode: BranchMode::Frozen,
		new_mode,
	});
	Ok(())
}

/// Clear a governance-induced Frozen state. No-op when not frozen, or when
/// frozen for a non-governance reason.
pub fn clear_governance_frozen_mode<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
) -> Result<(), DispatchError> {
	let state = branch_state_of::<T>(collateral_id, stable_id)?;
	match state.frozen {
		Some(state) if matches!(state.reason, FrozenReason::Governance) => {
			clear_frozen::<T>(collateral_id, stable_id)
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

/// Advance the autoline `effective_ceiling` toward `min(branch_debt + gap,
/// debt_ceiling)`. Increases are gated by `ceiling_ttl`; decreases apply
/// immediately. A frozen market pins or lowers the ceiling but never raises it.
/// Returns whether `state` was changed; a no-op (autoline disabled via
/// `ceiling_gap == 0`, or already at target) returns `false`.
pub fn ratchet_ceiling<T: Config>(
	state: &mut BranchState<T::AccountId, BalanceOf<T>>,
	config: &BranchConfig<BalanceOf<T>>,
	now: Millis,
) -> bool {
	if config.ceiling_gap.is_zero() {
		return false;
	}
	let target = state.debt.principal.saturating_add(config.ceiling_gap).min(config.debt_ceiling);
	if target < state.effective_ceiling {
		state.effective_ceiling = target;
		true
	} else if target > state.effective_ceiling &&
		!state.is_frozen() &&
		now >= state.ceiling_last_inc.saturating_add(config.ceiling_ttl)
	{
		state.effective_ceiling = target;
		state.ceiling_last_inc = now;
		true
	} else {
		false
	}
}

/// Permissionless: ratchet a market's autoline ceiling. A no-op poke (autoline
/// disabled, or the ceiling already at target) writes no storage.
pub fn poke_ceiling<T: Config>(
	collateral_id: T::CollateralAssetId,
	stable_id: T::StableAssetId,
) -> Result<(), DispatchError> {
	let config = branch_config_of::<T>(&collateral_id, &stable_id)?;
	if config.ceiling_gap.is_zero() {
		return Ok(());
	}
	let now = T::TimeProvider::now();
	let mut state = branch_state_of::<T>(&collateral_id, &stable_id)?;
	if ratchet_ceiling::<T>(&mut state, &config, now) {
		BranchStates::<T>::insert(&collateral_id, &stable_id, state);
	}
	Ok(())
}

/// Authorize a per-market admin call, returning the caller's [`AdminLevel`].
/// `full_admin` satisfies any `required`; `emergency_admin` satisfies only
/// `Emergency`.
pub fn ensure_branch_admin<T: Config>(
	origin: OriginFor<T>,
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	required: AdminLevel,
) -> Result<AdminLevel, DispatchError> {
	let who = ensure_signed(origin)?;
	let info =
		BranchAdmin::<T>::get((collateral_id, stable_id)).ok_or(Error::<T>::UnknownCollateral)?;
	if who == info.full_admin {
		return Ok(AdminLevel::Full);
	}
	if matches!(required, AdminLevel::Emergency) && who == info.emergency_admin {
		return Ok(AdminLevel::Emergency);
	}
	Err(Error::<T>::NotBranchAdmin.into())
}

/// Permissionless market creation. Validates the config against the governance
/// envelope and the oracle, takes the creation deposit (unless Root), seeds the
/// market, and stores its admins.
pub fn create_branch<T: Config>(
	collateral_id: T::CollateralAssetId,
	stable_id: T::StableAssetId,
	admins: BranchAdmins<T::AccountId>,
	config: BranchConfig<BalanceOf<T>>,
	depositor: Option<T::AccountId>,
) -> Result<(), DispatchError> {
	ensure!(T::BranchConfigGuard::get().permits(&config), Error::<T>::ConfigOutsideEnvelope);
	// A market the oracle cannot price cannot open.
	T::Oracle::provide_price(&collateral_id).map_err(|_| Error::<T>::OraclePriceNotAvailable)?;
	let deposit = match depositor {
		Some(who) => {
			let footprint =
				Footprint::from_mel::<BranchAdminInfo<T::AccountId, T::Consideration>>();
			let ticket = T::Consideration::new(&who, footprint)?;
			Some((who, ticket))
		},
		None => None,
	};
	register_branch::<T>(collateral_id.clone(), stable_id.clone(), config)?;
	BranchAdmin::<T>::insert(
		(&collateral_id, &stable_id),
		BranchAdminInfo {
			full_admin: admins.full_admin,
			emergency_admin: admins.emergency_admin,
			deposit,
		},
	);
	Ok(())
}

/// Remove an empty market: refund the deposit, release the redistribution-account
/// provider, tear down storage, and fire the deregistration hook.
pub fn remove_branch<T: Config>(
	collateral_id: T::CollateralAssetId,
	stable_id: T::StableAssetId,
) -> Result<(), DispatchError> {
	let state = branch_state_of::<T>(&collateral_id, &stable_id)?;
	ensure!(state.is_removable(), Error::<T>::MarketNotEmpty);
	ensure!(
		Vaults::<T>::iter_prefix((&collateral_id, &stable_id)).next().is_none(),
		Error::<T>::MarketNotEmpty
	);
	let info =
		BranchAdmin::<T>::get((&collateral_id, &stable_id)).ok_or(Error::<T>::UnknownCollateral)?;
	if let Some((who, ticket)) = info.deposit {
		ticket.drop(&who)?;
	}
	let redistribution_account = Pallet::<T>::redistribution_account(&collateral_id, &stable_id);
	if frame_system::Pallet::<T>::providers(&redistribution_account) > 0 {
		let _ = frame_system::Pallet::<T>::dec_providers(&redistribution_account);
	}
	BranchConfigs::<T>::remove((&collateral_id, &stable_id));
	BranchStates::<T>::remove(&collateral_id, &stable_id);
	BranchAdmin::<T>::remove((&collateral_id, &stable_id));
	T::OnBranchLifecycle::on_deregistered(&collateral_id, &stable_id)?;
	Pallet::<T>::deposit_event(Event::BranchRemoved { collateral_id, stable_id });
	Ok(())
}
