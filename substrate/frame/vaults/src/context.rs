//! In-memory state for one vault operation.
//!
//! [`VaultOp`] owns the loaded branch and vault drafts. Committing consumes both, preventing a
//! second touch or a mismatched write.

mod lifecycle;

use crate::{
	pallet::{
		BalanceOf, CollateralIdOf, CollateralRisks, Config, Error, Event, HoldReason, Millis,
		Pallet, StableIdOf, Vaults,
	},
	types::{
		BranchConfig, BranchState, DebtBreakdown, DebtCollateral, Vault, VaultListId, VaultStatus,
	},
};
use frame::{
	prelude::*,
	traits::{
		fungibles::MutateHold as FungiblesMutateHold, tokens::Restriction, DefensiveOption, Time,
	},
};
use linked_list_interface::{Position as ListPosition, SortedListInterface};
use pusd_primitives::{recovery_pricing::collateral_for_value_ceil, ProvidePrice};

struct MarketDraft<AccountId, Balance> {
	config: BranchConfig<Balance>,
	state: BranchState<AccountId, Balance>,
}

struct Context<T: Config> {
	collateral_id: CollateralIdOf<T>,
	stable_id: StableIdOf<T>,
	now: Millis,
	branch: MarketDraft<T::AccountId, BalanceOf<T>>,
	pending_interest_mint: BalanceOf<T>,
	pending_fee: BalanceOf<T>,
	tcr_baseline: DebtCollateral<BalanceOf<T>>,
	price: Option<FixedU128>,
}

/// State for one vault operation.
///
/// A commit can only write the vault loaded for `owner`.
pub struct VaultOp<T: Config> {
	ctx: Context<T>,
	owner: T::AccountId,
	vault: Vault<BalanceOf<T>>,
	status: VaultStatus,
	remove_on_commit: bool,
}

/// What [`VaultOp::detach_for_close`] released.
pub struct CloseOutcome<Balance> {
	/// Collateral to release to the recipient.
	pub collateral: Balance,
	/// Whether the close left the branch with no vault liability.
	pub branch_empties: bool,
	/// Orphan debt swept to bad debt because the branch emptied.
	pub orphan_debt: Balance,
}

/// What [`VaultOp::settle_recovery_residual`] moved.
pub struct ResidualSettlement<Balance> {
	/// Remaining vault debt recorded as branch bad debt.
	pub residual_debt: Balance,
	/// Collateral dust to release to the owner.
	pub collateral_dust: Balance,
	/// Orphan debt swept to bad debt because the branch emptied.
	pub swept_orphan_debt: Balance,
}

