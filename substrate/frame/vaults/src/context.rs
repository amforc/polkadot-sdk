//! Per-dispatch operation contexts with deferred yield routing.
//!
//! Two phases: [`OpContext::load`] opens the branch-level context, and
//! [`OpContext::touch`] consumes it into a [`VaultOp`] that owns the touched
//! vault row, its owner key, and its derived status. A commit consumes the
//! phase value, writes the row it owns, and applies the [`TcrGate`] the
//! operation declares. The protocol is structural: an operation cannot touch
//! twice, commit a row other than the one it touched, or commit without
//! naming its TCR stance.

use crate::{
	pallet::{BalanceOf, BranchStates, Config, Error, Event, HoldReason, Millis, Pallet, Vaults},
	types::{BranchConfig, BranchState, Vault, VaultListId, VaultStatus},
	utility_impls::TcrInputs,
};
use frame::{
	prelude::*,
	traits::{fungibles::MutateHold as FungiblesMutateHold, tokens::Restriction, Time},
};
use pusd_primitives::ProvidePrice;

/// TCR admissibility rule a commit declares, applied to the operation's
/// baseline → committed state change. Every commit names one, so each
/// operation's mode-rule stance is explicit (and greppable) at its commit
/// site.
pub(crate) enum TcrGate<'a, Balance> {
	/// Pre- and post-TCR are computed — surfacing arithmetic overflow. Unless
	/// `settlement` (a branch-emptying close, where worsening is allowed), the
	/// Normal/Safety mode rules apply: the op may not drop the branch TCR
	/// below the safety threshold, nor worsen it while already below.
	Check { price: FixedU128, config: &'a BranchConfig<Balance>, settlement: bool },
	/// No TCR computation: the op is structurally TCR-improving (repay,
	/// deposit, redemption) or gated elsewhere (MCR on the liquidation and
	/// recovery paths).
	Exempt,
}

/// Branch-level operation context: one branch-state read threaded through an
/// operation and committed once.
pub(crate) struct OpContext<T: Config> {
	pub collateral_id: T::CollateralAssetId,
	pub stable_id: T::StableAssetId,
	pub now: Millis,
	pub state: BranchState<T::AccountId, BalanceOf<T>>,
	pending_interest_mint: BalanceOf<T>,
	pending_fee: Option<BalanceOf<T>>,
	/// The post-accrual TCR inputs at load: the "pre" side of the commit's
	/// TCR gate. A touch preserves them (the debt sum and collateral are
	/// invariant under a touch), and the captured pair is structurally
	/// immutable, so it doubles as the [`VaultOp`] baseline.
	tcr_baseline: TcrInputs<BalanceOf<T>>,
	#[cfg(debug_assertions)]
	loaded: BranchState<T::AccountId, BalanceOf<T>>,
}

/// Vault-level operation: a [`OpContext::touch`]ed context that owns the vault
/// row it settled. Commits write `vault` under `owner`, so the touched row and
/// the committed row cannot diverge. Branch-level fields and helpers stay on
/// the composed [`OpContext`], reached as `op.ctx`.
pub(crate) struct VaultOp<T: Config> {
	pub ctx: OpContext<T>,
	pub owner: T::AccountId,
	pub vault: Vault<BalanceOf<T>>,
	pub status: VaultStatus,
	pending_interest_accrued: Option<BalanceOf<T>>,
}

impl<T: Config> OpContext<T> {
	/// Read the branch state and accrue aggregate interest in memory.
	pub fn load(
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
	) -> Result<Self, DispatchError> {
		let now = T::TimeProvider::now();
		let mut state = Pallet::<T>::branch_state_of(&collateral_id, &stable_id)?;
		#[cfg(debug_assertions)]
		let loaded = state.clone();

		let pending_interest_mint = Pallet::<T>::accrue_aggregate_interest(&mut state, now);

		// The accrual above folded pending aggregate interest into the state,
		// so the baseline debt is exactly the sum `compute_tcr` would see.
		let tcr_baseline = TcrInputs {
			collateral: state.total_collateral,
			debt: Pallet::<T>::accrued_branch_debt(&state, now),
		};
		Ok(Self {
			collateral_id,
			stable_id,
			now,
			tcr_baseline,
			state,
			pending_interest_mint,
			pending_fee: None,
			#[cfg(debug_assertions)]
			loaded,
		})
	}

