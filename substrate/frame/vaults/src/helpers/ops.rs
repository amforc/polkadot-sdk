use super::*;

pub fn open_vault<T: Config>(
	owner: T::AccountId,
	collateral_id: T::AssetId,
	initial_collateral: BalanceOf<T>,
	initial_debt: BalanceOf<T>,
	annual_rate: FixedU128,
	hint: Position<T::AccountId>,
) -> Result<(), DispatchError> {
	let mut context = OpContext::<T>::load(collateral_id)?;
	context.ensure_not_frozen()?;
	ensure!(
		!Vaults::<T>::contains_key(&context.collateral_id, &owner),
		Error::<T>::VaultAlreadyExists
	);
	let config = branch_config_of::<T>(&context.collateral_id)?;
	ensure!(initial_debt >= config.minimum_debt, Error::<T>::DebtBelowMinimum);
	ensure!(initial_collateral >= config.minimum_collateral, Error::<T>::InsufficientCollateral);
	validate_rate::<T>(&config, annual_rate)?;
	let price = T::Oracle::provide_price(&context.collateral_id)?.price;

	ensure!(
		context.state.debt.principal.saturating_add(initial_debt) <= config.debt_ceiling,
		Error::<T>::DebtCeilingExceeded
	);

	let upfront_fee = open_upfront_fee::<T>(&context.state, &config, initial_debt, annual_rate);

	let vault = Vault {
		debt: VaultDebt { principal: initial_debt, interest: upfront_fee },
		annual_rate,
		last_interest_time: context.state.interest_time(context.now),
		last_rate_update: context.now,
		redistribution_stake: initial_collateral,
		redistribution_snapshot: context.state.redistribution,
	};

	let total_debt = initial_debt.saturating_add(upfront_fee);
	ensure_above_icr::<T>(initial_collateral, total_debt, price, &config)?;

	let pre_tcr = compute_tcr::<T>(&context.state, price, context.now)?;
	let mut branch_state_after = context.state.clone();
	branch_state_after.attach_vault(&vault);
	branch_state_after.add_collateral(initial_collateral);
	let post_tcr = compute_tcr::<T>(&branch_state_after, price, context.now)?;
	enforce_mode_rules::<T>(&config, &context.state, pre_tcr, post_tcr, false)?;
	context.state = branch_state_after;

	T::CollateralAssets::hold(
		context.collateral_id.clone(),
		&HoldReason::VaultCollateral.into(),
		&owner,
		initial_collateral,
	)?;
	T::StableAsset::mint_into(&owner, initial_debt)?;
	context.charge_upfront_fee(&owner, upfront_fee);

	T::VaultLists::insert(
		VaultListId::Rate(context.collateral_id.clone()),
		owner.clone(),
		annual_rate,
		hint,
	)
	.map_err(map_error::<T>)?;

	Pallet::<T>::deposit_event(Event::Borrowed {
		collateral_id: context.collateral_id.clone(),
		owner: owner.clone(),
		recipient: owner.clone(),
		amount: initial_debt,
	});
	Pallet::<T>::deposit_event(Event::CollateralDeposited {
		collateral_id: context.collateral_id.clone(),
		owner: owner.clone(),
		from: owner.clone(),
		amount: initial_collateral,
	});
	Pallet::<T>::deposit_event(Event::VaultOpened {
		collateral_id: context.collateral_id.clone(),
		owner: owner.clone(),
	});
	context.commit_with_vault(&owner, &vault);
	Ok(())
}

/// Permissionless deposit. Dormant vaults must be revived by borrowing.
pub fn deposit_collateral_for<T: Config>(
	from: T::AccountId,
	owner: T::AccountId,
	collateral_id: T::AssetId,
	amount: BalanceOf<T>,
) -> Result<(), DispatchError> {
	let mut context = OpContext::<T>::load(collateral_id)?;
	context.ensure_not_frozen()?;
	let TouchedVault { mut vault, status } = context.touch(&owner)?;
	ensure!(!status.is_dormant(), Error::<T>::DebtBelowMinimum);

	T::CollateralAssets::transfer_and_hold(
		context.collateral_id.clone(),
		&HoldReason::VaultCollateral.into(),
		&from,
		&owner,
		amount,
		Precision::Exact,
		Preservation::Expendable,
		Fortitude::Polite,
	)?;

	context.state.add_collateral(amount);
	if status.is_active() {
		let old_stake = vault.redistribution_stake;
		let new_stake = old_stake.saturating_add(amount);
		context.state.refresh_vault_stake(vault.annual_rate, old_stake, new_stake);
		vault.redistribution_stake = new_stake;
	}

	Pallet::<T>::deposit_event(Event::CollateralDeposited {
		collateral_id: context.collateral_id.clone(),
		owner: owner.clone(),
		from,
		amount,
	});
	context.commit_with_vault(&owner, &vault);
	Ok(())
}

