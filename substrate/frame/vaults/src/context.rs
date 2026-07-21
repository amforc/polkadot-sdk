//! Per-dispatch operation contexts with deferred yield minting.
//!
//! Two phases: [`OpContext::load`] opens the branch-level context, and
//! [`OpContext::touch`] consumes it into a [`VaultOp`] that owns the touched
//! vault row, its owner key, and its derived status. A commit consumes the
//! phase value, writes the row it owns, and applies the [`TcrGate`] the
//! operation declares. The protocol is structural: an operation cannot touch
//! twice, commit a row other than the one it touched, or commit without
//! naming its TCR stance.

use crate::{
	math,
	pallet::{
		BalanceOf, BranchOf, CollateralIdOf, CollateralRisks, Config, Error, Event, HoldReason,
		Millis, Pallet, StableIdOf, Vaults,
	},
	types::{BranchConfig, Vault, VaultListId, VaultStatus},
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
pub(crate) enum TcrGate {
	/// Pre- and post-TCR are computed — surfacing arithmetic overflow — against
	/// the operation's own loaded branch config. Unless `settlement` (a
	/// branch-emptying close, where worsening is allowed), the Normal/Safety
	/// mode rules apply: the op may not drop the branch TCR below the safety
	/// threshold, nor worsen it while already below.
	Check { price: FixedU128, settlement: bool },
	/// No TCR computation: the op is structurally TCR-improving (repay,
	/// deposit, redemption) or gated elsewhere (MCR on the liquidation and
	/// recovery paths).
	Exempt,
}

/// Branch-level operation context: one branch-state read threaded through an
/// operation and committed once.
pub(crate) struct OpContext<T: Config> {
	pub collateral_id: CollateralIdOf<T>,
	pub stable_id: StableIdOf<T>,
	pub now: Millis,
	pub branch: BranchOf<T>,
	/// The stored branch's `BranchDebt::outstanding()` captured before the
	/// in-memory accrual; see [`Self::ensure_global_ceiling`].
	outstanding_at_load: BalanceOf<T>,
	pending_interest_mint: BalanceOf<T>,
	pending_fee: Option<BalanceOf<T>>,
	/// The post-accrual TCR inputs at load: the "pre" side of the commit's
	/// TCR gate. A touch preserves them (the debt sum and collateral are
	/// invariant under a touch), and the captured pair is structurally
	/// immutable, so it doubles as the [`VaultOp`] baseline.
	tcr_baseline: TcrInputs<BalanceOf<T>>,
	#[cfg(debug_assertions)]
	loaded: BranchOf<T>,
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
}

impl<T: Config> OpContext<T> {
	/// Read the branch state and accrue aggregate interest in memory.
	pub fn load(
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

	/// Refresh one vault. This is intentionally allowed while frozen.
	pub fn refresh(
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: &T::AccountId,
	) -> Result<(), DispatchError> {
		let op = Self::load(collateral_id, stable_id)?;
		let op = op.touch(owner)?;
		op.commit(TcrGate::Exempt)
	}

	pub fn ensure_not_frozen(&self) -> Result<(), DispatchError> {
		ensure!(!self.branch.state.is_frozen(), Error::<T>::BranchFrozen);
		Ok(())
	}

	/// The branch's rate-index list id, derived from the context's own keys so
	/// it can never drift from `collateral_id`/`stable_id`.
	pub fn rate_list(&self) -> VaultListId<CollateralIdOf<T>, StableIdOf<T>> {
		VaultListId::Rate(self.collateral_id.clone(), self.stable_id.clone())
	}

	/// Oracle price for this context's collateral.
	pub fn price(&self) -> Result<FixedU128, DispatchError> {
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
	pub fn ensure_global_ceiling(&self, price: FixedU128) -> Result<(), DispatchError> {
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

	/// This operation's branch config — a copy of the loaded record's, so no
	/// second storage read and no borrow of the op itself.
	pub fn config(&self) -> BranchConfig<BalanceOf<T>> {
		self.branch.config.clone()
	}

	/// Charge `owner` the upfront fee: the event is deposited now (reverted
	/// with the dispatch on error), the mint is deferred until commit so pUSD
	/// is only issued when the branch state is actually written.
	pub fn charge_upfront_fee(&mut self, owner: &T::AccountId, amount: BalanceOf<T>) {
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
	pub fn touch(mut self, owner: &T::AccountId) -> Result<VaultOp<T>, DispatchError> {
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

		// FinalRecovery vaults are not stake-bearing; their stake stays zero while
		// `collateral` persists on the row.
		if !status.is_final_recovery() && vault.redistribution_stake != vault.collateral {
			let new_stake = vault.collateral;
			self.branch.state.set_vault_stake(&mut vault, new_stake);
		}

		// A touch preserves the TCR inputs — principal + pending redistribution
		// move as a sum, collateral only changes hands, and the aggregate accrual
		// already ran at load — so the load baseline is the post-touch baseline.
		Ok(VaultOp { ctx: self, owner: owner.clone(), vault, status })
	}

	/// Attach a freshly-built vault row for `owner` — the only path on which a
	/// row enters storage without a touch. Unlike [`Self::touch`], the upfront
	/// fee may already be charged (an open computes its fee before the row
	/// exists).
	pub fn attach_new(self, owner: &T::AccountId, vault: Vault<BalanceOf<T>>) -> VaultOp<T> {
		VaultOp { ctx: self, owner: owner.clone(), vault, status: VaultStatus::Active }
	}

	fn assert_unclobbered(&self) {
		#[cfg(debug_assertions)]
		debug_assert_eq!(
			crate::pallet::Branches::<T>::get(&self.collateral_id, &self.stable_id).as_ref(),
			Some(&self.loaded),
			"Branches mutated behind OpContext"
		);
	}
}

impl<T: Config> VaultOp<T> {
	/// Write the owned vault row and the branch record, gated by `gate`.
	pub fn commit(self, gate: TcrGate) -> Result<(), DispatchError> {
		self.commit_inner(gate, true)
	}

	/// Remove the owned vault row and write the branch record, gated by `gate`.
	pub fn commit_removing_vault(self, gate: TcrGate) -> Result<(), DispatchError> {
		self.commit_inner(gate, false)
	}

	fn commit_inner(self, gate: TcrGate, keep_row: bool) -> Result<(), DispatchError> {
		enforce_tcr_gate::<T>(&self.ctx.tcr_baseline, &self.ctx.branch, self.ctx.now, gate)?;
		self.ctx.assert_unclobbered();
		let key = (&self.ctx.collateral_id, &self.ctx.stable_id, &self.owner);
		if keep_row {
			Vaults::<T>::insert(key, &self.vault);
		} else {
			Vaults::<T>::remove(key);
		}
		let branch = self.ctx.branch;
		Pallet::<T>::commit_branch(&self.ctx.collateral_id, &self.ctx.stable_id, branch)?;

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

/// Apply `gate` to one operation's `baseline` → committed-branch change. The
/// config the mode rules run against is the operation's own loaded record, so
/// the gate can never check a different config than the op used.
fn enforce_tcr_gate<T: Config>(
	baseline: &TcrInputs<BalanceOf<T>>,
	branch: &BranchOf<T>,
	now: Millis,
	gate: TcrGate,
) -> Result<(), DispatchError> {
	let TcrGate::Check { price, settlement } = gate else { return Ok(()) };
	let pre_tcr = Pallet::<T>::tcr_from_inputs(baseline, price)?;
	let post_tcr = Pallet::<T>::compute_tcr(&branch.state, price, now)?;
	Pallet::<T>::enforce_mode_rules(&branch.config, &branch.state, pre_tcr, post_tcr, settlement)
}
