//! Per-dispatch branch state context with deferred yield routing.

use super::{
	accounting::{accrue_aggregate_interest, mint_and_route_yield, pending_touch_for, YieldSource},
	*,
};

pub(crate) struct TouchedVault<Balance> {
	pub vault: Vault<Balance>,
	pub status: VaultStatus,
}

/// Threads one branch-state read through an operation and commits it once.
pub(crate) struct OpContext<T: Config> {
	pub collateral_id: T::AssetId,
	pub now: Millis,
	pub state: BranchState<T::AccountId, BalanceOf<T>>,
	pending_interest_mint: BalanceOf<T>,
	pending_fee: Option<(T::AccountId, BalanceOf<T>)>,
	pending_interest_accrued: Option<(T::AccountId, BalanceOf<T>)>,
	#[cfg(debug_assertions)]
	loaded: BranchState<T::AccountId, BalanceOf<T>>,
}

impl<T: Config> OpContext<T> {
	/// Read the branch state and accrue aggregate interest in memory.
	pub fn load(collateral_id: T::AssetId) -> Result<Self, DispatchError> {
		let now = T::TimeProvider::now();
		let mut state = branch_state_of::<T>(&collateral_id)?;
		#[cfg(debug_assertions)]
		let loaded = state.clone();

		let pending_interest_mint = accrue_aggregate_interest::<T>(&mut state, now);

		Ok(Self {
			collateral_id,
			now,
			state,
			pending_interest_mint,
			pending_fee: None,
			pending_interest_accrued: None,
			#[cfg(debug_assertions)]
			loaded,
		})
	}

	/// Refresh one vault. This is intentionally allowed while frozen.
	pub fn refresh(collateral_id: T::AssetId, owner: &T::AccountId) -> Result<(), DispatchError> {
		let mut context = Self::load(collateral_id)?;
		let touched = context.touch(owner)?;
		context.commit_with_vault(owner, &touched.vault);
		Ok(())
	}

	pub fn ensure_not_frozen(&self) -> Result<(), DispatchError> {
		ensure!(!self.state.is_frozen(), Error::<T>::BranchFrozen);
		Ok(())
	}

	/// The branch's rate-index list id.
	pub fn rate_list(&self) -> VaultListId<T::AssetId> {
		VaultListId::Rate(self.collateral_id.clone())
	}

	/// Oracle price for this context's collateral.
	pub fn price(&self) -> Result<FixedU128, DispatchError> {
		Ok(T::Oracle::provide_price(&self.collateral_id)?.price)
	}

	/// Branch config for this context's collateral.
	pub fn config(&self) -> Result<BranchConfig<BalanceOf<T>>, DispatchError> {
		branch_config_of::<T>(&self.collateral_id)
	}

	/// Adopt `next` as the branch state, but only if the TCR mode rules permit
	/// the pre→post transition. `is_settlement` relaxes the worsening checks on
	/// the liquidation/close settlement paths (see [`enforce_mode_rules`]).
	pub fn transition(
		&mut self,
		next: BranchState<T::AccountId, BalanceOf<T>>,
		config: &BranchConfig<BalanceOf<T>>,
		price: FixedU128,
		is_settlement: bool,
	) -> Result<(), DispatchError> {
		let pre_tcr = compute_tcr::<T>(&self.state, price, self.now)?;
		let post_tcr = compute_tcr::<T>(&next, price, self.now)?;
		enforce_mode_rules::<T>(config, &self.state, pre_tcr, post_tcr, is_settlement)?;
		self.state = next;
		Ok(())
	}