pub fn withdraw_collateral<T: Config>(
	owner: T::AccountId,
	collateral_id: T::AssetId,
	amount: BalanceOf<T>,
	recipient: T::AccountId,
) -> Result<(), DispatchError> {
	let mut context = OpContext::<T>::load(collateral_id)?;
	context.ensure_not_frozen()?;
	let price = T::Oracle::provide_price(&context.collateral_id)?.price;
	let TouchedVault { mut vault, status } = context.touch(&owner)?;
	ensure!(!status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);

	let config = branch_config_of::<T>(&context.collateral_id)?;
	let collateral = vault.redistribution_stake;
	ensure!(collateral >= amount, Error::<T>::InsufficientCollateral);

	let total_debt = vault.debt.total();
	let new_collateral = collateral.saturating_sub(amount);
	if !total_debt.is_zero() {
		ensure_above_icr::<T>(new_collateral, total_debt, price, &config)?;
	}

	let pre_tcr = compute_tcr::<T>(&context.state, price, context.now)?;
	let mut branch_state_after = context.state.clone();
	branch_state_after.remove_collateral(amount);
	branch_state_after.refresh_vault_stake(
		vault.annual_rate,
		vault.redistribution_stake,
		new_collateral,
	);
	let post_tcr = compute_tcr::<T>(&branch_state_after, price, context.now)?;
	enforce_mode_rules::<T>(&config, &context.state, pre_tcr, post_tcr, false)?;
	context.state = branch_state_after;

	T::CollateralAssets::transfer_on_hold(
		context.collateral_id.clone(),
		&HoldReason::VaultCollateral.into(),
		&owner,
		&recipient,
		amount,
		Precision::Exact,
		Restriction::Free,
		Fortitude::Polite,
	)?;

	vault.redistribution_stake = new_collateral;
	Pallet::<T>::deposit_event(Event::CollateralWithdrawn {
		collateral_id: context.collateral_id.clone(),
		owner: owner.clone(),
		recipient,
		amount,
	});
	context.commit_with_vault(&owner, &vault);
	Ok(())
}

pub fn borrow<T: Config>(
	owner: T::AccountId,
	collateral_id: T::AssetId,
	amount: BalanceOf<T>,
	maybe_new_rate: Option<FixedU128>,
	recipient: T::AccountId,
	hint: Position<T::AccountId>,
) -> Result<(), DispatchError> {
	let mut context = OpContext::<T>::load(collateral_id)?;
	context.ensure_not_frozen()?;
	let price = T::Oracle::provide_price(&context.collateral_id)?.price;
	let TouchedVault { mut vault, status: pre_status } = context.touch(&owner)?;
	ensure!(!pre_status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);

	let config = branch_config_of::<T>(&context.collateral_id)?;
	let old_rate = vault.annual_rate;
	let new_rate = maybe_new_rate.unwrap_or(old_rate);
	validate_rate::<T>(&config, new_rate)?;

	let new_ib_debt = vault.debt.principal.saturating_add(amount);
	ensure!(
		context.state.debt.principal.saturating_add(amount) <= config.debt_ceiling,
		Error::<T>::DebtCeilingExceeded
	);

	let cooldown_elapsed =
		context.now.saturating_sub(vault.last_rate_update) >= config.rate_adjustment_cooldown;
	let rate_change_fee_base = if maybe_new_rate.is_some() && !cooldown_elapsed {
		vault.debt.principal
	} else {
		BalanceOf::<T>::zero()
	};
	let (mut branch_state_after, upfront_fee) = simulate_borrow::<T>(
		&context.state,
		&config,
		&vault,
		amount,
		new_rate,
		rate_change_fee_base,
	);
	branch_state_after.debt.minted_interest =
		branch_state_after.debt.minted_interest.saturating_add(upfront_fee);

	let dormant_to_active = pre_status.is_dormant() && new_ib_debt >= config.minimum_debt;
	vault.debt.principal = new_ib_debt;
	vault.debt.interest = vault.debt.interest.saturating_add(upfront_fee);
	if maybe_new_rate.is_some() {
		vault.annual_rate = new_rate;
		vault.last_rate_update = context.now;
	}
	ensure!(vault.debt.principal >= config.minimum_debt, Error::<T>::DebtBelowMinimum);

	let collateral = vault.redistribution_stake;
	let total_debt = vault.debt.total();
	ensure_above_icr::<T>(collateral, total_debt, price, &config)?;

	let pre_tcr = compute_tcr::<T>(&context.state, price, context.now)?;
	let post_tcr = compute_tcr::<T>(&branch_state_after, price, context.now)?;
	enforce_mode_rules::<T>(&config, &context.state, pre_tcr, post_tcr, false)?;
	context.state = branch_state_after;

	if dormant_to_active {
		context.state.release_dormant_target(&owner);
	}

	T::StableAsset::mint_into(&recipient, amount)?;
	context.charge_upfront_fee(&owner, upfront_fee);

	if dormant_to_active {
		T::VaultLists::insert(
			VaultListId::Rate(context.collateral_id.clone()),
			owner.clone(),
			new_rate,
			hint,
		)
		.map_err(map_error::<T>)?;
		Pallet::<T>::deposit_event(Event::VaultStatusChanged {
			collateral_id: context.collateral_id.clone(),
			owner: owner.clone(),
			old_status: VaultStatus::Dormant,
			new_status: VaultStatus::Active,
		});
	} else if old_rate != new_rate {
		T::VaultLists::re_insert(
			VaultListId::Rate(context.collateral_id.clone()),
			owner.clone(),
			new_rate,
			hint,
		)
		.map_err(map_error::<T>)?;
	}

	if old_rate != new_rate {
		Pallet::<T>::deposit_event(Event::BorrowRateChanged {
			collateral_id: context.collateral_id.clone(),
			owner: owner.clone(),
			old_rate,
			new_rate,
		});
	}
	Pallet::<T>::deposit_event(Event::Borrowed {
		collateral_id: context.collateral_id.clone(),
		owner: owner.clone(),
		recipient,
		amount,
	});
	context.commit_with_vault(&owner, &vault);
	Ok(())
}

