//! Implementations for pallet extrinsics.

use crate::{
	context::{Commit, VaultOp},
	pallet::{
		BalanceOf, Branches, CollateralIdOf, Config, Error, Event, GlobalDebtCeilings, HoldReason,
		Pallet, RegistrationConfigOf, StableIdOf, StablecoinMarkets, Vaults,
	},
	types::{
		AdminLevel, AssetMinimums, BranchAdmins, BranchConfig, BranchConfigUpdate, BranchMode,
		BranchState, FrozenReason, FrozenState,
	},
};
use frame::{
	prelude::{
		storage::{StorageDoubleMap as _, StorageNMap as _},
		*,
	},
	traits::{
		fungibles::{
			Inspect as FungiblesInspect, Mutate as FungiblesMutate,
			MutateHold as FungiblesMutateHold,
		},
		tokens::Restriction,
		Consideration, Footprint, Time,
	},
};
use linked_list_interface::Position;
use pusd_primitives::{OnBranchLifecycle, ProvidePrice};

impl<T: Config> Pallet<T> {
	/// Opens a vault with collateral, debt, and an interest rate.
	pub(crate) fn do_open_vault(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		initial_collateral: BalanceOf<T>,
		initial_debt: BalanceOf<T>,
		annual_rate: FixedU128,
		hint: Position<T::AccountId>,
	) -> DispatchResult {
		let op = VaultOp::<T>::open(
			collateral_id,
			stable_id,
			&owner,
			initial_collateral,
			initial_debt,
			annual_rate,
			hint,
		)?;

		T::CollateralAssets::hold(
			op.collateral_id().clone(),
			&HoldReason::VaultCollateral.into(),
			&owner,
			initial_collateral,
		)?;
		T::StableAssets::mint_into(op.stable_id().clone(), &owner, initial_debt)?;
		Self::deposit_event(Event::VaultOpened {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner,
			collateral: initial_collateral,
			debt: initial_debt,
		});
		op.commit(Commit::Checked)
	}

	/// Deposits collateral into a vault.
	///
	/// Dormant vaults cannot receive deposits.
	pub(crate) fn do_deposit_collateral_for(
		from: T::AccountId,
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		ensure!(!amount.is_zero(), Error::<T>::ZeroAmount);
		// A deposit needs no price and only lowers risk, so a frozen branch still accepts it.
		let mut op = VaultOp::<T>::load(collateral_id, stable_id, &owner)?;
		op.add_collateral(amount)?;
		T::CollateralAssets::transfer_and_hold(
			op.collateral_id().clone(),
			&HoldReason::VaultCollateral.into(),
			&from,
			&owner,
			amount,
			Precision::Exact,
			Preservation::Expendable,
			Fortitude::Polite,
		)?;

		Self::deposit_event(Event::CollateralDeposited {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner,
			from,
			amount,
		});
		op.commit(Commit::Exempt)
	}

	/// Withdraws collateral and closes the vault if it becomes empty.
	pub(crate) fn do_withdraw_collateral(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
		recipient: T::AccountId,
	) -> DispatchResult {
		ensure!(!amount.is_zero(), Error::<T>::ZeroAmount);
		let mut op = VaultOp::<T>::load_priced(collateral_id, stable_id, &owner)?;
		if op.apply_collateral_withdrawal(amount)? {
			return op.finish_close(&recipient, Commit::Checked);
		}

		T::CollateralAssets::transfer_on_hold(
			op.collateral_id().clone(),
			&HoldReason::VaultCollateral.into(),
			&owner,
			&recipient,
			amount,
			Precision::Exact,
			Restriction::Free,
			Fortitude::Polite,
		)?;

		Self::deposit_event(Event::CollateralWithdrawn {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner,
			recipient,
			amount,
		});
		op.commit(Commit::Checked)
	}

	/// Borrows stable assets and optionally changes the vault rate.
	pub(crate) fn do_borrow(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
		maybe_new_rate: Option<FixedU128>,
		recipient: T::AccountId,
		hint: Position<T::AccountId>,
	) -> DispatchResult {
		ensure!(!amount.is_zero(), Error::<T>::ZeroAmount);
		let mut op = VaultOp::<T>::load_priced(collateral_id, stable_id, &owner)?;
		op.borrow(amount, maybe_new_rate, hint)?;

		T::StableAssets::mint_into(op.stable_id().clone(), &recipient, amount)?;

		Self::deposit_event(Event::Borrowed {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner,
			recipient,
			amount,
		});
		op.commit(Commit::Checked)
	}

