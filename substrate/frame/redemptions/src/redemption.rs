//! User redemption execution and read-only quoting.

use crate::{
	fees,
	pallet::{
		BalanceOf, CollateralIdOf, Config, Error, Event, Millis, Pallet, RedemptionConfigOf,
		RedemptionConfigs, RedemptionQuoteOf, RedemptionStates, SnapshotOf, StableCreditOf,
		StableIdOf,
	},
	recovery::RecoveryPlan,
	types::{RedemptionState, RedemptionTerms},
};
use frame::{
	deps::sp_runtime::{
		traits::{Saturating, Zero},
		FixedPointNumber, FixedU128,
	},
	prelude::*,
	traits::{
		fungibles::Balanced as FungiblesBalanced,
		tokens::{Fortitude, Precision, Preservation},
		OnUnbalanced, Time,
	},
};
use pusd_primitives::{
	recovery_pricing, reducible_debit, ProvidePrice, RedemptionSettlement, VaultInterface,
};

/// Inputs shared by ordinary and recovery redemptions.
struct RedemptionInputs<Balance> {
	config: crate::types::RedemptionConfig<Balance>,
	price: FixedU128,
}

/// Fee inputs read only for an ordinary redemption.
struct FeeInputs<Balance> {
	state: RedemptionState,
	now: Millis,
	decayed_fee: FixedU128,
	stablecoin_debt: Balance,
}

struct WalkContext<'a, T: Config> {
	redeemer: &'a T::AccountId,
	collateral_id: &'a CollateralIdOf<T>,
	stable_id: &'a StableIdOf<T>,
	recipient: &'a T::AccountId,
	price: FixedU128,
}

enum Step<Balance> {
	Redeem { debt: Balance, collateral: Balance },
	Skip,
	Stop,
}

struct OrdinaryResult<Balance> {
	remaining: Balance,
	steps: u32,
	debt: Balance,
	collateral: Balance,
}

impl<Balance: Zero> OrdinaryResult<Balance> {
	fn new(budget: Balance) -> Self {
		Self { remaining: budget, steps: 0, debt: Zero::zero(), collateral: Zero::zero() }
	}
}

impl<T: Config> Pallet<T> {
	/// `0` means "use the runtime ceiling".
	pub(crate) fn effective_step_cap(max_steps: u32) -> u32 {
		if max_steps == 0 {
			T::MaxRedemptionSteps::get()
		} else {
			max_steps.min(T::MaxRedemptionSteps::get())
		}
	}

	fn redemption_inputs(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		max_stable_in: BalanceOf<T>,
	) -> Result<RedemptionInputs<BalanceOf<T>>, Error<T>> {
		let config =
			RedemptionConfigs::<T>::get(stable_id).ok_or(Error::<T>::StablecoinNotRegistered)?;
		ensure!(
			max_stable_in >= config.minimum_redemption_amount,
			Error::<T>::BelowMinimumRedemptionAmount
		);
		let price =
			T::Oracle::provide_price(collateral_id).map_err(|_| Error::<T>::OracleUnavailable)?;
		ensure!(!price.is_zero(), Error::<T>::OracleUnavailable);
		Ok(RedemptionInputs { config, price })
	}

	fn fee_inputs(
		stable_id: &StableIdOf<T>,
		config: &RedemptionConfigOf<T>,
	) -> FeeInputs<BalanceOf<T>> {
		let now = T::TimeProvider::now();
		let state = RedemptionStates::<T>::get(stable_id);
		FeeInputs {
			state,
			now,
			decayed_fee: Self::decayed_dynamic_fee(&state, config, now),
			stablecoin_debt: T::Vaults::stablecoin_debt(stable_id),
		}
	}

	pub(crate) fn do_redeem(
		redeemer: &T::AccountId,
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		terms: RedemptionTerms<BalanceOf<T>>,
		recipient: &T::AccountId,
		max_steps: u32,
	) -> Result<u32, DispatchError> {
		let inputs = Self::redemption_inputs(collateral_id, stable_id, terms.max_stable_in)?;
		let first_target = T::Vaults::next_redemption_target(collateral_id, stable_id, None)
			.ok_or(Error::<T>::NoRedeemableVault)?;
		let context =
			WalkContext { redeemer, collateral_id, stable_id, recipient, price: inputs.price };

		if first_target.1.is_final_recovery() {
			return Self::redeem_recovery(&context, &inputs.config, first_target.0, terms);
		}

		Self::redeem_ordinary(&context, &inputs.config, first_target, terms, max_steps)
	}

