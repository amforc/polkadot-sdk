//! Vault status, index, settlement, and removal transitions.

use super::{Commit, VaultOp};
use crate::{
	pallet::{BalanceOf, Config, Error, Event, Pallet},
	recovery,
	types::{DebtCollateral, LiquidationSnapshot, Vault, VaultStatus},
};
use frame::prelude::*;
use linked_list_interface::{Outcome, Position as ListPosition, SortedListInterface};
use stablecoin_primitives::RedemptionStepSnapshot;

impl<T: Config> VaultOp<T> {
	/// Returns validated liquidation inputs.
	pub(crate) fn prepare_liquidation(
		&self,
	) -> Result<LiquidationSnapshot<BalanceOf<T>>, DispatchError> {
		ensure!(!self.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);
		let cr = self.ctx.collateralization_ratio(&self.vault.position())?;
		ensure!(
			cr < self.ctx.branch.config.minimum_collateralization_ratio,
			Error::<T>::VaultNotLiquidatable
		);
		ensure!(!self.is_only_stake_bearer(), Error::<T>::LastVaultCannotBeLiquidated);
		Ok(LiquidationSnapshot {
			debt: self.vault.debt.total(),
			redistribution_penalty: self.ctx.branch.config.redistribution_penalty,
		})
	}

	/// Returns the current values needed for one redemption step.
	pub(crate) fn redemption_snapshot(&self) -> RedemptionStepSnapshot<BalanceOf<T>> {
		self.vault.redemption_snapshot(self.status, &self.ctx.branch.config)
	}

	/// Moves a dormant vault back to the rate list.
	pub(crate) fn activate(&mut self, hint: ListPosition<T::AccountId>) -> DispatchResult {
		ensure!(self.status.is_dormant(), Error::<T>::InvalidVaultStatus);
		ensure!(
			self.vault.debt.total() >= self.ctx.branch.config.minimum_debt,
			Error::<T>::DebtBelowMinimum
		);
		self.join_rate_index(hint)
	}

	pub(super) fn join_rate_index(&mut self, hint: ListPosition<T::AccountId>) -> DispatchResult {
		self.leave_position()?;
		self.index_insert(hint)?;
		self.set_status(VaultStatus::Active)
	}

	fn go_dormant(&mut self) -> DispatchResult {
		self.leave_position()?;
		self.sync_dormant_target()?;
		self.set_status(VaultStatus::Dormant)
	}

	fn join_recovery_queue(&mut self) -> DispatchResult {
		self.leave_position()?;
		recovery::append::<T>(self.collateral_id(), self.stable_id(), self.owner.clone())?;
		self.set_status(VaultStatus::FinalRecovery)
	}