	/// Repays debt for a vault from another account.
	///
	/// `None` uses the live payoff and pays the terminal interest charge.
	pub(crate) fn do_repay_for(
		from: T::AccountId,
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: Option<BalanceOf<T>>,
	) -> DispatchResult {
		if let Some(amount) = amount {
			ensure!(!amount.is_zero(), Error::<T>::ZeroAmount);
		}
		// A repayment needs no price and only lowers risk, so a frozen branch still accepts it.
		let mut op = VaultOp::<T>::load(collateral_id, stable_id, &owner)?;
		let debt_before_terminal = op.vault().debt.total();
		let full_payoff = debt_before_terminal
			.checked_add(&op.vault().terminal_interest_charge())
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		// The live repayment must not exceed the requested amount or payoff.
		let repay = amount.map_or(full_payoff, |amount| amount.min(full_payoff));
		if repay >= debt_before_terminal {
			// Settling the debt without its terminal charge would strand the interest remainder.
			ensure!(
				amount.is_none_or(|amount| amount >= full_payoff),
				Error::<T>::TerminalChargeUnpaid
			);
		}
		T::StableAssets::burn_from(
			op.stable_id().clone(),
			&from,
			repay,
			Preservation::Expendable,
			Precision::Exact,
			Fortitude::Polite,
		)?;

		let payment = op.repay(repay)?;
		debug_assert_eq!(payment.total(), repay);

		let new_total = op.vault().debt.total();

		Self::deposit_event(Event::Repaid {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner: owner.clone(),
			from,
			amount: repay,
		});

		// Close a fully repaid vault when it has no collateral left. The close itself refuses a
		// frozen branch.
		if new_total.is_zero() && op.vault().collateral.is_zero() {
			op.load_price()?;
			return op.finish_close(&owner, Commit::Checked);
		}

		op.reconcile_after_debt_reduction()?;
		op.commit(Commit::Exempt)
	}

	/// Changes a vault's interest rate.
	pub(crate) fn do_change_rate(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		new_rate: FixedU128,
		hint: Position<T::AccountId>,
	) -> DispatchResult {
		let mut op = VaultOp::<T>::load_unfrozen(collateral_id, stable_id, &owner)?;
		if !op.change_rate(new_rate, hint)? {
			return op.commit(Commit::Exempt);
		}

		op.load_price()?;
		op.commit(Commit::Checked)
	}

	/// Closes a debt-free vault and sends its collateral to the recipient.
	pub(crate) fn do_close_vault(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		recipient: Option<T::AccountId>,
	) -> DispatchResult {
		let op = VaultOp::<T>::load_priced(collateral_id, stable_id, &owner)?;
		let recipient = recipient.unwrap_or(owner);

		op.finish_close(&recipient, Commit::Checked)
	}

