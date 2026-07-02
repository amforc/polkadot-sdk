use crate::{
	context::OpContext,
	math,
	pallet::{
		BalanceOf, BranchAdmin, BranchConfigs, BranchStates, Config, Error, Event,
		GlobalDebtCeiling, HoldReason, Pallet, PalletsOriginOf, Vaults,
	},
	recovery,
	types::{
		AdminLevel, BranchAdminInfo, BranchAdmins, BranchConfig, BranchConfigUpdate, BranchDebt,
		BranchMode, BranchRounding, BranchStakes, BranchState, DebtPayment, FrozenReason,
		FrozenState, InterestClock, RedistributionSnapshot, Vault, VaultDebt, VaultStatus,
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
		Consideration, ContainsPair, Footprint, Time,
	},
};
use pallet_linked_list::{Position, SortedListInterface};
use pusd_primitives::{OnBranchLifecycle, ProvidePrice};

/// Inputs to the shared [`Pallet::close_inner`] path beyond the operation
/// context. The two `AccountId` fields would be swap-prone as positional
/// arguments, so they are named here.
struct CloseRequest<'a, T: Config> {
	owner: &'a T::AccountId,
	recipient: &'a T::AccountId,
	vault: &'a Vault<BalanceOf<T>>,
	config: &'a BranchConfig<BalanceOf<T>>,
	status: VaultStatus,
	price: FixedU128,
	/// `Some` when a repay-to-zero closes an empty (no-collateral) vault: the
	/// payment to fold into the branch aggregates and the rate it was charged at.
	maybe_payment: Option<(DebtPayment<BalanceOf<T>>, FixedU128)>,
}

impl<T: Config> Pallet<T> {
	/// Enforce the per-collateral global debt ceiling. Every market on the collateral
	/// contributes its full outstanding stable liability — principal, minted interest,
	/// pending redistribution principal, and socialized bad debt — except that the
	/// borrowing market's `principal` is taken at its post-borrow value `principal_after`.
	/// The total, valued in the collateral's unit at `price`, must not exceed
	/// `GlobalDebtCeiling[collateral]` — the systemic backstop a single market's
	/// per-branch ceiling cannot see.
	///
	/// TODO: Two known limitations, deferred deliberately.
	/// (1) The fold sums `outstanding()` in raw units across *different* stable assets before one
	///     price conversion, which is only correct while every stable shares the same unit value
	///     ($1 par, same scale). Fix once the oracle is keyed by `(collateral, stable)`: convert
	///     each market's outstanding at its own pair price, then sum in collateral units.
	/// (2) The fold is O(markets on the collateral) inside every borrow while the extrinsic weight
	///     is flat, and market creation is permissionless — spamming markets on a popular
	///     collateral inflates every borrower's execution cost. Bound markets-per-collateral,
	///     scale the borrow weight by the fold length, or maintain a per-collateral aggregate.
	fn ensure_global_ceiling(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		principal_after: BalanceOf<T>,
		price: FixedU128,
	) -> Result<(), DispatchError> {
		let mut total = BalanceOf::<T>::zero();
		for (s, state) in BranchStates::<T>::iter_prefix(collateral_id) {
			total = total.saturating_add(state.debt.outstanding());
			if &s == stable_id {
				// The stored principal is pre-borrow; replace it with the post-borrow value.
				total = total.saturating_sub(state.debt.principal).saturating_add(principal_after);
			}
		}
		let collateral_debt = math::value_in_collateral::<BalanceOf<T>>(total, price)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		ensure!(
			collateral_debt <= GlobalDebtCeiling::<T>::get(collateral_id),
			Error::<T>::GlobalDebtCeilingExceeded
		);
		Ok(())
	}

	/// Enforce every borrow-admissibility ceiling for a market whose principal would
	/// become `principal_after`: the static per-market `debt_ceiling`, the autoline
	/// `effective_ceiling` (only when the autoline is enabled), and the
	/// per-collateral global ceiling. Shared by `do_open_vault` and `do_borrow`.
	fn ensure_within_ceilings(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		state: &BranchState<T::AccountId, BalanceOf<T>>,
		config: &BranchConfig<BalanceOf<T>>,
		principal_after: BalanceOf<T>,
		price: FixedU128,
	) -> Result<(), DispatchError> {
		ensure!(principal_after <= config.debt_ceiling, Error::<T>::DebtCeilingExceeded);
		if !config.ceiling_gap.is_zero() {
			ensure!(principal_after <= state.effective_ceiling, Error::<T>::DebtCeilingExceeded);
		}
		Self::ensure_global_ceiling(collateral_id, stable_id, principal_after, price)
	}