	/// Refresh one vault. This is intentionally allowed while frozen.
	pub fn refresh(
		collateral_id: T::CollateralAssetId,
		stable_id: T::StableAssetId,
		owner: &T::AccountId,
	) -> Result<(), DispatchError> {
		let op = Self::load(collateral_id, stable_id)?;
		let op = op.touch(owner)?;
		op.commit(TcrGate::Exempt)
	}

	pub fn ensure_not_frozen(&self) -> Result<(), DispatchError> {
		ensure!(!self.state.is_frozen(), Error::<T>::BranchFrozen);
		Ok(())
	}

	/// The branch's rate-index list id, derived from the context's own keys so
	/// it can never drift from `collateral_id`/`stable_id`.
	pub fn rate_list(&self) -> VaultListId<T::CollateralAssetId, T::StableAssetId> {
		VaultListId::Rate(self.collateral_id.clone(), self.stable_id.clone())
	}

	/// Oracle price for this context's collateral.
	pub fn price(&self) -> Result<FixedU128, DispatchError> {
		T::Oracle::provide_price(&self.collateral_id)
	}

	/// Branch config for this context's collateral.
	pub fn config(&self) -> Result<BranchConfig<BalanceOf<T>>, DispatchError> {
		Pallet::<T>::branch_config_of(&self.collateral_id, &self.stable_id)
	}

	/// Defer the upfront fee (charged to the committed vault's owner) until
	/// commit.
	pub fn charge_upfront_fee(&mut self, amount: BalanceOf<T>) {
		if amount.is_zero() {
			return;
		}
		debug_assert!(self.pending_fee.is_none(), "one upfront fee per dispatch");
		self.pending_fee = Some(amount);
	}

	/// Apply pending interest/redistribution to `owner`'s vault row in memory,
	/// consuming the branch context into a [`VaultOp`] that owns the row.
	pub fn touch(mut self, owner: &T::AccountId) -> Result<VaultOp<T>, DispatchError> {
		debug_assert!(self.pending_fee.is_none(), "fee charged before touch");
		let mut vault = Pallet::<T>::vault_of(&self.collateral_id, &self.stable_id, owner)?;
		let status = Pallet::<T>::vault_status_of(&self.collateral_id, &self.stable_id, owner);
		let pending = Pallet::<T>::pending_touch_for(&vault, &self.state, self.now);

		let mut pending_interest_accrued = None;
		if !pending.interest.is_zero() {
			vault.debt.interest = vault.debt.interest.saturating_add(pending.interest);
			pending_interest_accrued = Some(pending.interest);
		}
		if !pending.principal.is_zero() {
			self.state.absorb_redistributed_debt(&mut vault, pending.principal);
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

		if vault.redistribution_snapshot != self.state.redistribution {
			vault.redistribution_snapshot = self.state.redistribution;
		}
		vault.last_interest_time = self.state.interest_time(self.now);

		// FinalRecovery vaults are not stake-bearing; their stake stays zero while
		// `collateral` persists on the row.
		if !status.is_final_recovery() && vault.redistribution_stake != vault.collateral {
			let new_stake = vault.collateral;
			self.state.set_vault_stake(&mut vault, new_stake);
		}

		// A touch preserves the TCR inputs — principal + pending redistribution
		// move as a sum, collateral only changes hands, and the aggregate accrual
		// already ran at load — so the load baseline is the post-touch baseline.
		Ok(VaultOp { ctx: self, owner: owner.clone(), vault, status, pending_interest_accrued })
	}

	/// Attach a freshly-built vault row for `owner` — the only path on which a
	/// row enters storage without a touch. Unlike [`Self::touch`], the upfront
	/// fee may already be charged (an open computes its fee before the row
	/// exists).
	pub fn attach_new(self, owner: &T::AccountId, vault: Vault<BalanceOf<T>>) -> VaultOp<T> {
		VaultOp {
			ctx: self,
			owner: owner.clone(),
			vault,
			status: VaultStatus::Active,
			pending_interest_accrued: None,
		}
	}

	fn assert_unclobbered(&self) {
		#[cfg(debug_assertions)]
		debug_assert_eq!(
			BranchStates::<T>::get(&self.collateral_id, &self.stable_id).as_ref(),
			Some(&self.loaded),
			"BranchStates mutated behind OpContext"
		);
	}
}

impl<T: Config> VaultOp<T> {
	/// Write the owned vault row and the branch state, gated by `gate`.
	pub fn commit(self, gate: TcrGate<'_, BalanceOf<T>>) -> Result<(), DispatchError> {
		self.commit_inner(gate, true)
	}