	fn redeem_recovery(
		context: &WalkContext<'_, T>,
		config: &RedemptionConfigOf<T>,
		owner: T::AccountId,
		terms: RedemptionTerms<BalanceOf<T>>,
	) -> Result<u32, DispatchError> {
		let budget = terms
			.max_stable_in
			.min(Self::spendable_stable(context.stable_id, context.redeemer));
		let plan = T::Vaults::redeem_step(
			context.collateral_id,
			context.stable_id,
			&owner,
			context.recipient,
			|snapshot| Self::execute_recovery_step(context, config, &snapshot, budget),
		)?
		.ok_or(Error::<T>::NoRedeemableVault)?;
		let residual = if plan.settles_residual() {
			Self::settle_recovery_residual(context.collateral_id, context.stable_id, &owner)?
		} else {
			Zero::zero()
		};
		ensure!(!plan.debt().is_zero() || !residual.is_zero(), Error::<T>::NoRedeemableVault);
		let scaled_min =
			fees::scale_floor(terms.min_collateral_out, plan.debt(), terms.max_stable_in);
		ensure!(plan.collateral() >= scaled_min, Error::<T>::SlippageExceeded);

		Self::deposit_event(Event::RecoveryRedemptionExecuted {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			redeemer: context.redeemer.clone(),
			recipient: context.recipient.clone(),
			vault_owner: owner,
			stable_burned: plan.debt(),
			collateral_out: plan.collateral(),
			regime: plan.regime(),
		});
		Ok(1)
	}

	fn execute_recovery_step(
		context: &WalkContext<'_, T>,
		config: &RedemptionConfigOf<T>,
		snapshot: &SnapshotOf<T>,
		budget: BalanceOf<T>,
	) -> Result<
		(
			Option<RedemptionSettlement<StableCreditOf<T>, BalanceOf<T>>>,
			Option<RecoveryPlan<BalanceOf<T>>>,
		),
		DispatchError,
	> {
		let Some(mut plan) =
			Self::price_recovery(context.stable_id, snapshot, context.price, budget, config)
		else {
			return Ok((None, None));
		};
		let preservation = if plan.debt().is_zero() {
			None
		} else {
			let (funded, preservation) =
				Self::fundable_budget(context.stable_id, context.redeemer, plan.debt())?;
			if funded < plan.debt() {
				let Some(resized) = plan.resize(snapshot, context.price, funded) else {
					return Ok((None, None));
				};
				plan = resized;
			}
			Some(preservation)
		};
		if plan.debt().is_zero() && !plan.settles_residual() {
			return Ok((None, None));
		}
		let payment = match preservation {
			Some(preservation) => Some(Self::fund_redemption(
				context.stable_id,
				context.redeemer,
				plan.debt(),
				preservation,
			)?),
			None => None,
		};
		let settlement = payment.map(|debt_payment| RedemptionSettlement {
			debt_payment,
			collateral_to_recipient: plan.collateral(),
		});
		Ok((settlement, Some(plan)))
	}

	fn redeem_ordinary(
		context: &WalkContext<'_, T>,
		config: &RedemptionConfigOf<T>,
		first_target: (T::AccountId, pusd_primitives::VaultStatus),
		terms: RedemptionTerms<BalanceOf<T>>,
		max_steps: u32,
	) -> Result<u32, DispatchError> {
		let fee_inputs = Self::fee_inputs(context.stable_id, config);
		let spendable = Self::spendable_stable(context.stable_id, context.redeemer);
		let debt_budget =
			Self::ordinary_debt_budget(config, &fee_inputs, terms.max_stable_in, spendable);
		ensure!(
			debt_budget >= config.minimum_redemption_amount,
			Error::<T>::InsufficientStableBalance
		);

		let result = Self::execute_ordinary_walk(
			context,
			Self::effective_step_cap(max_steps),
			debt_budget,
			first_target,
		)?;
		ensure!(!result.debt.is_zero(), Error::<T>::NoRedeemableVault);

		let fee = fees::redemption_fee(
			result.debt,
			Self::charged_fee_rate(config, &fee_inputs, result.debt),
		);
		Self::charge_fee(context.stable_id, context.redeemer, fee)?;

		let redeemed = debt_budget.saturating_sub(result.remaining);
		let scaled_min = fees::scale_floor(terms.min_collateral_out, redeemed, terms.max_stable_in);
		ensure!(result.collateral >= scaled_min, Error::<T>::SlippageExceeded);

		Self::finalize_ordinary(context, config, &fee_inputs, &result, fee);
		Ok(result.steps)
	}