	pub(crate) fn do_open_vault(
		owner: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		initial_collateral: BalanceOf<T>,
		initial_debt: BalanceOf<T>,
		annual_rate: FixedU128,
		hint: Position<T::AccountId>,
	) -> Result<(), DispatchError> {
		let mut context = OpContext::<T>::load(collateral_id, stable_id)?;
		context.ensure_not_frozen()?;
		ensure!(
			!Vaults::<T>::contains_key((&context.collateral_id, &context.stable_id, &owner)),
			Error::<T>::VaultAlreadyExists
		);
		let config = context.config()?;
		ensure!(initial_debt >= config.minimum_debt, Error::<T>::DebtBelowMinimum);
		ensure!(
			initial_collateral >= config.minimum_collateral,
			Error::<T>::InsufficientCollateral
		);
		Self::validate_rate(&config, annual_rate)?;
		let price = context.price()?;

		// Advance the autoline in-band (still ttl-gated), so a borrower with valid
		// headroom does not need a separate `poke_ceiling` transaction first.
		Self::ratchet_ceiling(&mut context.state, &config, context.now);
		Self::ensure_within_ceilings(
			&context.collateral_id,
			&context.stable_id,
			&context.state,
			&config,
			context.state.debt.principal.saturating_add(initial_debt),
			price,
		)?;

		let upfront_fee =
			Self::open_upfront_fee(&context.state, &config, initial_debt, annual_rate);

		let vault = Vault {
			collateral: initial_collateral,
			debt: VaultDebt { principal: initial_debt, interest: upfront_fee },
			annual_rate,
			last_interest_time: context.state.interest_time(context.now),
			last_rate_update: context.now,
			redistribution_stake: initial_collateral,
			redistribution_snapshot: context.state.redistribution,
		};

		let total_debt = initial_debt.saturating_add(upfront_fee);
		Self::ensure_above_icr(initial_collateral, total_debt, price, &config)?;

		let mut branch_state_after = context.state.clone();
		branch_state_after.attach_vault(&vault);
		branch_state_after.add_collateral(initial_collateral);
		context.transition(branch_state_after, &config, price, false)?;

		T::CollateralAssets::hold(
			context.collateral_id.clone(),
			&HoldReason::VaultCollateral.into(),
			&owner,
			initial_collateral,
		)?;
		T::StableAssets::mint_into(context.stable_id.clone(), &owner, initial_debt)?;
		context.charge_upfront_fee(&owner, upfront_fee);

		T::VaultLists::insert(context.rate_list().clone(), owner.clone(), annual_rate, hint)
			.map_err(Self::map_error)?;

		Self::deposit_event(Event::Borrowed {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			owner: owner.clone(),
			recipient: owner.clone(),
			amount: initial_debt,
		});
		Self::deposit_event(Event::CollateralDeposited {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			owner: owner.clone(),
			from: owner.clone(),
			amount: initial_collateral,
		});
		Self::deposit_event(Event::VaultOpened {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			owner: owner.clone(),
		});
		context.commit_with_vault(&owner, &vault);
		Ok(())
	}

	/// Permissionless deposit. Dormant vaults must be revived by borrowing.
	pub(crate) fn do_deposit_collateral_for(
		from: T::AccountId,
		owner: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		amount: BalanceOf<T>,
	) -> Result<(), DispatchError> {
		let mut context = OpContext::<T>::load(collateral_id, stable_id)?;
		context.ensure_not_frozen()?;
		let (mut vault, status) = context.touch(&owner)?;
		ensure!(!status.is_dormant(), Error::<T>::DebtBelowMinimum);

		T::CollateralAssets::transfer_and_hold(
			context.collateral_id.clone(),
			&HoldReason::VaultCollateral.into(),
			&from,
			&owner,
			amount,
			Precision::Exact,
			Preservation::Expendable,
			Fortitude::Polite,
		)?;

		context.state.add_collateral(amount);
		vault.collateral = vault.collateral.saturating_add(amount);
		if status.is_active() {
			let old_stake = vault.redistribution_stake;
			let new_stake = old_stake.saturating_add(amount);
			context.state.refresh_vault_stake(vault.annual_rate, old_stake, new_stake);
			vault.redistribution_stake = new_stake;
		}

		Self::deposit_event(Event::CollateralDeposited {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			owner: owner.clone(),
			from,
			amount,
		});
		context.commit_with_vault(&owner, &vault);
		Ok(())
	}

