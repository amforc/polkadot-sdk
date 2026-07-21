//! Per-dispatch operation contexts with deferred yield minting.
//!
//! Two phases: [`BranchOp::load`] opens the branch-level draft, and
//! [`BranchOp::touch`] consumes it into a [`VaultOp`] that owns the touched
//! vault row, its owner key, and its derived status. A commit consumes the
//! phase value and writes the row it owns. The protocol is structural: an
//! operation cannot touch twice or commit a row other than the one it touched.

use crate::{
	math,
	pallet::{
		BalanceOf, BranchOf, CollateralIdOf, CollateralRisks, Config, Error, Event, HoldReason,
		Millis, Pallet, StableIdOf, Vaults,
	},
	recovery,
	types::{Vault, VaultDebt, VaultListId, VaultStatus},
	utility_impls::TcrInputs,
};
use frame::{
	prelude::*,
	traits::{fungibles::MutateHold as FungiblesMutateHold, tokens::Restriction, Time},
};
use pallet_linked_list::{Position, SortedListInterface};
use pusd_primitives::{ProvidePrice, RedemptionStepSnapshot};

/// Branch-level operation context: one branch-state read threaded through an
/// operation and committed once.
pub(crate) struct BranchOp<T: Config> {
	collateral_id: CollateralIdOf<T>,
	stable_id: StableIdOf<T>,
	now: Millis,
	branch: BranchOf<T>,
	outstanding_at_load: BalanceOf<T>,
	pending_interest_mint: BalanceOf<T>,
	pending_fee: Option<BalanceOf<T>>,
	tcr_baseline: TcrInputs<BalanceOf<T>>,
	#[cfg(debug_assertions)]
	loaded: BranchOf<T>,
}

/// Vault-level operation: a [`BranchOp::touch`]ed draft that owns the vault
/// row it settled. Commits write `vault` under `owner`, so the touched row and
/// the committed row cannot diverge.
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

enum TcrCheck {
	Exempt,
	Operation(FixedU128),
	Settlement(FixedU128),
}