pub fn repay_for<T: Config>(
	from: T::AccountId,
	owner: T::AccountId,
	collateral_id: T::AssetId,
	amount: BalanceOf<T>,
) -> Result<(), DispatchError> {
	let mut context = OpContext::<T>::load(collateral_id)?;
	context.ensure_not_frozen()?;
	let TouchedVault { mut vault, status: pre_status } = context.touch(&owner)?;
	ensure!(!pre_status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);

	let config = branch_config_of::<T>(&context.collateral_id)?;

	// Cap overpayment at the touched debt.
	let repay = amount.min(vault.debt.total());
	T::StableAsset::burn_from(
		&from,
		repay,
		Preservation::Expendable,
		Precision::Exact,
		Fortitude::Polite,
	)?;

	let payment = vault.debt.cancel(repay);
	debug_assert_eq!(payment.total(), repay);

	let new_total = vault.debt.total();
	if !new_total.is_zero() && new_total < config.minimum_debt {
		return Err(Error::<T>::DebtWouldBecomeDust.into());
	}

	if new_total.is_zero() {
		let price = T::Oracle::provide_price(&context.collateral_id)?.price;
		Pallet::<T>::deposit_event(Event::Repaid {
			collateral_id: context.collateral_id.clone(),
			owner: owner.clone(),
			from,
			amount: repay,
		});
		close_inner::<T>(
			context,
			&owner,
			&owner,
			&vault,
			pre_status,
			&config,
			price,
			Some((payment, vault.annual_rate)),
		)?;
		return Ok(());
	}

	context
		.state
		.apply_debt_payment(payment, vault.annual_rate, vault.debt.principal);
	Pallet::<T>::deposit_event(Event::Repaid {
		collateral_id: context.collateral_id.clone(),
		owner: owner.clone(),
		from,
		amount: repay,
	});
	context.commit_with_vault(&owner, &vault);
	Ok(())
}