	pub(crate) fn do_withdraw_collateral(
		owner: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		amount: BalanceOf<T>,
		recipient: T::AccountId,
	) -> Result<(), DispatchError> {
		let mut context = OpContext::<T>::load(collateral_id, stable_id)?;
		context.ensure_not_frozen()?;
		let price = context.price()?;
		let (mut vault, status) = context.touch(&owner)?;
		ensure!(!status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);

		let config = context.config()?;
		let collateral = vault.collateral;
		ensure!(collateral >= amount, Error::<T>::InsufficientCollateral);

		let total_debt = vault.debt.total();
		let new_collateral = collateral.saturating_sub(amount);
		if !total_debt.is_zero() {
			Self::ensure_above_icr(new_collateral, total_debt, price, &config)?;
		}

		let mut branch_state_after = context.state.clone();
		branch_state_after.remove_collateral(amount);
		branch_state_after.refresh_vault_stake(
			vault.annual_rate,
			vault.redistribution_stake,
			new_collateral,
		);
		context.transition(branch_state_after, &config, price, false)?;

		T::CollateralAssets::transfer_on_hold(
			context.collateral_id.clone(),
			&HoldReason::VaultCollateral.into(),
			&owner,
			&recipient,
			amount,
			Precision::Exact,
			Restriction::Free,
			Fortitude::Polite,
		)?;

		vault.redistribution_stake = new_collateral;
		vault.collateral = new_collateral;
		Self::deposit_event(Event::CollateralWithdrawn {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			owner: owner.clone(),
			recipient,
			amount,
		});
		context.commit_with_vault(&owner, &vault);
		Ok(())
	}

	pub(crate) fn do_borrow(
		owner: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		amount: BalanceOf<T>,
		maybe_new_rate: Option<FixedU128>,
		recipient: T::AccountId,
		hint: Position<T::AccountId>,
	) -> Result<(), DispatchError> {
		let mut context = OpContext::<T>::load(collateral_id, stable_id)?;
		context.ensure_not_frozen()?;
		let price = context.price()?;
		let (mut vault, pre_status) = context.touch(&owner)?;
		ensure!(!pre_status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);

		let config = context.config()?;
		let old_rate = vault.annual_rate;
		let new_rate = maybe_new_rate.unwrap_or(old_rate);
		Self::validate_rate(&config, new_rate)?;

		let new_ib_debt = vault.debt.principal.saturating_add(amount);
		// Advance the autoline in-band (still ttl-gated); see `do_open_vault`.
		Self::ratchet_ceiling(&mut context.state, &config, context.now);
		Self::ensure_within_ceilings(
			&context.collateral_id,
			&context.stable_id,
			&context.state,
			&config,
			context.state.debt.principal.saturating_add(amount),
			price,
		)?;

		let cooldown_elapsed = vault.cooldown_elapsed(&config, context.now);
		let rate_change_fee_base = vault.rate_change_base(maybe_new_rate, cooldown_elapsed);
		let (branch_state_after, upfront_fee) = Self::simulate_borrow(
			&context.state,
			&config,
			&vault,
			amount,
			new_rate,
			rate_change_fee_base,
		);

		let dormant_to_active = pre_status.is_dormant() && new_ib_debt >= config.minimum_debt;
		vault.debt.principal = new_ib_debt;
		vault.debt.interest = vault.debt.interest.saturating_add(upfront_fee);
		if old_rate != new_rate {
			vault.annual_rate = new_rate;
			vault.last_rate_update = context.now;
		}
		ensure!(vault.debt.principal >= config.minimum_debt, Error::<T>::DebtBelowMinimum);

		let collateral = vault.redistribution_stake;
		let total_debt = vault.debt.total();
		Self::ensure_above_icr(collateral, total_debt, price, &config)?;

		context.transition(branch_state_after, &config, price, false)?;

		if dormant_to_active {
			context.state.release_dormant_target(&owner);
		}

		T::StableAssets::mint_into(context.stable_id.clone(), &recipient, amount)?;
		context.charge_upfront_fee(&owner, upfront_fee);

		if dormant_to_active {
			T::VaultLists::insert(context.rate_list().clone(), owner.clone(), new_rate, hint)
				.map_err(Self::map_error)?;
			Self::deposit_event(Event::VaultStatusChanged {
				collateral_id: context.collateral_id.clone(),
				stable_id: context.stable_id.clone(),
				owner: owner.clone(),
				old_status: VaultStatus::Dormant,
				new_status: VaultStatus::Active,
			});
		} else if old_rate != new_rate {
			T::VaultLists::re_insert(context.rate_list().clone(), owner.clone(), new_rate, hint)
				.map_err(Self::map_error)?;
		}

		if old_rate != new_rate {
			Self::deposit_event(Event::BorrowRateChanged {
				collateral_id: context.collateral_id.clone(),
				stable_id: context.stable_id.clone(),
				owner: owner.clone(),
				old_rate,
				new_rate,
			});
		}
		Self::deposit_event(Event::Borrowed {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			owner: owner.clone(),
			recipient,
			amount,
		});
		context.commit_with_vault(&owner, &vault);
		Ok(())
	}

