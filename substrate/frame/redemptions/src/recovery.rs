//! Final-recovery pricing, Insurance Fund settlement, and Stability Pool offsets.

use crate::{
	pallet::{
		BalanceOf, CollateralIdOf, Config, Error, Pallet, RedemptionConfigOf, RedemptionConfigs,
		SnapshotOf, StableCreditOf, StableIdOf,
	},
	types::{RecoveryOffsetQuote, RecoveryRegime},
};
use frame::{
	deps::{
		frame_support::storage::{with_transaction, TransactionOutcome},
		sp_runtime::{
			traits::{Convert, Saturating, Zero},
			FixedPointNumber, FixedPointOperand, FixedU128,
		},
	},
	prelude::*,
	traits::{
		fungibles::Balanced as FungiblesBalanced,
		tokens::{Fortitude, Precision},
	},
};
use pusd_primitives::{
	debit_preservation, recovery_pricing, reducible_debit, ProvidePrice, RecoveryOffsetInterface,
	RecoveryOffsetResult, RedemptionSettlement, RedemptionStepSnapshot, VaultInterface,
};

/// One authoritative price for a `FinalRecovery` head.
///
/// Redemption execution, quotes, and Stability Pool offsets all consume this
/// plan. Only execution may resize it to what its payer can actually fund.
pub(crate) enum RecoveryPlan<Balance> {
	AbovePar {
		debt: Balance,
		collateral: Balance,
		bonus: FixedU128,
	},
	BelowPar {
		debt: Balance,
		collateral: Balance,
		split: pusd_primitives::InsuranceAdjusted<Balance>,
	},
}

impl<Balance: Copy> RecoveryPlan<Balance> {
	pub(crate) fn debt(&self) -> Balance {
		match self {
			Self::AbovePar { debt, .. } | Self::BelowPar { debt, .. } => *debt,
		}
	}

	pub(crate) fn collateral(&self) -> Balance {
		match self {
			Self::AbovePar { collateral, .. } | Self::BelowPar { collateral, .. } => *collateral,
		}
	}

	pub(crate) fn regime(&self) -> RecoveryRegime {
		match self {
			Self::AbovePar { .. } => RecoveryRegime::RecoveryBonus,
			Self::BelowPar { .. } => RecoveryRegime::InsuranceAdjusted,
		}
	}
}

impl<Balance: FixedPointOperand + Ord> RecoveryPlan<Balance> {
	/// Re-prices the plan for a smaller `budget`.
	///
	/// Returns `None` if a price conversion fails. The original plan priced the
	/// same regime at the same `price`, so this only happens on corrupt inputs;
	/// the caller must skip the target.
	pub(crate) fn resize(
		self,
		snapshot: &RedemptionStepSnapshot<Balance>,
		price: FixedU128,
		budget: Balance,
	) -> Option<Self> {
		match self {
			Self::AbovePar { bonus, .. } => {
				let debt = snapshot.debt.min(budget);
				let collateral =
					recovery_pricing::recovery_bonus_collateral_out(debt, bonus, price)?
						.min(snapshot.collateral);
				Some(Self::AbovePar { debt, collateral, bonus })
			},
			Self::BelowPar { split, .. } => {
				let debt = split.market_cancel_debt.min(budget);
				let collateral = recovery_pricing::recovery_rate_collateral_out(
					debt,
					split.recovery_rate,
					price,
				)?
				.min(snapshot.collateral);
				Some(Self::BelowPar { debt, collateral, split })
			},
		}
	}

	/// Full external cancellation unlocks the Insurance Fund residual.
	pub(crate) fn settles_residual(&self) -> bool {
		matches!(
			self,
			Self::BelowPar { debt, split, .. }
				if *debt == split.market_cancel_debt && !split.effective_cover.is_zero()
		)
	}
}

enum OffsetDecision<Balance> {
	NoTarget,
	BelowPar,
	Available { debt: Balance, collateral: Balance },
}

