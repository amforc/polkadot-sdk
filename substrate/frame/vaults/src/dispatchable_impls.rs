use crate::{
	context::{OpContext, TcrGate, VaultOp},
	pallet::{
		AssetRoles, BalanceOf, Branches, CollateralIdOf, Config, Error, Event, HoldReason, Pallet,
		PalletsOriginOf, StableIdOf, Vaults,
	},
	recovery,
	types::{
		AdminLevel, AssetRole, AssetRoleUsage, BranchAdmins, BranchConfig, BranchConfigUpdate,
		BranchMode, BranchState, FrozenReason, FrozenState, Vault, VaultDebt, VaultStatus,
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
use pallet_linked_list::{Position, SortedListInterface};
use pusd_primitives::{OnBranchLifecycle, ProvidePrice};

impl<T: Config> Pallet<T> {
	/// TODO: DOC
	fn apply_borrow_checked(
		op: &mut OpContext<T>,
		config: &BranchConfig<BalanceOf<T>>,
		vault: &Vault<BalanceOf<T>>,
		amount: BalanceOf<T>,
		new_rate: FixedU128,
		price: FixedU128,
	) -> Result<BalanceOf<T>, DispatchError> {
		Self::ratchet_ceiling(&mut op.branch.state, config, op.now);
		let principal_after = op.branch.state.debt.principal.saturating_add(amount);
		ensure!(principal_after <= config.debt_ceiling, Error::<T>::DebtCeilingExceeded);
		if !config.ceiling_gap.is_zero() {
			ensure!(
				principal_after <= op.branch.state.effective_ceiling,
				Error::<T>::DebtCeilingExceeded
			);
		}
		let upfront_fee =
			Self::apply_borrow(&mut op.branch.state, config, vault, amount, new_rate, op.now);
		op.ensure_global_ceiling(price)?;
		Ok(upfront_fee)
	}

	/// TODO: DOC
	pub(crate) fn do_open_vault(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		initial_collateral: BalanceOf<T>,
		initial_debt: BalanceOf<T>,
		annual_rate: FixedU128,
		hint: Position<T::AccountId>,
	) -> Result<(), DispatchError> {
		let mut op = OpContext::<T>::load(collateral_id, stable_id)?;
		op.ensure_not_frozen()?;
		ensure!(
			!Vaults::<T>::contains_key((&op.collateral_id, &op.stable_id, &owner)),
			Error::<T>::VaultAlreadyExists
		);
		let config = op.config();
		ensure!(initial_debt >= config.minimum_debt, Error::<T>::DebtBelowMinimum);
		ensure!(
			initial_collateral >= config.minimum_collateral,
			Error::<T>::InsufficientCollateral
		);
		Self::validate_rate(&config, annual_rate)?;
		let price = op.price()?;

		let mut vault =
			Self::open_scratch_row(&op.branch.state, annual_rate, initial_collateral, op.now);
		let upfront_fee =
			Self::apply_borrow_checked(&mut op, &config, &vault, initial_debt, annual_rate, price)?;
		vault.debt = VaultDebt { principal: initial_debt, interest: upfront_fee };

		let total_debt = initial_debt.saturating_add(upfront_fee);
		Self::ensure_above_icr(initial_collateral, total_debt, price, &config)?;

		op.branch.state.add_collateral(initial_collateral);

		T::CollateralAssets::hold(
			op.collateral_id.clone(),
			&HoldReason::VaultCollateral.into(),
			&owner,
			initial_collateral,
		)?;
		T::StableAssets::mint_into(op.stable_id.clone(), &owner, initial_debt)?;
		op.charge_upfront_fee(&owner, upfront_fee);

		T::VaultLists::insert(op.rate_list(), owner.clone(), annual_rate, hint)
			.map_err(Self::map_error)?;

		Self::deposit_event(Event::Borrowed {
			collateral_id: op.collateral_id.clone(),
			stable_id: op.stable_id.clone(),
			owner: owner.clone(),
			recipient: owner.clone(),
			amount: initial_debt,
		});
		Self::deposit_event(Event::CollateralDeposited {
			collateral_id: op.collateral_id.clone(),
			stable_id: op.stable_id.clone(),
			owner: owner.clone(),
			from: owner.clone(),
			amount: initial_collateral,
		});
		Self::deposit_event(Event::VaultOpened {
			collateral_id: op.collateral_id.clone(),
			stable_id: op.stable_id.clone(),
			owner: owner.clone(),
		});
		op.attach_new(&owner, vault).commit(TcrGate::Check { price, settlement: false })
	}

	/// Permissionless deposit. Dormant vaults must be revived by borrowing.
	pub(crate) fn do_deposit_collateral_for(
		from: T::AccountId,
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
	) -> Result<(), DispatchError> {
		let op = OpContext::<T>::load(collateral_id, stable_id)?;
		op.ensure_not_frozen()?;
		let mut op = op.touch(&owner)?;
		ensure!(!op.status.is_dormant(), Error::<T>::DebtBelowMinimum);

		T::CollateralAssets::transfer_and_hold(
			op.ctx.collateral_id.clone(),
			&HoldReason::VaultCollateral.into(),
			&from,
			&owner,
			amount,
			Precision::Exact,
			Preservation::Expendable,
			Fortitude::Polite,
		)?;

		op.ctx.branch.state.add_collateral(amount);
		op.vault.collateral = op.vault.collateral.saturating_add(amount);
		op.sync_stake_for(op.status);

		Self::deposit_event(Event::CollateralDeposited {
			collateral_id: op.ctx.collateral_id.clone(),
			stable_id: op.ctx.stable_id.clone(),
			owner,
			from,
			amount,
		});
		op.commit(TcrGate::Exempt)
	}

	/// TODO: DOC
	pub(crate) fn do_withdraw_collateral(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
		recipient: T::AccountId,
	) -> Result<(), DispatchError> {
		let op = OpContext::<T>::load(collateral_id, stable_id)?;
		op.ensure_not_frozen()?;
		let price = op.price()?;
		let mut op = op.touch(&owner)?;
		ensure!(!op.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);

		let config = op.ctx.config();
		let collateral = op.vault.collateral;
		ensure!(collateral >= amount, Error::<T>::InsufficientCollateral);

		let total_debt = op.vault.debt.total();
		let new_collateral = collateral.saturating_sub(amount);
		if !total_debt.is_zero() {
			Self::ensure_above_icr(new_collateral, total_debt, price, &config)?;
		}

		if total_debt.is_zero() && new_collateral.is_zero() {
			return Self::close_inner(op, &recipient, price, None);
		}

		op.ctx.branch.state.remove_collateral(amount);
		op.vault.collateral = new_collateral;
		op.sync_stake_for(op.status);

		T::CollateralAssets::transfer_on_hold(
			op.ctx.collateral_id.clone(),
			&HoldReason::VaultCollateral.into(),
			&owner,
			&recipient,
			amount,
			Precision::Exact,
			Restriction::Free,
			Fortitude::Polite,
		)?;

		Self::deposit_event(Event::CollateralWithdrawn {
			collateral_id: op.ctx.collateral_id.clone(),
			stable_id: op.ctx.stable_id.clone(),
			owner,
			recipient,
			amount,
		});
		op.commit(TcrGate::Check { price, settlement: false })
	}

	/// TODO: DOC
	pub(crate) fn do_borrow(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
		maybe_new_rate: Option<FixedU128>,
		recipient: T::AccountId,
		hint: Position<T::AccountId>,
	) -> Result<(), DispatchError> {
		let op = OpContext::<T>::load(collateral_id, stable_id)?;
		op.ensure_not_frozen()?;
		let price = op.price()?;
		let mut op = op.touch(&owner)?;
		ensure!(!op.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);

		let config = op.ctx.config();
		let old_rate = op.vault.annual_rate;
		let new_rate = maybe_new_rate.unwrap_or(old_rate);
		Self::validate_rate(&config, new_rate)?;

		let new_ib_debt = op.vault.debt.principal.saturating_add(amount);
		let upfront_fee =
			Self::apply_borrow_checked(&mut op.ctx, &config, &op.vault, amount, new_rate, price)?;

		let dormant_to_active = op.status.is_dormant() && new_ib_debt >= config.minimum_debt;
		op.vault.debt.principal = new_ib_debt;
		op.vault.debt.interest = op.vault.debt.interest.saturating_add(upfront_fee);
		if old_rate != new_rate {
			op.vault.annual_rate = new_rate;
			op.vault.last_rate_update = op.ctx.now;
		}
		ensure!(op.vault.debt.principal >= config.minimum_debt, Error::<T>::DebtBelowMinimum);

		let collateral = op.vault.redistribution_stake;
		let total_debt = op.vault.debt.total();
		Self::ensure_above_icr(collateral, total_debt, price, &config)?;

		if dormant_to_active {
			op.ctx.branch.state.release_dormant_target(&owner);
		}

		T::StableAssets::mint_into(op.ctx.stable_id.clone(), &recipient, amount)?;
		op.ctx.charge_upfront_fee(&owner, upfront_fee);

		if dormant_to_active {
			T::VaultLists::insert(op.ctx.rate_list(), owner.clone(), new_rate, hint)
				.map_err(Self::map_error)?;
			Self::deposit_event(Event::VaultStatusChanged {
				collateral_id: op.ctx.collateral_id.clone(),
				stable_id: op.ctx.stable_id.clone(),
				owner: owner.clone(),
				old_status: VaultStatus::Dormant,
				new_status: VaultStatus::Active,
			});
		} else if old_rate != new_rate {
			T::VaultLists::re_insert(op.ctx.rate_list(), owner.clone(), new_rate, hint)
				.map_err(Self::map_error)?;
		}

		if old_rate != new_rate {
			Self::deposit_event(Event::BorrowRateChanged {
				collateral_id: op.ctx.collateral_id.clone(),
				stable_id: op.ctx.stable_id.clone(),
				owner: owner.clone(),
				old_rate,
				new_rate,
			});
		}
		Self::deposit_event(Event::Borrowed {
			collateral_id: op.ctx.collateral_id.clone(),
			stable_id: op.ctx.stable_id.clone(),
			owner,
			recipient,
			amount,
		});
		op.commit(TcrGate::Check { price, settlement: false })
	}

	pub(crate) fn do_repay_for(
		from: T::AccountId,
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		amount: BalanceOf<T>,
	) -> Result<(), DispatchError> {
		let op = OpContext::<T>::load(collateral_id, stable_id)?;
		op.ensure_not_frozen()?;
		let mut op = op.touch(&owner)?;
		ensure!(!op.status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);

		let config = op.ctx.config();

		// Cap overpayment at the touched debt.
		let repay = amount.min(op.vault.debt.total());
		T::StableAssets::burn_from(
			op.ctx.stable_id.clone(),
			&from,
			repay,
			Preservation::Expendable,
			Precision::Exact,
			Fortitude::Polite,
		)?;

		let payment = op.vault.debt.cancel(repay);
		debug_assert_eq!(payment.total(), repay);

		let new_total = op.vault.debt.total();
		if !new_total.is_zero() && new_total < config.minimum_debt {
			return Err(Error::<T>::DebtWouldBecomeDust.into());
		}

		Self::deposit_event(Event::Repaid {
			collateral_id: op.ctx.collateral_id.clone(),
			stable_id: op.ctx.stable_id.clone(),
			owner: owner.clone(),
			from,
			amount: repay,
		});

		// A repaid-to-zero vault with no collateral left to reclaim (a
		// fully-redeemed husk) is closed outright — there is nothing to keep it
		// open for.
		if new_total.is_zero() && op.vault.collateral.is_zero() {
			let price = op.ctx.price()?;
			return Self::close_inner(op, &owner, price, Some(payment));
		}

		op.ctx.branch.state.apply_debt_payment(
			payment,
			op.vault.annual_rate,
			op.vault.debt.principal,
		);
		if new_total.is_zero() {
			// Repaying to zero does not close the vault: the collateral stays held and
			// the row survives as a zero-debt Dormant husk (mirroring a redeem-to-zero).
			// The owner reclaims collateral with an explicit `close_vault`. Keeping the
			// vault open lets debt be repaid in Safety mode purely to improve branch TCR.
			op.ctx.branch.state.release_dormant_target(&owner);
			if op.status.is_active() {
				T::VaultLists::remove(&op.ctx.rate_list(), &owner)
					.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
			}
		}
		op.commit(TcrGate::Exempt)
	}

	pub(crate) fn do_change_rate(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		new_rate: FixedU128,
		hint: Position<T::AccountId>,
	) -> Result<(), DispatchError> {
		let op = OpContext::<T>::load(collateral_id, stable_id)?;
		op.ensure_not_frozen()?;
		let mut op = op.touch(&owner)?;
		ensure!(op.status.is_active(), Error::<T>::InvalidVaultStatus);
		let old_rate = op.vault.annual_rate;
		if old_rate == new_rate {
			return op.commit(TcrGate::Exempt);
		}

		let config = op.ctx.config();
		Self::validate_rate(&config, new_rate)?;

		let upfront_fee = Self::apply_rate_change(
			&mut op.ctx.branch.state,
			&config,
			&op.vault,
			new_rate,
			op.ctx.now,
		);

		let price = op.ctx.price()?;
		op.ctx.charge_upfront_fee(&owner, upfront_fee);

		op.vault.annual_rate = new_rate;
		op.vault.last_rate_update = op.ctx.now;
		op.vault.debt.interest = op.vault.debt.interest.saturating_add(upfront_fee);

		T::VaultLists::re_insert(op.ctx.rate_list(), owner.clone(), new_rate, hint)
			.map_err(Self::map_error)?;
		Self::deposit_event(Event::BorrowRateChanged {
			collateral_id: op.ctx.collateral_id.clone(),
			stable_id: op.ctx.stable_id.clone(),
			owner,
			old_rate,
			new_rate,
		});
		op.commit(TcrGate::Check { price, settlement: false })
	}

	pub(crate) fn do_close_vault(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		recipient: Option<T::AccountId>,
	) -> Result<(), DispatchError> {
		let recipient = recipient.unwrap_or(owner.clone());
		let op = OpContext::<T>::load(collateral_id, stable_id)?;
		op.ensure_not_frozen()?;
		let price = op.price()?;
		let op = op.touch(&owner)?;
		ensure!(op.vault.debt.total().is_zero(), Error::<T>::DebtOutstanding);

		Self::close_inner(op, &recipient, price, None)
	}

	/// Shared close path. Consumes the vault operation at commit.
	/// `maybe_payment` is `Some` when a repay-to-zero closes an empty
	/// (no-collateral) vault: the payment to fold into the branch aggregates,
	/// at the vault's own rate.
	fn close_inner(
		mut op: VaultOp<T>,
		recipient: &T::AccountId,
		price: FixedU128,
		maybe_payment: Option<VaultDebt<BalanceOf<T>>>,
	) -> Result<(), DispatchError> {
		// The row tracks this market's collateral in every state, FinalRecovery
		// included (where the stake is zero but the collateral persists).
		let collateral = op.vault.collateral;
		if let Some(payment) = maybe_payment {
			op.ctx.branch.state.apply_debt_payment(
				payment,
				op.vault.annual_rate,
				op.vault.debt.principal,
			);
		}
		op.ctx.branch.state.detach_vault(&op.vault);
		op.ctx.branch.state.remove_collateral(collateral);
		op.ctx.branch.state.release_dormant_target(&op.owner);

		let branch_empties = op.ctx.branch.state.is_empty_of_liability();
		// Sweep ahead of the commit's TCR gate; defer the event until the close is
		// past every fallible step, just before commit.
		let orphan_debt = if branch_empties {
			op.ctx.branch.state.sweep_orphan_debt()
		} else {
			BalanceOf::<T>::zero()
		};

		if !collateral.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				op.ctx.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				&op.owner,
				recipient,
				collateral,
				Precision::Exact,
				Restriction::Free,
				Fortitude::Polite,
			)?;
		}

		match op.status {
			VaultStatus::Active => {
				// Active vaults must be in the rate index.
				T::VaultLists::remove(&op.ctx.rate_list(), &op.owner)
					.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
			},
			VaultStatus::FinalRecovery => {
				recovery::remove::<T>(&op.ctx.collateral_id, &op.ctx.stable_id, &op.owner)?;
			},
			VaultStatus::Dormant => {},
		}

		if !orphan_debt.is_zero() {
			Self::deposit_event(Event::BadDebtRecorded {
				collateral_id: op.ctx.collateral_id.clone(),
				stable_id: op.ctx.stable_id.clone(),
				amount: orphan_debt,
			});
		}
		Self::deposit_event(Event::VaultClosed {
			collateral_id: op.ctx.collateral_id.clone(),
			stable_id: op.ctx.stable_id.clone(),
			owner: op.owner.clone(),
			recipient: recipient.clone(),
		});
		// A branch-emptying close is a settlement: it may worsen TCR.
		op.commit_removing_vault(TcrGate::Check { price, settlement: branch_empties })
	}

	pub(crate) fn do_enter_final_recovery(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> Result<(), DispatchError> {
		let op = OpContext::<T>::load(collateral_id, stable_id)?;
		op.ensure_not_frozen()?;
		let price = op.price()?;
		let mut op = op.touch(&owner)?;
		ensure!(op.status.is_active(), Error::<T>::InvalidVaultStatus);

		let config = op.ctx.config();
		let collateral = op.vault.redistribution_stake;
		let total_debt = op.vault.debt.total();
		Self::ensure_below_mcr(collateral, total_debt, price, &config)?;

		ensure!(
			op.ctx.branch.state.stakes.total == op.vault.redistribution_stake,
			Error::<T>::NotLastEligibleVault
		);

		T::VaultLists::remove(&op.ctx.rate_list(), &owner)
			.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
		op.sync_stake_for(VaultStatus::FinalRecovery);
		recovery::append::<T>(&op.ctx.collateral_id, &op.ctx.stable_id, owner.clone())?;

		Self::deposit_event(Event::VaultStatusChanged {
			collateral_id: op.ctx.collateral_id.clone(),
			stable_id: op.ctx.stable_id.clone(),
			owner,
			old_status: VaultStatus::Active,
			new_status: VaultStatus::FinalRecovery,
		});
		op.commit(TcrGate::Exempt)
	}

	/// Explicit `FinalRecovery` exit. Rejoins the rate index only above `MinimumDebt`.
	pub(crate) fn do_exit_final_recovery(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		hint: Position<T::AccountId>,
	) -> Result<(), DispatchError> {
		let op = OpContext::<T>::load(collateral_id, stable_id)?;
		op.ensure_not_frozen()?;
		let price = op.price()?;
		let mut op = op.touch(&owner)?;
		ensure!(op.status.is_final_recovery(), Error::<T>::InvalidVaultStatus);

		let config = op.ctx.config();
		let collateral = op.vault.collateral;
		let total_debt = op.vault.debt.total();
		Self::ensure_at_or_above_mcr(collateral, total_debt, price, &config)?;

		let rejoin_active = total_debt >= config.minimum_debt;
		let new_status = if rejoin_active { VaultStatus::Active } else { VaultStatus::Dormant };

		recovery::remove::<T>(&op.ctx.collateral_id, &op.ctx.stable_id, &owner)?;
		op.sync_stake_for(new_status);
		op.vault.redistribution_snapshot = op.ctx.branch.state.redistribution;
		if !rejoin_active &&
			!total_debt.is_zero() &&
			!op.ctx.branch.state.try_park_dormant_target(owner.clone())
		{
			return Err(Error::<T>::DormantTargetOccupied.into());
		}
		if rejoin_active {
			T::VaultLists::insert(op.ctx.rate_list(), owner.clone(), op.vault.annual_rate, hint)
				.map_err(Self::map_error)?;
		}
		Self::deposit_event(Event::VaultStatusChanged {
			collateral_id: op.ctx.collateral_id.clone(),
			stable_id: op.ctx.stable_id.clone(),
			owner,
			old_status: VaultStatus::FinalRecovery,
			new_status,
		});
		op.commit(TcrGate::Exempt)
	}

	/// Permissionless Dormant to Active revival.
	pub(crate) fn do_activate_dormant(
		owner: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		hint: Position<T::AccountId>,
	) -> Result<(), DispatchError> {
		let op = OpContext::<T>::load(collateral_id, stable_id)?;
		op.ensure_not_frozen()?;
		let mut op = op.touch(&owner)?;
		ensure!(op.status.is_dormant(), Error::<T>::InvalidVaultStatus);
		let config = op.ctx.config();
		ensure!(op.vault.debt.total() >= config.minimum_debt, Error::<T>::DebtBelowMinimum);

		T::VaultLists::insert(op.ctx.rate_list(), owner.clone(), op.vault.annual_rate, hint)
			.map_err(Self::map_error)?;
		op.ctx.branch.state.release_dormant_target(&owner);

		Self::deposit_event(Event::VaultStatusChanged {
			collateral_id: op.ctx.collateral_id.clone(),
			stable_id: op.ctx.stable_id.clone(),
			owner,
			old_status: VaultStatus::Dormant,
			new_status: VaultStatus::Active,
		});
		op.commit(TcrGate::Exempt)
	}

	/// Claim one market reference to `asset` in `role`.
	fn claim_asset_role(asset: &CollateralIdOf<T>, role: AssetRole) -> Result<(), DispatchError> {
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

	/// Release one market reference to `asset` in `role`, deleting the entry
	/// when the last reference goes.
	fn release_asset_role(asset: &CollateralIdOf<T>, role: AssetRole) -> Result<(), DispatchError> {
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

	/// Claim the two role references a market holds: its collateral as
	/// `Collateral`, and its stablecoin as `Stable` under the coin's image in
	/// the collateral-id namespace.
	fn claim_market_roles(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<(), DispatchError> {
		let stable_key = T::StableToCollateralId::convert(stable_id.clone());
		ensure!(*collateral_id != stable_key, Error::<T>::StableCollateralCollision);
		Self::claim_asset_role(collateral_id, AssetRole::Collateral)?;
		Self::claim_asset_role(&stable_key, AssetRole::Stable)
	}

	/// Release the two role references claimed by [`Self::claim_market_roles`].
	fn release_market_roles(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<(), DispatchError> {
		let stable_key = T::StableToCollateralId::convert(stable_id.clone());
		Self::release_asset_role(collateral_id, AssetRole::Collateral)?;
		Self::release_asset_role(&stable_key, AssetRole::Stable)
	}

	/// Permissionless market creation. Validates the config against the governance
	/// envelope and the oracle, takes the creation deposit, and seeds
	/// the whole market record. The
	/// deposit, the role counters, the `Branches` row, the provider reference, and
	/// every sibling lifecycle row are committed together.
	pub(crate) fn do_create_branch(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		admins: BranchAdmins<PalletsOriginOf<T>>,
		config: BranchConfig<BalanceOf<T>>,
		depositor: Option<T::AccountId>,
	) -> Result<(), DispatchError> {
		ensure!(T::BranchConfigGuard::get().permits(&config), Error::<T>::ConfigOutsideEnvelope);
		// A market the oracle cannot price cannot open.
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
		// The pallet mints a market's stablecoin permissionlessly, so that asset must
		// never be trusted as collateral — in this market or any sibling — else its
		// owner could mint unbacked collateral.
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

	/// Remove an empty market: fire the deregistration hook, tear down the
	/// record, release both assets' role references and the
	/// redistribution-account provider, and refund the deposit.
	pub(crate) fn do_remove_branch(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> Result<(), DispatchError> {
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

	/// Authorize, validate, and apply a single-field config update, emitting
	/// `ParameterUpdated`. The required admin tier, the `Emergency`-only "must be
	/// defensive" rule, and the governance-envelope check are all derived from the
	/// `update` itself ([`BranchConfigUpdate::required_level`] /
	/// [`BranchConfigUpdate::is_defensive`] and
	/// [`crate::types::BranchConfigGuard::permits`]), so the `set_param`
	/// dispatchable is a thin wrapper over this one path. The whole post-update
	/// config is re-validated through the same `permits` gate `create_branch`
	/// applies, keeping envelope enforcement in a single place.
	pub(crate) fn do_set_param(
		origin: OriginFor<T>,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		update: BranchConfigUpdate<BalanceOf<T>>,
	) -> Result<(), DispatchError> {
		let level =
			Self::ensure_branch_admin(origin, &collateral_id, &stable_id, update.required_level())?;
		let guard = T::BranchConfigGuard::get();
		Branches::<T>::try_mutate_exists(
			&collateral_id,
			&stable_id,
			|maybe| -> Result<(), DispatchError> {
				let branch = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
				let config = &mut branch.config;
				if matches!(level, AdminLevel::Emergency) {
					ensure!(update.is_defensive(config), Error::<T>::DefensiveActionNotDefensive);
				}
				update.clone().apply_to(config);
				ensure!(guard.permits(config), Error::<T>::ConfigOutsideEnvelope);
				Ok(())
			},
		)?;
		Self::deposit_event(Event::ParameterUpdated { collateral_id, stable_id, update });
		Ok(())
	}

	/// Set or clear the governance-induced `Frozen` state. No-op when the
	/// market is already frozen (for any reason) on freeze, or not
	/// governance-frozen on clear.
	pub(crate) fn do_set_governance_frozen(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		frozen: bool,
	) -> Result<(), DispatchError> {
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

	/// Move the market's persisted frozen flag to `target`, emitting one
	/// `ModeChanged` derived symmetrically from the loaded record: mode before
	/// the flag flips, mode after.
	///
	/// Entering flushes pending aggregate interest first so the frozen window
	/// accrues nothing; leaving folds the completed window into
	/// `interest_epoch` so `interest_time(now)` stays continuous. Callers gate
	/// on the current frozen state, so the same-state arms are unreachable.
	fn transition_frozen(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		target: Option<FrozenReason>,
	) -> Result<(), DispatchError> {
		let now = T::TimeProvider::now();
		let mut branch = Self::branch_of(collateral_id, stable_id)?;
		let old_mode = Self::mode_of(&branch, collateral_id, now).unwrap_or(BranchMode::Normal);
		let minted = match (branch.state.frozen, target) {
			(None, Some(_)) => {
				// Flush before freezing so the frozen window accrues nothing.
				Self::accrue_aggregate_interest(&mut branch.state, now)
			},
			(Some(frozen), None) => {
				// Keep `interest_time(now)` continuous across the frozen window.
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
		Self::commit_branch(collateral_id, stable_id, branch)?;
		// Mint only after the state is written, mirroring the operation-context
		// commit.
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

	/// Reconcile oracle-driven Frozen state with the live oracle.
	pub(crate) fn do_refresh_branch(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
	) -> Result<(), DispatchError> {
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

	/// Permissionless: ratchet a market's autoline ceiling. A no-op poke (autoline
	/// disabled, or the ceiling already at target) writes no storage.
	pub(crate) fn do_poke_ceiling(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
	) -> Result<(), DispatchError> {
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