	pub(crate) fn do_repay_for(
		from: T::AccountId,
		owner: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		amount: BalanceOf<T>,
	) -> Result<(), DispatchError> {
		let mut context = OpContext::<T>::load(collateral_id, stable_id)?;
		context.ensure_not_frozen()?;
		let (mut vault, pre_status) = context.touch(&owner)?;
		ensure!(!pre_status.is_final_recovery(), Error::<T>::VaultInFinalRecovery);

		let config = context.config()?;

		// Cap overpayment at the touched debt.
		let repay = amount.min(vault.debt.total());
		T::StableAssets::burn_from(
			context.stable_id.clone(),
			&from,
			repay,
			Preservation::Expendable,
			Precision::Exact,
			Fortitude::Polite,
		)?;

		let payment = vault.debt.cancel(repay);
		debug_assert_eq!(payment.total(), repay);

		let new_total = vault.debt.total();
		if !new_total.is_zero() && new_total < config.minimum_debt {
			return Err(Error::<T>::DebtWouldBecomeDust.into());
		}

		if new_total.is_zero() {
			Self::deposit_event(Event::Repaid {
				collateral_id: context.collateral_id.clone(),
				stable_id: context.stable_id.clone(),
				owner: owner.clone(),
				from,
				amount: repay,
			});
			// A vault with no collateral left to reclaim (a fully-redeemed husk) is
			// closed outright — there is nothing to keep it open for.
			if vault.collateral.is_zero() {
				let price = context.price()?;
				Self::close_inner(
					context,
					CloseRequest {
						owner: &owner,
						recipient: &owner,
						vault: &vault,
						config: &config,
						status: pre_status,
						price,
						maybe_payment: Some((payment, vault.annual_rate)),
					},
				)?;
				return Ok(());
			}
			// Repaying to zero does not close the vault: the collateral stays held and
			// the row survives as a zero-debt Dormant husk (mirroring a redeem-to-zero).
			// The owner reclaims collateral with an explicit `close_vault`. Keeping the
			// vault open lets debt be repaid in Safety mode purely to improve branch TCR.
			context
				.state
				.apply_debt_payment(payment, vault.annual_rate, vault.debt.principal);
			context.state.release_dormant_target(&owner);
			if pre_status.is_active() {
				T::VaultLists::remove(context.rate_list(), &owner)
					.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
			}
			context.commit_with_vault(&owner, &vault);
			return Ok(());
		}

		context
			.state
			.apply_debt_payment(payment, vault.annual_rate, vault.debt.principal);
		Self::deposit_event(Event::Repaid {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			owner: owner.clone(),
			from,
			amount: repay,
		});
		context.commit_with_vault(&owner, &vault);
		Ok(())
	}

	pub(crate) fn do_change_rate(
		owner: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		new_rate: FixedU128,
		hint: Position<T::AccountId>,
	) -> Result<(), DispatchError> {
		let mut context = OpContext::<T>::load(collateral_id, stable_id)?;
		context.ensure_not_frozen()?;
		let (mut vault, status) = context.touch(&owner)?;
		ensure!(status.is_active(), Error::<T>::InvalidVaultStatus);
		let old_rate = vault.annual_rate;
		if old_rate == new_rate {
			context.commit_with_vault(&owner, &vault);
			return Ok(());
		}

		let config = context.config()?;
		Self::validate_rate(&config, new_rate)?;

		let cooldown_elapsed = vault.cooldown_elapsed(&config, context.now);
		let (branch_state_after, upfront_fee) =
			Self::simulate_change_rate(&context.state, &config, &vault, new_rate, cooldown_elapsed);

		let price = context.price()?;
		context.transition(branch_state_after, &config, price, false)?;

		context.charge_upfront_fee(&owner, upfront_fee);

		vault.annual_rate = new_rate;
		vault.last_rate_update = context.now;
		vault.debt.interest = vault.debt.interest.saturating_add(upfront_fee);

		T::VaultLists::re_insert(context.rate_list().clone(), owner.clone(), new_rate, hint)
			.map_err(Self::map_error)?;
		Self::deposit_event(Event::BorrowRateChanged {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			owner: owner.clone(),
			old_rate,
			new_rate,
		});
		context.commit_with_vault(&owner, &vault);
		Ok(())
	}

