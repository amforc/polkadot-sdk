//! User redemption execution and read-only quoting.

use crate::{
	fees,
	pallet::{
		BalanceOf, CollateralIdOf, Config, Error, Event, Millis, Pallet, RedemptionConfigOf,
		RedemptionConfigs, RedemptionQuoteOf, RedemptionStates, SnapshotOf, StableCreditOf,
		StableIdOf,
	},
	types::{RedemptionState, RedemptionTerms},
};
use frame::{
	deps::sp_runtime::{
		traits::{Saturating, Zero},
		ArithmeticError, FixedU128,
	},
	prelude::*,
	traits::{
		fungibles::{self, Balanced as FungiblesBalanced},
		tokens::{Fortitude, Precision, Preservation},
		OnUnbalanced, Time,
	},
};
use pusd_primitives::{
	recovery_pricing, reducible_debit, CollateralRatio, ProvidePrice, RedemptionSettlement,
	VaultInterface,
};

/// Inputs shared by ordinary and recovery redemptions.
struct RedemptionInputs<Balance> {
	config: crate::types::RedemptionConfig<Balance>,
	price: FixedU128,
}

/// Fee inputs read only for an ordinary redemption.
struct FeeInputs {
	state: RedemptionState,
	now: Millis,
	curve: fees::DynamicFeeCurve,
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
		max_stable_to_spend: BalanceOf<T>,
	) -> Result<RedemptionInputs<BalanceOf<T>>, Error<T>> {
		let config =
			RedemptionConfigs::<T>::get(stable_id).ok_or(Error::<T>::StablecoinNotRegistered)?;
		// A budget below the minimum cannot buy it however small the fee.
		ensure!(
			max_stable_to_spend >= config.minimum_redemption_amount,
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
	) -> Result<FeeInputs, ArithmeticError> {
		let now = T::TimeProvider::now();
		let state = RedemptionStates::<T>::get(stable_id);
		let curve = fees::DynamicFeeCurve::try_new(
			state.dynamic_fee_at(now, config),
			T::Vaults::stablecoin_debt(stable_id),
			config,
		)?;
		Ok(FeeInputs { state, now, curve })
	}

	pub(crate) fn do_redeem(
		redeemer: &T::AccountId,
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		terms: RedemptionTerms<BalanceOf<T>>,
		recipient: &T::AccountId,
		max_steps: u32,
	) -> Result<u32, DispatchError> {
		let inputs = Self::redemption_inputs(collateral_id, stable_id, terms.max_stable_to_spend)?;
		let first_target = T::Vaults::next_redemption_target(collateral_id, stable_id, None)
			.ok_or(Error::<T>::NoRedeemableVault)?;
		let context =
			WalkContext { redeemer, collateral_id, stable_id, recipient, price: inputs.price };

		if first_target.1.is_final_recovery() {
			return Self::redeem_recovery(&context, &inputs.config, first_target.0, terms);
		}

		Self::redeem_ordinary(&context, &inputs.config, first_target, terms, max_steps)
	}

	/// Settle the `FinalRecovery` FIFO head in one step. The redeemer funds the
	/// externally-cancellable debt; when that cancels in full, the Insurance
	/// Fund cover is withdrawn and merged into the same payment, so the whole
	/// vault debt is cancelled by one settlement.
	fn redeem_recovery(
		context: &WalkContext<'_, T>,
		config: &RedemptionConfigOf<T>,
		owner: T::AccountId,
		terms: RedemptionTerms<BalanceOf<T>>,
	) -> Result<u32, DispatchError> {
		let snapshot = T::Vaults::project_redemption_snapshot(
			context.collateral_id,
			context.stable_id,
			&owner,
		)?;
		// Recovery charges no fee, so its single debit may consume the account: a spend that
		// would leave less than the minimum balance is repriced below to the amount that keeps it.
		let expendable =
			Self::reducible_stable(context.stable_id, context.redeemer, Preservation::Expendable);
		let budget = terms.max_stable_to_spend.min(expendable);
		let plan =
			Self::price_recovery(context.stable_id, &snapshot, context.price, budget, config)
				.ok_or(Error::<T>::NoRedeemableVault)?;
		let (plan, preservation) = if plan.debt().is_zero() {
			(plan, None)
		} else {
			let (funded, preservation) =
				Self::fundable_budget(context.stable_id, context.redeemer, plan.debt())?;
			let plan = if funded < plan.debt() {
				Self::price_recovery(context.stable_id, &snapshot, context.price, funded, config)
					.ok_or(Error::<T>::NoRedeemableVault)?
			} else {
				plan
			};
			(plan, Some(preservation))
		};

		let insurance_cover = plan.insurance_cover();
		ensure!(
			!plan.debt().is_zero() || !insurance_cover.is_zero(),
			Error::<T>::NoRedeemableVault
		);
		// Recovery charges no fee, so the debt is the whole spend.
		let scaled_min =
			fees::scale_floor(terms.min_collateral_out, plan.debt(), terms.max_stable_to_spend);
		ensure!(plan.collateral() >= scaled_min, Error::<T>::SlippageExceeded);

		let mut debt_payment = match preservation {
			Some(preservation) => Self::debit_redeemer(
				context.stable_id,
				context.redeemer,
				plan.debt(),
				preservation,
			)?,
			None => StableCreditOf::<T>::zero(context.stable_id.clone()),
		};
		if !insurance_cover.is_zero() {
			let cover = Self::withdraw_insurance_cover(context.stable_id, insurance_cover)?;
			if let Err(cover) = debt_payment.subsume(cover) {
				// Both credits carry `stable_id`, so the merge cannot fail;
				// refuse the settlement and let the dispatch roll back.
				drop(cover);
				return Err(Error::<T>::InsuranceFundWithdrawFailed.into());
			}
		}

		T::Vaults::redeem_step(
			context.collateral_id,
			context.stable_id,
			&owner,
			context.recipient,
			RedemptionSettlement { debt_payment, collateral_to_recipient: plan.collateral() },
		)?;

		Self::deposit_event(Event::RecoveryRedemptionExecuted {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			redeemer: context.redeemer.clone(),
			recipient: context.recipient.clone(),
			vault_owner: owner,
			stable_burned: plan.debt(),
			insurance_cover,
			collateral_out: plan.collateral(),
			regime: plan.regime(),
		});
		Ok(1)
	}

	fn redeem_ordinary(
		context: &WalkContext<'_, T>,
		config: &RedemptionConfigOf<T>,
		first_target: (T::AccountId, pusd_primitives::VaultStatus),
		terms: RedemptionTerms<BalanceOf<T>>,
		max_steps: u32,
	) -> Result<u32, DispatchError> {
		let fee_inputs = Self::fee_inputs(context.stable_id, config)?;
		let payable =
			Self::reducible_stable(context.stable_id, context.redeemer, Preservation::Preserve);
		let debt_budget =
			Self::ordinary_debt_budget(&fee_inputs, terms.max_stable_to_spend.min(payable));
		// A short budget is the terms' fault when the balance covers them, else the balance's.
		let short_budget = if payable >= terms.max_stable_to_spend {
			Error::<T>::BelowMinimumRedemptionAmount
		} else {
			Error::<T>::InsufficientStableBalance
		};
		ensure!(debt_budget >= config.minimum_redemption_amount, short_budget);
		let planned_fee = fee_inputs.curve.fee(debt_budget);

		let mut payment = Self::debit_redeemer(
			context.stable_id,
			context.redeemer,
			debt_budget.checked_add(&planned_fee).ok_or(ArithmeticError::Overflow)?,
			Preservation::Preserve,
		)?;
		let result = Self::execute_ordinary_walk(
			context,
			Self::effective_step_cap(max_steps),
			debt_budget,
			first_target,
			&mut payment,
		)?;
		ensure!(!result.debt.is_zero(), Error::<T>::NoRedeemableVault);

		// The `payment` reserved `planned_fee`. The fee rate is monotonic only up to fixed-point
		// rounding, so the cap keeps a shorter walk from costing more than the plan it was
		// funded against.
		let fee = fee_inputs.curve.fee(result.debt).min(planned_fee);
		Self::route_fee(&mut payment, fee)?;
		Self::refund_change(context.redeemer, payment)?;

		let stable_spent = result.debt.saturating_add(fee);
		let scaled_min =
			fees::scale_floor(terms.min_collateral_out, stable_spent, terms.max_stable_to_spend);
		ensure!(result.collateral >= scaled_min, Error::<T>::SlippageExceeded);

		Self::finalize_ordinary(context, &fee_inputs, &result, fee);
		Ok(result.steps)
	}

	/// Execute the ordinary Vaults-owned queue. Recovery is handled before this walk starts.
	/// Each settled step carves its debt out of `payment`.
	fn execute_ordinary_walk(
		context: &WalkContext<'_, T>,
		step_cap: u32,
		debt_budget: BalanceOf<T>,
		first_target: (T::AccountId, pusd_primitives::VaultStatus),
		payment: &mut StableCreditOf<T>,
	) -> Result<OrdinaryResult<BalanceOf<T>>, DispatchError> {
		debug_assert!(payment.peek() >= debt_budget);
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

			let applied = Self::apply_ordinary_step(context, &owner, result.remaining, payment)?;
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
		let redeemable = match pusd_primitives::collateralization_ratio(&snapshot.position(), price)
		{
			Ok(CollateralRatio::Ratio(ratio)) => ratio >= FixedU128::one(),
			// Nothing to redeem without debt, and an overflowing ratio is not guessed at.
			Ok(CollateralRatio::DebtFree) | Err(_) => false,
		};
		if !redeemable {
			return if snapshot.status.is_dormant() { Step::Stop } else { Step::Skip };
		}

		let debt = snapshot.size_within(budget);
		if debt.is_zero() {
			return Step::Stop;
		}
		// A failed face-value conversion cannot price this or any later target.
		let Some(collateral) = recovery_pricing::collateral_for_value_floor(debt, price) else {
			return Step::Stop;
		};
		Step::Redeem { debt, collateral: collateral.min(snapshot.collateral) }
	}

	/// Price one target from its projection, carve its debt out of `payment`, and apply the
	/// settlement. `Skip` and `Stop` never touch storage.
	fn apply_ordinary_step(
		context: &WalkContext<'_, T>,
		owner: &T::AccountId,
		budget: BalanceOf<T>,
		payment: &mut StableCreditOf<T>,
	) -> Result<Step<BalanceOf<T>>, DispatchError> {
		debug_assert!(payment.peek() >= budget);
		let snapshot = T::Vaults::project_redemption_snapshot(
			context.collateral_id,
			context.stable_id,
			owner,
		)?;
		let priced = Self::price_ordinary_step(&snapshot, context.price, budget);
		let Step::Redeem { debt, collateral } = priced else {
			return Ok(priced);
		};

		let debt_payment = Self::exact_credit(payment.extract(debt), debt)?;
		T::Vaults::redeem_step(
			context.collateral_id,
			context.stable_id,
			owner,
			context.recipient,
			RedemptionSettlement { debt_payment, collateral_to_recipient: collateral },
		)?;
		Ok(Step::Redeem { debt, collateral })
	}

	/// The part of `need` one debit can take from the redeemer, and its preservation. A
	/// shortfall is returned for re-pricing only when `need` would leave less than the minimum
	/// balance and something is still payable; any other shortfall is an insufficient balance.
	fn fundable_budget(
		stable_id: &StableIdOf<T>,
		redeemer: &T::AccountId,
		need: BalanceOf<T>,
	) -> Result<(BalanceOf<T>, Preservation), Error<T>> {
		let (funded, preservation) =
			reducible_debit::<T::StableAssets, _>(stable_id.clone(), redeemer, need);
		if funded < need {
			ensure!(preservation == Preservation::Preserve, Error::<T>::InsufficientStableBalance);
			ensure!(!funded.is_zero(), Error::<T>::InsufficientStableBalance);
		}
		Ok((funded, preservation))
	}

	/// Withdraws exactly `amount` of the stablecoin from the redeemer as a credit.
	fn debit_redeemer(
		stable_id: &StableIdOf<T>,
		redeemer: &T::AccountId,
		amount: BalanceOf<T>,
		preservation: Preservation,
	) -> Result<StableCreditOf<T>, DispatchError> {
		let credit = <T::StableAssets as FungiblesBalanced<_>>::withdraw(
			stable_id.clone(),
			redeemer,
			amount,
			Precision::Exact,
			preservation,
			Fortitude::Polite,
		)?;
		Self::exact_credit(credit, amount)
	}

	/// Accepts `credit` only if it carries exactly `amount`: vaults and the fee handler take
	/// whatever a credit carries, so any other size would settle an unpriced amount.
	pub(crate) fn exact_credit(
		credit: StableCreditOf<T>,
		amount: BalanceOf<T>,
	) -> Result<StableCreditOf<T>, DispatchError> {
		if credit.peek() == amount {
			Ok(credit)
		} else {
			drop(credit);
			Err(DispatchError::Corruption)
		}
	}

	/// The stablecoin the redeemer can pay under `preservation`, net of freezes.
	fn reducible_stable(
		stable_id: &StableIdOf<T>,
		redeemer: &T::AccountId,
		preservation: Preservation,
	) -> BalanceOf<T> {
		<T::StableAssets as fungibles::Inspect<_>>::reducible_balance(
			stable_id.clone(),
			redeemer,
			preservation,
			Fortitude::Polite,
		)
	}

	/// Calculates the debt that `stable_budget` buys on this redemption's curve, including the fee.
	///
	/// Only the budget limits the walk. The aggregate debt in the curve can exclude a terminal
	/// charge that a full payoff cancels.
	fn ordinary_debt_budget(fee_inputs: &FeeInputs, stable_budget: BalanceOf<T>) -> BalanceOf<T> {
		fees::max_debt_for_budget(stable_budget, |debt| fee_inputs.curve.fee(debt))
	}

	/// Carves the fee out of `payment` and routes it to the fee handler.
	fn route_fee(payment: &mut StableCreditOf<T>, fee: BalanceOf<T>) -> DispatchResult {
		if fee.is_zero() {
			return Ok(());
		}
		let credit = Self::exact_credit(payment.extract(fee), fee)?;
		T::FeeHandler::on_unbalanced(credit);
		Ok(())
	}

	/// Returns the unspent part of `payment` to the redeemer. The preserving debit kept the
	/// account alive, so a refused deposit is an invariant break.
	fn refund_change(redeemer: &T::AccountId, change: StableCreditOf<T>) -> DispatchResult {
		if change.peek().is_zero() {
			return Ok(());
		}
		<T::StableAssets as FungiblesBalanced<_>>::resolve(redeemer, change).map_err(|change| {
			drop(change);
			DispatchError::Corruption
		})
	}

	fn finalize_ordinary(
		context: &WalkContext<'_, T>,
		fee_inputs: &FeeInputs,
		result: &OrdinaryResult<BalanceOf<T>>,
		fee: BalanceOf<T>,
	) {
		let new_fee = fee_inputs.curve.raised_dynamic_fee(result.debt.unique_saturated_into());
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

	/// Builds an indicative read-only quote from projected post-touch snapshots.
	pub(crate) fn quote_redeem(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		max_stable_to_spend: BalanceOf<T>,
		max_steps: u32,
	) -> Result<RedemptionQuoteOf<T>, DispatchError> {
		let inputs = Self::redemption_inputs(collateral_id, stable_id, max_stable_to_spend)?;
		let mut targets = T::Vaults::redemption_quote_targets(collateral_id, stable_id);
		let first_owner = targets.next().ok_or(Error::<T>::NoRedeemableVault)?;
		let first = T::Vaults::project_redemption_snapshot(collateral_id, stable_id, &first_owner)?;

		if first.status.is_final_recovery() {
			let plan = Self::price_recovery(
				stable_id,
				&first,
				inputs.price,
				max_stable_to_spend,
				&inputs.config,
			)
			.filter(|plan| !plan.debt().is_zero() || !plan.insurance_cover().is_zero())
			.ok_or(Error::<T>::NoRedeemableVault)?;
			return Ok(RedemptionQuoteOf::<T> {
				debt_cancelled: plan.debt(),
				collateral_out: plan.collateral(),
				fee: Zero::zero(),
				steps: 1,
				truncated: false,
			});
		}

		let fee_inputs = Self::fee_inputs(stable_id, &inputs.config)?;
		let debt_budget = Self::ordinary_debt_budget(&fee_inputs, max_stable_to_spend);
		ensure!(
			debt_budget >= inputs.config.minimum_redemption_amount,
			Error::<T>::BelowMinimumRedemptionAmount
		);
		Self::quote_ordinary(
			collateral_id,
			stable_id,
			&inputs,
			&fee_inputs,
			debt_budget,
			Self::effective_step_cap(max_steps),
			first,
			targets,
		)
	}

	fn quote_ordinary(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		inputs: &RedemptionInputs<BalanceOf<T>>,
		fee_inputs: &FeeInputs,
		debt_budget: BalanceOf<T>,
		step_cap: u32,
		first: SnapshotOf<T>,
		mut targets: impl Iterator<Item = T::AccountId>,
	) -> Result<RedemptionQuoteOf<T>, DispatchError> {
		let mut quote = RedemptionQuoteOf::<T>::default();
		let mut next = Some(first);

		loop {
			let remaining = debt_budget.saturating_sub(quote.debt_cancelled);
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
		quote.fee = fee_inputs.curve.fee(quote.debt_cancelled);
		Ok(quote)
	}
}
