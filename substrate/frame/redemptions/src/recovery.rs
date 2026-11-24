//! Final-recovery pricing, Insurance Fund cover, and Stability Pool offsets.

use crate::{
	pallet::{
		BalanceOf, CollateralIdOf, Config, Error, Pallet, RedemptionConfigOf, SnapshotOf,
		StableCreditOf, StableIdOf, Stablecoins,
	},
	types::RecoveryRegime,
};
use frame::{
	deps::sp_runtime::{
		traits::{Convert, Zero},
		FixedU128,
	},
	prelude::*,
	traits::DefensiveSaturating,
};
use stablecoin_primitives::{
	debit_preservation, recovery_pricing, reducible_debit, CollateralRatio,
	RecoveryOffsetInterface, RecoveryOffsetResult, RedemptionSettlement, VaultInterface,
};

/// One authoritative price for a `FinalRecovery` head.
///
/// Redemption execution and Stability Pool offsets both consume this plan.
pub(crate) struct RecoveryPlan<Balance> {
	/// Debt the payer cancels.
	pub(crate) debt: Balance,
	/// Collateral paid for that debt.
	pub(crate) collateral: Balance,
	pub(crate) regime: RecoveryRegime,
	/// Insurance Fund payment, nonzero only for a full below-par settlement: a partial one
	/// leaves the vault in the FIFO and must not draw from the fund.
	pub(crate) insurance_cover: Balance,
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
	) -> Result<RecoveryPlan<BalanceOf<T>>, Error<T>> {
		match Self::price_at_or_above_par(snapshot, price, budget, config)? {
			Some(plan) => Ok(plan),
			None => Self::price_below_par(stable_id, snapshot, price, budget)
				.ok_or(Error::<T>::NoRedeemableVault),
		}
	}

	/// Price a head at or above par. `None` marks a below-par head, which needs the Insurance
	/// Fund's balance to price and which only a redemption settles.
	fn price_at_or_above_par(
		snapshot: &SnapshotOf<T>,
		price: FixedU128,
		budget: BalanceOf<T>,
		config: &RedemptionConfigOf<T>,
	) -> Result<Option<RecoveryPlan<BalanceOf<T>>>, Error<T>> {
		ensure!(
			snapshot.status.is_final_recovery(),
			Self::unexpected("both callers price only a head looked up as FinalRecovery")
		);

		let ratio =
			match stablecoin_primitives::collateralization_ratio(&snapshot.position(), price) {
				Ok(CollateralRatio::Ratio(ratio)) => ratio,
				// A head without debt has nothing to settle, and an overflowing ratio gets no plan.
				Ok(CollateralRatio::DebtFree) | Err(_) => {
					return Err(Error::<T>::NoRedeemableVault)
				},
			};
		if ratio < FixedU128::one() {
			return Ok(None);
		}
		// The bonus keeps settlement attractive past MCR, so redeemers push the head up to ICR
		// and it exits with the buffer a borrower opens with rather than one tick above the
		// liquidation line. Above ICR only the exit remains, unless a sub-minimum debt could be
		// kept from exiting by an occupied Dormant slot; that escape hatch pays face value.
		let past_settlement = ratio > snapshot.initial_collateralization_ratio;
		if past_settlement {
			ensure!(snapshot.debt < snapshot.minimum_debt, Error::<T>::FinalRecoveryExitRequired);
		}
		let bonus = if past_settlement {
			FixedU128::zero()
		} else {
			recovery_pricing::recovery_bonus(
				ratio,
				config.final_recovery_bonus_buffer,
				snapshot.redistribution_penalty,
			)
		};
		let debt = snapshot.size_within(budget);
		let collateral = recovery_pricing::recovery_bonus_collateral_out(debt, bonus, price)
			.ok_or(Error::<T>::NoRedeemableVault)?
			.min(snapshot.collateral);
		Ok(Some(RecoveryPlan {
			debt,
			collateral,
			regime: RecoveryRegime::RecoveryBonus,
			insurance_cover: BalanceOf::<T>::zero(),
		}))
	}

	/// Returns `None` if a calculation overflows.
	fn price_below_par(
		stable_id: &StableIdOf<T>,
		snapshot: &SnapshotOf<T>,
		price: FixedU128,
		budget: BalanceOf<T>,
	) -> Option<RecoveryPlan<BalanceOf<T>>> {
		// The collateral value rounds up, so the fund never covers more than the true shortfall
		// and the redeemer never pays less than the collateral is worth. It stays within the
		// below-par split: the regime check floors, so `floor(C) < debt` and `ceil(C) <= debt`.
		let collateral_value = recovery_pricing::collateral_value_ceil(snapshot.collateral, price)?;
		let shortfall = snapshot.debt.defensive_saturating_sub(collateral_value);
		let cover = Self::insurance_fund_cover(stable_id, shortfall);
		let split = recovery_pricing::insurance_adjusted(snapshot.debt, collateral_value, cover)?;
		if budget >= split.market_cancel_debt {
			// The market debt buys the whole collateral by definition and the cover cancels the
			// rest, so the settlement leaves neither debt nor collateral behind.
			return Some(RecoveryPlan {
				debt: split.market_cancel_debt,
				collateral: snapshot.collateral,
				regime: RecoveryRegime::InsuranceAdjusted,
				insurance_cover: split.effective_cover,
			});
		}

		// A partial settlement draws no Insurance Fund cover but prices collateral on the same
		// split.
		let debt = budget;
		let collateral = recovery_pricing::recovery_collateral_out(
			debt,
			snapshot.collateral,
			split.market_cancel_debt,
		)?;
		Some(RecoveryPlan {
			debt,
			collateral,
			regime: RecoveryRegime::InsuranceAdjusted,
			insurance_cover: BalanceOf::<T>::zero(),
		})
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
		defensive_assert!(!cover.is_zero());
		let account = T::InsuranceFundAccount::convert(stable_id.clone());
		let preservation =
			debit_preservation::<T::StableAssets, _>(stable_id.clone(), &account, cover);
		// The cover was capped at the fund's reducible balance when priced, in
		// this same dispatch; a failure means the payer drained the fund
		// mid-redemption (it is the fund itself) and must abort.
		let credit = Self::withdraw_stable(stable_id, &account, cover, preservation)
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

	/// Locate and price the recovery head for an offset, as a pure read.
	fn offset_decision(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		max_debt_to_cancel: BalanceOf<T>,
	) -> Result<OffsetDecision<T::AccountId, BalanceOf<T>>, DispatchError> {
		let Some(owner) = Self::final_recovery_head(collateral_id, stable_id) else {
			return Ok(OffsetDecision::NoTarget);
		};
		let config = Stablecoins::<T>::get(stable_id)
			.ok_or(Error::<T>::StablecoinNotRegistered)?
			.config;
		let price = Self::collateral_price(collateral_id)?;
		let snapshot = T::Vaults::project_redemption_snapshot(collateral_id, stable_id, &owner)?;
		// An offset never draws Insurance Fund cover, so a below-par head is refused before the
		// fund's balance is read.
		let plan = Self::price_at_or_above_par(&snapshot, price, max_debt_to_cancel, &config);
		Ok(match plan {
			// An unpriceable or exitable head is no offset target; the pool sees an empty FIFO.
			Err(Error::<T>::NoRedeemableVault | Error::<T>::FinalRecoveryExitRequired) => {
				OffsetDecision::NoTarget
			},
			Err(error) => return Err(error.into()),
			Ok(None) => OffsetDecision::BelowPar,
			Ok(Some(plan)) if plan.debt.is_zero() => OffsetDecision::NoTarget,
			Ok(Some(plan)) => {
				OffsetDecision::Available { owner, debt: plan.debt, collateral: plan.collateral }
			},
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
		mut payment: StableCreditOf<T>,
		collateral_recipient: &T::AccountId,
	) -> Result<(RecoveryOffsetResult<BalanceOf<T>>, StableCreditOf<T>), DispatchError> {
		// The payment's own asset names the branch: a coin mismatch is
		// unrepresentable rather than an error. The decision is a pure read,
		// so non-applied outcomes return the payment without touching storage.
		let stable_id = payment.asset();
		match Self::offset_decision(collateral_id, &stable_id, payment.peek())? {
			OffsetDecision::NoTarget => Ok((RecoveryOffsetResult::NoTarget, payment)),
			OffsetDecision::BelowPar => Ok((RecoveryOffsetResult::BelowPar, payment)),
			OffsetDecision::Available { owner, debt, collateral } => {
				let debt_payment = Self::exact_credit(payment.extract(debt), debt)?;
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