	/// Apply pending interest/redistribution to a vault row in memory.
	pub fn touch(
		&mut self,
		owner: &T::AccountId,
	) -> Result<TouchedVault<BalanceOf<T>>, DispatchError> {
		let mut vault = vault_of::<T>(&self.collateral_id, owner)?;
		let status = vault.status::<T>(&self.collateral_id, owner);
		let pending = pending_touch_for::<T>(&vault, &self.state, self.now);

		if !pending.interest.is_zero() {
			vault.debt.interest = vault.debt.interest.saturating_add(pending.interest);
			debug_assert!(self.pending_interest_accrued.is_none(), "one touch per context");
			self.pending_interest_accrued = Some((owner.clone(), pending.interest));
		}
		if !pending.principal.is_zero() {
			self.state.debt.pending_redistribution_principal = self
				.state
				.debt
				.pending_redistribution_principal
				.saturating_sub(pending.principal);
			self.state.debt.principal = self.state.debt.principal.saturating_add(pending.principal);
			// Replace the avg-rate redistribution contribution with this
			// vault's own-rate contribution.
			let delta_weight_per_stake = self
				.state
				.redistribution
				.weight_per_stake
				.saturating_sub(vault.redistribution_snapshot.weight_per_stake);
			let weight_to_remove =
				delta_weight_per_stake.saturating_mul_int(vault.redistribution_stake);
			let principal_before = vault.debt.principal;
			vault.debt.principal = vault.debt.principal.saturating_add(pending.principal);
			self.state.debt.weighted_principal_sum = self
				.state
				.debt
				.weighted_principal_sum
				.saturating_sub(weight_to_remove)
				.saturating_sub(vault.annual_rate.saturating_mul_int(principal_before))
				.saturating_add(vault.annual_rate.saturating_mul_int(vault.debt.principal));
		}
		if !pending.collateral.is_zero() {
			// Already counted in `state.total_collateral`; only the hold moves.
			T::CollateralAssets::transfer_on_hold(
				self.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				&Pallet::<T>::redistribution_account(),
				owner,
				pending.collateral,
				Precision::Exact,
				Restriction::OnHold,
				Fortitude::Polite,
			)?;
		}

		if vault.redistribution_snapshot != self.state.redistribution {
			vault.redistribution_snapshot = self.state.redistribution;
		}
		vault.last_interest_time = self.state.interest_time(self.now);

		// FinalRecovery vaults are not stake-bearing.
		if !status.is_final_recovery() {
			let held = T::CollateralAssets::balance_on_hold(
				self.collateral_id.clone(),
				&HoldReason::VaultCollateral.into(),
				owner,
			);
			if vault.redistribution_stake != held {
				self.state
					.refresh_vault_stake(vault.annual_rate, vault.redistribution_stake, held);
				vault.redistribution_stake = held;
			}
		}

		Ok(TouchedVault { vault, status })
	}

	pub fn charge_upfront_fee(&mut self, owner: &T::AccountId, amount: BalanceOf<T>) {
		if amount.is_zero() {
			return;
		}
		debug_assert!(self.pending_fee.is_none(), "one upfront fee per dispatch");
		self.pending_fee = Some((owner.clone(), amount));
	}

	pub fn commit_with_vault(self, owner: &T::AccountId, vault: &Vault<BalanceOf<T>>) {
		self.assert_unclobbered();
		Vaults::<T>::insert(&self.collateral_id, owner, vault);
		BranchStates::<T>::insert(&self.collateral_id, &self.state);
		self.finish();
	}

	pub fn commit_removing_vault(self, owner: &T::AccountId) {
		self.assert_unclobbered();
		Vaults::<T>::remove(&self.collateral_id, owner);
		BranchStates::<T>::insert(&self.collateral_id, &self.state);
		self.finish();
	}

	/// Runs external hooks after the storage commit.
	fn finish(self) {
		if !self.pending_interest_mint.is_zero() {
			mint_and_route_yield::<T>(
				&self.collateral_id,
				self.pending_interest_mint,
				YieldSource::BranchInterest,
			);
		}
		if let Some((owner, amount)) = self.pending_interest_accrued {
			Pallet::<T>::deposit_event(Event::InterestAccrued {
				collateral_id: self.collateral_id.clone(),
				owner,
				amount,
			});
		}
		if let Some((fee_owner, fee)) = self.pending_fee {
			mint_and_route_yield::<T>(&self.collateral_id, fee, YieldSource::UpfrontFee);
			Pallet::<T>::deposit_event(Event::UpfrontFeeCharged {
				collateral_id: self.collateral_id.clone(),
				owner: fee_owner,
				amount: fee,
			});
		}
	}

	fn assert_unclobbered(&self) {
		#[cfg(debug_assertions)]
		debug_assert_eq!(
			BranchStates::<T>::get(&self.collateral_id).as_ref(),
			Some(&self.loaded),
			"BranchStates mutated behind OpContext"
		);
	}
}
