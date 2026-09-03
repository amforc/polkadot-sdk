//! Final-recovery pricing, Insurance Fund cover, and Stability Pool offsets.

use crate::{
	pallet::{
		BalanceOf, CollateralIdOf, Config, Error, Pallet, RedemptionConfigOf, RedemptionConfigs,
		SnapshotOf, StableCreditOf, StableIdOf,
	},
	types::{RecoveryOffsetQuote, RecoveryRegime},
};
use frame::{
	deps::sp_runtime::{
		traits::{Convert, Saturating, Zero},
		FixedPointNumber, FixedU128,
	},
	prelude::*,
	traits::{
		fungibles::Balanced as FungiblesBalanced,
		tokens::{Fortitude, Precision},
	},
};
use pusd_primitives::{
	debit_preservation, recovery_pricing, reducible_debit, CollateralRatio, ProvidePrice,
	RecoveryOffsetInterface, RecoveryOffsetResult, RedemptionSettlement, VaultInterface,
};

/// One authoritative price for a `FinalRecovery` head.
///
/// Redemption execution, quotes, and Stability Pool offsets all consume this
/// plan. Re-price with a smaller budget when the payer cannot fund it in full.
pub(crate) enum RecoveryPlan<Balance> {
	AbovePar {
		debt: Balance,
		collateral: Balance,
	},
	BelowPar {
		debt: Balance,
		collateral: Balance,
		/// Insurance Fund payment for a full settlement.
		///
		/// A partial settlement must not draw from the fund.
		insurance_cover: Balance,
	},
}

impl<Balance: Copy + Zero> RecoveryPlan<Balance> {
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

	/// Returns the Insurance Fund payment for a full settlement.
	///
	/// Returns zero for a partial settlement because the vault remains in the FIFO.
	pub(crate) fn insurance_cover(&self) -> Balance {
		match self {
			Self::AbovePar { .. } => Balance::zero(),
			Self::BelowPar { insurance_cover, .. } => *insurance_cover,
		}
	}
}

enum OffsetDecision<AccountId, Balance> {
	NoTarget,
	BelowPar,
	Available { owner: AccountId, debt: Balance, collateral: Balance },
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

		let ratio = match pusd_primitives::collateralization_ratio(&snapshot.position(), price) {
			Ok(CollateralRatio::Ratio(ratio)) => ratio,
			// A head without debt has nothing to settle, and an overflowing ratio gets no plan.
			Ok(CollateralRatio::DebtFree) | Err(_) => return None,
		};
		if ratio >= FixedU128::one() {
			let bonus = recovery_pricing::recovery_bonus(
				ratio,
				config.final_recovery_bonus_buffer,
				snapshot.redistribution_penalty,
			);
			let debt = snapshot.size_within(budget);
			let collateral = recovery_pricing::recovery_bonus_collateral_out(debt, bonus, price)?
				.min(snapshot.collateral);
			return Some(RecoveryPlan::AbovePar { debt, collateral });
		}