impl<T: Config> BranchOp<T> {
	/// Read the branch state and accrue aggregate interest in memory.
	fn load(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> Result<Self, DispatchError> {
		let now = T::TimeProvider::now();
		let mut branch = Pallet::<T>::branch_of(&collateral_id, &stable_id)?;
		#[cfg(debug_assertions)]
		let loaded = branch.clone();

		let outstanding_at_load = branch.state.debt.outstanding();
		let pending_interest_mint = Pallet::<T>::accrue_aggregate_interest(&mut branch.state, now);

		// The accrual above folded pending aggregate interest into the state,
		// so the baseline debt is exactly the sum `compute_tcr` would see.
		let tcr_baseline = TcrInputs {
			collateral: branch.state.total_collateral,
			debt: Pallet::<T>::accrued_branch_debt(&branch.state, now),
		};
		Ok(Self {
			collateral_id,
			stable_id,
			now,
			tcr_baseline,
			branch,
			outstanding_at_load,
			pending_interest_mint,
			pending_fee: None,
			#[cfg(debug_assertions)]
			loaded,
		})
	}

	pub(crate) fn load_unfrozen(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> Result<Self, DispatchError> {
		let op = Self::load(collateral_id, stable_id)?;
		op.ensure_not_frozen()?;
		Ok(op)
	}

	/// Refresh one vault. This is intentionally allowed while frozen.
	pub(crate) fn refresh(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<(), DispatchError> {
		let op = Self::load(collateral_id, stable_id)?;
		let op = op.touch(owner)?;
		op.commit_exempt()
	}

	fn ensure_not_frozen(&self) -> Result<(), DispatchError> {
		ensure!(!self.branch.state.is_frozen(), Error::<T>::BranchFrozen);
		Ok(())
	}

	/// The branch's rate-index list id, derived from the context's own keys so
	/// it can never drift from `collateral_id`/`stable_id`.
	fn rate_list(&self) -> VaultListId<CollateralIdOf<T>, StableIdOf<T>> {
		VaultListId::Rate(self.collateral_id.clone(), self.stable_id.clone())
	}

	/// Oracle price for this context's collateral.
	pub(crate) fn price(&self) -> Result<FixedU128, DispatchError> {
		T::Oracle::provide_price(&self.collateral_id)
	}

	/// Enforce the per-collateral global debt ceiling.
	///
	/// TODO: One known limitation: the aggregate sums
	/// `outstanding()` in raw units across *different* stable assets before one
	/// price conversion, which is only correct while every stable shares the
	/// same unit value ($1 par, same scale). Fix once the oracle is keyed by
	/// `(collateral, stable)`: convert each market's outstanding at its own
	/// pair price, then sum in collateral units.
	fn ensure_global_ceiling(&self, price: FixedU128) -> Result<(), DispatchError> {
		let risk = CollateralRisks::<T>::get(&self.collateral_id);
		let projected_total = risk
			.outstanding
			.saturating_sub(self.outstanding_at_load)
			.saturating_add(self.branch.state.debt.outstanding());
		let collateral_debt = math::value_in_collateral::<BalanceOf<T>>(projected_total, price)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		ensure!(collateral_debt <= risk.debt_ceiling, Error::<T>::GlobalDebtCeilingExceeded);
		Ok(())
	}

	pub(crate) fn create_vault(
		mut self,
		owner: &T::AccountId,
		initial_collateral: BalanceOf<T>,
		initial_debt: BalanceOf<T>,
		annual_rate: FixedU128,
		price: FixedU128,
		hint: Position<T::AccountId>,
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
		self.branch.state.total_collateral = self
			.branch
			.state
			.total_collateral
			.checked_add(&initial_collateral)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
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
		let config = &self.branch.config;
		let state = &mut self.branch.state;
		Pallet::<T>::validate_rate(config, new_rate)?;
		let vault_principal_after = vault
			.debt
			.principal
			.checked_add(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		ensure!(vault_principal_after >= config.minimum_debt, Error::<T>::DebtBelowMinimum);

		Pallet::<T>::ratchet_ceiling(state, config, self.now);
		let principal_after = state
			.debt
			.principal
			.checked_add(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		ensure!(principal_after <= config.debt_ceiling, Error::<T>::DebtCeilingExceeded);
		if !config.ceiling_gap.is_zero() {
			ensure!(principal_after <= state.effective_ceiling, Error::<T>::DebtCeilingExceeded);
		}
		let upfront_fee =
			Pallet::<T>::apply_borrow_unchecked(state, config, vault, amount, new_rate, self.now);
		self.ensure_global_ceiling(price)?;
		Pallet::<T>::ensure_above_icr(
			vault.collateral,
			vault.debt.total(),
			price,
			&self.branch.config,
		)?;
		Ok(upfront_fee)
	}

	/// Charge `owner` the upfront fee: the event is deposited now (reverted
	/// with the dispatch on error), the mint is deferred until commit so pUSD
	/// is only issued when the branch state is actually written.
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

	/// Apply pending interest/redistribution to `owner`'s vault row in memory,
	/// consuming the branch context into a [`VaultOp`] that owns the row.
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
			// Already counted in `state.total_collateral`; only the hold moves.
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

		// A touch preserves the TCR inputs — principal + pending redistribution
		// move as a sum, collateral only changes hands, and the aggregate accrual
		// already ran at load — so the load baseline is the post-touch baseline.
		let mut op = VaultOp { ctx: self, owner: owner.clone(), vault, status, removed: false };
		op.sync_stake();
		Ok(op)
	}

	/// Attach a freshly-built vault row for `owner` — the only path on which a
	/// row enters storage without a touch. Unlike [`Self::touch`], the upfront
	/// fee may already be charged (an open computes its fee before the row
	/// exists).
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

	fn assert_unclobbered(&self) {
		#[cfg(debug_assertions)]
		debug_assert_eq!(
			crate::pallet::Branches::<T>::get(&self.collateral_id, &self.stable_id).as_ref(),
			Some(&self.loaded),
			"Branches mutated behind BranchOp"
		);
	}
}

impl<T: Config> VaultOp<T> {
	pub(crate) fn collateral_id(&self) -> &CollateralIdOf<T> {
		&self.ctx.collateral_id
	}

	pub(crate) fn stable_id(&self) -> &StableIdOf<T> {
		&self.ctx.stable_id
	}

	pub(crate) fn owner(&self) -> &T::AccountId {
		&self.owner
	}

	pub(crate) fn status(&self) -> VaultStatus {
		self.status
	}

	pub(crate) fn vault(&self) -> &Vault<BalanceOf<T>> {
		&self.vault
	}

	pub(crate) fn price(&self) -> Result<FixedU128, DispatchError> {
		self.ctx.price()
	}

	/// Validate and apply a collateral withdrawal, returning `true` instead when
	/// withdrawing the full amount should close the now-empty vault.
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
			Pallet::<T>::ensure_above_icr(collateral_after, debt, price, &self.ctx.branch.config)?;
		}
		if debt.is_zero() && collateral_after.is_zero() {
			return Ok(true);
		}
		self.apply_collateral_removal(amount, collateral_after)?;
		Ok(false)
	}

	pub(crate) fn ensure_liquidatable(&self, price: FixedU128) -> DispatchResult {
		ensure!(!self.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);
		let cr = pusd_primitives::collateralization_ratio::<BalanceOf<T>>(
			self.vault.collateral,
			self.vault.debt.total(),
			price,
		)
		.ok_or(Error::<T>::VaultNotLiquidatable)?;
		ensure!(
			cr < self.ctx.branch.config.minimum_collateralization_ratio,
			Error::<T>::VaultNotLiquidatable
		);
		ensure!(!self.is_only_stake_bearer(), Error::<T>::LastVaultCannotBeLiquidated);
		Ok(())
	}

	pub(crate) fn redemption_snapshot(&self) -> RedemptionStepSnapshot<BalanceOf<T>> {
		RedemptionStepSnapshot {
			status: self.status,
			debt: self.vault.debt.total(),
			collateral: self.vault.collateral,
			redistribution_penalty: self.ctx.branch.config.redistribution_penalty,
		}
	}

	fn rate_list(&self) -> VaultListId<CollateralIdOf<T>, StableIdOf<T>> {
		self.ctx.rate_list()
	}

	pub(crate) fn add_collateral(&mut self, amount: BalanceOf<T>) -> Result<(), DispatchError> {
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

	pub(crate) fn remove_collateral(&mut self, amount: BalanceOf<T>) -> Result<(), DispatchError> {
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
	) -> Result<(), DispatchError> {
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

	pub(crate) fn borrow(
		&mut self,
		amount: BalanceOf<T>,
		maybe_new_rate: Option<FixedU128>,
		price: FixedU128,
		hint: Position<T::AccountId>,
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

	/// Apply and index a real rate change. Returns `false` for an equal-rate no-op.
	pub(crate) fn change_rate(
		&mut self,
		new_rate: FixedU128,
		hint: Position<T::AccountId>,
	) -> Result<bool, DispatchError> {
		ensure!(self.status.is_active(), Error::<T>::InvalidVaultStatus);
		let old_rate = self.vault.annual_rate;
		if old_rate == new_rate {
			return Ok(false);
		}
		let config = &self.ctx.branch.config;
		let state = &mut self.ctx.branch.state;
		Pallet::<T>::validate_rate(config, new_rate)?;
		let upfront_fee =
			Pallet::<T>::apply_rate_change(state, config, &mut self.vault, new_rate, self.ctx.now);
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

	pub(crate) fn repayment_amount(&self, requested: BalanceOf<T>) -> BalanceOf<T> {
		requested.min(self.vault.debt.total())
	}

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

	pub(crate) fn redeem(&mut self, amount: BalanceOf<T>) -> VaultDebt<BalanceOf<T>> {
		let payment = self.vault.debt.cancel(amount.min(self.vault.debt.total()));
		self.ctx.branch.state.apply_debt_payment(
			payment.clone(),
			self.vault.annual_rate,
			self.vault.debt.principal,
		);
		payment
	}

	pub(crate) fn activate(&mut self, hint: Position<T::AccountId>) -> Result<(), DispatchError> {
		ensure!(self.status.is_dormant(), Error::<T>::InvalidVaultStatus);
		ensure!(
			self.vault.debt.total() >= self.ctx.branch.config.minimum_debt,
			Error::<T>::DebtBelowMinimum
		);
		self.activate_dormant_unchecked(hint)
	}

	fn activate_dormant_unchecked(
		&mut self,
		hint: Position<T::AccountId>,
	) -> Result<(), DispatchError> {
		debug_assert!(self.status.is_dormant());
		T::VaultLists::insert(self.rate_list(), self.owner.clone(), self.vault.annual_rate, hint)
			.map_err(Pallet::<T>::map_error)?;
		self.ctx.branch.state.release_dormant_target(&self.owner);
		self.set_status(VaultStatus::Active);
		Ok(())
	}

	fn reindex(&self, hint: Position<T::AccountId>) -> DispatchResult {
		ensure!(self.status.is_active(), Error::<T>::InvalidVaultStatus);
		T::VaultLists::re_insert(self.rate_list(), self.owner.clone(), self.vault.annual_rate, hint)
			.map(|_| ())
			.map_err(|e| Pallet::<T>::map_error(e).into())
	}

	pub(crate) fn enter_final_recovery(&mut self, price: FixedU128) -> DispatchResult {
		ensure!(self.status.is_active(), Error::<T>::InvalidVaultStatus);
		Pallet::<T>::ensure_below_mcr(
			self.vault.collateral,
			self.vault.debt.total(),
			price,
			&self.ctx.branch.config,
		)?;
		ensure!(self.is_only_stake_bearer(), Error::<T>::NotLastEligibleVault);
		T::VaultLists::remove(&self.rate_list(), &self.owner)
			.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
		recovery::append::<T>(self.collateral_id(), self.stable_id(), self.owner.clone())?;
		self.set_status(VaultStatus::FinalRecovery);
		Ok(())
	}

	pub(crate) fn exit_final_recovery(
		&mut self,
		price: FixedU128,
		hint: Position<T::AccountId>,
	) -> DispatchResult {
		ensure!(self.status.is_final_recovery(), Error::<T>::InvalidVaultStatus);
		Pallet::<T>::ensure_at_or_above_mcr(
			self.vault.collateral,
			self.vault.debt.total(),
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
		self.vault.redistribution_snapshot = self.ctx.branch.state.redistribution;
		Ok(())
	}

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
				self.vault.redistribution_snapshot = self.ctx.branch.state.redistribution;
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

	pub(crate) fn apply_liquidation(
		&mut self,
		debt_offset: BalanceOf<T>,
		redistribution_collateral: BalanceOf<T>,
	) -> DispatchResult {
		let redistributed_debt = self
			.vault
			.debt
			.total()
			.checked_sub(&debt_offset)
			.ok_or(Error::<T>::InvalidLiquidationSettlement)?;
		let collateral_out = self
			.vault
			.collateral
			.checked_sub(&redistribution_collateral)
			.ok_or(Error::<T>::InvalidLiquidationSettlement)?;

		self.remove_from_lifecycle()?;
		self.ctx.branch.state.detach_vault(&self.vault);
		self.ctx.branch.state.release_dormant_target(&self.owner);
		self.ctx.branch.state.total_collateral = self
			.ctx
			.branch
			.state
			.total_collateral
			.checked_sub(&collateral_out)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		if !redistributed_debt.is_zero() || !redistribution_collateral.is_zero() {
			self.ctx
				.branch
				.state
				.record_redistribution(redistributed_debt, redistribution_collateral, self.ctx.now)
				.ok_or(Error::<T>::RedistributionWouldOverflow)?;
		}
		self.removed = true;
		Ok(())
	}

	pub(crate) fn detach_for_close(
		&mut self,
	) -> Result<(BalanceOf<T>, bool, BalanceOf<T>), DispatchError> {
		ensure!(self.vault.debt.total().is_zero(), Error::<T>::DebtOutstanding);
		let collateral = self.vault.collateral;
		self.remove_from_lifecycle()?;
		self.ctx.branch.state.detach_vault(&self.vault);
		self.ctx.branch.state.release_dormant_target(&self.owner);
		self.ctx.branch.state.total_collateral = self
			.ctx
			.branch
			.state
			.total_collateral
			.checked_sub(&collateral)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		let branch_empties = self.ctx.branch.state.is_empty_of_liability();
		let orphan_debt = if branch_empties {
			self.ctx.branch.state.sweep_orphan_debt()
		} else {
			BalanceOf::<T>::zero()
		};
		self.removed = true;
		Ok((collateral, branch_empties, orphan_debt))
	}

	pub(crate) fn settle_recovery_residual(
		&mut self,
	) -> Result<(BalanceOf<T>, BalanceOf<T>, BalanceOf<T>), DispatchError> {
		ensure!(self.status.is_final_recovery(), Error::<T>::InvalidVaultStatus);
		let residual = self.vault.debt.total();
		let dust = self.vault.collateral;
		self.remove_from_lifecycle()?;
		self.ctx.branch.state.detach_vault(&self.vault);
		self.ctx.branch.state.record_bad_debt(residual);
		self.ctx.branch.state.total_collateral = self
			.ctx
			.branch
			.state
			.total_collateral
			.checked_sub(&dust)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		let swept = if self.ctx.branch.state.is_empty_of_liability() {
			self.ctx.branch.state.sweep_orphan_debt()
		} else {
			BalanceOf::<T>::zero()
		};
		self.removed = true;
		Ok((residual, dust, swept))
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

	/// Eligible vaults stake their full collateral; `FinalRecovery` vaults do not
	/// participate in redistribution.
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

	pub(crate) fn commit_exempt(self) -> Result<(), DispatchError> {
		self.commit_inner(RowAction::Keep, TcrCheck::Exempt)
	}

	pub(crate) fn commit_checked(self, price: FixedU128) -> Result<(), DispatchError> {
		self.commit_inner(RowAction::Keep, TcrCheck::Operation(price))
	}

	pub(crate) fn remove_exempt(self) -> Result<(), DispatchError> {
		self.commit_inner(RowAction::Remove, TcrCheck::Exempt)
	}

	pub(crate) fn remove_checked(self, price: FixedU128) -> Result<(), DispatchError> {
		self.commit_inner(RowAction::Remove, TcrCheck::Operation(price))
	}

	pub(crate) fn remove_settlement(self, price: FixedU128) -> Result<(), DispatchError> {
		self.commit_inner(RowAction::Remove, TcrCheck::Settlement(price))
	}

	fn commit_inner(self, row_action: RowAction, tcr_check: TcrCheck) -> Result<(), DispatchError> {
		let keep_row = matches!(row_action, RowAction::Keep);
		if keep_row == self.removed {
			return Err(DispatchError::Corruption);
		}
		match tcr_check {
			TcrCheck::Exempt => {},
			TcrCheck::Operation(price) => enforce_tcr_check::<T>(
				&self.ctx.tcr_baseline,
				&self.ctx.branch,
				self.ctx.now,
				price,
				false,
			)?,
			TcrCheck::Settlement(price) => enforce_tcr_check::<T>(
				&self.ctx.tcr_baseline,
				&self.ctx.branch,
				self.ctx.now,
				price,
				true,
			)?,
		}
		self.ctx.assert_unclobbered();
		let key = (&self.ctx.collateral_id, &self.ctx.stable_id, &self.owner);
		if keep_row {
			Vaults::<T>::insert(key, &self.vault);
		} else {
			Vaults::<T>::remove(key);
		}
		let branch = self.ctx.branch;
		Pallet::<T>::commit_branch(
			&self.ctx.collateral_id,
			&self.ctx.stable_id,
			self.ctx.outstanding_at_load,
			branch,
		)?;

		// Mint only after the state is written; the two amounts stay separate
		// credits so the fee handler's per-credit rounding is unchanged.
		if !self.ctx.pending_interest_mint.is_zero() {
			Pallet::<T>::mint_and_route_yield(
				&self.ctx.collateral_id,
				&self.ctx.stable_id,
				self.ctx.pending_interest_mint,
			);
		}
		if let Some(fee) = self.ctx.pending_fee {
			Pallet::<T>::mint_and_route_yield(&self.ctx.collateral_id, &self.ctx.stable_id, fee);
		}
		Ok(())
	}
}

/// Check one operation's baseline-to-committed branch change against the
/// operation's own loaded config.
fn enforce_tcr_check<T: Config>(
	baseline: &TcrInputs<BalanceOf<T>>,
	branch: &BranchOf<T>,
	now: Millis,
	price: FixedU128,
	settlement: bool,
) -> Result<(), DispatchError> {
	let pre_tcr = Pallet::<T>::tcr_from_inputs(baseline, price)?;
	let post_tcr = Pallet::<T>::compute_tcr(&branch.state, price, now)?;
	Pallet::<T>::enforce_mode_rules(&branch.config, &branch.state, pre_tcr, post_tcr, settlement)
}