	pub(crate) fn do_close_vault(
		owner: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		recipient: Option<T::AccountId>,
	) -> Result<(), DispatchError> {
		let recipient = recipient.unwrap_or(owner.clone());
		let mut context = OpContext::<T>::load(collateral_id, stable_id)?;
		context.ensure_not_frozen()?;
		let price = context.price()?;
		let (vault, status) = context.touch(&owner)?;
		ensure!(vault.debt.total().is_zero(), Error::<T>::DebtOutstanding);

		let config = context.config()?;
		Self::close_inner(
			context,
			CloseRequest {
				owner: &owner,
				recipient: &recipient,
				vault: &vault,
				config: &config,
				status,
				price,
				maybe_payment: None,
			},
		)
	}

	/// Shared close path. Consumes the operation context at commit.
	fn close_inner(
		mut context: OpContext<T>,
		request: CloseRequest<'_, T>,
	) -> Result<(), DispatchError> {
		let CloseRequest { owner, recipient, vault, config, status, price, maybe_payment } =
			request;
		// The row tracks this market's collateral in every state, FinalRecovery
		// included (where the stake is zero but the collateral persists).
		let collateral = vault.collateral;
		let mut branch_state_after = context.state.clone();
		if let Some((payment, rate)) = maybe_payment {
			branch_state_after.apply_debt_payment(payment, rate, vault.debt.principal);
		}
		branch_state_after.detach_vault(vault);
		branch_state_after.remove_collateral(collateral);
		branch_state_after.release_dormant_target(owner);

		let branch_empties = branch_state_after.is_empty_of_liability();
		// Sweep now — it mutates `branch_state_after` ahead of the TCR check — but defer the
		// event until the close is past every fallible step, just before commit.
		let orphan_debt = if branch_empties {
			branch_state_after.sweep_orphan_debt()
		} else {
			BalanceOf::<T>::zero()
		};

		context.transition(branch_state_after, config, price, branch_empties)?;

		if !collateral.is_zero() {
			T::CollateralAssets::transfer_on_hold(
				context.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				owner,
				recipient,
				collateral,
				Precision::Exact,
				Restriction::Free,
				Fortitude::Polite,
			)?;
		}

		match status {
			VaultStatus::Active => {
				// Active vaults must be in the rate index.
				T::VaultLists::remove(context.rate_list(), owner)
					.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
			},
			VaultStatus::FinalRecovery => {
				recovery::remove::<T>(&context.collateral_id, &context.stable_id, owner)?;
			},
			VaultStatus::Dormant => {},
		}

		if !orphan_debt.is_zero() {
			Self::deposit_event(Event::BadDebtRecorded {
				collateral_id: context.collateral_id.clone(),
				stable_id: context.stable_id.clone(),
				amount: orphan_debt,
			});
		}
		Self::deposit_event(Event::VaultClosed {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			owner: owner.clone(),
			recipient: recipient.clone(),
		});
		context.commit_removing_vault(owner);
		Ok(())
	}

	pub(crate) fn do_poke(
		owner: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
	) -> Result<(), DispatchError> {
		OpContext::<T>::refresh(collateral_id, stable_id, &owner)
	}

	pub(crate) fn do_enter_final_recovery(
		owner: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
	) -> Result<(), DispatchError> {
		let mut context = OpContext::<T>::load(collateral_id, stable_id)?;
		context.ensure_not_frozen()?;
		let price = context.price()?;
		let (mut vault, status) = context.touch(&owner)?;
		ensure!(status.is_active(), Error::<T>::InvalidVaultStatus);

		let config = context.config()?;
		let collateral = vault.redistribution_stake;
		let total_debt = vault.debt.total();
		Self::ensure_below_mcr(collateral, total_debt, price, &config)?;

		ensure!(
			context.state.stakes.total == vault.redistribution_stake,
			Error::<T>::NotLastEligibleVault
		);

		T::VaultLists::remove(context.rate_list(), &owner)
			.map_err(|_| Error::<T>::RateIndexInvariantBroken)?;
		context.state.refresh_vault_stake(
			vault.annual_rate,
			vault.redistribution_stake,
			BalanceOf::<T>::zero(),
		);
		vault.redistribution_stake = BalanceOf::<T>::zero();
		recovery::append::<T>(
			&mut context.state,
			&context.collateral_id,
			&context.stable_id,
			owner.clone(),
		)?;

		Self::deposit_event(Event::VaultStatusChanged {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			owner: owner.clone(),
			old_status: VaultStatus::Active,
			new_status: VaultStatus::FinalRecovery,
		});
		context.commit_with_vault(&owner, &vault);
		Ok(())
	}

