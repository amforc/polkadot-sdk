//! Vault status, index, settlement, and removal transitions.

use super::VaultOp;
use crate::{
	pallet::{BalanceOf, Config, Error, Event, HoldReason, Pallet},
	recovery,
	types::{DebtCollateral, LiquidationSnapshot, Vault, VaultStatus},
};
use frame::{
	prelude::*,
	traits::{
		fungibles::MutateHold as FungiblesMutateHold,
		tokens::{Fortitude, Precision, Restriction},
	},
};
use linked_list_interface::{Position as ListPosition, SortedListInterface};
use pusd_primitives::{collateralization_ratio, RedemptionStepSnapshot};

impl<T: Config> VaultOp<T> {
	/// Attributes terminal interest and returns validated liquidation inputs.
	pub(crate) fn prepare_liquidation(
		&mut self,
	) -> Result<LiquidationSnapshot<BalanceOf<T>>, DispatchError> {
		self.finalize_terminal_interest()?;
		let price = self.ctx.price()?;
		ensure!(!self.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);
		let cr = collateralization_ratio(&self.vault.position(), price)
			.ok_or(Error::<T>::VaultNotLiquidatable)?;
		ensure!(
			cr < self.ctx.config.minimum_collateralization_ratio,
			Error::<T>::VaultNotLiquidatable
		);
		ensure!(!self.is_only_stake_bearer(), Error::<T>::LastVaultCannotBeLiquidated);
		Ok(LiquidationSnapshot {
			debt: self.vault.debt.total(),
			redistribution_penalty: self.ctx.config.redistribution_penalty,
		})
	}

	/// Returns the current values needed for one redemption step.
	pub(crate) fn redemption_snapshot(&self) -> RedemptionStepSnapshot<BalanceOf<T>> {
		self.vault
			.redemption_snapshot(self.status, self.ctx.config.redistribution_penalty)
	}