		// A partial settlement excludes the terminal charge and Insurance Fund cover.
		let collateral_value = price.saturating_mul_int(snapshot.collateral);
		let full_payoff = snapshot.full_payoff();
		let full_shortfall = full_payoff.saturating_sub(collateral_value);
		let full_cover = Self::insurance_fund_cover(stable_id, full_shortfall);
		let full_split =
			recovery_pricing::insurance_adjusted(full_payoff, collateral_value, full_cover)?;
		let (split, debt, insurance_cover) = if budget >= full_split.market_cancel_debt {
			(full_split, full_split.market_cancel_debt, full_split.effective_cover)
		} else if snapshot.terminal_interest_charge.is_zero() {
			(full_split, full_split.market_cancel_debt.min(budget), BalanceOf::<T>::zero())
		} else {
			let base_shortfall = snapshot.debt.saturating_sub(collateral_value);
			let base_cover = Self::insurance_fund_cover(stable_id, base_shortfall);
			let split =
				recovery_pricing::insurance_adjusted(snapshot.debt, collateral_value, base_cover)?;
			let debt = snapshot.partial_cap(split.market_cancel_debt).min(budget);
			(split, debt, BalanceOf::<T>::zero())
		};
		let collateral =
			recovery_pricing::recovery_rate_collateral_out(debt, split.recovery_rate, price)?
				.min(snapshot.collateral);
		Some(RecoveryPlan::BelowPar { debt, collateral, insurance_cover })
	}

	fn insurance_fund_cover(stable_id: &StableIdOf<T>, shortfall: BalanceOf<T>) -> BalanceOf<T> {
		reducible_debit::<T::StableAssets, _>(
			stable_id.clone(),
			&T::InsuranceFundAccount::convert(stable_id.clone()),
			shortfall,
		)
		.0
	}

	/// Withdraws cover as a credit that cancels vault debt in the same settlement.
	pub(crate) fn withdraw_insurance_cover(
		stable_id: &StableIdOf<T>,
		cover: BalanceOf<T>,
	) -> Result<StableCreditOf<T>, DispatchError> {
		debug_assert!(!cover.is_zero());
		let account = T::InsuranceFundAccount::convert(stable_id.clone());
		let preservation =
			debit_preservation::<T::StableAssets, _>(stable_id.clone(), &account, cover);
		// The cover was capped at the fund's reducible balance when priced, in
		// this same dispatch; a failure means the payer drained the fund
		// mid-redemption (it is the fund itself) and must abort.
		let credit = <T::StableAssets as FungiblesBalanced<_>>::withdraw(
			stable_id.clone(),
			&account,
			cover,
			Precision::Exact,
			preservation,
			Fortitude::Polite,
		)
		.map_err(|_| Error::<T>::InsuranceFundWithdrawFailed)?;
		Self::exact_credit(credit, cover)
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

	/// Locate and price the recovery head for an offset, as a pure read.
	fn offset_decision(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		max_debt_to_cancel: BalanceOf<T>,
	) -> Result<OffsetDecision<T::AccountId, BalanceOf<T>>, DispatchError> {
		let Some(owner) = Self::final_recovery_head(collateral_id, stable_id) else {
			return Ok(OffsetDecision::NoTarget);
		};
		let (config, price) = Self::offset_inputs(collateral_id, stable_id)?;
		let snapshot = T::Vaults::project_redemption_snapshot(collateral_id, stable_id, &owner)?;
		let plan = Self::price_recovery(stable_id, &snapshot, price, max_debt_to_cancel, &config);
		Ok(match plan {
			None => OffsetDecision::NoTarget,
			Some(RecoveryPlan::BelowPar { .. }) => OffsetDecision::BelowPar,
			Some(RecoveryPlan::AbovePar { debt, .. }) if debt.is_zero() => OffsetDecision::NoTarget,
			Some(RecoveryPlan::AbovePar { debt, collateral }) => {
				OffsetDecision::Available { owner, debt, collateral }
			},
		})
	}

	/// Quote how much debt a Stability Pool may cancel against the recovery head.
	pub fn preview_recovery_offset(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		max_debt_to_cancel: BalanceOf<T>,
	) -> Result<RecoveryOffsetQuote<BalanceOf<T>>, DispatchError> {
		Ok(match Self::offset_decision(collateral_id, stable_id, max_debt_to_cancel)? {
			OffsetDecision::NoTarget => RecoveryOffsetQuote::NoTarget,
			OffsetDecision::BelowPar => RecoveryOffsetQuote::BelowPar,
			OffsetDecision::Available { debt, .. } => RecoveryOffsetQuote::Available { debt },
		})
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
		// unrepresentable rather than an error. The decision is a pure read,
		// so non-applied outcomes return the payment without touching storage.
		let stable_id = payment.asset();
		match Self::offset_decision(collateral_id, &stable_id, payment.peek())? {
			OffsetDecision::NoTarget => Ok((RecoveryOffsetResult::NoTarget, payment)),
			OffsetDecision::BelowPar => Ok((RecoveryOffsetResult::BelowPar, payment)),
			OffsetDecision::Available { owner, debt, collateral } => {
				let mut payment = payment;
				let debt_payment = payment.extract(debt);
				debug_assert_eq!(debt_payment.peek(), debt);
				T::Vaults::redeem_step(
					collateral_id,
					&stable_id,
					&owner,
					collateral_recipient,
					RedemptionSettlement { debt_payment, collateral_to_recipient: collateral },
				)?;
				Ok((RecoveryOffsetResult::Applied { collateral_out: collateral }, payment))
			},
		}
	}
}
