//! In-memory state for market and vault operations.
//!
//! [`BranchOp`] loads a market. Touching a vault turns it into a [`VaultOp`]. A commit consumes the
//! operation, so it cannot touch twice or write a different vault.

use crate::{
	math,
	pallet::{
		BalanceOf, BranchOf, CollateralIdOf, CollateralRisks, Config, Error, Event, HoldReason,
		Millis, Pallet, StableIdOf, Vaults,
	},
	recovery,
	types::{Position, Vault, VaultDebt, VaultListId, VaultStatus},
};
use frame::{
	prelude::*,
	traits::{fungibles::MutateHold as FungiblesMutateHold, tokens::Restriction, Time},
};
use linked_list_interface::{Position as ListPosition, SortedListInterface};
use pusd_primitives::{
	collateralization_ratio, LiquidationSnapshot, ProvidePrice, RedemptionStepSnapshot,
};

/// State for one market operation, loaded and committed once.
pub(crate) struct BranchOp<T: Config> {
	collateral_id: CollateralIdOf<T>,
	stable_id: StableIdOf<T>,
	now: Millis,
	branch: BranchOf<T>,
	pending_interest_mint: BalanceOf<T>,
	pending_fee: Option<BalanceOf<T>>,
	tcr_baseline: Position<BalanceOf<T>>,
	#[cfg(debug_assertions)]
	loaded: BranchOf<T>,
}

/// State for one vault operation.
///
/// A commit can only write the vault loaded for `owner`.
pub(crate) struct VaultOp<T: Config> {
	ctx: BranchOp<T>,
	owner: T::AccountId,
	vault: Vault<BalanceOf<T>>,
	status: VaultStatus,
	removed: bool,
}

enum RowAction {
	Keep,
	Remove,
}

/// What [`VaultOp::detach_for_close`] released.
pub(crate) struct CloseOutcome<Balance> {
	/// Collateral to release to the recipient.
	pub collateral: Balance,
	/// Whether the close left the branch with no vault liability.
	pub branch_empties: bool,
	/// Orphan debt swept to bad debt because the branch emptied.
	pub orphan_debt: Balance,
}

/// What [`VaultOp::settle_recovery_residual`] moved.
pub(crate) struct ResidualSettlement<Balance> {
	/// Remaining vault debt recorded as branch bad debt.
	pub residual_debt: Balance,
	/// Collateral dust to release to the owner.
	pub collateral_dust: Balance,
	/// Orphan debt swept to bad debt because the branch emptied.
	pub swept_orphan_debt: Balance,
}

enum TcrCheck {
	Exempt,
	Operation(FixedU128),
	Settlement,
}

