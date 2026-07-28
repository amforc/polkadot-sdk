//! Vault index, status, and removal transitions.

use super::{CloseOutcome, ResidualSettlement, VaultOp};
use crate::{
	pallet::{BalanceOf, Config, Error, Event, Pallet},
	recovery,
	types::{Position, VaultStatus},
};
use frame::prelude::*;
use linked_list_interface::{Position as ListPosition, SortedListInterface};
use pusd_primitives::{collateralization_ratio, LiquidationSnapshot, RedemptionStepSnapshot};

impl<T: Config> VaultOp<T> {
	/// Checks whether the vault may be liquidated.
	pub(crate) fn ensure_liquidatable(&self) -> DispatchResult {
		let price = self.ctx.price()?;
		ensure!(!self.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);
		let cr = collateralization_ratio(&self.vault.position(), price)
			.ok_or(Error::<T>::VaultNotLiquidatable)?;
		ensure!(
			cr < self.ctx.branch.config.minimum_collateralization_ratio,
			Error::<T>::VaultNotLiquidatable
		);
		ensure!(!self.is_only_stake_bearer(), Error::<T>::LastVaultCannotBeLiquidated);
		Ok(())
	}

	/// Returns the current values needed for one redemption step.
	pub(crate) fn redemption_snapshot(&self) -> RedemptionStepSnapshot<BalanceOf<T>> {
		RedemptionStepSnapshot {
			status: self.status,
			debt: self.vault.debt.total(),
			collateral: self.vault.collateral,
			redistribution_penalty: self.ctx.branch.config.redistribution_penalty,
		}
	}

	/// Returns the values needed to settle this liquidation.
	pub(crate) fn liquidation_snapshot(&self) -> LiquidationSnapshot<BalanceOf<T>> {
		LiquidationSnapshot {
			debt: self.vault.debt.total(),
			redistribution_penalty: self.ctx.branch.config.redistribution_penalty,
		}
	}

	/// Moves a dormant vault back to the rate list.
	pub(crate) fn activate(&mut self, hint: ListPosition<T::AccountId>) -> DispatchResult {
		ensure!(self.status.is_dormant(), Error::<T>::InvalidVaultStatus);
		ensure!(
			self.vault.debt.total() >= self.ctx.branch.config.minimum_debt,
			Error::<T>::DebtBelowMinimum
		);
		self.activate_dormant_unchecked(hint)
	}

	pub(super) fn activate_dormant_unchecked(
		&mut self,
		hint: ListPosition<T::AccountId>,
	) -> DispatchResult {
		debug_assert!(self.status.is_dormant());
		T::VaultLists::insert(self.rate_list(), self.owner.clone(), self.vault.annual_rate, hint)
			.map_err(Pallet::<T>::map_error)?;
		self.ctx.branch.state.release_dormant_target(&self.owner);
		self.set_status(VaultStatus::Active);
		Ok(())
	}

	/// Moves an active vault to its new rate-list position.
	pub(super) fn reindex(&self, hint: ListPosition<T::AccountId>) -> DispatchResult {
		ensure!(self.status.is_active(), Error::<T>::InvalidVaultStatus);
		T::VaultLists::re_insert(self.rate_list(), self.owner.clone(), self.vault.annual_rate, hint)
			.map(|_| ())
			.map_err(|e| Pallet::<T>::map_error(e).into())
	}

	/// Moves an unsafe last eligible vault into final recovery.
	pub(crate) fn enter_final_recovery(&mut self) -> DispatchResult {
		let price = self.ctx.price()?;
		ensure!(self.status.is_active(), Error::<T>::InvalidVaultStatus);
		Pallet::<T>::ensure_below_mcr(&self.vault.position(), price, &self.ctx.branch.config)?;
		ensure!(self.is_only_stake_bearer(), Error::<T>::NotLastEligibleVault);
		T::VaultLists::remove(&self.rate_list(), &self.owner)
			.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
		recovery::append::<T>(self.collateral_id(), self.stable_id(), self.owner.clone())?;
		self.set_status(VaultStatus::FinalRecovery);
		Ok(())
	}