impl<T: Config> Pallet<T> {
	/// Price a recovery head. This is the only recovery regime selection in the pallet.
	pub(crate) fn price_recovery(
		stable_id: &StableIdOf<T>,
		snapshot: &SnapshotOf<T>,
		price: FixedU128,
		budget: BalanceOf<T>,
		config: &RedemptionConfigOf<T>,
	) -> Option<RecoveryPlan<BalanceOf<T>>> {
		if !snapshot.status.is_final_recovery() {
			return None;
		}

		let ratio = pusd_primitives::collateralization_ratio(&snapshot.position(), price)?;
		if ratio >= FixedU128::one() {
			let bonus = recovery_pricing::recovery_bonus(
				ratio,
				config.final_recovery_bonus_buffer,
				snapshot.redistribution_penalty,
			);
			let debt = snapshot.debt.min(budget);
			let collateral = recovery_pricing::recovery_bonus_collateral_out(debt, bonus, price)?
				.min(snapshot.collateral);
			return Some(RecoveryPlan::AbovePar { debt, collateral, bonus });
		}

		// `ratio < 1` implies `collateral_value < debt`, so the split is in range.
		let collateral_value = price.saturating_mul_int(snapshot.collateral);
		let shortfall = snapshot.debt.saturating_sub(collateral_value);
		let cover = Self::insurance_fund_cover(stable_id, shortfall);
		let split = recovery_pricing::insurance_adjusted(snapshot.debt, collateral_value, cover)?;
		let debt = split.market_cancel_debt.min(budget);
		let collateral =
			recovery_pricing::recovery_rate_collateral_out(debt, split.recovery_rate, price)?
				.min(snapshot.collateral);
		Some(RecoveryPlan::BelowPar { debt, collateral, split })
	}

	fn insurance_fund_cover(stable_id: &StableIdOf<T>, shortfall: BalanceOf<T>) -> BalanceOf<T> {
		reducible_debit::<T::StableAssets, _>(
			stable_id.clone(),
			&T::InsuranceFundAccount::convert(stable_id.clone()),
			shortfall,
		)
		.0
	}

