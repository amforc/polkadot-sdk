use super::*;

/// Fully-accrued total branch debt (the TCR numerator): principal + minted
/// interest + pending aggregate interest + pending redistribution principal +
/// bad debt + ownerless debt. Single definition shared by [`compute_tcr`] and
/// the `branch_debt` redemption-fee accessor so the two cannot diverge.
pub fn accrued_branch_debt<T: Config>(
	state: &BranchState<T::AccountId, BalanceOf<T>>,
	now: Millis,
) -> BalanceOf<T> {
	let elapsed = state.interest_time(now).saturating_sub(state.debt.last_interest_time);
	let pending_aggregate =
		math::simple_interest_ceil(state.debt.weighted_principal_sum, FixedU128::one(), elapsed);
	state
		.debt
		.principal
		.saturating_add(state.debt.minted_interest)
		.saturating_add(pending_aggregate)
		.saturating_add(state.debt.pending_redistribution_principal)
		.saturating_add(state.debt.bad_debt)
		.saturating_add(state.rounding.ownerless_pusd_debt)
}

/// Compute TCR including aggregate interest accrued since the last update.
pub fn compute_tcr<T: Config>(
	state: &BranchState<T::AccountId, BalanceOf<T>>,
	price: FixedU128,
	now: Millis,
) -> Result<FixedU128, DispatchError> {
	let total_debt = accrued_branch_debt::<T>(state, now);
	if total_debt.is_zero() {
		// Branch with no debt is treated as "infinitely well-collateralized".
		return Ok(FixedU128::max_value());
	}
	let value = price
		.checked_mul_int(state.total_collateral)
		.ok_or(Error::<T>::ArithmeticOverflow)?;
	FixedU128::checked_from_rational(value, total_debt)
		.ok_or_else(|| Error::<T>::ArithmeticOverflow.into())
}

/// Accrue aggregate branch interest in memory and return the new amount.
pub(super) fn accrue_aggregate_interest<T: Config>(
	state: &mut BranchState<T::AccountId, BalanceOf<T>>,
	now: Millis,
) -> BalanceOf<T> {
	let tau = state.interest_time(now);
	let elapsed = tau.saturating_sub(state.debt.last_interest_time);
	if elapsed == 0 {
		return BalanceOf::<T>::zero();
	}
	let new_interest =
		math::simple_interest_ceil(state.debt.weighted_principal_sum, FixedU128::one(), elapsed);
	state.debt.last_interest_time = tau;
	if new_interest.is_zero() {
		return BalanceOf::<T>::zero();
	}
	state.debt.minted_interest = state.debt.minted_interest.saturating_add(new_interest);
	new_interest
}

/// Origin of a pUSD yield credit routed by [`mint_and_route_yield`].
#[derive(Debug, Clone, Copy)]
pub(super) enum YieldSource {
	/// Aggregate branch interest accrued at `OpContext::load`.
	BranchInterest,
	/// Upfront fee charged on borrow / change-rate.
	UpfrontFee,
}

/// Issue `amount` pUSD and route per `SpYieldShare`: a portion goes to
/// `T::SpYieldSink`, the residual goes to `T::FeeHandler`.
pub(super) fn mint_and_route_yield<T: Config>(
	collateral_id: &T::CollateralAssetId,
	stable_id: &T::StableAssetId,
	amount: BalanceOf<T>,
	source: YieldSource,
) {
	let credit = T::StableAssets::issue(stable_id.clone(), amount);
	let share: Permill = T::SpYieldShare::get();
	let sp_amount = share * credit.peek();
	let (sp_credit, residual) = credit.split(sp_amount);
	if let Err(e) = <T::SpYieldSink as pusd_primitives::OnBranchYield<_, _, _>>::on_branch_yield(
		collateral_id.clone(),
		stable_id.clone(),
		sp_credit,
	) {
		crate::log!(error, "SpYieldSink rejected {:?}: {:?}", source, e);
	}
	T::FeeHandler::on_unbalanced(residual);
}

/// Deltas the next vault touch would apply.
pub(crate) struct PendingTouch<Balance> {
	/// Capped redistributed principal moved into `vault.debt.principal`
	/// (and out of `state.debt.pending_redistribution_principal`).
	pub principal: Balance,
	/// Redistributed collateral released to the owner's hold.
	pub collateral: Balance,
	/// Stored-principal pending interest plus redistribution interest, both
	/// folded into `vault.debt.interest`.
	pub interest: Balance,
}