	/// Execute the ordinary Vaults-owned queue. Recovery is handled before this walk starts.
	fn execute_ordinary_walk(
		context: &WalkContext<'_, T>,
		step_cap: u32,
		debt_budget: BalanceOf<T>,
		first_target: (T::AccountId, pusd_primitives::VaultStatus),
	) -> Result<OrdinaryResult<BalanceOf<T>>, DispatchError> {
		let mut result = OrdinaryResult::new(debt_budget);
		let mut cursor = None;
		let mut next = Some(first_target);

		while result.steps < step_cap {
			let target = next.take().or_else(|| {
				T::Vaults::next_redemption_target(
					context.collateral_id,
					context.stable_id,
					cursor.as_ref(),
				)
			});
			let Some((owner, status)) = target else {
				break;
			};
			if result.remaining.is_zero() || status.is_final_recovery() {
				break;
			}

			let applied = T::Vaults::redeem_step(
				context.collateral_id,
				context.stable_id,
				&owner,
				context.recipient,
				|snapshot| Self::execute_ordinary_step(context, &snapshot, result.remaining),
			)?;
			result.steps = result.steps.saturating_add(1);

			match applied {
				Step::Stop => break,
				Step::Skip => cursor = Some(owner),
				Step::Redeem { debt, collateral } => {
					result.remaining = result.remaining.saturating_sub(debt);
					result.debt = result.debt.saturating_add(debt);
					result.collateral = result.collateral.saturating_add(collateral);
				},
			}
		}
		Ok(result)
	}

	fn price_ordinary_step(
		snapshot: &SnapshotOf<T>,
		price: FixedU128,
		budget: BalanceOf<T>,
	) -> Step<BalanceOf<T>> {
		if snapshot.status.is_final_recovery() {
			return Step::Stop;
		}
		let redeemable = matches!(
			pusd_primitives::collateralization_ratio(&snapshot.position(), price),
			Some(ratio) if ratio >= FixedU128::one()
		);
		if !redeemable {
			return if snapshot.status.is_dormant() { Step::Stop } else { Step::Skip };
		}

		let debt = snapshot.debt.min(budget);
		if debt.is_zero() {
			return Step::Stop;
		}
		// A failed face-value conversion cannot price this or any later target.
		let Some(collateral) = recovery_pricing::collateral_for_value_floor(debt, price) else {
			return Step::Stop;
		};
		Step::Redeem { debt, collateral: collateral.min(snapshot.collateral) }
	}

	fn execute_ordinary_step(
		context: &WalkContext<'_, T>,
		snapshot: &SnapshotOf<T>,
		budget: BalanceOf<T>,
	) -> Result<
		(Option<RedemptionSettlement<StableCreditOf<T>, BalanceOf<T>>>, Step<BalanceOf<T>>),
		DispatchError,
	> {
		match Self::price_ordinary_step(snapshot, context.price, budget) {
			Step::Redeem { mut debt, mut collateral } => {
				let (funded, preservation) =
					Self::fundable_budget(context.stable_id, context.redeemer, debt)?;
				if funded < debt {
					match Self::price_ordinary_step(snapshot, context.price, funded) {
						Step::Redeem { debt: resized_debt, collateral: resized_collateral } => {
							debt = resized_debt;
							collateral = resized_collateral;
						},
						_ => return Ok((None, Step::Stop)),
					}
				}
				let debt_payment =
					Self::fund_redemption(context.stable_id, context.redeemer, debt, preservation)?;
				Ok((
					Some(RedemptionSettlement {
						debt_payment,
						collateral_to_recipient: collateral,
					}),
					Step::Redeem { debt, collateral },
				))
			},
			Step::Skip => Ok((None, Step::Skip)),
			Step::Stop => Ok((None, Step::Stop)),
		}
	}