pub fn change_rate<T: Config>(
	owner: T::AccountId,
	collateral_id: T::AssetId,
	new_rate: FixedU128,
	hint: Position<T::AccountId>,
) -> Result<(), DispatchError> {
	let mut context = OpContext::<T>::load(collateral_id)?;
	context.ensure_not_frozen()?;
	let TouchedVault { mut vault, status } = context.touch(&owner)?;
	ensure!(status.is_active(), Error::<T>::InvalidVaultStatus);
	let old_rate = vault.annual_rate;
	if old_rate == new_rate {
		context.commit_with_vault(&owner, &vault);
		return Ok(());
	}

	let config = branch_config_of::<T>(&context.collateral_id)?;
	validate_rate::<T>(&config, new_rate)?;

	let cooldown_elapsed =
		context.now.saturating_sub(vault.last_rate_update) >= config.rate_adjustment_cooldown;
	let (mut branch_state_after, upfront_fee) =
		simulate_change_rate::<T>(&context.state, &config, &vault, new_rate, cooldown_elapsed);
	branch_state_after.debt.minted_interest =
		branch_state_after.debt.minted_interest.saturating_add(upfront_fee);

	let price = T::Oracle::provide_price(&context.collateral_id)?.price;
	let pre_tcr = compute_tcr::<T>(&context.state, price, context.now)?;
	let post_tcr = compute_tcr::<T>(&branch_state_after, price, context.now)?;
	enforce_mode_rules::<T>(&config, &context.state, pre_tcr, post_tcr, false)?;
	context.state = branch_state_after;

	context.charge_upfront_fee(&owner, upfront_fee);

	vault.annual_rate = new_rate;
	vault.last_rate_update = context.now;
	vault.debt.interest = vault.debt.interest.saturating_add(upfront_fee);

	T::VaultLists::re_insert(
		VaultListId::Rate(context.collateral_id.clone()),
		owner.clone(),
		new_rate,
		hint,
	)
	.map_err(map_error::<T>)?;
	Pallet::<T>::deposit_event(Event::BorrowRateChanged {
		collateral_id: context.collateral_id.clone(),
		owner: owner.clone(),
		old_rate,
		new_rate,
	});
	context.commit_with_vault(&owner, &vault);
	Ok(())
}

pub fn close_vault<T: Config>(
	owner: T::AccountId,
	collateral_id: T::AssetId,
	recipient: Option<T::AccountId>,
) -> Result<(), DispatchError> {
	let recipient = recipient.unwrap_or(owner.clone());
	let mut context = OpContext::<T>::load(collateral_id)?;
	context.ensure_not_frozen()?;
	let price = T::Oracle::provide_price(&context.collateral_id)?.price;
	let TouchedVault { vault, status } = context.touch(&owner)?;
	ensure!(vault.debt.total().is_zero(), Error::<T>::DebtOutstanding);

	let config = branch_config_of::<T>(&context.collateral_id)?;
	close_inner::<T>(context, &owner, &recipient, &vault, status, &config, price, None)
}

/// Shared close path. Consumes the operation context at commit.
#[allow(clippy::too_many_arguments)]
fn close_inner<T: Config>(
	mut context: OpContext<T>,
	owner: &T::AccountId,
	recipient: &T::AccountId,
	vault: &Vault<BalanceOf<T>>,
	status: VaultStatus,
	config: &BranchConfig<BalanceOf<T>>,
	price: FixedU128,
	maybe_payment: Option<(DebtPayment<BalanceOf<T>>, FixedU128)>,
) -> Result<(), DispatchError> {
	// FinalRecovery stake is zero, so use the live hold.
	let collateral = T::CollateralAssets::balance_on_hold(
		context.collateral_id.clone(),
		&HoldReason::VaultCollateral.into(),
		owner,
	);
	let mut branch_state_after = context.state.clone();
	if let Some((payment, rate)) = maybe_payment {
		branch_state_after.apply_debt_payment(payment, rate, vault.debt.principal);
	}
	branch_state_after.detach_vault(vault);
	branch_state_after.remove_collateral(collateral);
	branch_state_after.release_dormant_target(owner);

	let branch_empties = branch_state_after.is_empty_of_liability();
	// Sweep now — it mutates `branch_state_after` ahead of the TCR check — but defer the
	// event until the close is past every fallible step, just before commit.
	let orphan_debt = if branch_empties {
		branch_state_after.sweep_orphan_debt()
	} else {
		BalanceOf::<T>::zero()
	};

	let pre_tcr = compute_tcr::<T>(&context.state, price, context.now)?;
	let post_tcr = compute_tcr::<T>(&branch_state_after, price, context.now)?;
	enforce_mode_rules::<T>(config, &context.state, pre_tcr, post_tcr, branch_empties)?;
	context.state = branch_state_after;

	if !collateral.is_zero() {
		T::CollateralAssets::transfer_on_hold(
			context.collateral_id.clone(),
			&HoldReason::VaultCollateral.into(),
			owner,
			recipient,
			collateral,
			Precision::Exact,
			Restriction::Free,
			Fortitude::Polite,
		)?;
	}

	match status {
		VaultStatus::Active => {
			// Active vaults must be in the rate index.
			T::VaultLists::remove(&VaultListId::Rate(context.collateral_id.clone()), owner)
				.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
		},
		VaultStatus::FinalRecovery => {
			recovery::remove::<T>(&context.collateral_id, owner)?;
		},
		VaultStatus::Dormant => {},
	}

	if !orphan_debt.is_zero() {
		Pallet::<T>::deposit_event(Event::BadDebtRecorded {
			collateral_id: context.collateral_id.clone(),
			amount: orphan_debt,
		});
	}
	Pallet::<T>::deposit_event(Event::VaultClosed {
		collateral_id: context.collateral_id.clone(),
		owner: owner.clone(),
		recipient: recipient.clone(),
	});
	context.commit_removing_vault(owner);
	Ok(())
}