	/// Settle and burn the Insurance Fund portion after the vault step commits.
	pub(crate) fn settle_recovery_residual(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<BalanceOf<T>, DispatchError> {
		let residual = T::Vaults::settle_recovery_residual(collateral_id, stable_id, owner)
			.map_err(|_| Error::<T>::RecoverySettlementFailed)?;
		if residual.is_zero() {
			return Ok(residual);
		}

		let account = T::InsuranceFundAccount::convert(stable_id.clone());
		let preservation =
			debit_preservation::<T::StableAssets, _>(stable_id.clone(), &account, residual);
		let credit = <T::StableAssets as FungiblesBalanced<_>>::withdraw(
			stable_id.clone(),
			&account,
			residual,
			Precision::Exact,
			preservation,
			Fortitude::Polite,
		)
		.map_err(|_| Error::<T>::InsuranceFundBurnFailed)?;
		let surplus = T::Vaults::heal(collateral_id, credit);
		if !surplus.peek().is_zero() {
			log::error!(
				target: crate::LOG_TARGET,
				"insurance heal left surplus settling residual {residual:?}"
			);
			drop(surplus);
			return Err(Error::<T>::InsuranceFundBurnFailed.into());
		}
		drop(surplus);
		Ok(residual)
	}

	fn final_recovery_head(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Option<T::AccountId> {
		let (owner, status) = T::Vaults::next_redemption_target(collateral_id, stable_id, None)?;
		status.is_final_recovery().then_some(owner)
	}

	fn offset_inputs(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<(RedemptionConfigOf<T>, FixedU128), DispatchError> {
		let config =
			RedemptionConfigs::<T>::get(stable_id).ok_or(Error::<T>::StablecoinNotRegistered)?;
		let price =
			T::Oracle::provide_price(collateral_id).map_err(|_| Error::<T>::OracleUnavailable)?;
		ensure!(!price.is_zero(), Error::<T>::OracleUnavailable);
		Ok((config, price))
	}

	fn offset_decision(plan: Option<RecoveryPlan<BalanceOf<T>>>) -> OffsetDecision<BalanceOf<T>> {
		match plan {
			None => OffsetDecision::NoTarget,
			Some(RecoveryPlan::BelowPar { .. }) => OffsetDecision::BelowPar,
			Some(RecoveryPlan::AbovePar { debt, .. }) if debt.is_zero() => OffsetDecision::NoTarget,
			Some(RecoveryPlan::AbovePar { debt, collateral, .. }) => {
				OffsetDecision::Available { debt, collateral }
			},
		}
	}

	/// Quote how much debt a Stability Pool may cancel against the recovery head.
	pub fn preview_recovery_offset(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		max_debt_to_cancel: BalanceOf<T>,
	) -> Result<RecoveryOffsetQuote<BalanceOf<T>>, DispatchError> {
		let Some(owner) = Self::final_recovery_head(collateral_id, stable_id) else {
			return Ok(RecoveryOffsetQuote::NoTarget);
		};
		let (config, price) = Self::offset_inputs(collateral_id, stable_id)?;
		let snapshot = T::Vaults::project_redemption_snapshot(collateral_id, stable_id, &owner)?;
		Ok(
			match Self::offset_decision(Self::price_recovery(
				stable_id,
				&snapshot,
				price,
				max_debt_to_cancel,
				&config,
			)) {
				OffsetDecision::NoTarget => RecoveryOffsetQuote::NoTarget,
				OffsetDecision::BelowPar => RecoveryOffsetQuote::BelowPar,
				OffsetDecision::Available { debt, .. } => RecoveryOffsetQuote::Available { debt },
			},
		)
	}
}

impl<T: Config> RecoveryOffsetInterface for Pallet<T> {
	type CollateralId = CollateralIdOf<T>;
	type AccountId = T::AccountId;
	type Balance = BalanceOf<T>;
	type Credit = StableCreditOf<T>;

	fn execute_recovery_offset(
		collateral_id: &CollateralIdOf<T>,
		payment: StableCreditOf<T>,
		collateral_recipient: &T::AccountId,
	) -> Result<(RecoveryOffsetResult<BalanceOf<T>>, StableCreditOf<T>), DispatchError> {
		// The payment's own asset names the market: a coin mismatch is
		// unrepresentable rather than an error.
		let stable_id = &payment.asset();
		let Some(owner) = Self::final_recovery_head(collateral_id, stable_id) else {
			return Ok((RecoveryOffsetResult::NoTarget, payment));
		};
		let (config, price) = Self::offset_inputs(collateral_id, stable_id)?;
		let budget = payment.peek();

		with_transaction(|| {
			let mut payment = payment;
			let step = T::Vaults::redeem_step(
				collateral_id,
				stable_id,
				&owner,
				collateral_recipient,
				|snapshot| match Self::offset_decision(Self::price_recovery(
					stable_id, &snapshot, price, budget, &config,
				)) {
					OffsetDecision::NoTarget => Ok((None, RecoveryOffsetResult::NoTarget)),
					OffsetDecision::BelowPar => Ok((None, RecoveryOffsetResult::BelowPar)),
					OffsetDecision::Available { debt, collateral } => {
						let debt_payment = payment.extract(debt);
						debug_assert_eq!(debt_payment.peek(), debt);
						Ok((
							Some(RedemptionSettlement {
								debt_payment,
								collateral_to_recipient: collateral,
							}),
							RecoveryOffsetResult::Applied { collateral_out: collateral },
						))
					},
				},
			);

			match step {
				Err(error) => TransactionOutcome::Rollback(Err(error)),
				Ok(result @ RecoveryOffsetResult::Applied { .. }) => {
					TransactionOutcome::Commit(Ok((result, payment)))
				},
				Ok(result) => TransactionOutcome::Rollback(Ok((result, payment))),
			}
		})
	}
}