	/// Moves the last unsafe eligible vault into final recovery.
	pub(crate) fn do_enter_final_recovery(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> DispatchResult {
		let mut op = VaultOp::<T>::load_priced(collateral_id, stable_id, &owner)?;
		op.enter_final_recovery()?;
		op.commit(Commit::Exempt)
	}

	/// Removes a vault from final recovery.
	///
	/// It rejoins the rate list only when its debt meets the minimum.
	pub(crate) fn do_exit_final_recovery(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		hint: Position<T::AccountId>,
	) -> DispatchResult {
		let mut op = VaultOp::<T>::load_priced(collateral_id, stable_id, &owner)?;
		op.exit_final_recovery(hint)?;
		op.commit(Commit::Exempt)
	}

	/// Activates a dormant vault. Anyone may call this.
	pub(crate) fn do_activate_dormant(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		hint: Position<T::AccountId>,
	) -> DispatchResult {
		let mut op = VaultOp::<T>::load_unfrozen(collateral_id, stable_id, &owner)?;
		op.activate(hint)?;
		op.commit(Commit::Exempt)
	}

	/// Checks role exclusivity for a new market and counts it against its stablecoin.
	///
	/// A stable asset cannot also be collateral, or its issuer could create unbacked collateral.
	/// The stable direction — "is this collateral someone's stablecoin?" — reads
	/// [`StablecoinMarkets`]; the collateral direction — "is this stablecoin someone's
	/// collateral?" — is a prefix probe on the collateral-first-keyed registry itself.
	fn claim_stablecoin_market(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<u32, DispatchError> {
		let stable_key = T::StableToCollateralId::convert(stable_id.clone());
		ensure!(*collateral_id != stable_key, Error::<T>::StableCollateralCollision);
		ensure!(
			!StablecoinMarkets::<T>::contains_key(collateral_id),
			Error::<T>::StableCollateralCollision
		);
		ensure!(
			!Branches::<T>::contains_prefix(&stable_key),
			Error::<T>::StableCollateralCollision
		);
		StablecoinMarkets::<T>::try_mutate(&stable_key, |maybe| {
			let markets =
				maybe.unwrap_or_default().checked_add(1).ok_or(Error::<T>::ArithmeticOverflow)?;
			*maybe = Some(markets);
			Ok(markets)
		})
	}

	/// Removes one market reference for its stablecoin.
	///
	/// The entry is removed with its last market.
	fn release_stablecoin_market(stable_id: &StableIdOf<T>) -> Result<u32, DispatchError> {
		let stable_key = T::StableToCollateralId::convert(stable_id.clone());
		StablecoinMarkets::<T>::try_mutate(&stable_key, |maybe| match maybe {
			Some(markets) if *markets > 0 => {
				*markets -= 1;
				let remaining = *markets;
				if remaining == 0 {
					*maybe = None;
				}
				Ok(remaining)
			},
			_ => {
				defensive!("stablecoin market count missing or under-counted on release");
				Err(DispatchError::Corruption)
			},
		})
	}

	/// Rejects a configuration that contradicts itself or breaches the runtime's limits.
	///
	/// Every path that writes a [`BranchConfig`] goes through this, so registration and later
	/// parameter changes are held to one standard. The reported defect names the failing rule:
	/// with a dozen of them across the two checks, a single opaque error would leave a market
	/// creator guessing.
	///
	/// Both assets must already exist: their minimum balances are the scale the market's own
	/// amounts are judged on, and an absent asset reports no minimum at all.
	fn ensure_config_allowed(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		config: &BranchConfig<BalanceOf<T>>,
	) -> DispatchResult {
		let minimums = AssetMinimums {
			collateral: T::CollateralAssets::minimum_balance(collateral_id.clone()),
			stable: T::StableAssets::minimum_balance(stable_id.clone()),
		};
		if let Some(defect) = config.structural_defect(&minimums) {
			return Err(Error::<T>::InvalidBranchConfig(defect).into());
		}
		if let Some(violation) = T::BranchConfigBounds::get().violation(config) {
			return Err(Error::<T>::ConfigOutsideEnvelope(violation).into());
		}
		Ok(())
	}

	/// Creates a market after checking its limits, assets, and oracle price.
	///
	/// The market record, deposit, stablecoin market count, and lifecycle state are created
	/// together.
	pub(crate) fn do_create_branch(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		admins: BranchAdmins<T::AccountId>,
		config: BranchConfig<BalanceOf<T>>,
		lifecycle_config: RegistrationConfigOf<T>,
		depositor: Option<T::AccountId>,
	) -> DispatchResult {
		// A market needs a valid collateral price.
		T::Oracle::provide_price(&collateral_id)
			.map_err(|_| Error::<T>::OraclePriceNotAvailable)?;
		ensure!(
			!Branches::<T>::contains_key(&collateral_id, &stable_id),
			Error::<T>::BranchAlreadyRegistered
		);
		ensure!(
			T::CollateralAssets::asset_exists(collateral_id.clone()),
			Error::<T>::UnknownCollateral
		);
		ensure!(T::StableAssets::asset_exists(stable_id.clone()), Error::<T>::UnknownStable);
		Self::ensure_config_allowed(&collateral_id, &stable_id, &config)?;
		let stablecoin_markets = Self::claim_stablecoin_market(&collateral_id, &stable_id)?;
		let deposit = match depositor {
			Some(who) => {
				let footprint = Footprint::from_mel::<(crate::pallet::BranchOf<T>, u32)>();
				let ticket = T::BranchConsideration::new(&who, footprint)?;
				Some((who, ticket))
			},
			None => None,
		};
		let lifecycle_depositor = deposit.as_ref().map(|(who, _)| who.clone());
		Self::ensure_fee_account_receivable(&stable_id)?;
		let redistribution_account = Self::redistribution_account(&collateral_id, &stable_id);
		frame_system::Pallet::<T>::inc_providers(&redistribution_account);
		Self::seed_redistribution_custody(
			&collateral_id,
			&stable_id,
			&Self::custody_funder(deposit.as_ref().map(|(who, _)| who), &admins),
		)?;
		let now = T::TimeProvider::now();
		Branches::<T>::insert(
			&collateral_id,
			&stable_id,
			crate::types::Branch { state: BranchState::fresh(now), config, admins, deposit },
		);
		T::OnBranchLifecycle::on_registered(
			&collateral_id,
			&stable_id,
			stablecoin_markets,
			lifecycle_config,
			lifecycle_depositor.as_ref(),
		)?;
		Self::deposit_event(Event::BranchRegistered { collateral_id, stable_id });
		Ok(())
	}

	/// Removes an empty market and refunds its deposit and custody seed.
	///
	/// This also releases its stablecoin market count and provider reference, and calls the
	/// lifecycle hook.
	/// The seed follows [`Self::custody_funder`] on the stored record, so a privileged market
	/// refunds its current full administrator.
	pub(crate) fn do_remove_branch(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> DispatchResult {
		let branch = Self::branch_of(&collateral_id, &stable_id)?;
		ensure!(branch.state.is_removable(), Error::<T>::BranchNotEmpty);
		ensure!(
			!Vaults::<T>::contains_prefix((&collateral_id, &stable_id)),
			Error::<T>::BranchNotEmpty
		);
		let remaining_stablecoin_markets = Self::release_stablecoin_market(&stable_id)?;
		T::OnBranchLifecycle::on_deregistered(
			&collateral_id,
			&stable_id,
			remaining_stablecoin_markets,
		)?;
		let removed_outstanding = Branches::<T>::take(&collateral_id, &stable_id)
			.map(|removed| removed.state.debt.outstanding())
			.unwrap_or_default();
		defensive_assert!(removed_outstanding.is_zero(), "market removal requires zero debt");
		// The refund kills the collateral account, so its consumer references are gone before the
		// provider reference is released.
		Self::refund_redistribution_custody(
			&collateral_id,
			&stable_id,
			&Self::custody_funder(branch.deposit.as_ref().map(|(who, _)| who), &branch.admins),
		)?;
		let redistribution_account = Self::redistribution_account(&collateral_id, &stable_id);
		if let Err(err) = frame_system::Pallet::<T>::dec_providers(&redistribution_account) {
			defensive!("redistribution-account provider reference not released", err);
		}
		if let Some((who, ticket)) = branch.deposit {
			ticket.drop(&who)?;
		}
		Self::deposit_event(Event::BranchRemoved { collateral_id, stable_id });
		Ok(())
	}

	/// Replaces the administrators of a market.
	pub(crate) fn do_set_branch_admins(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		admins: BranchAdmins<T::AccountId>,
	) -> DispatchResult {
		Branches::<T>::try_mutate_exists(&collateral_id, &stable_id, |maybe| -> DispatchResult {
			let branch = maybe.as_mut().ok_or(Error::<T>::BranchNotFound)?;
			branch.admins = admins.clone();
			Ok(())
		})?;
		let BranchAdmins { full_admin, emergency_admin } = admins;
		Self::deposit_event(Event::BranchAdminsChanged {
			collateral_id,
			stable_id,
			full_admin,
			emergency_admin,
		});
		Ok(())
	}

	/// Applies one authorized market setting update.
	///
	/// Emergency administrators may only reduce risk. The full result must stay within global
	/// limits. `level` must come from [`Pallet::ensure_branch_admin`].
	pub(crate) fn do_set_param(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		update: BranchConfigUpdate<BalanceOf<T>>,
		level: AdminLevel,
	) -> DispatchResult {
		Branches::<T>::try_mutate_exists(&collateral_id, &stable_id, |maybe| -> DispatchResult {
			let branch = maybe.as_mut().ok_or(Error::<T>::BranchNotFound)?;
			if matches!(level, AdminLevel::Emergency) {
				ensure!(
					update.is_defensive(&branch.config),
					Error::<T>::DefensiveActionNotDefensive
				);
			}
			update.clone().apply_to(&mut branch.config);
			Self::ensure_config_allowed(&collateral_id, &stable_id, &branch.config)?;
			Ok(())
		})?;
		Self::deposit_event(Event::ParameterUpdated { collateral_id, stable_id, update });
		Ok(())
	}

	/// Sets or clears an administrative freeze.
	///
	/// Freezing does nothing if the market is already frozen. Clearing only removes an
	/// administrative freeze.
	pub(crate) fn do_set_governance_frozen(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		frozen: bool,
	) -> DispatchResult {
		let state = Self::branch_of(collateral_id, stable_id)?.state;
		match (state.frozen, frozen) {
			(None, true) => {
				Self::transition_frozen(collateral_id, stable_id, Some(FrozenReason::Governance))
			},
			(Some(current), false) if matches!(current.reason, FrozenReason::Governance) => {
				Self::transition_frozen(collateral_id, stable_id, None)
			},
			_ => Ok(()),
		}
	}

	/// Sets the global debt limit across every market issuing one stable asset.
	///
	/// An empty record is removed from storage.
	pub(crate) fn do_set_global_debt_ceiling(stable_id: StableIdOf<T>, ceiling: BalanceOf<T>) {
		if ceiling.is_zero() {
			GlobalDebtCeilings::<T>::remove(&stable_id);
		} else {
			GlobalDebtCeilings::<T>::insert(&stable_id, ceiling);
		}
		Self::deposit_event(Event::GlobalDebtCeilingSet { stable_id, ceiling });
	}

	/// Changes the stored freeze state and emits the market mode change.
	///
	/// Freezing applies pending interest first. Unfreezing skips the frozen time so no interest is
	/// charged for it.
	fn transition_frozen(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		target: Option<FrozenReason>,
	) -> DispatchResult {
		let (minted, old_mode, new_mode) =
			Self::try_mutate_branch_state(collateral_id, stable_id, |config, state, now| {
				let old_mode =
					Self::mode_of(state, config, collateral_id, now).unwrap_or(BranchMode::Normal);
				let minted = match (state.frozen, target) {
					(None, Some(_)) => {
						// Apply interest up to the start of the freeze.
						Self::accrue_aggregate_interest(state, now)?
					},
					(Some(frozen), None) => {
						// Remove the frozen period from market interest time.
						let frozen_window = now.saturating_sub(frozen.entered_at);
						state.interest_epoch = state.interest_epoch.saturating_add(frozen_window);
						Zero::zero()
					},
					(None, None) | (Some(_), Some(_)) => {
						debug_assert!(false, "callers gate on the current frozen state");
						Zero::zero()
					},
				};
				state.frozen = target.map(|reason| FrozenState { reason, entered_at: now });
				let new_mode =
					Self::mode_of(state, config, collateral_id, now).unwrap_or(BranchMode::Normal);
				Ok((minted, old_mode, new_mode))
			})?;
		// Mint interest only after storing the updated market.
		if !minted.is_zero() {
			Self::mint_and_route_yield(collateral_id, stable_id, minted)?;
		}
		Self::deposit_event(Event::ModeChanged {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			old_mode,
			new_mode,
		});
		Ok(())
	}

	/// Updates the oracle freeze to match the current price result.
	pub(crate) fn do_refresh_branch(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> DispatchResult {
		let state = Self::branch_of(collateral_id, stable_id)?.state;
		let oracle_ok = T::Oracle::provide_price(collateral_id).is_ok();
		match (state.frozen, oracle_ok) {
			(Some(state), true) if matches!(state.reason, FrozenReason::OracleFailure) => {
				Self::transition_frozen(collateral_id, stable_id, None)
			},
			(None, false) => {
				Self::transition_frozen(collateral_id, stable_id, Some(FrozenReason::OracleFailure))
			},
			_ => Ok(()),
		}
	}
}