pub fn poke<T: Config>(
	owner: T::AccountId,
	collateral_id: T::AssetId,
) -> Result<(), DispatchError> {
	OpContext::<T>::refresh(collateral_id, &owner)
}

pub fn enter_final_recovery<T: Config>(
	owner: T::AccountId,
	collateral_id: T::AssetId,
) -> Result<(), DispatchError> {
	let mut context = OpContext::<T>::load(collateral_id)?;
	context.ensure_not_frozen()?;
	let price = T::Oracle::provide_price(&context.collateral_id)?.price;
	let TouchedVault { mut vault, status } = context.touch(&owner)?;
	ensure!(status.is_active(), Error::<T>::InvalidVaultStatus);

	let config = branch_config_of::<T>(&context.collateral_id)?;
	let collateral = vault.redistribution_stake;
	let total_debt = vault.debt.total();
	ensure_below_mcr::<T>(collateral, total_debt, price, &config)?;

	ensure!(
		context.state.stakes.total == vault.redistribution_stake,
		Error::<T>::NotLastEligibleVault
	);

	T::VaultLists::remove(&VaultListId::Rate(context.collateral_id.clone()), &owner)
		.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
	context.state.refresh_vault_stake(
		vault.annual_rate,
		vault.redistribution_stake,
		BalanceOf::<T>::zero(),
	);
	vault.redistribution_stake = BalanceOf::<T>::zero();
	recovery::append::<T>(&mut context.state, &context.collateral_id, owner.clone())?;

	Pallet::<T>::deposit_event(Event::VaultStatusChanged {
		collateral_id: context.collateral_id.clone(),
		owner: owner.clone(),
		old_status: VaultStatus::Active,
		new_status: VaultStatus::FinalRecovery,
	});
	context.commit_with_vault(&owner, &vault);
	Ok(())
}

/// Explicit `FinalRecovery` exit. Rejoins the rate index only above `MinimumDebt`.
pub fn exit_final_recovery<T: Config>(
	owner: T::AccountId,
	collateral_id: T::AssetId,
	hint: Position<T::AccountId>,
) -> Result<(), DispatchError> {
	let mut context = OpContext::<T>::load(collateral_id)?;
	context.ensure_not_frozen()?;
	let price = T::Oracle::provide_price(&context.collateral_id)?.price;
	let TouchedVault { mut vault, status } = context.touch(&owner)?;
	ensure!(status.is_final_recovery(), Error::<T>::InvalidVaultStatus);

	let config = branch_config_of::<T>(&context.collateral_id)?;
	let collateral = T::CollateralAssets::balance_on_hold(
		context.collateral_id.clone(),
		&HoldReason::VaultCollateral.into(),
		&owner,
	);
	let total_debt = vault.debt.total();
	ensure_at_or_above_mcr::<T>(collateral, total_debt, price, &config)?;

	let rejoin_active = total_debt >= config.minimum_debt;
	let new_status = if rejoin_active { VaultStatus::Active } else { VaultStatus::Dormant };

	recovery::remove::<T>(&context.collateral_id, &owner)?;
	vault.redistribution_stake = collateral;
	vault.redistribution_snapshot = context.state.redistribution;
	context
		.state
		.refresh_vault_stake(vault.annual_rate, BalanceOf::<T>::zero(), collateral);
	if !rejoin_active &&
		!total_debt.is_zero() &&
		!context.state.try_park_dormant_target(owner.clone())
	{
		return Err(Error::<T>::DormantTargetOccupied.into());
	}
	if rejoin_active {
		T::VaultLists::insert(
			VaultListId::Rate(context.collateral_id.clone()),
			owner.clone(),
			vault.annual_rate,
			hint,
		)
		.map_err(map_error::<T>)?;
	}
	Pallet::<T>::deposit_event(Event::VaultStatusChanged {
		collateral_id: context.collateral_id.clone(),
		owner: owner.clone(),
		old_status: VaultStatus::FinalRecovery,
		new_status,
	});
	context.commit_with_vault(&owner, &vault);
	Ok(())
}

