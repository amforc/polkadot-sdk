//! In-memory state for one vault operation.
//!
//! [`VaultOp`] owns the loaded branch and vault drafts. Committing consumes both, preventing a
//! second touch or a mismatched write.

mod lifecycle;

use crate::{
	pallet::{
		BalanceOf, CollateralIdOf, Config, Error, Event, HoldReason, Millis, Pallet, StableIdOf,
		Vaults,
	},
	types::{
		BranchConfig, BranchState, DebtBreakdown, DebtCollateral, Vault, VaultListId, VaultRecord,
		VaultStatus,
	},
};
use frame::{
	prelude::*,
	traits::{
		fungibles::MutateHold as FungiblesMutateHold,
		tokens::{Fortitude, Precision, Restriction},
		Consideration, Convert, DefensiveOption, Time,
	},
};
use linked_list_interface::{Position as ListPosition, SortedListInterface};
use pusd_primitives::ProvidePrice;

struct Context<T: Config> {
	collateral_id: CollateralIdOf<T>,
	stable_id: StableIdOf<T>,
	now: Millis,
	config: BranchConfig<BalanceOf<T>>,
	state: BranchState<T::AccountId, BalanceOf<T>>,
	pending_interest_mint: BalanceOf<T>,
	pending_rounding_fee_mint: BalanceOf<T>,
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
	deposit: T::VaultConsideration,
	status: VaultStatus,
}