	fn set_status(&mut self, new_status: VaultStatus) -> DispatchResult {
		let old_status = core::mem::replace(&mut self.status, new_status);
		if old_status == new_status {
			return Ok(());
		}
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

	fn leave_position(&mut self) -> DispatchResult {
		match self.status {
			VaultStatus::Active => self.index_remove(),
			VaultStatus::FinalRecovery => {
				recovery::remove::<T>(self.collateral_id(), self.stable_id(), &self.owner)
			},
			VaultStatus::Dormant => {
				self.ctx.branch.state.release_dormant_target(&self.owner);
				Ok(())
			},
		}
	}

	/// Moves an active vault to its new rate-list position.
	pub(super) fn reindex(&mut self, hint: ListPosition<T::AccountId>) -> DispatchResult {
		ensure!(self.status.is_active(), Error::<T>::InvalidVaultStatus);
		let outcome = T::VaultLists::re_insert(
			self.ctx.rate_list(),
			self.owner.clone(),
			self.vault.annual_rate,
			hint,
		)
		.map_err(Pallet::<T>::map_error)?;
		if let Outcome::Relocated { steps } = outcome {
			self.hint_repair_steps.saturating_accrue(steps);
		}
		Ok(())
	}

	/// Moves an unsafe last eligible vault into final recovery.
	pub(crate) fn enter_final_recovery(&mut self) -> DispatchResult {
		ensure!(self.status.is_active(), Error::<T>::InvalidVaultStatus);
		self.ctx.ensure_below_mcr(&self.vault.position())?;
		ensure!(self.is_only_stake_bearer(), Error::<T>::NotLastEligibleVault);
		self.join_recovery_queue()
	}

	/// Removes a safe vault from final recovery.
	pub(crate) fn exit_final_recovery(
		&mut self,
		hint: ListPosition<T::AccountId>,
	) -> DispatchResult {
		ensure!(self.status.is_final_recovery(), Error::<T>::InvalidVaultStatus);
		self.ctx.ensure_at_or_above_mcr(&self.vault.position())?;
		if self.vault.debt.total() >= self.ctx.branch.config.minimum_debt {
			self.join_rate_index(hint)
		} else {
			self.go_dormant()
		}
	}

	/// FinalRecovery debt stays available for recovery redemption, even below the debt minimum.
	pub(crate) fn reconcile_after_debt_reduction(&mut self) -> DispatchResult {
		let total = self.vault.debt.total();
		let dormant = match self.status {
			VaultStatus::Active | VaultStatus::Dormant => {
				total < self.ctx.branch.config.minimum_debt
			},
			VaultStatus::FinalRecovery => total.is_zero(),
		};
		if dormant {
			self.go_dormant()
		} else {
			Ok(())
		}
	}

	/// Parks a dormant vault that still owes debt as the redemption target and releases a
	/// debt-free one, which has nothing left to redeem.
	fn sync_dormant_target(&mut self) -> DispatchResult {
		if self.vault.debt.total().is_zero() {
			self.ctx.branch.state.release_dormant_target(&self.owner);
		} else {
			ensure!(
				self.ctx.branch.state.try_park_dormant_target(self.owner.clone()),
				Error::<T>::DormantTargetOccupied
			);
		}
		Ok(())
	}

	fn is_only_stake_bearer(&self) -> bool {
		self.ctx.branch.state.stakes.total == self.vault.redistribution_stake
	}

	/// Removes the vault from its index and takes its contribution out of every branch total.
	///
	/// `collateral_out` is the collateral that leaves the branch with the vault.
	fn detach(&mut self, collateral_out: BalanceOf<T>) -> DispatchResult {
		self.leave_position()?;
		self.ctx.branch.state.replace_vault(Some(&self.vault), None)?;
		self.ctx
			.branch
			.state
			.total_collateral
			.ensure_sub_assign(collateral_out)
			.map_err(|_| Error::<T>::ArithmeticOverflow)?;
		Ok(())
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
		let collateral_out = self
			.vault
			.collateral
			.checked_sub(&redistribution.collateral)
			.ok_or(Error::<T>::InvalidLiquidationSettlement)?;
		self.detach(collateral_out)?;
		if !redistribution.debt.is_zero() || !redistribution.collateral.is_zero() {
			self.ctx
				.branch
				.state
				.record_redistribution(redistribution, self.ctx.now)
				.ok_or(Error::<T>::RedistributionWouldOverflow)?;
		}
		self.persist(true, None)
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
		let collateral = self.vault.collateral;
		// Only collateral-releasing exits need an unfrozen branch and a price. Empty-row
		// cleanup cannot worsen collateralization.
		if !collateral.is_zero() {
			self.ctx.ensure_not_frozen()?;
			if matches!(commit, Commit::Checked) {
				self.ctx.load_price()?;
			}
		}
		ensure!(self.vault.debt.total().is_zero(), Error::<T>::DebtOutstanding);
		self.detach(collateral)?;
		let branch_empties = self.ctx.branch.state.is_empty_of_liability();
		if branch_empties {
			ensure!(
				self.ctx.branch.state.debt.minted_interest.is_zero(),
				DispatchError::Corruption
			);
			ensure!(
				self.ctx.branch.state.debt.interest_ledger_settled(),
				DispatchError::Corruption
			);
			// Neither the aggregate remainder nor interest charged ahead of it has a liability
			// owner once the branch is empty.
			self.ctx.branch.state.debt.aggregate_interest_remainder = 0;
			self.ctx.branch.state.debt.interest_minted_ahead = Zero::zero();
		}
		if !collateral.is_zero() {
			self.release_collateral(recipient, collateral)?;
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
			Commit::Checked if !collateral.is_zero() && !branch_empties => {
				self.ctx.ensure_mode_rules()?
			},
			Commit::Checked | Commit::Exempt => {},
		}
		self.persist(true, None)
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
				.branch
				.state
				.stake_for(self.vault.collateral)
				.ok_or(Error::<T>::ArithmeticOverflow)?
		};
		if before != self.vault || self.vault.redistribution_stake != target {
			self.vault.redistribution_stake = target;
			self.ctx.branch.state.replace_vault(Some(&before), Some(&self.vault))?;
		}
		Ok(())
	}
}