/// Project a vault touch without mutating storage.
pub(crate) fn pending_touch_for<T: Config>(
	vault: &Vault<BalanceOf<T>>,
	state: &BranchState<T::AccountId, BalanceOf<T>>,
	now: Millis,
) -> PendingTouch<BalanceOf<T>> {
	let tau = state.interest_time(now);
	let elapsed = tau.saturating_sub(vault.last_interest_time);
	let principal_interest =
		math::simple_interest_floor(vault.debt.principal, vault.annual_rate, elapsed);

	let redistribution = state.redistribution;
	let snap = vault.redistribution_snapshot;
	if snap == redistribution {
		return PendingTouch {
			principal: BalanceOf::<T>::zero(),
			collateral: BalanceOf::<T>::zero(),
			interest: principal_interest,
		};
	}

	let delta_debt_per_stake = redistribution.debt_per_stake.saturating_sub(snap.debt_per_stake);
	let delta_collat_per_stake =
		redistribution.collateral_per_stake.saturating_sub(snap.collateral_per_stake);
	let delta_dt_per_stake =
		redistribution.debt_time_per_stake.saturating_sub(snap.debt_time_per_stake);
	// Cap against the branch counter; rounding dust stays in branch aggregates.
	let raw_principal = delta_debt_per_stake.saturating_mul_int(vault.redistribution_stake);
	let principal = core::cmp::min(raw_principal, state.debt.pending_redistribution_principal);
	let collateral = delta_collat_per_stake.saturating_mul_int(vault.redistribution_stake);
	// Keep this in branch interest time, matching the liquidation writer.
	let now_fp = FixedU128::saturating_from_integer(tau);
	let extra_per_stake =
		now_fp.saturating_mul(delta_debt_per_stake).saturating_sub(delta_dt_per_stake);
	let rate_factor = vault
		.annual_rate
		.checked_div(&FixedU128::saturating_from_integer(pusd_primitives::MILLIS_PER_YEAR))
		.defensive_unwrap_or_else(FixedU128::zero);
	let redistribution_interest = extra_per_stake
		.saturating_mul(rate_factor)
		.saturating_mul_int(vault.redistribution_stake);

	PendingTouch {
		principal,
		collateral,
		interest: principal_interest.saturating_add(redistribution_interest),
	}
}

/// Upfront fee for opening a vault at the post-open average branch rate.
pub fn open_upfront_fee<T: Config>(
	state: &BranchState<T::AccountId, BalanceOf<T>>,
	config: &BranchConfig<BalanceOf<T>>,
	new_debt: BalanceOf<T>,
	new_rate: FixedU128,
) -> BalanceOf<T> {
	let total_ib = state
		.debt
		.principal
		.saturating_add(state.debt.pending_redistribution_principal)
		.saturating_add(new_debt);
	let weighted = state
		.debt
		.weighted_principal_sum
		.saturating_add(new_rate.saturating_mul_int(new_debt));
	let avg = math::average_branch_rate(weighted, total_ib);
	math::simple_interest_ceil(new_debt, avg, config.upfront_fee_period)
}

fn avg_rate<T: Config>(state: &BranchState<T::AccountId, BalanceOf<T>>) -> FixedU128 {
	math::average_branch_rate(
		state.debt.weighted_principal_sum,
		state.debt.principal.saturating_add(state.debt.pending_redistribution_principal),
	)
}

/// Simulate `borrow` for the live path and fee prediction.
///
/// `rate_change_fee_base` is the existing principal that the rate-change
/// component of the upfront fee is charged against (zero when the call is a
/// pure debt increase or the cooldown has elapsed).
pub(super) fn simulate_borrow<T: Config>(
	state: &BranchState<T::AccountId, BalanceOf<T>>,
	config: &BranchConfig<BalanceOf<T>>,
	vault: &Vault<BalanceOf<T>>,
	debt_increase: BalanceOf<T>,
	new_rate: FixedU128,
	rate_change_fee_base: BalanceOf<T>,
) -> (BranchState<T::AccountId, BalanceOf<T>>, BalanceOf<T>) {
	let mut branch_state_after = state.clone();
	branch_state_after.debt.principal = state.debt.principal.saturating_add(debt_increase);
	let weighted_old = vault.annual_rate.saturating_mul_int(vault.debt.principal);
	let weighted_new =
		new_rate.saturating_mul_int(vault.debt.principal.saturating_add(debt_increase));
	branch_state_after.debt.weighted_principal_sum = state
		.debt
		.weighted_principal_sum
		.saturating_sub(weighted_old)
		.saturating_add(weighted_new);
	if new_rate != vault.annual_rate {
		let stake_w_old = vault.annual_rate.saturating_mul_int(vault.redistribution_stake);
		let stake_w_new = new_rate.saturating_mul_int(vault.redistribution_stake);
		branch_state_after.stakes.weighted_sum = state
			.stakes
			.weighted_sum
			.saturating_sub(stake_w_old)
			.saturating_add(stake_w_new);
	}
	let avg = avg_rate::<T>(&branch_state_after);
	let fee = math::simple_interest_ceil(
		debt_increase.saturating_add(rate_change_fee_base),
		avg,
		config.upfront_fee_period,
	);
	branch_state_after.debt.minted_interest =
		branch_state_after.debt.minted_interest.saturating_add(fee);
	(branch_state_after, fee)
}

/// Simulate `change_rate` for the live path and fee prediction.
pub(super) fn simulate_change_rate<T: Config>(
	state: &BranchState<T::AccountId, BalanceOf<T>>,
	config: &BranchConfig<BalanceOf<T>>,
	vault: &Vault<BalanceOf<T>>,
	new_rate: FixedU128,
	cooldown_elapsed: bool,
) -> (BranchState<T::AccountId, BalanceOf<T>>, BalanceOf<T>) {
	let mut branch_state_after = state.clone();
	branch_state_after.change_vault_rate(
		vault.annual_rate,
		new_rate,
		vault.debt.principal,
		vault.redistribution_stake,
	);
	let fee = if cooldown_elapsed {
		BalanceOf::<T>::zero()
	} else {
		let avg = avg_rate::<T>(&branch_state_after);
		math::simple_interest_ceil(vault.debt.principal, avg, config.upfront_fee_period)
	};
	branch_state_after.debt.minted_interest =
		branch_state_after.debt.minted_interest.saturating_add(fee);
	(branch_state_after, fee)
}