	/// Removes a safe vault from final recovery.
	pub(crate) fn exit_final_recovery(
		&mut self,
		hint: ListPosition<T::AccountId>,
	) -> DispatchResult {
		let price = self.ctx.price()?;
		ensure!(self.status.is_final_recovery(), Error::<T>::InvalidVaultStatus);
		Pallet::<T>::ensure_at_or_above_mcr(
			&self.vault.position(),
			price,
			&self.ctx.branch.config,
		)?;
		let total_debt = self.vault.debt.total();
		let new_status = if total_debt >= self.ctx.branch.config.minimum_debt {
			VaultStatus::Active
		} else {
			VaultStatus::Dormant
		};
		recovery::remove::<T>(self.collateral_id(), self.stable_id(), &self.owner)?;
		if new_status.is_active() {
			T::VaultLists::insert(
				self.rate_list(),
				self.owner.clone(),
				self.vault.annual_rate,
				hint,
			)
			.map_err(Pallet::<T>::map_error)?;
		} else if !self.vault.debt.total().is_zero() &&
			!self.ctx.branch.state.try_park_dormant_target(self.owner.clone())
		{
			return Err(Error::<T>::DormantTargetOccupied.into());
		}
		self.set_status(new_status);
		Ok(())
	}

	/// Updates the vault status after its debt falls.
	pub(crate) fn reconcile_after_debt_reduction(&mut self) -> DispatchResult {
		let total = self.vault.debt.total();
		let below_minimum = total < self.ctx.branch.config.minimum_debt;
		match self.status {
			VaultStatus::Active if below_minimum => {
				T::VaultLists::remove(&self.rate_list(), &self.owner)
					.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
				if total.is_zero() {
					self.ctx.branch.state.release_dormant_target(&self.owner);
				} else if !self.ctx.branch.state.try_park_dormant_target(self.owner.clone()) {
					return Err(Error::<T>::DormantTargetOccupied.into());
				}
				self.set_status(VaultStatus::Dormant);
			},
			VaultStatus::Dormant if total.is_zero() => {
				self.ctx.branch.state.release_dormant_target(&self.owner);
			},
			VaultStatus::Dormant if below_minimum => {
				ensure!(
					self.ctx.branch.state.try_park_dormant_target(self.owner.clone()),
					Error::<T>::DormantTargetOccupied
				);
			},
			VaultStatus::FinalRecovery if total.is_zero() => {
				recovery::remove::<T>(self.collateral_id(), self.stable_id(), &self.owner)?;
				self.ctx.branch.state.release_dormant_target(&self.owner);
				self.set_status(VaultStatus::Dormant);
			},
			_ => {},
		}
		Ok(())
	}

	fn remove_from_lifecycle(&mut self) -> DispatchResult {
		match self.status {
			VaultStatus::Active => T::VaultLists::remove(&self.rate_list(), &self.owner)
				.map_err(|_| Error::<T>::RateIndexInvariantBroken)?,
			VaultStatus::FinalRecovery => {
				recovery::remove::<T>(self.collateral_id(), self.stable_id(), &self.owner)?;
			},
			VaultStatus::Dormant => {},
		}
		Ok(())
	}

	fn is_only_stake_bearer(&self) -> bool {
		self.ctx.branch.state.stakes.total == self.vault.redistribution_stake
	}