/// Permissionless Dormant to Active revival.
pub fn activate_dormant<T: Config>(
	owner: T::AccountId,
	collateral_id: T::AssetId,
	hint: Position<T::AccountId>,
) -> Result<(), DispatchError> {
	let mut context = OpContext::<T>::load(collateral_id)?;
	context.ensure_not_frozen()?;
	let TouchedVault { vault, status } = context.touch(&owner)?;
	ensure!(status.is_dormant(), Error::<T>::InvalidVaultStatus);
	let config = branch_config_of::<T>(&context.collateral_id)?;
	ensure!(vault.debt.total() >= config.minimum_debt, Error::<T>::DebtBelowMinimum);

	T::VaultLists::insert(
		VaultListId::Rate(context.collateral_id.clone()),
		owner.clone(),
		vault.annual_rate,
		hint,
	)
	.map_err(map_error::<T>)?;
	context.state.release_dormant_target(&owner);

	Pallet::<T>::deposit_event(Event::VaultStatusChanged {
		collateral_id: context.collateral_id.clone(),
		owner: owner.clone(),
		old_status: VaultStatus::Dormant,
		new_status: VaultStatus::Active,
	});
	context.commit_with_vault(&owner, &vault);
	Ok(())
}

/// Refresh the next handful of vaults across each branch using the cursor.
pub fn on_idle_walk<T: Config>(remaining: Weight) -> Weight {
	let per_call = T::WeightInfo::on_idle_one_vault();
	if remaining.any_lt(per_call) {
		return Weight::zero();
	}
	let mut consumed = Weight::zero();
	let mut budget = T::MaxOnIdleVaultRefresh::get();
	let touch_one = |collateral_id: &T::AssetId, owner: &T::AccountId| -> bool {
		if !Vaults::<T>::contains_key(collateral_id, owner) {
			return true;
		}
		let _ = with_storage_layer::<(), DispatchError, _>(|| {
			OpContext::<T>::refresh(collateral_id.clone(), owner)
		});
		true
	};
	for collateral_id in BranchConfigs::<T>::iter_keys() {
		let collateral_id = &collateral_id;
		if budget == 0 || (remaining.saturating_sub(consumed)).any_lt(per_call) {
			break;
		}
		let _ = with_storage_layer::<(), DispatchError, _>(|| refresh_branch::<T>(collateral_id));
		let Some(branch) = BranchStates::<T>::get(collateral_id) else { continue };
		let rate_list = VaultListId::Rate(collateral_id.clone());
		let initial_cursor = branch.idle_cursor.or_else(|| T::VaultLists::head(&rate_list));
		let mut cursor = initial_cursor.clone();
		let final_recovery_head = recovery::next_target::<T>(collateral_id);
		let dormant_target = branch.dormant_redemption_target;

		while budget > 0 {
			let Some(owner) = cursor.clone() else { break };
			if !touch_one(collateral_id, &owner) {
				break;
			}
			cursor = T::VaultLists::neighbors(&rate_list, &owner).and_then(|p| p.next);
			budget = budget.saturating_sub(1);
			consumed = consumed.saturating_add(per_call);
			if (remaining.saturating_sub(consumed)).any_lt(per_call) {
				break;
			}
		}

		let try_extra = |owner: T::AccountId, budget: &mut u32, consumed: &mut Weight| {
			if *budget == 0 || (remaining.saturating_sub(*consumed)).any_lt(per_call) {
				return;
			}
			if touch_one(collateral_id, &owner) {
				*budget = budget.saturating_sub(1);
				*consumed = consumed.saturating_add(per_call);
			}
		};
		if let Some(owner) = final_recovery_head {
			try_extra(owner, &mut budget, &mut consumed);
		}
		if let Some(owner) = dormant_target {
			try_extra(owner, &mut budget, &mut consumed);
		}

		if cursor != initial_cursor {
			BranchStates::<T>::mutate(collateral_id, |maybe| {
				if let Some(state) = maybe {
					state.idle_cursor = cursor.take();
				}
			});
		}
	}
	consumed
}