	fn fundable_budget(
		stable_id: &StableIdOf<T>,
		redeemer: &T::AccountId,
		need: BalanceOf<T>,
	) -> Result<(BalanceOf<T>, Preservation), Error<T>> {
		let (funded, preservation) =
			reducible_debit::<T::StableAssets, _>(stable_id.clone(), redeemer, need);
		if funded < need {
			ensure!(preservation == Preservation::Preserve, Error::<T>::InsufficientStableBalance);
		}
		Ok((funded, preservation))
	}

	fn fund_redemption(
		stable_id: &StableIdOf<T>,
		redeemer: &T::AccountId,
		debt: BalanceOf<T>,
		preservation: Preservation,
	) -> Result<StableCreditOf<T>, DispatchError> {
		let credit = <T::StableAssets as FungiblesBalanced<_>>::withdraw(
			stable_id.clone(),
			redeemer,
			debt,
			Precision::Exact,
			preservation,
			Fortitude::Polite,
		)?;
		debug_assert_eq!(credit.peek(), debt);
		Ok(credit)
	}

	fn spendable_stable(stable_id: &StableIdOf<T>, redeemer: &T::AccountId) -> BalanceOf<T> {
		reducible_debit::<T::StableAssets, _>(
			stable_id.clone(),
			redeemer,
			BalanceOf::<T>::max_value(),
		)
		.0
	}

	fn ordinary_debt_budget(
		config: &RedemptionConfigOf<T>,
		fee_inputs: &FeeInputs<BalanceOf<T>>,
		max_stable_in: BalanceOf<T>,
		spendable: BalanceOf<T>,
	) -> BalanceOf<T> {
		let max_debt = max_stable_in.min(fee_inputs.stablecoin_debt);
		fees::max_debt_for_budget(spendable, max_debt, |debt| {
			fees::redemption_fee(debt, Self::charged_fee_rate(config, fee_inputs, debt))
		})
	}

	fn charge_fee(
		stable_id: &StableIdOf<T>,
		redeemer: &T::AccountId,
		fee: BalanceOf<T>,
	) -> DispatchResult {
		if fee.is_zero() {
			return Ok(());
		}
		let (funded, preservation) =
			reducible_debit::<T::StableAssets, _>(stable_id.clone(), redeemer, fee);
		ensure!(funded >= fee, Error::<T>::InsufficientStableBalance);
		let credit = <T::StableAssets as FungiblesBalanced<_>>::withdraw(
			stable_id.clone(),
			redeemer,
			fee,
			Precision::Exact,
			preservation,
			Fortitude::Polite,
		)?;
		debug_assert_eq!(credit.peek(), fee);
		T::FeeHandler::on_unbalanced(credit);
		Ok(())
	}

	fn finalize_ordinary(
		context: &WalkContext<'_, T>,
		config: &RedemptionConfigOf<T>,
		fee_inputs: &FeeInputs<BalanceOf<T>>,
		result: &OrdinaryResult<BalanceOf<T>>,
		fee: BalanceOf<T>,
	) {
		let new_fee = Self::raised_dynamic_fee(config, fee_inputs, result.debt);
		RedemptionStates::<T>::insert(
			context.stable_id,
			RedemptionState { dynamic_fee: new_fee, last_fee_operation: fee_inputs.now },
		);
		if new_fee != fee_inputs.state.dynamic_fee {
			Self::deposit_event(Event::RedemptionDynamicFeeUpdated {
				stable_id: context.stable_id.clone(),
				old_dynamic_fee: fee_inputs.state.dynamic_fee,
				new_dynamic_fee: new_fee,
			});
		}
		Self::deposit_event(Event::OrdinaryRedemptionExecuted {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			redeemer: context.redeemer.clone(),
			recipient: context.recipient.clone(),
			stable_burned: result.debt,
			collateral_out: result.collateral,
			fee,
			steps: result.steps,
		});
	}

	fn charged_fee_rate(
		config: &RedemptionConfigOf<T>,
		fee_inputs: &FeeInputs<BalanceOf<T>>,
		redeemed: BalanceOf<T>,
	) -> FixedU128 {
		let raised = Self::raised_dynamic_fee(config, fee_inputs, redeemed);
		fees::fee_rate(raised, config.base_fee, config.fee_ceiling)
	}