	/// Remove the owned vault row and write the branch state, gated by `gate`.
	pub fn commit_removing_vault(
		self,
		gate: TcrGate<'_, BalanceOf<T>>,
	) -> Result<(), DispatchError> {
		self.commit_inner(gate, false)
	}

	fn commit_inner(
		self,
		gate: TcrGate<'_, BalanceOf<T>>,
		keep_row: bool,
	) -> Result<(), DispatchError> {
		enforce_tcr_gate::<T>(&self.ctx.tcr_baseline, &self.ctx.state, self.ctx.now, gate)?;
		self.ctx.assert_unclobbered();
		let key = (&self.ctx.collateral_id, &self.ctx.stable_id, &self.owner);
		if keep_row {
			Vaults::<T>::insert(key, &self.vault);
		} else {
			Vaults::<T>::remove(key);
		}
		BranchStates::<T>::insert(&self.ctx.collateral_id, &self.ctx.stable_id, &self.ctx.state);

		let Self { ctx, owner, pending_interest_accrued, .. } = self;
		let interest_accrued = pending_interest_accrued.map(|amount| (owner.clone(), amount));
		let fee = ctx.pending_fee.map(|amount| (owner, amount));
		flush_deferred::<T>(
			ctx.collateral_id,
			ctx.stable_id,
			ctx.pending_interest_mint,
			interest_accrued,
			fee,
		);
		Ok(())
	}
}

/// Apply `gate` to one operation's `baseline` → `state` change.
fn enforce_tcr_gate<T: Config>(
	baseline: &TcrInputs<BalanceOf<T>>,
	state: &BranchState<T::AccountId, BalanceOf<T>>,
	now: Millis,
	gate: TcrGate<'_, BalanceOf<T>>,
) -> Result<(), DispatchError> {
	let TcrGate::Check { price, config, settlement } = gate else { return Ok(()) };
	let pre_tcr = Pallet::<T>::tcr_from_inputs(baseline, price)?;
	let post_tcr = Pallet::<T>::compute_tcr(state, price, now)?;
	Pallet::<T>::enforce_mode_rules(config, state, pre_tcr, post_tcr, settlement)
}

/// Deferred effects, run after the storage commit: aggregate interest minting,
/// the touched vault's `InterestAccrued`, and the op's upfront fee.
fn flush_deferred<T: Config>(
	collateral_id: T::CollateralAssetId,
	stable_id: T::StableAssetId,
	interest_mint: BalanceOf<T>,
	interest_accrued: Option<(T::AccountId, BalanceOf<T>)>,
	fee: Option<(T::AccountId, BalanceOf<T>)>,
) {
	if !interest_mint.is_zero() {
		Pallet::<T>::mint_and_route_yield(&stable_id, interest_mint);
	}
	if let Some((owner, amount)) = interest_accrued {
		Pallet::<T>::deposit_event(Event::InterestAccrued {
			collateral_id: collateral_id.clone(),
			stable_id: stable_id.clone(),
			owner,
			amount,
		});
	}
	if let Some((owner, amount)) = fee {
		Pallet::<T>::mint_and_route_yield(&stable_id, amount);
		Pallet::<T>::deposit_event(Event::UpfrontFeeCharged {
			collateral_id,
			stable_id,
			owner,
			amount,
		});
	}
}