impl<T: Config> Context<T> {
	/// Loads a market and applies its pending interest in memory.
	fn load(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> Result<Self, DispatchError> {
		let now = T::TimeProvider::now();
		let stored = Pallet::<T>::branch_of(&collateral_id, &stable_id)?;
		let mut state = stored.state;
		let pending_interest_mint = Pallet::<T>::accrue_aggregate_interest(&mut state, now)?;

		// Interest is already included, so this matches the debt used by `compute_tcr`.
		let tcr_baseline = DebtCollateral {
			collateral: state.total_collateral,
			debt: Pallet::<T>::accrued_branch_debt(&state, now),
		};
		Ok(Self {
			collateral_id,
			stable_id,
			now,
			config: stored.config,
			state,
			pending_interest_mint,
			pending_rounding_fee_mint: BalanceOf::<T>::zero(),
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
		ensure!(!self.state.is_frozen(), Error::<T>::BranchFrozen);
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

	/// Checks the stablecoin-wide debt limit against this operation's post-state.
	fn ensure_global_ceiling(&self) -> DispatchResult {
		let projected_total = Pallet::<T>::projected_stablecoin_debt(
			&self.collateral_id,
			&self.stable_id,
			&self.state,
			self.now,
		)?;
		ensure!(
			projected_total <= T::GlobalDebtCeiling::convert(self.stable_id.clone()),
			Error::<T>::GlobalDebtCeilingExceeded
		);
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
			initial_collateral >= self.config.minimum_collateral,
			Error::<T>::InsufficientCollateral
		);
		let mut vault =
			Pallet::<T>::open_scratch_row(&self.state, annual_rate, initial_collateral, self.now);
		let upfront_fee =
			self.apply_checked_borrow(&mut vault, initial_debt, annual_rate, price)?;
		let total_collateral = self
			.state
			.total_collateral
			.checked_add(&initial_collateral)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.state.total_collateral = total_collateral;
		self.charge_upfront_fee(owner, upfront_fee);
		// Charged only after every in-memory check, so a rejected open reports the validation
		// error rather than the deposit's.
		let deposit = T::VaultConsideration::new(
			owner,
			Pallet::<T>::vault_footprint(&self.collateral_id, &self.stable_id, owner),
		)?;
		T::VaultLists::insert(self.rate_list(), owner.clone(), annual_rate, hint)
			.map_err(Pallet::<T>::map_error)?;
		self.attach_new(owner, vault, deposit)
	}

	fn apply_checked_borrow(
		&mut self,
		vault: &mut Vault<BalanceOf<T>>,
		amount: BalanceOf<T>,
		new_rate: FixedU128,
		price: FixedU128,
	) -> Result<BalanceOf<T>, DispatchError> {
		Pallet::<T>::validate_rate(&self.config, new_rate)?;
		let vault_principal_after = vault
			.debt
			.principal
			.checked_add(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		ensure!(vault_principal_after >= self.config.minimum_debt, Error::<T>::DebtBelowMinimum);

		let principal_after = self
			.state
			.debt
			.principal
			.checked_add(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		ensure!(principal_after <= self.config.debt_ceiling, Error::<T>::DebtCeilingExceeded);
		let upfront_fee = Pallet::<T>::apply_borrow_unchecked(
			&mut self.state,
			&self.config,
			vault,
			amount,
			new_rate,
			self.now,
		)?;
		self.ensure_global_ceiling()?;
		Pallet::<T>::ensure_above_icr(&vault.position(), price, &self.config)?;
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
		Pallet::<T>::validate_rate(&self.config, new_rate)?;
		Ok(Some(Pallet::<T>::apply_rate_change(
			&mut self.state,
			&self.config,
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
		let VaultRecord { mut vault, deposit } =
			Pallet::<T>::record_of(&self.collateral_id, &self.stable_id, owner)?;
		let status = Pallet::<T>::vault_status_of(&self.collateral_id, &self.stable_id, owner);
		let (pending, interest_to_mint) =
			Pallet::<T>::apply_vault_touch(&mut self.state, &mut vault, status, self.now)?;
		self.pending_interest_mint = self
			.pending_interest_mint
			.checked_add(&interest_to_mint)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		if !pending.redistribution.collateral.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				self.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				&Pallet::<T>::redistribution_account(&self.collateral_id, &self.stable_id),
				owner,
				pending.redistribution.collateral,
				Precision::Exact,
				Restriction::OnHold,
				Fortitude::Polite,
			)?;
		}

		if !pending.interest.interest.is_zero() {
			Pallet::<T>::deposit_event(Event::InterestAccrued {
				collateral_id: self.collateral_id.clone(),
				stable_id: self.stable_id.clone(),
				owner: owner.clone(),
				amount: pending.interest.interest,
			});
		}
		// A touch only realizes accrued interest, so it does not change the TCR.
		Ok(VaultOp { ctx: self, owner: owner.clone(), vault, deposit, status })
	}

	/// Attaches a new vault without touching an existing row.
	///
	/// Its upfront fee may already be recorded.
	fn attach_new(
		mut self,
		owner: &T::AccountId,
		vault: Vault<BalanceOf<T>>,
		deposit: T::VaultConsideration,
	) -> Result<VaultOp<T>, DispatchError> {
		self.state.vault_count =
			self.state.vault_count.checked_add(1).ok_or(Error::<T>::ArithmeticOverflow)?;
		let mut op = VaultOp {
			ctx: self,
			owner: owner.clone(),
			vault,
			deposit,
			status: VaultStatus::Active,
		};
		op.sync_stake()?;
		Ok(op)
	}
}

impl<T: Config> VaultOp<T> {
	/// Loads an existing vault in any branch mode and applies its pending changes.
	///
	/// A frozen branch stops interest time, so the touch realizes nothing new. Only operations
	/// that need no price and cannot raise risk, such as a collateral deposit or a repayment,
	/// may load this way; the rest use [`Self::load_unfrozen`] or [`Self::load_priced`].
	pub(crate) fn load(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<Self, DispatchError> {
		Context::<T>::load(collateral_id, stable_id)?.touch(owner)
	}

	/// Loads an existing vault from an unfrozen branch and applies its pending changes.
	pub(crate) fn load_unfrozen(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<Self, DispatchError> {
		Context::<T>::load_unfrozen(collateral_id, stable_id)?.touch(owner)
	}

	/// Loads an existing vault from an unfrozen branch, caching its price before touching it.
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
		Self::load(collateral_id, stable_id, owner)?.commit_exempt()
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
				&self.ctx.config,
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
		let before = self.vault.clone();
		let vault_collateral = self
			.vault
			.collateral
			.checked_add(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		let branch_collateral = self
			.ctx
			.state
			.total_collateral
			.checked_add(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.vault.collateral = vault_collateral;
		self.ctx.state.total_collateral = branch_collateral;
		self.sync_stake_from(before)?;
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
		let before = self.vault.clone();
		let branch_collateral = self
			.ctx
			.state
			.total_collateral
			.checked_sub(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.vault.collateral = vault_collateral;
		self.ctx.state.total_collateral = branch_collateral;
		self.sync_stake_from(before)?;
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
				self.vault.debt.total() >= self.ctx.config.minimum_debt,
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

	/// Charges the terminal unit before the vault stops owning the liability.
	pub(super) fn finalize_terminal_interest(&mut self) -> Result<BalanceOf<T>, DispatchError> {
		let charge = self.vault.terminal_interest_charge();
		if charge.is_zero() {
			return Ok(charge);
		}
		let uncovered = self
			.ctx
			.state
			.debt
			.attribute_interest(charge)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.ctx.pending_rounding_fee_mint = self
			.ctx
			.pending_rounding_fee_mint
			.checked_add(&uncovered)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.vault.debt.interest = self
			.vault
			.debt
			.interest
			.checked_add(&charge)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.vault.interest_remainder = 0;
		Pallet::<T>::deposit_event(Event::InterestRoundingFeeCharged {
			collateral_id: self.ctx.collateral_id.clone(),
			stable_id: self.ctx.stable_id.clone(),
			owner: self.owner.clone(),
			amount: charge,
		});
		Ok(charge)
	}

	/// Repays debt while enforcing the minimum remaining debt.
	///
	/// A `FinalRecovery` vault may repay too: the payment only lowers its debt, and a vault that
	/// stays below par afterwards still settles under recovery pricing.
	///
	/// Returns the principal and interest removed.
	pub(crate) fn repay(
		&mut self,
		amount: BalanceOf<T>,
	) -> Result<crate::types::DebtBreakdown<BalanceOf<T>>, DispatchError> {
		let payment = self.cancel_debt(amount)?;
		let total_after = self.vault.debt.total();
		ensure!(
			total_after.is_zero() || total_after >= self.ctx.config.minimum_debt,
			Error::<T>::DebtWouldBecomeDust
		);
		Ok(payment)
	}

	/// Cancels an exact debt payment without a minimum-debt check.
	///
	/// A full payment includes terminal interest. A partial payment must preserve one base-debt
	/// unit when the vault has fractional interest.
	///
	/// Returns the principal and interest removed.
	pub(crate) fn cancel_debt(
		&mut self,
		amount: BalanceOf<T>,
	) -> Result<DebtBreakdown<BalanceOf<T>>, DispatchError> {
		let base_debt = self.vault.debt.total();
		let full_payoff = base_debt
			.checked_add(&self.vault.terminal_interest_charge())
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		if amount == full_payoff {
			self.finalize_terminal_interest()?;
		} else {
			// Preserve a liability owner for the terminal interest.
			ensure!(amount < base_debt, Error::<T>::InvalidRedemptionSettlement);
		}
		let before = self.vault.clone();
		let payment = self.vault.debt.cancel(amount);
		debug_assert_eq!(payment.total(), amount);
		self.ctx.state.replace_vault(Some(&before), Some(&self.vault))?;
		Ok(payment)
	}

	/// Commits the operation without checking the TCR change.
	pub(crate) fn commit_exempt(self) -> DispatchResult {
		self.persist(false)
	}

	/// Commits the operation after checking the TCR change.
	pub(crate) fn commit_checked(self) -> DispatchResult {
		self.ensure_checked_commit()?;
		self.persist(false)
	}

	fn ensure_checked_commit(&self) -> DispatchResult {
		let price = self.ctx.price()?;
		let pre_tcr = Pallet::<T>::tcr_from_inputs(&self.ctx.tcr_baseline, price)?;
		let post_tcr = Pallet::<T>::compute_tcr(&self.ctx.state, price, self.ctx.now)?;
		Pallet::<T>::enforce_mode_rules(&self.ctx.config, &self.ctx.state, pre_tcr, post_tcr)?;
		Ok(())
	}

	fn persist(self, remove: bool) -> DispatchResult {
		let VaultOp { ctx, owner, vault, deposit, .. } = self;
		let collateral_id = ctx.collateral_id.clone();
		let stable_id = ctx.stable_id.clone();
		let key = (&collateral_id, &stable_id, &owner);
		if remove {
			Vaults::<T>::remove(key);
			// The row is gone, so its deposit returns to the owner: on close and on liquidation
			// alike, as the ticket is attributable to the owner only.
			deposit.drop(&owner)?;
		} else {
			Vaults::<T>::insert(key, &VaultRecord { vault, deposit });
		}
		let Context {
			now,
			state,
			pending_interest_mint,
			pending_rounding_fee_mint,
			pending_fee,
			..
		} = ctx;
		Pallet::<T>::commit_branch(&collateral_id, &stable_id, now, state)?;

		// Mint after writing state. Keep both amounts separate to preserve fee rounding.
		if !pending_interest_mint.is_zero() {
			Pallet::<T>::mint_and_route_yield(&collateral_id, &stable_id, pending_interest_mint)?;
		}
		if !pending_fee.is_zero() {
			Pallet::<T>::mint_and_route_yield(&collateral_id, &stable_id, pending_fee)?;
		}
		if !pending_rounding_fee_mint.is_zero() {
			Pallet::<T>::mint_rounding_fee(&stable_id, pending_rounding_fee_mint)?;
		}
		Ok(())
	}
}