	fn raised_dynamic_fee(
		config: &RedemptionConfigOf<T>,
		fee_inputs: &FeeInputs<BalanceOf<T>>,
		redeemed: BalanceOf<T>,
	) -> FixedU128 {
		let fraction = FixedU128::checked_from_rational(redeemed, fee_inputs.stablecoin_debt)
			.unwrap_or_else(FixedU128::one);
		fees::increased_dynamic_fee(
			fee_inputs.decayed_fee,
			fraction,
			config.dynamic_fee_increase_divisor,
			config.dynamic_fee_floor,
			config.dynamic_fee_ceiling,
		)
	}

	fn decayed_dynamic_fee(
		state: &RedemptionState,
		config: &RedemptionConfigOf<T>,
		now: Millis,
	) -> FixedU128 {
		fees::decay_dynamic_fee(
			state.dynamic_fee,
			now.saturating_sub(state.last_fee_operation),
			config.dynamic_fee_decay_period,
		)
		.max(config.dynamic_fee_floor)
		.min(config.dynamic_fee_ceiling)
	}

	/// Build an indicative read-only quote from projected post-touch snapshots.
	pub(crate) fn quote_redeem(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		max_stable_in: BalanceOf<T>,
		max_steps: u32,
	) -> Result<RedemptionQuoteOf<T>, DispatchError> {
		let inputs = Self::redemption_inputs(collateral_id, stable_id, max_stable_in)?;
		let mut targets = T::Vaults::redemption_quote_targets(collateral_id, stable_id);
		let first_owner = targets.next().ok_or(Error::<T>::NoRedeemableVault)?;
		let first = T::Vaults::project_redemption_snapshot(collateral_id, stable_id, &first_owner)?;

		if first.status.is_final_recovery() {
			let plan = Self::price_recovery(
				stable_id,
				&first,
				inputs.price,
				max_stable_in,
				&inputs.config,
			)
			.filter(|plan| !plan.debt().is_zero() || plan.settles_residual())
			.ok_or(Error::<T>::NoRedeemableVault)?;
			return Ok(RedemptionQuoteOf::<T> {
				debt_cancelled: plan.debt(),
				collateral_out: plan.collateral(),
				fee: Zero::zero(),
				steps: 1,
				truncated: false,
			});
		}

		let fee_inputs = Self::fee_inputs(stable_id, &inputs.config);
		Self::quote_ordinary(
			collateral_id,
			stable_id,
			&inputs,
			&fee_inputs,
			max_stable_in,
			Self::effective_step_cap(max_steps),
			first,
			targets,
		)
	}

	fn quote_ordinary(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		inputs: &RedemptionInputs<BalanceOf<T>>,
		fee_inputs: &FeeInputs<BalanceOf<T>>,
		max_stable_in: BalanceOf<T>,
		step_cap: u32,
		first: SnapshotOf<T>,
		mut targets: impl Iterator<Item = T::AccountId>,
	) -> Result<RedemptionQuoteOf<T>, DispatchError> {
		let mut quote = RedemptionQuoteOf::<T>::default();
		let mut next = Some(first);

		loop {
			let remaining = max_stable_in.saturating_sub(quote.debt_cancelled);
			if remaining.is_zero() {
				break;
			}
			if quote.steps >= step_cap {
				quote.truncated = true;
				break;
			}
			let snapshot = match next.take() {
				Some(first) => first,
				None => {
					let Some(owner) = targets.next() else {
						break;
					};
					T::Vaults::project_redemption_snapshot(collateral_id, stable_id, &owner)?
				},
			};
			quote.steps = quote.steps.saturating_add(1);

			match Self::price_ordinary_step(&snapshot, inputs.price, remaining) {
				Step::Stop => break,
				Step::Skip => {},
				Step::Redeem { debt, collateral } => {
					quote.debt_cancelled = quote.debt_cancelled.saturating_add(debt);
					quote.collateral_out = quote.collateral_out.saturating_add(collateral);
				},
			}
		}

		ensure!(!quote.debt_cancelled.is_zero(), Error::<T>::NoRedeemableVault);
		quote.fee = fees::redemption_fee(
			quote.debt_cancelled,
			Self::charged_fee_rate(&inputs.config, fee_inputs, quote.debt_cancelled),
		);
		Ok(quote)
	}
}