impl<T: Config> Context<T> {
	/// Loads a market and applies its pending interest in memory.
	fn load(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> Result<Self, DispatchError> {
		let now = T::TimeProvider::now();
		let stored = Pallet::<T>::branch_of(&collateral_id, &stable_id)?;
		let mut branch = MarketDraft { config: stored.config, state: stored.state };
		let pending_interest_mint = Pallet::<T>::accrue_aggregate_interest(&mut branch.state, now)?;

		// Interest is already included, so this matches the debt used by `compute_tcr`.
		let tcr_baseline = DebtCollateral {
			collateral: branch.state.total_collateral,
			debt: Pallet::<T>::accrued_branch_debt(&branch.state, now),
		};
		Ok(Self {
			collateral_id,
			stable_id,
			now,
			branch,
			pending_interest_mint,
			pending_fee: BalanceOf::<T>::zero(),
			tcr_baseline,
			price: None,
		})
	}

	fn load_unfrozen(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> Result<Self, DispatchError> {
		let ctx = Self::load(collateral_id, stable_id)?;
		ctx.ensure_not_frozen()?;
		Ok(ctx)
	}

	fn ensure_not_frozen(&self) -> DispatchResult {
		ensure!(!self.branch.state.is_frozen(), Error::<T>::BranchFrozen);
		Ok(())
	}

	/// Returns the rate-list ID for this market.
	fn rate_list(&self) -> VaultListId<CollateralIdOf<T>, StableIdOf<T>> {
		VaultListId::Rate(self.collateral_id.clone(), self.stable_id.clone())
	}

	fn load_price(&mut self) -> DispatchResult {
		if self.price.is_some() {
			return Ok(());
		}
		let price = T::Oracle::provide_price(&self.collateral_id)?;
		self.price = Some(price);
		Ok(())
	}

	fn price(&self) -> Result<FixedU128, DispatchError> {
		self.price.defensive_ok_or(DispatchError::Corruption)
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
		let collateral_debt = collateral_for_value_ceil(projected_total, price)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		ensure!(collateral_debt <= risk.debt_ceiling, Error::<T>::GlobalDebtCeilingExceeded);
		Ok(())
	}

	/// Prepares a new vault and updates the market state in memory.
	fn create_vault(
		mut self,
		owner: &T::AccountId,
		initial_collateral: BalanceOf<T>,
		initial_debt: BalanceOf<T>,
		annual_rate: FixedU128,
		hint: ListPosition<T::AccountId>,
	) -> Result<VaultOp<T>, DispatchError> {
		let price = self.price()?;
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
		self.attach_new(owner, vault)
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
		)?;
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
		)?))
	}

	/// Records an upfront fee and defers minting until commit.
	fn charge_upfront_fee(&mut self, owner: &T::AccountId, amount: BalanceOf<T>) {
		if amount.is_zero() {
			return;
		}
		debug_assert!(self.pending_fee.is_zero(), "one upfront fee per dispatch");
		self.pending_fee = self.pending_fee.saturating_add(amount);
		Pallet::<T>::deposit_event(Event::UpfrontFeeCharged {
			collateral_id: self.collateral_id.clone(),
			stable_id: self.stable_id.clone(),
			owner: owner.clone(),
			amount,
		});
	}

	/// Applies pending interest and redistribution to a vault in memory.
	fn touch(mut self, owner: &T::AccountId) -> Result<VaultOp<T>, DispatchError> {
		debug_assert!(self.pending_fee.is_zero(), "fee charged before touch");
		let mut vault = Pallet::<T>::vault_of(&self.collateral_id, &self.stable_id, owner)?;
		let status = Pallet::<T>::vault_status_of(&self.collateral_id, &self.stable_id, owner);
		let pending =
			Pallet::<T>::apply_vault_touch(&mut self.branch.state, &mut vault, status, self.now)?;

		if !pending.interest.is_zero() {
			Pallet::<T>::deposit_event(Event::InterestAccrued {
				collateral_id: self.collateral_id.clone(),
				stable_id: self.stable_id.clone(),
				owner: owner.clone(),
				amount: pending.interest,
			});
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
		}

		// Touching only moves existing debt and collateral, so it does not change the TCR.
		Ok(VaultOp { ctx: self, owner: owner.clone(), vault, status, remove_on_commit: false })
	}

	/// Attaches a new vault without touching an existing row.
	///
	/// Its upfront fee may already be recorded.
	fn attach_new(
		self,
		owner: &T::AccountId,
		vault: Vault<BalanceOf<T>>,
	) -> Result<VaultOp<T>, DispatchError> {
		let mut op = VaultOp {
			ctx: self,
			owner: owner.clone(),
			vault,
			status: VaultStatus::Active,
			remove_on_commit: false,
		};
		op.sync_stake()?;
		Ok(op)
	}
}