	/// Explicit `FinalRecovery` exit. Rejoins the rate index only above `MinimumDebt`.
	pub(crate) fn do_exit_final_recovery(
		owner: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		hint: Position<T::AccountId>,
	) -> Result<(), DispatchError> {
		let mut context = OpContext::<T>::load(collateral_id, stable_id)?;
		context.ensure_not_frozen()?;
		let price = context.price()?;
		let (mut vault, status) = context.touch(&owner)?;
		ensure!(status.is_final_recovery(), Error::<T>::InvalidVaultStatus);

		let config = context.config()?;
		let collateral = vault.collateral;
		let total_debt = vault.debt.total();
		Self::ensure_at_or_above_mcr(collateral, total_debt, price, &config)?;

		let rejoin_active = total_debt >= config.minimum_debt;
		let new_status = if rejoin_active { VaultStatus::Active } else { VaultStatus::Dormant };

		recovery::remove::<T>(&context.collateral_id, &context.stable_id, &owner)?;
		vault.redistribution_stake = collateral;
		vault.redistribution_snapshot = context.state.redistribution;
		context
			.state
			.refresh_vault_stake(vault.annual_rate, BalanceOf::<T>::zero(), collateral);
		if !rejoin_active &&
			!total_debt.is_zero() &&
			!context.state.try_park_dormant_target(owner.clone())
		{
			return Err(Error::<T>::DormantTargetOccupied.into());
		}
		if rejoin_active {
			T::VaultLists::insert(
				context.rate_list().clone(),
				owner.clone(),
				vault.annual_rate,
				hint,
			)
			.map_err(Self::map_error)?;
		}
		Self::deposit_event(Event::VaultStatusChanged {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			owner: owner.clone(),
			old_status: VaultStatus::FinalRecovery,
			new_status,
		});
		context.commit_with_vault(&owner, &vault);
		Ok(())
	}

	/// Permissionless Dormant to Active revival.
	pub(crate) fn do_activate_dormant(
		owner: T::AccountId,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		hint: Position<T::AccountId>,
	) -> Result<(), DispatchError> {
		let mut context = OpContext::<T>::load(collateral_id, stable_id)?;
		context.ensure_not_frozen()?;
		let (vault, status) = context.touch(&owner)?;
		ensure!(status.is_dormant(), Error::<T>::InvalidVaultStatus);
		let config = context.config()?;
		ensure!(vault.debt.total() >= config.minimum_debt, Error::<T>::DebtBelowMinimum);

		T::VaultLists::insert(context.rate_list().clone(), owner.clone(), vault.annual_rate, hint)
			.map_err(Self::map_error)?;
		context.state.release_dormant_target(&owner);

		Self::deposit_event(Event::VaultStatusChanged {
			collateral_id: context.collateral_id.clone(),
			stable_id: context.stable_id.clone(),
			owner: owner.clone(),
			old_status: VaultStatus::Dormant,
			new_status: VaultStatus::Active,
		});
		context.commit_with_vault(&owner, &vault);
		Ok(())
	}

