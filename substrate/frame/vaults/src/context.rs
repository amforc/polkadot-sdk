//! In-memory state for one vault operation.
//!
//! [`VaultOp`] owns the loaded branch and vault drafts. Committing consumes both, preventing a
//! second touch or a mismatched write.

mod lifecycle;

use crate::{
	pallet::{
		BalanceOf, BranchOf, CollateralIdOf, Config, Error, Event, HoldReason, Millis, Pallet,
		StableIdOf, Vaults,
	},
	types::{
		DebtBreakdown, DebtCollateral, LiquidationTouch, Vault, VaultListId, VaultRecord,
		VaultStatus,
	},
	utility_impls::BranchContribution,
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
use pusd_primitives::{
	collateralization_ratio, BranchMode, BranchSnapshot, CollateralRatio, ProvidePrice,
};

struct Context<T: Config> {
	collateral_id: CollateralIdOf<T>,
	stable_id: StableIdOf<T>,
	now: Millis,
	branch: BranchOf<T>,
	stored_contribution: BranchContribution<BalanceOf<T>>,
	pending_interest_mint: BalanceOf<T>,
	pending_fee: BalanceOf<T>,
	tcr_baseline: DebtCollateral<BalanceOf<T>>,
	price: Option<FixedU128>,
}

/// Whether a commit runs the collateralization mode gate before persisting.
pub(crate) enum Commit {
	/// Enforce the mode rules, as borrower operations must.
	Checked,
	/// Skip the gate, for operations that cannot raise risk or are already exempt from it.
	Exempt,
}

/// Where a touch leaves the vault's pending redistribution share.
pub(crate) enum ShareCustody {
	/// Moved onto the owner's hold, as every operation that keeps the vault needs.
	Owner,
	/// Left on the redistribution account, for a liquidation that seizes it anyway.
	Retained,
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
	/// Redistribution collateral the vault owns but the redistribution account still holds.
	retained_redistribution: BalanceOf<T>,
	/// For a liquidation, what the touch realized. It rides on the liquidation event instead of
	/// events of its own; `None` for every other operation, which emits them.
	liquidation_touch: Option<LiquidationTouch<BalanceOf<T>>>,
}

impl<T: Config> Context<T> {
	/// Loads a market and applies its pending interest in memory.
	fn load(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> Result<Self, DispatchError> {
		let now = T::TimeProvider::now();
		let mut branch = Pallet::<T>::branch_of(&collateral_id, &stable_id)?;
		let stored_contribution = Pallet::<T>::branch_contribution(&branch.state, now)?;
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
			stored_contribution,
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

	fn load_priced(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> Result<Self, DispatchError> {
		let mut ctx = Self::load_unfrozen(collateral_id, stable_id)?;
		ctx.load_price()?;
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

	/// The market as this operation hands it to the Stability Pool.
	///
	/// The mode comes from the loaded state and the cached price, so the pool needs no branch,
	/// oracle, or clock read of its own. An operation that loaded no price reads the oracle once
	/// here. A price the oracle cannot give, or a ratio that does not compute, reports `Frozen`:
	/// the pool then declines, which is what it does for a market it cannot classify.
	fn branch_snapshot(&self) -> BranchSnapshot {
		let mode = if self.branch.state.is_frozen() {
			BranchMode::Frozen
		} else {
			let price = match self.price {
				Some(price) => Ok(price),
				None => T::Oracle::provide_price(&self.collateral_id),
			};
			price
				.and_then(|price| {
					Pallet::<T>::mode_at_price(
						&self.branch.state,
						&self.branch.config,
						price,
						self.now,
					)
				})
				.unwrap_or(BranchMode::Frozen)
		};
		BranchSnapshot { mode, now: self.now }
	}

	fn collateralization_ratio(
		&self,
		position: &DebtCollateral<BalanceOf<T>>,
	) -> Result<CollateralRatio, DispatchError> {
		Ok(collateralization_ratio(position, self.price()?)?)
	}

	/// Ensures a vault's collateralization ratio is at or above this market's ICR.
	fn ensure_above_icr(&self, position: &DebtCollateral<BalanceOf<T>>) -> DispatchResult {
		let cr = self.collateralization_ratio(position)?;
		ensure!(
			cr >= self.branch.config.initial_collateralization_ratio,
			Error::<T>::UnsafeCollateralizationRatio
		);
		Ok(())
	}

	/// Ensures a vault's collateralization ratio is strictly below this market's MCR.
	fn ensure_below_mcr(&self, position: &DebtCollateral<BalanceOf<T>>) -> DispatchResult {
		let cr = self.collateralization_ratio(position)?;
		ensure!(
			cr < self.branch.config.minimum_collateralization_ratio,
			Error::<T>::CollateralizationRatioTooHealthy
		);
		Ok(())
	}

	/// Ensures a vault's collateralization ratio is at or above this market's MCR.
	fn ensure_at_or_above_mcr(&self, position: &DebtCollateral<BalanceOf<T>>) -> DispatchResult {
		let cr = self.collateralization_ratio(position)?;
		ensure!(
			cr >= self.branch.config.minimum_collateralization_ratio,
			Error::<T>::CollateralizationRatioTooLow
		);
		Ok(())
	}

	fn ensure_valid_rate(&self, rate: FixedU128) -> DispatchResult {
		Pallet::<T>::validate_rate(&self.branch.config, rate)
	}

	fn apply_borrow_transition(
		&mut self,
		vault: &mut Vault<BalanceOf<T>>,
		debt_increase: BalanceOf<T>,
		new_rate: FixedU128,
	) -> Result<BalanceOf<T>, DispatchError> {
		Pallet::<T>::apply_borrow_unchecked(
			&mut self.branch.state,
			&self.branch.config,
			vault,
			debt_increase,
			new_rate,
			self.now,
		)
	}

	fn post_tcr(&self) -> Result<CollateralRatio, DispatchError> {
		Pallet::<T>::compute_tcr(&self.branch.state, self.price()?, self.now)
	}

	/// Applies the Normal or Safety mode rule to this operation's post-state.
	fn ensure_mode_rules(&self) -> DispatchResult {
		let price = self.price()?;
		let pre_tcr = collateralization_ratio(&self.tcr_baseline, price)?;
		let post_tcr = self.post_tcr()?;
		if self.branch.state.is_frozen() {
			return Err(Error::<T>::BranchFrozen.into());
		}
		if pre_tcr < self.branch.config.safety_collateralization_ratio {
			ensure!(post_tcr >= pre_tcr, Error::<T>::SafetyModeTcrWorsening);
		} else {
			ensure!(
				post_tcr >= self.branch.config.safety_collateralization_ratio,
				Error::<T>::WouldEnterSafetyMode
			);
		}
		Ok(())
	}

	/// Checks the stablecoin-wide debt limit against this operation's post-state.
	fn ensure_global_ceiling(&self) -> DispatchResult {
		let projected_total = Pallet::<T>::projected_stablecoin_debt(
			&self.stable_id,
			&self.stored_contribution,
			&self.branch.state,
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
		let upfront_fee = self.apply_checked_borrow(&mut vault, initial_debt, annual_rate)?;
		let total_collateral = self
			.branch
			.state
			.total_collateral
			.checked_add(&initial_collateral)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.branch.state.total_collateral = total_collateral;
		// The vault is announced before its fee, so a reader of the events never sees a charge
		// against a vault it does not know yet.
		Pallet::<T>::deposit_event(Event::VaultOpened {
			collateral_id: self.collateral_id.clone(),
			stable_id: self.stable_id.clone(),
			owner: owner.clone(),
			collateral: initial_collateral,
			debt: initial_debt,
			annual_rate,
		});
		self.charge_upfront_fee(owner, upfront_fee);
		// Charged only after every in-memory check, so a rejected open reports the validation
		// error rather than the deposit's.
		let deposit = T::VaultConsideration::new(
			owner,
			Pallet::<T>::vault_footprint(&self.collateral_id, &self.stable_id, owner),
		)?;
		let op = self.attach_new(owner, vault, deposit)?;
		op.index_insert(hint)?;
		Ok(op)
	}

	fn apply_checked_borrow(
		&mut self,
		vault: &mut Vault<BalanceOf<T>>,
		amount: BalanceOf<T>,
		new_rate: FixedU128,
	) -> Result<BalanceOf<T>, DispatchError> {
		self.ensure_valid_rate(new_rate)?;
		let vault_principal_after = vault
			.debt
			.principal
			.checked_add(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		ensure!(
			vault_principal_after >= self.branch.config.minimum_debt,
			Error::<T>::DebtBelowMinimum
		);

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
		let upfront_fee = self.apply_borrow_transition(vault, amount, new_rate)?;
		self.ensure_global_ceiling()?;
		self.ensure_above_icr(&vault.position())?;
		Ok(upfront_fee)
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
	///
	/// The vault's redistribution share moves onto the owner's hold, or stays in custody when
	/// `custody` says so: a liquidation seizes every unit the owner holds, so moving the share
	/// first only adds a round trip through the owner's account.
	fn touch(
		mut self,
		owner: &T::AccountId,
		custody: ShareCustody,
	) -> Result<VaultOp<T>, DispatchError> {
		debug_assert!(self.pending_fee.is_zero(), "fee charged before touch");
		let VaultRecord { mut vault, deposit } =
			Pallet::<T>::record_of(&self.collateral_id, &self.stable_id, owner)?;
		let status = Pallet::<T>::vault_status_of(&self.collateral_id, &self.stable_id, owner);
		let (pending, interest_to_mint) =
			Pallet::<T>::apply_vault_touch(&mut self.branch.state, &mut vault, status, self.now)?;
		self.pending_interest_mint = self
			.pending_interest_mint
			.checked_add(&interest_to_mint)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		// Rounding the vault's interest up can add a unit the aggregate did not project. That unit
		// is the protocol's, so the baseline takes it and the operation answers only for its own
		// change.
		self.tcr_baseline.debt = Pallet::<T>::accrued_branch_debt(&self.branch.state, self.now);
		let retained_redistribution = match custody {
			ShareCustody::Retained => pending.redistribution.collateral,
			ShareCustody::Owner => {
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
				BalanceOf::<T>::zero()
			},
		};

		let liquidation_touch = match custody {
			ShareCustody::Retained => Some(LiquidationTouch {
				interest: pending.interest,
				redistribution: pending.redistribution,
			}),
			ShareCustody::Owner => {
				Self::emit_touch_events(
					&self.collateral_id,
					&self.stable_id,
					owner,
					pending.interest,
					pending.redistribution,
				);
				None
			},
		};
		Ok(VaultOp {
			ctx: self,
			owner: owner.clone(),
			vault,
			deposit,
			status,
			retained_redistribution,
			liquidation_touch,
		})
	}

	// Reports what a touch realized on a vault the operation keeps.
	fn emit_touch_events(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		owner: &T::AccountId,
		interest: BalanceOf<T>,
		redistribution: DebtCollateral<BalanceOf<T>>,
	) {
		if !interest.is_zero() {
			Pallet::<T>::deposit_event(Event::InterestAccrued {
				collateral_id: collateral_id.clone(),
				stable_id: stable_id.clone(),
				owner: owner.clone(),
				amount: interest,
			});
		}
		// No other event carries a vault's share: it follows from the stake, which events omit.
		let DebtCollateral { debt, collateral } = redistribution;
		if !debt.is_zero() || !collateral.is_zero() {
			Pallet::<T>::deposit_event(Event::RedistributionApplied {
				collateral_id: collateral_id.clone(),
				stable_id: stable_id.clone(),
				owner: owner.clone(),
				debt,
				collateral,
			});
		}
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
		self.branch.state.vault_count = self
			.branch
			.state
			.vault_count
			.checked_add(1)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		let mut op = VaultOp {
			ctx: self,
			owner: owner.clone(),
			vault,
			deposit,
			status: VaultStatus::Active,
			retained_redistribution: BalanceOf::<T>::zero(),
			liquidation_touch: None,
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
		Context::<T>::load(collateral_id, stable_id)?.touch(owner, ShareCustody::Owner)
	}

	/// Loads an existing vault from an unfrozen branch and applies its pending changes.
	pub(crate) fn load_unfrozen(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<Self, DispatchError> {
		Context::<T>::load_unfrozen(collateral_id, stable_id)?.touch(owner, ShareCustody::Owner)
	}

	/// Loads an existing vault from an unfrozen branch, caching its price before touching it.
	pub(crate) fn load_priced(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<Self, DispatchError> {
		Context::<T>::load_priced(collateral_id, stable_id)?.touch(owner, ShareCustody::Owner)
	}

	/// Loads a vault for liquidation: priced, from an unfrozen branch, with its redistribution
	/// share left in custody.
	///
	/// The loaded vault still counts the share, so the owner holds
	/// [`Self::retained_redistribution`] less than [`Self::vault`] records.
	pub(crate) fn load_for_liquidation(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<Self, DispatchError> {
		Context::<T>::load_priced(collateral_id, stable_id)?.touch(owner, ShareCustody::Retained)
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
		let ctx = Context::<T>::load_priced(collateral_id, stable_id)?;
		ctx.create_vault(owner, initial_collateral, initial_debt, annual_rate, hint)
	}

	/// Applies pending changes to a vault, even when its branch is frozen.
	pub(crate) fn refresh(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
	) -> DispatchResult {
		Self::load(collateral_id, stable_id, owner)?.commit(Commit::Exempt)
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

	/// What the touch realized, for an operation loaded with [`Self::load_for_liquidation`].
	pub(crate) fn liquidation_touch(
		&self,
	) -> Result<LiquidationTouch<BalanceOf<T>>, DispatchError> {
		self.liquidation_touch.defensive_ok_or(DispatchError::Corruption)
	}

	/// Returns the vault's redistribution collateral that the redistribution account still holds.
	///
	/// Nonzero only for a vault loaded with [`Self::load_for_liquidation`].
	pub(crate) const fn retained_redistribution(&self) -> BalanceOf<T> {
		self.retained_redistribution
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
		ensure!(!self.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);
		let collateral_after = self
			.vault
			.collateral
			.checked_sub(&amount)
			.ok_or(Error::<T>::InsufficientCollateral)?;
		let debt = self.vault.debt.total();
		self.ctx
			.ensure_above_icr(&DebtCollateral { debt, collateral: collateral_after })?;
		if debt.is_zero() && collateral_after.is_zero() {
			return Ok(true);
		}
		self.remove_collateral(amount)?;
		Ok(false)
	}

	fn rate_list(&self) -> VaultListId<CollateralIdOf<T>, StableIdOf<T>> {
		self.ctx.rate_list()
	}

	/// Places the vault in the rate index at its current rate.
	pub(super) fn index_insert(&self, hint: ListPosition<T::AccountId>) -> DispatchResult {
		T::VaultLists::insert(self.rate_list(), self.owner.clone(), self.vault.annual_rate, hint)
			.map(|_| ())
			.map_err(|e| Pallet::<T>::map_error(e).into())
	}

	/// Removes the vault from the rate index, where it must be present.
	pub(super) fn index_remove(&self) -> DispatchResult {
		T::VaultLists::remove(&self.rate_list(), &self.owner)
			.map_err(|_| Error::<T>::RateIndexInvariantBroken.into())
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
			.branch
			.state
			.total_collateral
			.checked_add(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.vault.collateral = vault_collateral;
		self.ctx.branch.state.total_collateral = branch_collateral;
		self.sync_stake_from(before)?;
		Ok(())
	}

	/// Removes collateral from the vault and market totals.
	pub(crate) fn remove_collateral(&mut self, amount: BalanceOf<T>) -> DispatchResult {
		let before = self.vault.clone();
		let vault_collateral = self
			.vault
			.collateral
			.checked_sub(&amount)
			.ok_or(Error::<T>::InsufficientCollateral)?;
		let branch_collateral = self
			.ctx
			.branch
			.state
			.total_collateral
			.checked_sub(&amount)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		self.vault.collateral = vault_collateral;
		self.ctx.branch.state.total_collateral = branch_collateral;
		self.sync_stake_from(before)?;
		Ok(())
	}

	/// Transfers held vault collateral from the owner to `to`, released from the hold.
	///
	/// Moves funds only; the caller adjusts vault and market accounting.
	pub(crate) fn release_collateral(
		&self,
		to: &T::AccountId,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		T::CollateralAssets::transfer_on_hold(
			self.collateral_id().clone(),
			&HoldReason::VaultCollateral.into(),
			&self.owner,
			to,
			amount,
			Precision::Exact,
			Restriction::Free,
			Fortitude::Polite,
		)?;
		Ok(())
	}

	/// Adds debt and optionally changes the vault rate.
	pub(crate) fn borrow(
		&mut self,
		amount: BalanceOf<T>,
		maybe_new_rate: Option<FixedU128>,
		hint: ListPosition<T::AccountId>,
	) -> DispatchResult {
		ensure!(!self.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);
		let old_rate = self.vault.annual_rate;
		let new_rate = maybe_new_rate.unwrap_or(old_rate);
		let dormant_to_active = self.status.is_dormant();
		let rate_changed = old_rate != new_rate;
		let upfront_fee = self.ctx.apply_checked_borrow(&mut self.vault, amount, new_rate)?;
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
			self.emit_rate_changed(old_rate);
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
		if old_rate == new_rate {
			return Ok(false);
		}
		self.ctx.ensure_valid_rate(new_rate)?;
		// A rate change is a borrow of nothing, so both are priced by the same rule.
		let upfront_fee =
			self.ctx
				.apply_borrow_transition(&mut self.vault, BalanceOf::<T>::zero(), new_rate)?;
		self.ctx.charge_upfront_fee(&self.owner, upfront_fee);
		self.reindex(hint)?;
		self.emit_rate_changed(old_rate);
		Ok(true)
	}

	/// Reports the move from `old_rate` to the rate the vault now carries.
	fn emit_rate_changed(&self, old_rate: FixedU128) {
		debug_assert_ne!(old_rate, self.vault.annual_rate);
		Pallet::<T>::deposit_event(Event::BorrowRateChanged {
			collateral_id: self.ctx.collateral_id.clone(),
			stable_id: self.ctx.stable_id.clone(),
			owner: self.owner.clone(),
			old_rate,
			new_rate: self.vault.annual_rate,
		});
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
	) -> Result<DebtBreakdown<BalanceOf<T>>, DispatchError> {
		let payment = self.cancel_debt(amount)?;
		let total_after = self.vault.debt.total();
		ensure!(
			total_after.is_zero() || total_after >= self.ctx.branch.config.minimum_debt,
			Error::<T>::DebtWouldBecomeDust
		);
		Ok(payment)
	}

	/// Cancels an exact debt payment without a minimum-debt check.
	///
	/// A payment that clears the debt also forfeits the interest the vault prepaid, so a debt-free
	/// vault carries no excess.
	///
	/// Returns the principal and interest removed.
	pub(crate) fn cancel_debt(
		&mut self,
		amount: BalanceOf<T>,
	) -> Result<DebtBreakdown<BalanceOf<T>>, DispatchError> {
		ensure!(amount <= self.vault.debt.total(), Error::<T>::InvalidRedemptionSettlement);
		let before = self.vault.clone();
		let payment = self.vault.debt.cancel(amount);
		debug_assert_eq!(payment.total(), amount);
		if self.vault.debt.total().is_zero() {
			self.vault.interest_prepaid = 0;
		}
		self.ctx.branch.state.replace_vault(Some(&before), Some(&self.vault))?;
		Ok(payment)
	}

	/// The market as this operation hands it to the Stability Pool.
	pub(crate) fn branch_snapshot(&self) -> BranchSnapshot {
		self.ctx.branch_snapshot()
	}

	/// Commits the operation, running the mode gate when `commit` asks for it.
	pub(crate) fn commit(self, commit: Commit) -> DispatchResult {
		match commit {
			Commit::Checked => self.ctx.ensure_mode_rules()?,
			Commit::Exempt => {},
		}
		self.persist(false)
	}

	/// Writes the vault and the market, then issues what the operation owes.
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
		// Taken before the state moves into storage; the stored row equals it once committed, so
		// the yield route sees the market the engine just wrote. A commit that mints nothing
		// takes none, so it reads no oracle.
		let yield_due = !ctx.pending_interest_mint.is_zero() || !ctx.pending_fee.is_zero();
		let branch = if yield_due { Some(ctx.branch_snapshot()) } else { None };
		let Context {
			now,
			branch: branch_row,
			stored_contribution,
			pending_interest_mint,
			pending_fee,
			..
		} = ctx;
		Pallet::<T>::commit_branch(
			&collateral_id,
			&stable_id,
			now,
			&stored_contribution,
			branch_row,
		)?;

		// Mint after writing state.
		if let Some(branch) = branch {
			Pallet::<T>::issue_interest(&collateral_id, &stable_id, branch, pending_interest_mint)?;
			if !pending_fee.is_zero() {
				Pallet::<T>::mint_and_route_yield(&collateral_id, &stable_id, branch, pending_fee)?;
			}
		}
		Ok(())
	}
}
