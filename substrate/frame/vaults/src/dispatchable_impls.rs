//! Implementations for pallet extrinsics.

use crate::{
	context::{BranchOp, VaultOp},
	pallet::{
		AssetRoles, BalanceOf, Branches, CollateralIdOf, CollateralRisks, Config, Error, Event,
		HoldReason, Pallet, StableIdOf, Vaults,
	},
	types::{
		AdminLevel, AssetRole, AssetRoleUsage, BranchAdmins, BranchConfig, BranchConfigUpdate,
		BranchMode, BranchState, FrozenReason, FrozenState,
	},
};
use frame::{
	prelude::*,
	traits::{
		fungibles::{
			Inspect as FungiblesInspect, Mutate as FungiblesMutate,
			MutateHold as FungiblesMutateHold,
		},
		tokens::Restriction,
		Consideration, Footprint, Time,
	},
};
use pallet_linked_list::Position;
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
		let op = BranchOp::<T>::load_unfrozen(collateral_id, stable_id)?;
		let price = op.price()?;
		let op =
			op.create_vault(&owner, initial_collateral, initial_debt, annual_rate, price, hint)?;

		T::CollateralAssets::hold(
			op.collateral_id().clone(),
			&HoldReason::VaultCollateral.into(),
			&owner,
			initial_collateral,
		)?;
		T::StableAssets::mint_into(op.stable_id().clone(), &owner, initial_debt)?;
		Self::deposit_event(Event::Borrowed {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner: owner.clone(),
			recipient: owner.clone(),
			amount: initial_debt,
		});
		Self::deposit_event(Event::CollateralDeposited {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner: owner.clone(),
			from: owner.clone(),
			amount: initial_collateral,
		});
		Self::deposit_event(Event::VaultOpened {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner,
		});
		op.commit_checked(price)
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
		ensure!(!amount.is_zero(), Error::<T>::ZeroDepositAmount);
		let op = BranchOp::<T>::load_unfrozen(collateral_id, stable_id)?;
		let mut op = op.touch(&owner)?;
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
		op.commit_exempt()
	}

	/// Withdraws collateral and closes the vault if it becomes empty.
	pub(crate) fn do_withdraw_collateral(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
		recipient: T::AccountId,
	) -> DispatchResult {
		ensure!(!amount.is_zero(), Error::<T>::ZeroWithdrawAmount);
		let op = BranchOp::<T>::load_unfrozen(collateral_id, stable_id)?;
		let price = op.price()?;
		let mut op = op.touch(&owner)?;
		if op.apply_collateral_withdrawal(amount, price)? {
			return Self::close_inner(op, &recipient, price);
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
		op.commit_checked(price)
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
		ensure!(!amount.is_zero(), Error::<T>::ZeroBorrowAmount);
		let op = BranchOp::<T>::load_unfrozen(collateral_id, stable_id)?;
		let price = op.price()?;
		let mut op = op.touch(&owner)?;
		op.borrow(amount, maybe_new_rate, price, hint)?;

		T::StableAssets::mint_into(op.stable_id().clone(), &recipient, amount)?;

		Self::deposit_event(Event::Borrowed {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner,
			recipient,
			amount,
		});
		op.commit_checked(price)
	}

	/// Repays debt for a vault from another account.
	pub(crate) fn do_repay_for(
		from: T::AccountId,
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		ensure!(!amount.is_zero(), Error::<T>::ZeroRepayAmount);
		let op = BranchOp::<T>::load_unfrozen(collateral_id, stable_id)?;
		let mut op = op.touch(&owner)?;
		ensure!(!op.status().is_final_recovery(), Error::<T>::VaultInFinalRecovery);
		// Never burn more than the current debt.
		let repay = op.repayment_amount(amount);
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

		// Close a fully repaid vault when it has no collateral left.
		if new_total.is_zero() && op.vault().collateral.is_zero() {
			let price = op.price()?;
			return Self::close_inner(op, &owner, price);
		}

		op.reconcile_after_debt_reduction()?;
		op.commit_exempt()
	}

	/// Changes a vault's interest rate.
	pub(crate) fn do_change_rate(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		new_rate: FixedU128,
		hint: Position<T::AccountId>,
	) -> DispatchResult {
		let op = BranchOp::<T>::load_unfrozen(collateral_id, stable_id)?;
		let mut op = op.touch(&owner)?;
		if !op.change_rate(new_rate, hint)? {
			return op.commit_exempt();
		}

		let price = op.price()?;
		op.commit_checked(price)
	}

	/// Closes a debt-free vault and sends its collateral to the recipient.
	pub(crate) fn do_close_vault(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		recipient: Option<T::AccountId>,
	) -> DispatchResult {
		let op = BranchOp::<T>::load_unfrozen(collateral_id, stable_id)?;
		let price = op.price()?;
		let op = op.touch(&owner)?;
		let recipient = recipient.unwrap_or(owner);

		Self::close_inner(op, &recipient, price)
	}

	/// Closes a vault and returns its collateral.
	fn close_inner(
		mut op: VaultOp<T>,
		recipient: &T::AccountId,
		price: FixedU128,
	) -> DispatchResult {
		let (collateral, branch_empties, orphan_debt) = op.detach_for_close()?;

		if !collateral.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				op.collateral_id().clone(),
				&HoldReason::VaultCollateral.into(),
				op.owner(),
				recipient,
				collateral,
				Precision::Exact,
				Restriction::Free,
				Fortitude::Polite,
			)?;
		}

		if !orphan_debt.is_zero() {
			Self::deposit_event(Event::BadDebtRecorded {
				collateral_id: op.collateral_id().clone(),
				stable_id: op.stable_id().clone(),
				amount: orphan_debt,
			});
		}
		Self::deposit_event(Event::VaultClosed {
			collateral_id: op.collateral_id().clone(),
			stable_id: op.stable_id().clone(),
			owner: op.owner().clone(),
			recipient: recipient.clone(),
		});
		// Closing the last liable vault may reduce the TCR.
		if branch_empties {
			op.remove_settlement()
		} else {
			op.remove_checked(price)
		}
	}

	/// Moves the last unsafe eligible vault into final recovery.
	pub(crate) fn do_enter_final_recovery(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> DispatchResult {
		let op = BranchOp::<T>::load_unfrozen(collateral_id, stable_id)?;
		let price = op.price()?;
		let mut op = op.touch(&owner)?;
		op.enter_final_recovery(price)?;
		op.commit_exempt()
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
		let op = BranchOp::<T>::load_unfrozen(collateral_id, stable_id)?;
		let price = op.price()?;
		let mut op = op.touch(&owner)?;
		op.exit_final_recovery(price, hint)?;
		op.commit_exempt()
	}

	/// Activates a dormant vault. Anyone may call this.
	pub(crate) fn do_activate_dormant(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		hint: Position<T::AccountId>,
	) -> DispatchResult {
		let op = BranchOp::<T>::load_unfrozen(collateral_id, stable_id)?;
		let mut op = op.touch(&owner)?;
		op.activate(hint)?;
		op.commit_exempt()
	}

	/// Adds one market reference for an asset role.
	fn claim_asset_role(asset: &CollateralIdOf<T>, role: AssetRole) -> DispatchResult {
		AssetRoles::<T>::try_mutate(asset, |maybe| match maybe {
			None => {
				*maybe = Some(AssetRoleUsage { role, markets: 1 });
				Ok(())
			},
			Some(usage) if usage.role == role => {
				usage.markets =
					usage.markets.checked_add(1).ok_or(Error::<T>::ArithmeticOverflow)?;
				Ok(())
			},
			Some(_) => Err(Error::<T>::StableCollateralCollision.into()),
		})
	}

	/// Removes one market reference for an asset role.
	///
	/// The entry is removed with its last reference.
	fn release_asset_role(asset: &CollateralIdOf<T>, role: AssetRole) -> DispatchResult {
		AssetRoles::<T>::try_mutate(asset, |maybe| match maybe {
			Some(usage) if usage.role == role && usage.markets > 0 => {
				usage.markets -= 1;
				if usage.markets == 0 {
					*maybe = None;
				}
				Ok(())
			},
			_ => {
				defensive!("asset role missing, mismatched, or under-counted on release");
				Err(DispatchError::Corruption)
			},
		})
	}

	/// Records the collateral and stable roles used by a market.
	fn claim_market_roles(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> DispatchResult {
		let stable_key = T::StableToCollateralId::convert(stable_id.clone());
		ensure!(*collateral_id != stable_key, Error::<T>::StableCollateralCollision);
		Self::claim_asset_role(collateral_id, AssetRole::Collateral)?;
		Self::claim_asset_role(&stable_key, AssetRole::Stable)
	}

	/// Removes the asset roles used by a market.
	fn release_market_roles(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> DispatchResult {
		let stable_key = T::StableToCollateralId::convert(stable_id.clone());
		Self::release_asset_role(collateral_id, AssetRole::Collateral)?;
		Self::release_asset_role(&stable_key, AssetRole::Stable)
	}

	/// Creates a market after checking its limits, assets, and oracle price.
	///
	/// The market record, deposit, asset roles, and lifecycle state are created together.
	pub(crate) fn do_create_branch(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		admins: BranchAdmins<T::AccountId>,
		config: BranchConfig<BalanceOf<T>>,
		depositor: Option<T::AccountId>,
	) -> DispatchResult {
		ensure!(T::BranchConfigGuard::get().permits(&config), Error::<T>::ConfigOutsideEnvelope);
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
		// A stable asset cannot also be collateral, or its issuer could create unbacked collateral.
		Self::claim_market_roles(&collateral_id, &stable_id)?;
		let deposit = match depositor {
			Some(who) => {
				let footprint = Footprint::from_mel::<(
					crate::pallet::BranchOf<T>,
					AssetRoleUsage,
					AssetRoleUsage,
				)>();
				let ticket = T::Consideration::new(&who, footprint)?;
				Some((who, ticket))
			},
			None => None,
		};
		let redistribution_account = Self::redistribution_account(&collateral_id, &stable_id);
		frame_system::Pallet::<T>::inc_providers(&redistribution_account);
		let now = T::TimeProvider::now();
		Branches::<T>::insert(
			&collateral_id,
			&stable_id,
			crate::types::Branch {
				state: BranchState::fresh(&config, now),
				config,
				admins,
				deposit,
			},
		);
		T::OnBranchLifecycle::on_registered(&collateral_id, &stable_id)?;
		Self::deposit_event(Event::BranchRegistered { collateral_id, stable_id });
		Ok(())
	}

	/// Removes an empty market and refunds its deposit.
	///
	/// This also releases its asset roles and provider reference, and calls the lifecycle hook.
	pub(crate) fn do_remove_branch(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> DispatchResult {
		let branch = Self::branch_of(&collateral_id, &stable_id)?;
		ensure!(branch.state.is_removable(), Error::<T>::MarketNotEmpty);
		ensure!(
			Vaults::<T>::iter_prefix((&collateral_id, &stable_id)).next().is_none(),
			Error::<T>::MarketNotEmpty
		);
		T::OnBranchLifecycle::on_deregistered(&collateral_id, &stable_id)?;
		let removed_outstanding = Branches::<T>::take(&collateral_id, &stable_id)
			.map(|removed| removed.state.debt.outstanding())
			.unwrap_or_default();
		defensive_assert!(
			removed_outstanding.is_zero(),
			"market removal must leave the CollateralRisks aggregate untouched"
		);
		Self::release_market_roles(&collateral_id, &stable_id)?;
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
			let branch = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
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
		let guard = T::BranchConfigGuard::get();
		Branches::<T>::try_mutate_exists(&collateral_id, &stable_id, |maybe| -> DispatchResult {
			let branch = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
			let config = &mut branch.config;
			if matches!(level, AdminLevel::Emergency) {
				ensure!(update.is_defensive(config), Error::<T>::DefensiveActionNotDefensive);
			}
			update.clone().apply_to(config);
			ensure!(guard.permits(config), Error::<T>::ConfigOutsideEnvelope);
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

	/// Sets the global debt limit for one collateral asset.
	///
	/// An empty record is removed from storage.
	pub(crate) fn do_set_global_debt_ceiling(
		collateral_id: CollateralIdOf<T>,
		ceiling: BalanceOf<T>,
	) {
		CollateralRisks::<T>::mutate_exists(&collateral_id, |maybe| {
			let risk = maybe.get_or_insert_default();
			risk.debt_ceiling = ceiling;
			if risk.is_empty() {
				maybe.take();
			}
		});
		Self::deposit_event(Event::GlobalDebtCeilingSet { collateral_id, ceiling });
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
		let now = T::TimeProvider::now();
		let mut branch = Self::branch_of(collateral_id, stable_id)?;
		let outstanding_before = branch.state.debt.outstanding();
		let old_mode = Self::mode_of(&branch, collateral_id, now).unwrap_or(BranchMode::Normal);
		let minted = match (branch.state.frozen, target) {
			(None, Some(_)) => {
				// Apply interest up to the start of the freeze.
				Self::accrue_aggregate_interest(&mut branch.state, now)
			},
			(Some(frozen), None) => {
				// Remove the frozen period from market interest time.
				let frozen_window = now.saturating_sub(frozen.entered_at);
				branch.state.interest_epoch =
					branch.state.interest_epoch.saturating_add(frozen_window);
				Zero::zero()
			},
			(None, None) | (Some(_), Some(_)) => {
				debug_assert!(false, "callers gate on the current frozen state");
				Zero::zero()
			},
		};
		branch.state.frozen = target.map(|reason| FrozenState { reason, entered_at: now });
		let new_mode = Self::mode_of(&branch, collateral_id, now).unwrap_or(BranchMode::Normal);
		Self::commit_branch(collateral_id, stable_id, outstanding_before, branch)?;
		// Mint interest only after storing the updated market.
		if !minted.is_zero() {
			Self::mint_and_route_yield(collateral_id, stable_id, minted);
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

	/// Updates a market's automatic debt limit. Anyone may call this.
	///
	/// Storage is unchanged when automatic updates are disabled or the limit is already current.
	pub(crate) fn do_poke_ceiling(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> DispatchResult {
		let mut branch = Self::branch_of(&collateral_id, &stable_id)?;
		if branch.config.ceiling_gap.is_zero() {
			return Ok(());
		}
		let now = T::TimeProvider::now();
		if Self::ratchet_ceiling(&mut branch.state, &branch.config, now) {
			Branches::<T>::insert(&collateral_id, &stable_id, branch);
		}
		Ok(())
	}
}