impl<T: Config> VaultOp<T> {
	/// Loads an existing vault from an unfrozen branch and applies its pending changes.
	pub(crate) fn load(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<Self, DispatchError> {
		Context::<T>::load_unfrozen(collateral_id, stable_id)?.touch(owner)
	}

	/// Loads an existing vault, caching its price before touching it.
	pub(crate) fn load_priced(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<Self, DispatchError> {
		let mut ctx = Context::<T>::load_unfrozen(collateral_id, stable_id)?;
		ctx.load_price()?;
		ctx.touch(owner)
	}

	/// Prepares a new vault in an unfrozen branch.
	pub(crate) fn open(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
		initial_collateral: BalanceOf<T>,
		initial_debt: BalanceOf<T>,
		annual_rate: FixedU128,
		hint: ListPosition<T::AccountId>,
	) -> Result<Self, DispatchError> {
		let mut ctx = Context::<T>::load_unfrozen(collateral_id, stable_id)?;
		ctx.load_price()?;
		ctx.create_vault(owner, initial_collateral, initial_debt, annual_rate, hint)
	}

	/// Applies pending changes to a vault, even when its branch is frozen.
	pub(crate) fn refresh(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
	) -> DispatchResult {
		Context::<T>::load(collateral_id, stable_id)?.touch(owner)?.commit_exempt()
	}

	/// Returns the collateral asset ID.
	pub(crate) const fn collateral_id(&self) -> &CollateralIdOf<T> {
		&self.ctx.collateral_id
	}

	/// Returns the stable asset ID.
	pub(crate) const fn stable_id(&self) -> &StableIdOf<T> {
		&self.ctx.stable_id
	}

	/// Returns the vault owner.
	pub(crate) const fn owner(&self) -> &T::AccountId {
		&self.owner
	}

	/// Returns the current vault state.
	pub(crate) const fn vault(&self) -> &Vault<BalanceOf<T>> {
		&self.vault
	}

	/// Queries and caches the oracle price for this operation.
	pub(crate) fn load_price(&mut self) -> DispatchResult {
		self.ctx.load_price()
	}

	/// Applies a collateral withdrawal.
	///
	/// Returns `true` when the empty vault should be closed.
	pub(crate) fn apply_collateral_withdrawal(
		&mut self,
		amount: BalanceOf<T>,
	) -> Result<bool, DispatchError> {
		let price = self.ctx.price()?;
		ensure!(!self.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);
		let collateral_after = self
			.vault
			.collateral
			.checked_sub(&amount)
			.ok_or(Error::<T>::InsufficientCollateral)?;
		let debt = self.vault.debt.total();
		if !debt.is_zero() {
			Pallet::<T>::ensure_above_icr(
				&DebtCollateral { debt, collateral: collateral_after },
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
		self.sync_stake()?;
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
		self.sync_stake()?;
		Ok(())
	}

	/// Adds debt and optionally changes the vault rate.
	pub(crate) fn borrow(
		&mut self,
		amount: BalanceOf<T>,
		maybe_new_rate: Option<FixedU128>,
		hint: ListPosition<T::AccountId>,
	) -> DispatchResult {
		let price = self.ctx.price()?;
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
	) -> Result<crate::types::DebtBreakdown<BalanceOf<T>>, DispatchError> {
		ensure!(!self.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);
		let payment = self.redeem(amount)?;
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
	pub(crate) fn redeem(
		&mut self,
		amount: BalanceOf<T>,
	) -> Result<DebtBreakdown<BalanceOf<T>>, DispatchError> {
		let before = self.vault.clone();
		let payment = self.vault.debt.cancel(amount.min(self.vault.debt.total()));
		self.ctx.branch.state.replace_vault(Some(&before), Some(&self.vault))?;
		Ok(payment)
	}

	/// Commits the operation without checking the TCR change.
	pub(crate) fn commit_exempt(self) -> DispatchResult {
		self.persist()
	}

	/// Commits the operation after checking the TCR change.
	pub(crate) fn commit_checked(self) -> DispatchResult {
		let price = self.ctx.price()?;
		let pre_tcr = Pallet::<T>::tcr_from_inputs(&self.ctx.tcr_baseline, price)?;
		let post_tcr = Pallet::<T>::compute_tcr(&self.ctx.branch.state, price, self.ctx.now)?;
		Pallet::<T>::enforce_mode_rules(
			&self.ctx.branch.config,
			&self.ctx.branch.state,
			pre_tcr,
			post_tcr,
		)?;
		self.persist()
	}

	fn persist(self) -> DispatchResult {
		let collateral_id = self.ctx.collateral_id.clone();
		let stable_id = self.ctx.stable_id.clone();
		let key = (&collateral_id, &stable_id, &self.owner);
		if self.remove_on_commit {
			Vaults::<T>::remove(key);
		} else {
			Vaults::<T>::insert(key, &self.vault);
		}
		let Context { now, branch, pending_interest_mint, pending_fee, .. } = self.ctx;
		Pallet::<T>::commit_branch(&collateral_id, &stable_id, now, branch.state)?;

		// Mint after writing state. Keep both amounts separate to preserve fee rounding.
		if !pending_interest_mint.is_zero() {
			Pallet::<T>::mint_and_route_yield(&collateral_id, &stable_id, pending_interest_mint);
		}
		if !pending_fee.is_zero() {
			Pallet::<T>::mint_and_route_yield(&collateral_id, &stable_id, pending_fee);
		}
		Ok(())
	}
}