	fn register_branch(
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		config: BranchConfig<BalanceOf<T>>,
	) -> Result<(), DispatchError> {
		ensure!(
			!BranchConfigs::<T>::contains_key((&collateral_id, &stable_id)),
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
		ensure!(
			!T::SameAsset::contains(&collateral_id, &stable_id),
			Error::<T>::StableCollateralCollision
		);
		for (existing_collateral, existing_stable) in BranchConfigs::<T>::iter_keys() {
			ensure!(
				!T::SameAsset::contains(&existing_collateral, &stable_id),
				Error::<T>::StableCollateralCollision
			);
			ensure!(
				!T::SameAsset::contains(&collateral_id, &existing_stable),
				Error::<T>::StableCollateralCollision
			);
		}
		ensure!(
			BranchConfigs::<T>::count() < <T::MaxBranches as Get<u32>>::get(),
			Error::<T>::TooManyBranches
		);
		let redistribution_account = Self::redistribution_account(&collateral_id, &stable_id);
		if frame_system::Pallet::<T>::providers(&redistribution_account) == 0 {
			frame_system::Pallet::<T>::inc_providers(&redistribution_account);
		}
		let now = T::TimeProvider::now();
		let initial_ceiling = if config.ceiling_gap.is_zero() {
			config.debt_ceiling
		} else {
			config.ceiling_gap.min(config.debt_ceiling)
		};
		BranchConfigs::<T>::insert((&collateral_id, &stable_id), config);
		BranchStates::<T>::insert(
			&collateral_id,
			&stable_id,
			BranchState {
				total_collateral: BalanceOf::<T>::zero(),
				debt: BranchDebt {
					principal: BalanceOf::<T>::zero(),
					minted_interest: BalanceOf::<T>::zero(),
					pending_redistribution_principal: BalanceOf::<T>::zero(),
					bad_debt: BalanceOf::<T>::zero(),
					weighted_principal_sum: BalanceOf::<T>::zero(),
					// Interest time is 0 at the epoch base (`now`); see `interest_time`.
					last_interest_time: Zero::zero(),
				},
				stakes: BranchStakes {
					total: BalanceOf::<T>::zero(),
					weighted_sum: BalanceOf::<T>::zero(),
				},
				rounding: BranchRounding::default(),
				redistribution: RedistributionSnapshot::default(),
				interest_clock: InterestClock { epoch_base: now, frozen_elapsed: Zero::zero() },
				next_final_recovery_nonce: 0,
				dormant_redemption_target: None,
				idle_cursor: None,
				frozen: None,
				effective_ceiling: initial_ceiling,
				ceiling_last_inc: now,
			},
		);
		T::OnBranchLifecycle::on_registered(&collateral_id, &stable_id)?;
		Self::deposit_event(Event::BranchRegistered { collateral_id, stable_id });
		Ok(())
	}

	/// Permissionless market creation. Validates the config against the governance
	/// envelope and the oracle, takes the creation deposit (unless Root), seeds the
	/// market, and stores its admins.
	pub(crate) fn do_create_branch(
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		admins: BranchAdmins<PalletsOriginOf<T>>,
		config: BranchConfig<BalanceOf<T>>,
		depositor: Option<T::AccountId>,
	) -> Result<(), DispatchError> {
		ensure!(T::BranchConfigGuard::get().permits(&config), Error::<T>::ConfigOutsideEnvelope);
		// A market the oracle cannot price cannot open.
		T::Oracle::provide_price(&collateral_id)
			.map_err(|_| Error::<T>::OraclePriceNotAvailable)?;
		let deposit = match depositor {
			Some(who) => {
				let footprint = Footprint::from_mel::<
					BranchAdminInfo<PalletsOriginOf<T>, T::AccountId, T::Consideration>,
				>();
				let ticket = T::Consideration::new(&who, footprint)?;
				Some((who, ticket))
			},
			None => None,
		};
		Self::register_branch(collateral_id.clone(), stable_id.clone(), config)?;
		BranchAdmin::<T>::insert(
			(&collateral_id, &stable_id),
			BranchAdminInfo {
				full_admin: admins.full_admin,
				emergency_admin: admins.emergency_admin,
				deposit,
			},
		);
		Ok(())
	}

	/// Remove an empty market: refund the deposit, release the redistribution-account
	/// provider, tear down storage, and fire the deregistration hook.
	pub(crate) fn do_remove_branch(
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
	) -> Result<(), DispatchError> {
		let state = Self::branch_state_of(&collateral_id, &stable_id)?;
		ensure!(state.is_removable(), Error::<T>::MarketNotEmpty);
		ensure!(
			Vaults::<T>::iter_prefix((&collateral_id, &stable_id)).next().is_none(),
			Error::<T>::MarketNotEmpty
		);
		let info = BranchAdmin::<T>::get((&collateral_id, &stable_id))
			.ok_or(Error::<T>::UnknownCollateral)?;
		if let Some((who, ticket)) = info.deposit {
			ticket.drop(&who)?;
		}
		let redistribution_account = Self::redistribution_account(&collateral_id, &stable_id);
		if frame_system::Pallet::<T>::providers(&redistribution_account) > 0 {
			let _ = frame_system::Pallet::<T>::dec_providers(&redistribution_account);
		}
		BranchConfigs::<T>::remove((&collateral_id, &stable_id));
		BranchStates::<T>::remove(&collateral_id, &stable_id);
		BranchAdmin::<T>::remove((&collateral_id, &stable_id));
		T::OnBranchLifecycle::on_deregistered(&collateral_id, &stable_id)?;
		Self::deposit_event(Event::BranchRemoved { collateral_id, stable_id });
		Ok(())
	}

	/// Authorize, validate, and apply a single-field config update, emitting
	/// `ParameterUpdated`. The required admin tier, the `Emergency`-only "must be
	/// defensive" rule, and the governance-envelope check are all derived from the
	/// `update` itself ([`BranchConfigUpdate::required_level`] /
	/// [`BranchConfigUpdate::is_defensive`] and
	/// [`crate::types::BranchConfigGuard::permits`]), so each `set_*` dispatchable
	/// is a thin wrapper over this one path. The whole post-update config is
	/// re-validated through the same `permits` gate `create_branch` applies,
	/// keeping envelope enforcement in a single place.
	pub(crate) fn do_set_param(
		origin: OriginFor<T>,
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		update: BranchConfigUpdate<BalanceOf<T>>,
	) -> Result<(), DispatchError> {
		let level =
			Self::ensure_branch_admin(origin, &collateral_id, &stable_id, update.required_level())?;
		let guard = T::BranchConfigGuard::get();
		BranchConfigs::<T>::try_mutate(
			(&collateral_id, &stable_id),
			|maybe| -> Result<_, DispatchError> {
				let config = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
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

	pub(crate) fn do_enable_frozen_mode(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Result<(), DispatchError> {
		if Self::branch_state_of(collateral_id, stable_id)?.is_frozen() {
			return Ok(());
		}
		Self::enter_frozen(collateral_id, stable_id, FrozenReason::Governance)
	}

	fn enter_frozen(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
		reason: FrozenReason,
	) -> Result<(), DispatchError> {
		let now = T::TimeProvider::now();
		let old_mode = Self::current_mode(collateral_id, stable_id).unwrap_or(BranchMode::Normal);
		BranchStates::<T>::try_mutate(
			collateral_id,
			stable_id,
			|maybe| -> Result<_, DispatchError> {
				let state = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
				// Flush before freezing so the frozen window accrues nothing.
				let minted = Self::accrue_aggregate_interest(state, now);
				if !minted.is_zero() {
					Self::mint_and_route_yield(stable_id, minted);
				}
				state.frozen = Some(FrozenState { reason, entered_at: now });
				Ok(())
			},
		)?;
		Self::deposit_event(Event::ModeChanged {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			old_mode,
			new_mode: BranchMode::Frozen,
		});
		Ok(())
	}

	/// Reconcile oracle-driven Frozen state with the live oracle.
	pub(crate) fn do_refresh_branch(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Result<(), DispatchError> {
		let state = Self::branch_state_of(collateral_id, stable_id)?;
		let oracle_ok = T::Oracle::provide_price(collateral_id).is_ok();
		match (state.frozen, oracle_ok) {
			(Some(state), true) if matches!(state.reason, FrozenReason::OracleFailure) => {
				Self::clear_frozen(collateral_id, stable_id)
			},
			(None, false) => {
				Self::enter_frozen(collateral_id, stable_id, FrozenReason::OracleFailure)
			},
			_ => Ok(()),
		}
	}

	fn clear_frozen(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Result<(), DispatchError> {
		let now = T::TimeProvider::now();
		BranchStates::<T>::try_mutate(
			collateral_id,
			stable_id,
			|maybe| -> Result<_, DispatchError> {
				let state = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
				// Keep `interest_time(now)` continuous across the frozen window.
				let entered_at = state.frozen.as_ref().map(|state| state.entered_at);
				if let Some(entered_at) = entered_at {
					let frozen_window = now.saturating_sub(entered_at);
					state.interest_clock.frozen_elapsed =
						state.interest_clock.frozen_elapsed.saturating_add(frozen_window);
				}
				state.frozen = None;
				Ok(())
			},
		)?;
		// `Frozen` is the only persisted mode, so the branch always leaves it for the
		// live TCR-derived mode.
		let new_mode = Self::current_mode(collateral_id, stable_id).unwrap_or(BranchMode::Normal);
		Self::deposit_event(Event::ModeChanged {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			old_mode: BranchMode::Frozen,
			new_mode,
		});
		Ok(())
	}

	/// Clear a governance-induced Frozen state. No-op when not frozen, or when
	/// frozen for a non-governance reason.
	pub(crate) fn do_clear_governance_frozen_mode(
		collateral_id: &T::CollateralAssetId,
		stable_id: &T::StableAssetId,
	) -> Result<(), DispatchError> {
		let state = Self::branch_state_of(collateral_id, stable_id)?;
		match state.frozen {
			Some(state) if matches!(state.reason, FrozenReason::Governance) => {
				Self::clear_frozen(collateral_id, stable_id)
			},
			_ => Ok(()),
		}
	}

	/// Permissionless: ratchet a market's autoline ceiling. A no-op poke (autoline
	/// disabled, or the ceiling already at target) writes no storage.
	pub(crate) fn do_poke_ceiling(
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
	) -> Result<(), DispatchError> {
		let config = Self::branch_config_of(&collateral_id, &stable_id)?;
		if config.ceiling_gap.is_zero() {
			return Ok(());
		}
		let now = T::TimeProvider::now();
		let mut state = Self::branch_state_of(&collateral_id, &stable_id)?;
		if Self::ratchet_ceiling(&mut state, &config, now) {
			BranchStates::<T>::insert(&collateral_id, &stable_id, state);
		}
		Ok(())
	}
}