	/// Removes a liquidated vault and records what its liquidation redistributes.
	pub(crate) fn apply_liquidation(
		&mut self,
		redistribution: Position<BalanceOf<T>>,
	) -> DispatchResult {
		ensure!(
			redistribution.debt <= self.vault.debt.total(),
			Error::<T>::InvalidLiquidationSettlement
		);
		let collateral_out = self
			.vault
			.collateral
			.checked_sub(&redistribution.collateral)
			.ok_or(Error::<T>::InvalidLiquidationSettlement)?;

		self.remove_from_lifecycle()?;
		self.ctx.branch.state.detach_vault(&self.vault);
		self.ctx.branch.state.release_dormant_target(&self.owner);
		let total_collateral = self
			.ctx
			.branch
			.state
			.total_collateral
			.checked_sub(&collateral_out)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.ctx.branch.state.total_collateral = total_collateral;
		if !redistribution.debt.is_zero() || !redistribution.collateral.is_zero() {
			self.ctx
				.branch
				.state
				.record_redistribution(redistribution, self.ctx.now)
				.ok_or(Error::<T>::RedistributionWouldOverflow)?;
		}
		self.remove_on_commit = true;
		Ok(())
	}

	/// Detaches a debt-free vault for closing.
	pub(crate) fn detach_for_close(&mut self) -> Result<CloseOutcome<BalanceOf<T>>, DispatchError> {
		ensure!(self.vault.debt.total().is_zero(), Error::<T>::DebtOutstanding);
		let collateral = self.vault.collateral;
		self.remove_from_lifecycle()?;
		self.ctx.branch.state.detach_vault(&self.vault);
		self.ctx.branch.state.release_dormant_target(&self.owner);
		let total_collateral = self
			.ctx
			.branch
			.state
			.total_collateral
			.checked_sub(&collateral)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.ctx.branch.state.total_collateral = total_collateral;
		let branch_empties = self.ctx.branch.state.is_empty_of_liability();
		let orphan_debt = if branch_empties {
			self.ctx.branch.state.sweep_orphan_debt()
		} else {
			BalanceOf::<T>::zero()
		};
		self.remove_on_commit = true;
		Ok(CloseOutcome { collateral, branch_empties, orphan_debt })
	}

	/// Removes a final-recovery vault and moves its remaining debt to bad debt.
	pub(crate) fn settle_recovery_residual(
		&mut self,
	) -> Result<ResidualSettlement<BalanceOf<T>>, DispatchError> {
		ensure!(self.status.is_final_recovery(), Error::<T>::InvalidVaultStatus);
		let residual_debt = self.vault.debt.total();
		let collateral_dust = self.vault.collateral;
		self.remove_from_lifecycle()?;
		self.ctx.branch.state.detach_vault(&self.vault);
		self.ctx.branch.state.record_bad_debt(residual_debt);
		let total_collateral = self
			.ctx
			.branch
			.state
			.total_collateral
			.checked_sub(&collateral_dust)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.ctx.branch.state.total_collateral = total_collateral;
		let swept_orphan_debt = if self.ctx.branch.state.is_empty_of_liability() {
			self.ctx.branch.state.sweep_orphan_debt()
		} else {
			BalanceOf::<T>::zero()
		};
		self.remove_on_commit = true;
		Ok(ResidualSettlement { residual_debt, collateral_dust, swept_orphan_debt })
	}

	fn set_status(&mut self, new_status: VaultStatus) {
		let old_status = self.status;
		if old_status == new_status {
			return;
		}
		self.status = new_status;
		self.sync_stake();
		Pallet::<T>::deposit_event(Event::VaultStatusChanged {
			collateral_id: self.ctx.collateral_id.clone(),
			stable_id: self.ctx.stable_id.clone(),
			owner: self.owner.clone(),
			old_status,
			new_status,
		});
	}

	/// Keeps redistribution stake equal to collateral, except during final recovery.
	pub(super) fn sync_stake(&mut self) {
		let target = if self.status.is_final_recovery() {
			BalanceOf::<T>::zero()
		} else {
			self.vault.collateral
		};
		if self.vault.redistribution_stake != target {
			self.ctx.branch.state.set_vault_stake(&mut self.vault, target);
		}
	}
}