	/// Moves a dormant vault back to the rate list.
	pub(crate) fn activate(&mut self, hint: ListPosition<T::AccountId>) -> DispatchResult {
		ensure!(self.status.is_dormant(), Error::<T>::InvalidVaultStatus);
		ensure!(
			self.vault.debt.total() >= self.ctx.config.minimum_debt,
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
		self.ctx.state.release_dormant_target(&self.owner);
		self.set_status(VaultStatus::Active)
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
		Pallet::<T>::ensure_below_mcr(&self.vault.position(), price, &self.ctx.config)?;
		ensure!(self.is_only_stake_bearer(), Error::<T>::NotLastEligibleVault);
		T::VaultLists::remove(&self.rate_list(), &self.owner)
			.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
		recovery::append::<T>(self.collateral_id(), self.stable_id(), self.owner.clone())?;
		self.set_status(VaultStatus::FinalRecovery)
	}

	/// Removes a safe vault from final recovery.
	pub(crate) fn exit_final_recovery(
		&mut self,
		hint: ListPosition<T::AccountId>,
	) -> DispatchResult {
		let price = self.ctx.price()?;
		ensure!(self.status.is_final_recovery(), Error::<T>::InvalidVaultStatus);
		Pallet::<T>::ensure_at_or_above_mcr(&self.vault.position(), price, &self.ctx.config)?;
		let total_debt = self.vault.debt.total();
		let new_status = if total_debt >= self.ctx.config.minimum_debt {
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
			!self.ctx.state.try_park_dormant_target(self.owner.clone())
		{
			return Err(Error::<T>::DormantTargetOccupied.into());
		}
		self.set_status(new_status)
	}

	/// Updates the vault status after its debt falls.
	pub(crate) fn reconcile_after_debt_reduction(&mut self) -> DispatchResult {
		let total = self.vault.debt.total();
		let below_minimum = total < self.ctx.config.minimum_debt;
		match self.status {
			VaultStatus::Active if below_minimum => {
				T::VaultLists::remove(&self.rate_list(), &self.owner)
					.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
				if total.is_zero() {
					self.ctx.state.release_dormant_target(&self.owner);
				} else if !self.ctx.state.try_park_dormant_target(self.owner.clone()) {
					return Err(Error::<T>::DormantTargetOccupied.into());
				}
				self.set_status(VaultStatus::Dormant)?;
			},
			VaultStatus::Dormant if total.is_zero() => {
				self.ctx.state.release_dormant_target(&self.owner);
				self.sync_stake()?;
			},
			VaultStatus::Dormant if below_minimum => {
				ensure!(
					self.ctx.state.try_park_dormant_target(self.owner.clone()),
					Error::<T>::DormantTargetOccupied
				);
			},
			VaultStatus::FinalRecovery if total.is_zero() => {
				recovery::remove::<T>(self.collateral_id(), self.stable_id(), &self.owner)?;
				self.ctx.state.release_dormant_target(&self.owner);
				self.set_status(VaultStatus::Dormant)?;
			},
			_ => {},
		}
		Ok(())
	}

	fn remove_from_lifecycle(&self) -> DispatchResult {
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
		self.ctx.state.stakes.total == self.vault.redistribution_stake
	}

	/// Commits a liquidation and records the residual for redistribution.
	pub(crate) fn finish_liquidation(
		mut self,
		redistribution: DebtCollateral<BalanceOf<T>>,
	) -> DispatchResult {
		ensure!(
			redistribution.debt <= self.vault.debt.total(),
			Error::<T>::InvalidLiquidationSettlement
		);
		// A removed liability must not retain fractional interest.
		ensure!(self.vault.interest_remainder == 0, DispatchError::Corruption);
		let collateral_out = self
			.vault
			.collateral
			.checked_sub(&redistribution.collateral)
			.ok_or(Error::<T>::InvalidLiquidationSettlement)?;

		self.remove_from_lifecycle()?;
		self.ctx.state.replace_vault(Some(&self.vault), None)?;
		self.ctx.state.vault_count =
			self.ctx.state.vault_count.checked_sub(1).ok_or(DispatchError::Corruption)?;
		self.ctx.state.release_dormant_target(&self.owner);
		let total_collateral = self
			.ctx
			.state
			.total_collateral
			.checked_sub(&collateral_out)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.ctx.state.total_collateral = total_collateral;
		if !redistribution.debt.is_zero() || !redistribution.collateral.is_zero() {
			self.ctx
				.state
				.record_redistribution(redistribution, self.ctx.now)
				.ok_or(Error::<T>::RedistributionWouldOverflow)?;
		}
		self.persist(true)
	}

	/// Closes a debt-free vault and commits its collateral release.
	pub(crate) fn finish_close(mut self, recipient: &T::AccountId) -> DispatchResult {
		ensure!(self.vault.debt.total().is_zero(), Error::<T>::DebtOutstanding);
		// A debt-free vault must not retain fractional interest.
		ensure!(self.vault.interest_remainder == 0, DispatchError::Corruption);
		let collateral = self.vault.collateral;
		self.remove_from_lifecycle()?;
		self.ctx.state.replace_vault(Some(&self.vault), None)?;
		self.ctx.state.vault_count =
			self.ctx.state.vault_count.checked_sub(1).ok_or(DispatchError::Corruption)?;
		self.ctx.state.release_dormant_target(&self.owner);
		let total_collateral = self
			.ctx
			.state
			.total_collateral
			.checked_sub(&collateral)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.ctx.state.total_collateral = total_collateral;
		let branch_empties = self.ctx.state.is_empty_of_liability();
		if branch_empties {
			ensure!(
				self.ctx.state.debt.minted_interest.is_zero() &&
					self.ctx.state.debt.interest_ledger_settled(),
				DispatchError::Corruption
			);
			// The aggregate remainder has no liability owner and must not issue another unit.
			self.ctx.state.debt.aggregate_interest_remainder = 0;
		}
		if !collateral.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				self.collateral_id().clone(),
				&HoldReason::VaultCollateral.into(),
				&self.owner,
				recipient,
				collateral,
				Precision::Exact,
				Restriction::Free,
				Fortitude::Polite,
			)?;
		}

		Pallet::<T>::deposit_event(Event::VaultClosed {
			collateral_id: self.collateral_id().clone(),
			stable_id: self.stable_id().clone(),
			owner: self.owner.clone(),
			recipient: recipient.clone(),
			collateral,
		});
		// An empty branch has no collateralization ratio to protect.
		if !branch_empties {
			self.ensure_checked_commit()?;
		}
		self.persist(true)
	}

	fn set_status(&mut self, new_status: VaultStatus) -> DispatchResult {
		let old_status = self.status;
		if old_status == new_status {
			return Ok(());
		}
		self.status = new_status;
		self.sync_stake()?;
		Pallet::<T>::deposit_event(Event::VaultStatusChanged {
			collateral_id: self.ctx.collateral_id.clone(),
			stable_id: self.ctx.stable_id.clone(),
			owner: self.owner.clone(),
			old_status,
			new_status,
		});
		Ok(())
	}

	/// Recomputes redistribution stake from the latest liquidation snapshot.
	///
	/// Final-recovery vaults and debt-free vaults must not receive new liability.
	pub(super) fn sync_stake(&mut self) -> DispatchResult {
		let before = self.vault.clone();
		self.sync_stake_from(before)
	}

	/// Recomputes stake and replaces the specified accounting contribution.
	pub(super) fn sync_stake_from(&mut self, before: Vault<BalanceOf<T>>) -> DispatchResult {
		let target = if self.status.is_final_recovery() || self.vault.debt.total().is_zero() {
			BalanceOf::<T>::zero()
		} else {
			self.ctx
				.state
				.stake_for(self.vault.collateral)
				.ok_or(Error::<T>::ArithmeticOverflow)?
		};
		if before != self.vault || self.vault.redistribution_stake != target {
			self.vault.redistribution_stake = target;
			self.ctx.state.replace_vault(Some(&before), Some(&self.vault))?;
		}
		Ok(())
	}
}