impl<T: Config> BranchOp<T> {
	/// Loads a market and applies its pending interest in memory.
	fn load(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> Result<Self, DispatchError> {
		let now = T::TimeProvider::now();
		let mut branch = Pallet::<T>::branch_of(&collateral_id, &stable_id)?;
		#[cfg(debug_assertions)]
		let loaded = branch.clone();
		let pending_interest_mint = Pallet::<T>::accrue_aggregate_interest(&mut branch.state, now);

		// Interest is already included, so this matches the debt used by `compute_tcr`.
		let tcr_baseline = Position {
			collateral: branch.state.total_collateral,
			debt: Pallet::<T>::accrued_branch_debt(&branch.state, now),
		};
		Ok(Self {
			collateral_id,
			stable_id,
			now,
			branch,
			pending_interest_mint,
			pending_fee: None,
			tcr_baseline,
			#[cfg(debug_assertions)]
			loaded,
		})
	}

	/// Loads a market and rejects it if frozen.
	pub(crate) fn load_unfrozen(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> Result<Self, DispatchError> {
		let op = Self::load(collateral_id, stable_id)?;
		op.ensure_not_frozen()?;
		Ok(op)
	}

	/// Applies pending changes to one vault, even when the market is frozen.
	pub(crate) fn refresh_vault(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
	) -> DispatchResult {
		let op = Self::load(collateral_id, stable_id)?;
		let op = op.touch(owner)?;
		op.commit_exempt()
	}

	fn ensure_not_frozen(&self) -> DispatchResult {
		ensure!(!self.branch.state.is_frozen(), Error::<T>::BranchFrozen);
		Ok(())
	}

	/// Returns the rate-list ID for this market.
	fn rate_list(&self) -> VaultListId<CollateralIdOf<T>, StableIdOf<T>> {
		VaultListId::Rate(self.collateral_id.clone(), self.stable_id.clone())
	}

	/// Returns the oracle price for this collateral.
	pub(crate) fn price(&self) -> Result<FixedU128, DispatchError> {
		T::Oracle::provide_price(&self.collateral_id)
	}

	/// Checks the global debt limit for this collateral.
	///
	/// TODO: Debt from different stable assets is added before price conversion. This is correct
	/// only while they use the same unit value and scale. Once prices are keyed by market, convert
	/// each market before adding its debt.
	fn ensure_global_ceiling(
		&self,
		price: FixedU128,
		operation_debt_increase: BalanceOf<T>,
	) -> DispatchResult {
		let risk = CollateralRisks::<T>::get(&self.collateral_id);
		let projected_total = risk
			.outstanding
			.checked_add(&self.pending_interest_mint)
			.and_then(|total| total.checked_add(&operation_debt_increase))
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		let collateral_debt = math::value_in_collateral(projected_total, price)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		ensure!(collateral_debt <= risk.debt_ceiling, Error::<T>::GlobalDebtCeilingExceeded);
		Ok(())
	}

	/// Prepares a new vault and updates the market state in memory.
	pub(crate) fn create_vault(
		mut self,
		owner: &T::AccountId,
		initial_collateral: BalanceOf<T>,
		initial_debt: BalanceOf<T>,
		annual_rate: FixedU128,
		price: FixedU128,
		hint: ListPosition<T::AccountId>,
	) -> Result<VaultOp<T>, DispatchError> {
		ensure!(
			!Vaults::<T>::contains_key((&self.collateral_id, &self.stable_id, owner)),
			Error::<T>::VaultAlreadyExists
		);
		ensure!(
			initial_collateral >= self.branch.config.minimum_collateral,
			Error::<T>::InsufficientCollateral
		);
		let mut vault = Pallet::<T>::open_scratch_row(
			&self.branch.state,
			annual_rate,
			initial_collateral,
			self.now,
		);
		let upfront_fee =
			self.apply_checked_borrow(&mut vault, initial_debt, annual_rate, price)?;
		let total_collateral = self
			.branch
			.state
			.total_collateral
			.checked_add(&initial_collateral)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.branch.state.total_collateral = total_collateral;
		self.charge_upfront_fee(owner, upfront_fee);
		T::VaultLists::insert(self.rate_list(), owner.clone(), annual_rate, hint)
			.map_err(Pallet::<T>::map_error)?;
		Ok(self.attach_new(owner, vault))
	}

	fn apply_checked_borrow(
		&mut self,
		vault: &mut Vault<BalanceOf<T>>,
		amount: BalanceOf<T>,
		new_rate: FixedU128,
		price: FixedU128,
	) -> Result<BalanceOf<T>, DispatchError> {
		Pallet::<T>::validate_rate(&self.branch.config, new_rate)?;
		let vault_principal_after = vault
			.debt
			.principal
			.checked_add(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		ensure!(
			vault_principal_after >= self.branch.config.minimum_debt,
			Error::<T>::DebtBelowMinimum
		);

		Pallet::<T>::ratchet_ceiling(&mut self.branch.state, &self.branch.config, self.now);
		let principal_after = self
			.branch
			.state
			.debt
			.principal
			.checked_add(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		ensure!(
			principal_after <= self.branch.config.debt_ceiling,
			Error::<T>::DebtCeilingExceeded
		);
		if !self.branch.config.ceiling_gap.is_zero() {
			ensure!(
				principal_after <= self.branch.state.effective_ceiling,
				Error::<T>::DebtCeilingExceeded
			);
		}
		let upfront_fee = Pallet::<T>::apply_borrow_unchecked(
			&mut self.branch.state,
			&self.branch.config,
			vault,
			amount,
			new_rate,
			self.now,
		);
		let operation_debt_increase =
			amount.checked_add(&upfront_fee).ok_or(Error::<T>::ArithmeticOverflow)?;
		self.ensure_global_ceiling(price, operation_debt_increase)?;
		Pallet::<T>::ensure_above_icr(&vault.position(), price, &self.branch.config)?;
		Ok(upfront_fee)
	}

	fn apply_checked_rate_change(
		&mut self,
		vault: &mut Vault<BalanceOf<T>>,
		new_rate: FixedU128,
	) -> Result<Option<BalanceOf<T>>, DispatchError> {
		if vault.annual_rate == new_rate {
			return Ok(None);
		}
		Pallet::<T>::validate_rate(&self.branch.config, new_rate)?;
		Ok(Some(Pallet::<T>::apply_rate_change(
			&mut self.branch.state,
			&self.branch.config,
			vault,
			new_rate,
			self.now,
		)))
	}

	/// Records an upfront fee and defers minting until commit.
	fn charge_upfront_fee(&mut self, owner: &T::AccountId, amount: BalanceOf<T>) {
		if amount.is_zero() {
			return;
		}
		debug_assert!(self.pending_fee.is_none(), "one upfront fee per dispatch");
		self.pending_fee = Some(amount);
		Pallet::<T>::deposit_event(Event::UpfrontFeeCharged {
			collateral_id: self.collateral_id.clone(),
			stable_id: self.stable_id.clone(),
			owner: owner.clone(),
			amount,
		});
	}

	/// Applies pending interest and redistribution to a vault in memory.
	pub(crate) fn touch(mut self, owner: &T::AccountId) -> Result<VaultOp<T>, DispatchError> {
		debug_assert!(self.pending_fee.is_none(), "fee charged before touch");
		let mut vault = Pallet::<T>::vault_of(&self.collateral_id, &self.stable_id, owner)?;
		let status = Pallet::<T>::vault_status_of(&self.collateral_id, &self.stable_id, owner);
		let pending = Pallet::<T>::pending_touch_for(&vault, &self.branch.state, self.now);

		if !pending.interest.is_zero() {
			vault.debt.interest = vault.debt.interest.saturating_add(pending.interest);
			Pallet::<T>::deposit_event(Event::InterestAccrued {
				collateral_id: self.collateral_id.clone(),
				stable_id: self.stable_id.clone(),
				owner: owner.clone(),
				amount: pending.interest,
			});
		}
		if !pending.principal.is_zero() {
			self.branch.state.absorb_redistributed_debt(&mut vault, pending.principal);
		}
		if !pending.collateral.is_zero() {
			// This collateral is already in the market total. Only its hold moves.
			T::CollateralAssets::transfer_on_hold(
				self.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				&Pallet::<T>::redistribution_account(&self.collateral_id, &self.stable_id),
				owner,
				pending.collateral,
				Precision::Exact,
				Restriction::OnHold,
				Fortitude::Polite,
			)?;
			vault.collateral = vault.collateral.saturating_add(pending.collateral);
		}

		if vault.redistribution_snapshot != self.branch.state.redistribution {
			vault.redistribution_snapshot = self.branch.state.redistribution;
		}
		vault.last_interest_time = self.branch.state.interest_time(self.now);

		// Touching only moves existing debt and collateral, so it does not change the TCR.
		let mut op = VaultOp { ctx: self, owner: owner.clone(), vault, status, removed: false };
		op.sync_stake();
		Ok(op)
	}

	/// Attaches a new vault without touching an existing row.
	///
	/// Its upfront fee may already be recorded.
	fn attach_new(self, owner: &T::AccountId, vault: Vault<BalanceOf<T>>) -> VaultOp<T> {
		let mut op = VaultOp {
			ctx: self,
			owner: owner.clone(),
			vault,
			status: VaultStatus::Active,
			removed: false,
		};
		op.sync_stake();
		op
	}
}

impl<T: Config> VaultOp<T> {
	/// Returns the collateral asset ID.
	pub(crate) fn collateral_id(&self) -> &CollateralIdOf<T> {
		&self.ctx.collateral_id
	}

	/// Returns the stable asset ID.
	pub(crate) fn stable_id(&self) -> &StableIdOf<T> {
		&self.ctx.stable_id
	}

	/// Returns the vault owner.
	pub(crate) fn owner(&self) -> &T::AccountId {
		&self.owner
	}

	/// Returns the vault status.
	pub(crate) fn status(&self) -> VaultStatus {
		self.status
	}

	/// Returns the current vault state.
	pub(crate) fn vault(&self) -> &Vault<BalanceOf<T>> {
		&self.vault
	}

	/// Returns the oracle price for this collateral.
	pub(crate) fn price(&self) -> Result<FixedU128, DispatchError> {
		self.ctx.price()
	}

	/// Applies a collateral withdrawal.
	///
	/// Returns `true` when the empty vault should be closed.
	pub(crate) fn apply_collateral_withdrawal(
		&mut self,
		amount: BalanceOf<T>,
		price: FixedU128,
	) -> Result<bool, DispatchError> {
		ensure!(!self.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);
		let collateral_after = self
			.vault
			.collateral
			.checked_sub(&amount)
			.ok_or(Error::<T>::InsufficientCollateral)?;
		let debt = self.vault.debt.total();
		if !debt.is_zero() {
			Pallet::<T>::ensure_above_icr(
				&Position { debt, collateral: collateral_after },
				price,
				&self.ctx.branch.config,
			)?;
		}
		if debt.is_zero() && collateral_after.is_zero() {
			return Ok(true);
		}
		self.apply_collateral_removal(amount, collateral_after)?;
		Ok(false)
	}

	/// Checks whether the vault may be liquidated.
	pub(crate) fn ensure_liquidatable(&self, price: FixedU128) -> DispatchResult {
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

	fn rate_list(&self) -> VaultListId<CollateralIdOf<T>, StableIdOf<T>> {
		self.ctx.rate_list()
	}

	/// Adds collateral to the vault and market totals.
	pub(crate) fn add_collateral(&mut self, amount: BalanceOf<T>) -> DispatchResult {
		ensure!(!self.status.is_dormant(), Error::<T>::InvalidVaultStatus);
		let vault_collateral = self
			.vault
			.collateral
			.checked_add(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		let branch_collateral = self
			.ctx
			.branch
			.state
			.total_collateral
			.checked_add(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.vault.collateral = vault_collateral;
		self.ctx.branch.state.total_collateral = branch_collateral;
		self.sync_stake();
		Ok(())
	}

	/// Removes collateral from the vault and market totals.
	pub(crate) fn remove_collateral(&mut self, amount: BalanceOf<T>) -> DispatchResult {
		let vault_collateral = self
			.vault
			.collateral
			.checked_sub(&amount)
			.ok_or(Error::<T>::InsufficientCollateral)?;
		self.apply_collateral_removal(amount, vault_collateral)
	}

	fn apply_collateral_removal(
		&mut self,
		amount: BalanceOf<T>,
		vault_collateral: BalanceOf<T>,
	) -> DispatchResult {
		let branch_collateral = self
			.ctx
			.branch
			.state
			.total_collateral
			.checked_sub(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.vault.collateral = vault_collateral;
		self.ctx.branch.state.total_collateral = branch_collateral;
		self.sync_stake();
		Ok(())
	}

	/// Adds debt and optionally changes the vault rate.
	pub(crate) fn borrow(
		&mut self,
		amount: BalanceOf<T>,
		maybe_new_rate: Option<FixedU128>,
		price: FixedU128,
		hint: ListPosition<T::AccountId>,
	) -> DispatchResult {
		ensure!(!self.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);
		let old_rate = self.vault.annual_rate;
		let new_rate = maybe_new_rate.unwrap_or(old_rate);
		let dormant_to_active = self.status.is_dormant();
		let rate_changed = old_rate != new_rate;
		let upfront_fee =
			self.ctx.apply_checked_borrow(&mut self.vault, amount, new_rate, price)?;
		self.ctx.charge_upfront_fee(&self.owner, upfront_fee);
		if dormant_to_active {
			debug_assert!(
				self.vault.debt.total() >= self.ctx.branch.config.minimum_debt,
				"the checked principal floor implies the total-debt floor"
			);
			self.activate_dormant_unchecked(hint)?;
		} else if rate_changed {
			self.reindex(hint)?;
		}
		if rate_changed {
			Pallet::<T>::deposit_event(Event::BorrowRateChanged {
				collateral_id: self.ctx.collateral_id.clone(),
				stable_id: self.ctx.stable_id.clone(),
				owner: self.owner.clone(),
				old_rate,
				new_rate,
			});
		}
		Ok(())
	}

	/// Changes the rate and updates the rate list.
	///
	/// Returns `false` if the rate did not change.
	pub(crate) fn change_rate(
		&mut self,
		new_rate: FixedU128,
		hint: ListPosition<T::AccountId>,
	) -> Result<bool, DispatchError> {
		ensure!(self.status.is_active(), Error::<T>::InvalidVaultStatus);
		let old_rate = self.vault.annual_rate;
		let Some(upfront_fee) = self.ctx.apply_checked_rate_change(&mut self.vault, new_rate)?
		else {
			return Ok(false);
		};
		self.ctx.charge_upfront_fee(&self.owner, upfront_fee);
		self.reindex(hint)?;
		Pallet::<T>::deposit_event(Event::BorrowRateChanged {
			collateral_id: self.ctx.collateral_id.clone(),
			stable_id: self.ctx.stable_id.clone(),
			owner: self.owner.clone(),
			old_rate,
			new_rate,
		});
		Ok(true)
	}

	/// Caps a requested repayment at the current vault debt.
	pub(crate) fn repayment_amount(&self, requested: BalanceOf<T>) -> BalanceOf<T> {
		requested.min(self.vault.debt.total())
	}

	/// Repays debt while enforcing the minimum remaining debt.
	///
	/// Returns the principal and interest removed.
	pub(crate) fn repay(
		&mut self,
		amount: BalanceOf<T>,
	) -> Result<crate::types::VaultDebt<BalanceOf<T>>, DispatchError> {
		let payment = self.redeem(amount);
		let total_after = self.vault.debt.total();
		ensure!(
			total_after.is_zero() || total_after >= self.ctx.branch.config.minimum_debt,
			Error::<T>::DebtWouldBecomeDust
		);
		Ok(payment)
	}

	/// Cancels debt without enforcing the minimum remaining debt.
	///
	/// Returns the principal and interest removed.
	pub(crate) fn redeem(&mut self, amount: BalanceOf<T>) -> VaultDebt<BalanceOf<T>> {
		let payment = self.vault.debt.cancel(amount.min(self.vault.debt.total()));
		self.ctx.branch.state.apply_debt_payment(
			payment.clone(),
			self.vault.annual_rate,
			self.vault.debt.principal,
		);
		payment
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

	fn activate_dormant_unchecked(&mut self, hint: ListPosition<T::AccountId>) -> DispatchResult {
		debug_assert!(self.status.is_dormant());
		T::VaultLists::insert(self.rate_list(), self.owner.clone(), self.vault.annual_rate, hint)
			.map_err(Pallet::<T>::map_error)?;
		self.ctx.branch.state.release_dormant_target(&self.owner);
		self.set_status(VaultStatus::Active);
		Ok(())
	}

	fn reindex(&self, hint: ListPosition<T::AccountId>) -> DispatchResult {
		ensure!(self.status.is_active(), Error::<T>::InvalidVaultStatus);
		T::VaultLists::re_insert(self.rate_list(), self.owner.clone(), self.vault.annual_rate, hint)
			.map(|_| ())
			.map_err(|e| Pallet::<T>::map_error(e).into())
	}

	/// Moves an unsafe last eligible vault into final recovery.
	pub(crate) fn enter_final_recovery(&mut self, price: FixedU128) -> DispatchResult {
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
		price: FixedU128,
		hint: ListPosition<T::AccountId>,
	) -> DispatchResult {
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

	/// Removes a liquidated vault and records what its liquidation
	/// redistributes onto the remaining vaults.
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
			let now = self.ctx.now;
			self.ctx
				.branch
				.state
				.record_redistribution(redistribution, now)
				.ok_or(Error::<T>::RedistributionWouldOverflow)?;
		}
		self.removed = true;
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
		self.removed = true;
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
		self.removed = true;
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

	/// Uses all vault collateral as redistribution stake, or zero during final recovery.
	fn sync_stake(&mut self) {
		let target = if self.status.is_final_recovery() {
			BalanceOf::<T>::zero()
		} else {
			self.vault.collateral
		};
		if self.vault.redistribution_stake != target {
			self.ctx.branch.state.set_vault_stake(&mut self.vault, target);
		}
	}

	/// Stores the vault without checking the TCR change.
	pub(crate) fn commit_exempt(self) -> DispatchResult {
		self.commit_inner(RowAction::Keep, TcrCheck::Exempt)
	}

	/// Stores the vault after checking the TCR change.
	pub(crate) fn commit_checked(self, price: FixedU128) -> DispatchResult {
		self.commit_inner(RowAction::Keep, TcrCheck::Operation(price))
	}

	/// Removes the vault without checking the TCR change.
	pub(crate) fn remove_exempt(self) -> DispatchResult {
		self.commit_inner(RowAction::Remove, TcrCheck::Exempt)
	}

	/// Removes the vault after checking the TCR change.
	pub(crate) fn remove_checked(self, price: FixedU128) -> DispatchResult {
		self.commit_inner(RowAction::Remove, TcrCheck::Operation(price))
	}

	/// Removes the vault as a settlement from an unfrozen market.
	pub(crate) fn remove_settlement(self) -> DispatchResult {
		self.commit_inner(RowAction::Remove, TcrCheck::Settlement)
	}

	fn commit_inner(self, row_action: RowAction, tcr_check: TcrCheck) -> DispatchResult {
		let keep_row = matches!(row_action, RowAction::Keep);
		if keep_row == self.removed {
			return Err(DispatchError::Corruption);
		}
		match tcr_check {
			TcrCheck::Exempt => {},
			TcrCheck::Operation(price) => enforce_operation_tcr::<T>(
				&self.ctx.tcr_baseline,
				&self.ctx.branch,
				self.ctx.now,
				price,
			)?,
			TcrCheck::Settlement => self.ctx.ensure_not_frozen()?,
		}
		#[cfg(debug_assertions)]
		debug_assert_eq!(
			crate::pallet::Branches::<T>::get(&self.ctx.collateral_id, &self.ctx.stable_id)
				.as_ref(),
			Some(&self.ctx.loaded),
			"Branches mutated behind BranchOp"
		);
		let collateral_id = self.ctx.collateral_id.clone();
		let stable_id = self.ctx.stable_id.clone();
		let key = (&collateral_id, &stable_id, &self.owner);
		if keep_row {
			Vaults::<T>::insert(key, &self.vault);
		} else {
			Vaults::<T>::remove(key);
		}
		let BranchOp { now, branch, pending_interest_mint, pending_fee, .. } = self.ctx;
		Pallet::<T>::commit_branch(&collateral_id, &stable_id, now, branch)?;

		// Mint after writing state. Keep both amounts separate to preserve fee rounding.
		if !pending_interest_mint.is_zero() {
			Pallet::<T>::mint_and_route_yield(&collateral_id, &stable_id, pending_interest_mint);
		}
		if let Some(fee) = pending_fee {
			Pallet::<T>::mint_and_route_yield(&collateral_id, &stable_id, fee);
		}
		Ok(())
	}
}

/// Checks the TCR change against the market mode rules.
fn enforce_operation_tcr<T: Config>(
	baseline: &Position<BalanceOf<T>>,
	branch: &BranchOf<T>,
	now: Millis,
	price: FixedU128,
) -> DispatchResult {
	let pre_tcr = Pallet::<T>::tcr_from_inputs(baseline, price)?;
	let post_tcr = Pallet::<T>::compute_tcr(&branch.state, price, now)?;
	Pallet::<T>::enforce_mode_rules(&branch.config, &branch.state, pre_tcr, post_tcr)
}
