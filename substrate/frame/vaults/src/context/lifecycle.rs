//! Vault status, index, settlement, and removal transitions.

use super::{Commit, VaultOp};
use crate::{
	liquidation::LiquidationSnapshot,
	pallet::{BalanceOf, Config, Error, Event, HoldReason, Pallet},
	recovery,
	types::{DebtCollateral, Vault, VaultStatus},
};
use frame::{
	prelude::*,
	traits::{
		fungibles::MutateHold as FungiblesMutateHold,
		tokens::{Fortitude, Precision, Restriction},
	},
};
use linked_list_interface::{Position as ListPosition, SortedListInterface};
use pusd_primitives::RedemptionStepSnapshot;

impl<T: Config> VaultOp<T> {
	/// Attributes terminal interest and returns validated liquidation inputs.
	pub(crate) fn prepare_liquidation(
		&mut self,
	) -> Result<LiquidationSnapshot<BalanceOf<T>>, DispatchError> {
		self.finalize_terminal_interest()?;
		let price = self.ctx.price()?;
		ensure!(!self.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);
		let cr = self.ctx.collateralization_ratio(&self.vault.position())?;
		ensure!(
			cr < self.ctx.config.minimum_collateralization_ratio,
			Error::<T>::VaultNotLiquidatable
		);
		ensure!(!self.is_only_stake_bearer(), Error::<T>::LastVaultCannotBeLiquidated);
		Ok(LiquidationSnapshot {
			debt: self.vault.debt.total(),
			price,
			config: self.ctx.config.liquidation,
		})
	}

	/// Returns the current values needed for one redemption step.
	pub(crate) fn redemption_snapshot(&self) -> RedemptionStepSnapshot<BalanceOf<T>> {
		self.vault.redemption_snapshot(self.status, &self.ctx.config)
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
		self.index_insert(hint)?;
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
		ensure!(self.status.is_active(), Error::<T>::InvalidVaultStatus);
		self.ctx.ensure_below_mcr(&self.vault.position())?;
		ensure!(self.is_only_stake_bearer(), Error::<T>::NotLastEligibleVault);
		self.index_remove()?;
		recovery::append::<T>(self.collateral_id(), self.stable_id(), self.owner.clone())?;
		self.set_status(VaultStatus::FinalRecovery)
	}

	/// Removes a safe vault from final recovery.
	pub(crate) fn exit_final_recovery(
		&mut self,
		hint: ListPosition<T::AccountId>,
	) -> DispatchResult {
		ensure!(self.status.is_final_recovery(), Error::<T>::InvalidVaultStatus);
		self.ctx.ensure_at_or_above_mcr(&self.vault.position())?;
		let new_status = if self.vault.debt.total() >= self.ctx.config.minimum_debt {
			VaultStatus::Active
		} else {
			VaultStatus::Dormant
		};
		recovery::remove::<T>(self.collateral_id(), self.stable_id(), &self.owner)?;
		if new_status.is_active() {
			self.index_insert(hint)?;
		} else {
			self.sync_dormant_target()?;
		}
		self.set_status(new_status)
	}

	/// Updates the vault status after its debt falls.
	pub(crate) fn reconcile_after_debt_reduction(&mut self) -> DispatchResult {
		let total = self.vault.debt.total();
		let below_minimum = total < self.ctx.config.minimum_debt;
		match self.status {
			VaultStatus::Active if below_minimum => {
				self.index_remove()?;
				self.sync_dormant_target()?;
				self.set_status(VaultStatus::Dormant)?;
			},
			VaultStatus::Dormant if below_minimum => {
				self.sync_dormant_target()?;
				if total.is_zero() {
					self.sync_stake()?;
				}
			},
			VaultStatus::FinalRecovery if total.is_zero() => {
				recovery::remove::<T>(self.collateral_id(), self.stable_id(), &self.owner)?;
				self.sync_dormant_target()?;
				self.set_status(VaultStatus::Dormant)?;
			},
			_ => {},
		}
		Ok(())
	}

	/// Parks a dormant vault that still owes debt as the redemption target and releases a
	/// debt-free one, which has nothing left to redeem.
	fn sync_dormant_target(&mut self) -> DispatchResult {
		if self.vault.debt.total().is_zero() {
			self.ctx.state.release_dormant_target(&self.owner);
		} else {
			ensure!(
				self.ctx.state.try_park_dormant_target(self.owner.clone()),
				Error::<T>::DormantTargetOccupied
			);
		}
		Ok(())
	}

	fn remove_from_lifecycle(&self) -> DispatchResult {
		match self.status {
			VaultStatus::Active => self.index_remove()?,
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

	/// Removes the vault from its index and takes its contribution out of every branch total.
	///
	/// `collateral_out` is the collateral that leaves the branch with the vault.
	fn detach(&mut self, collateral_out: BalanceOf<T>) -> DispatchResult {
		// A removed liability must not retain fractional interest.
		ensure!(self.vault.interest_remainder == 0, DispatchError::Corruption);
		self.remove_from_lifecycle()?;
		self.ctx.state.replace_vault(Some(&self.vault), None)?;
		self.ctx.state.vault_count =
			self.ctx.state.vault_count.checked_sub(1).ok_or(DispatchError::Corruption)?;
		self.ctx.state.release_dormant_target(&self.owner);
		self.ctx.state.total_collateral = self
			.ctx
			.state
			.total_collateral
			.checked_sub(&collateral_out)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		Ok(())
	}

	/// Commits a liquidation and records the residual for redistribution.
	pub(crate) fn finish_liquidation(
		mut self,
		redistribution: DebtCollateral<BalanceOf<T>>,
	) -> DispatchResult {
		ensure!(redistribution.debt <= self.vault.debt.total(), Error::<T>::InvalidLiquidationPlan);
		let collateral_out = self
			.vault
			.collateral
			.checked_sub(&redistribution.collateral)
			.ok_or(Error::<T>::InvalidLiquidationPlan)?;
		self.detach(collateral_out)?;
		if !redistribution.debt.is_zero() || !redistribution.collateral.is_zero() {
			self.ctx
				.state
				.record_redistribution(redistribution, self.ctx.now)
				.ok_or(Error::<T>::RedistributionWouldOverflow)?;
		}
		self.persist(true)
	}

	/// Closes a debt-free vault and commits its collateral release.
	///
	/// A redemption settlement commits exempt: it answers to no collateralization gate and
	/// carries no oracle price. It closes only a vault with neither debt nor collateral, so the
	/// branch ratio cannot move.
	pub(crate) fn finish_close(
		mut self,
		recipient: &T::AccountId,
		commit: Commit,
	) -> DispatchResult {
		// A close is a lifecycle exit that the freeze holds back, whatever its reason. Only the
		// frozen-tolerant repayment path can reach here on a frozen branch.
		self.ctx.ensure_not_frozen()?;
		ensure!(self.vault.debt.total().is_zero(), Error::<T>::DebtOutstanding);
		let collateral = self.vault.collateral;
		self.detach(collateral)?;
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
		match commit {
			Commit::Checked if !branch_empties => self.ctx.ensure_mode_rules()?,
			Commit::Checked | Commit::Exempt => {},
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
	/// A debt-free Dormant vault remains eligible because redistribution can give it debt again.
	pub(super) fn sync_stake(&mut self) -> DispatchResult {
		let before = self.vault.clone();
		self.sync_stake_from(before)
	}

	/// Recomputes stake and replaces the specified accounting contribution.
	pub(super) fn sync_stake_from(&mut self, before: Vault<BalanceOf<T>>) -> DispatchResult {
		let target = if self.status.is_final_recovery() {
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
