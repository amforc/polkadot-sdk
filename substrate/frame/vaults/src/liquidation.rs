//! Plans and settles each liquidation as one atomic transaction.
//!
//! Seizure rounds up so fractional units cannot decrease the configured borrower loss.
//! Each initial path share rounds down to prevent collateral over-allocation.
//! The remainder rule then preserves exact collateral conservation.
//!
//! Planning drops a JIT trade under the keeper's slippage floor and re-plans once, so the
//! liquidation proceeds without the optional contribution. The keeper's collateral legs — the
//! compensation and the JIT share — are dropped the same way when the keeper's account cannot
//! receive them.
//!
//! Final recovery uses the same reward calculation, but does not always pay it.

use crate::{
	context::VaultOp,
	pallet::{
		BalanceOf, CollateralCreditOf, CollateralIdOf, Config, Error, Event, HoldReason, Pallet,
		StableIdOf,
	},
	types::{DebtCollateral, JitTerms, LiquidationConfig, LiquidationOutcome},
};
use core::cmp::Ordering;
use frame::{
	arithmetic::{
		AtLeast32BitUnsigned, CheckedAdd, FixedPointOperand, FixedU128, Permill, Saturating, Zero,
	},
	prelude::*,
	traits::{
		fungibles::{
			Balanced as FungiblesBalanced, BalancedHold as FungiblesBalancedHold,
			Inspect as FungiblesInspect, MutateHold as FungiblesMutateHold,
		},
		tokens::{DepositConsequence, Fortitude, Precision, Preservation, Provenance},
	},
};
use pusd_primitives::{
	math::collateral_for_value_ceil, mul_div_floor, reducible_debit, BranchSnapshot, OffsetLegs,
	StabilityPoolInspect, StabilityPoolOffset,
};

// The pool as one liquidation's quotes read it.
type PoolQuoteOf<T> = <<T as Config>::StabilityPool as StabilityPoolInspect<
	CollateralIdOf<T>,
	StableIdOf<T>,
	BalanceOf<T>,
>>::Quote;

#[derive(Clone)]
pub struct LiquidationSnapshot<Balance> {
	pub debt: Balance,
	pub price: FixedU128,
	pub config: LiquidationConfig<Balance>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LiquidationSplit<Balance> {
	active_pool: Balance,
	keeper_jit: Balance,
	pending_pool: Balance,
	redistribution: Balance,
}

impl<Balance: Zero> LiquidationSplit<Balance> {
	fn zero() -> Self {
		Self {
			active_pool: Balance::zero(),
			keeper_jit: Balance::zero(),
			pending_pool: Balance::zero(),
			redistribution: Balance::zero(),
		}
	}
}

impl<Balance: CheckedAdd> LiquidationSplit<Balance> {
	fn checked_total(&self) -> Option<Balance> {
		self.active_pool
			.checked_add(&self.keeper_jit)?
			.checked_add(&self.pending_pool)?
			.checked_add(&self.redistribution)
	}
}

struct LiquidationPlan<Balance> {
	debt: LiquidationSplit<Balance>,
	collateral: LiquidationSplit<Balance>,
	seized: Balance,
	keeper_reward: Balance,
	owner_surplus: Balance,
}

impl<Balance: FixedPointOperand + AtLeast32BitUnsigned> LiquidationPlan<Balance> {
	// Reallocates the plan with no keeper compensation. Debt, seizure, and owner surplus stay as
	// sized, so the borrower's loss is unchanged and the freed reward flows to the resolution
	// paths.
	fn without_keeper_reward(self, config: &LiquidationConfig<Balance>) -> Option<Self> {
		let collateral = allocate_collateral(self.seized, self.debt, config)?;
		Some(Self { collateral, keeper_reward: Balance::zero(), ..self })
	}
}

// A quoted plan with the account treatment its JIT stablecoin burn must use.
struct LiquidationQuote<Balance> {
	plan: LiquidationPlan<Balance>,
	jit_preservation: Preservation,
}

// The inputs every candidate quote of one liquidation shares; only the JIT terms vary. The pool is
// read once for all of them, and `None` means it has no capacity.
struct LiquidationQuoter<'a, T: Config> {
	keeper: &'a T::AccountId,
	collateral_id: &'a CollateralIdOf<T>,
	stable_id: &'a StableIdOf<T>,
	pool: Option<PoolQuoteOf<T>>,
	snapshot: &'a LiquidationSnapshot<BalanceOf<T>>,
	collateral_total: BalanceOf<T>,
}

impl<T: Config> Pallet<T> {
	/// Executes liquidation and commits all custody changes.
	pub(crate) fn do_liquidate(
		keeper: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: T::AccountId,
		jit: JitTerms<BalanceOf<T>>,
	) -> DispatchResult {
		let op =
			VaultOp::<T>::load_for_liquidation(collateral_id.clone(), stable_id.clone(), &owner)?;
		let snapshot = op.prepare_liquidation()?;
		let branch = op.branch_snapshot();
		// The touch left the vault's redistribution share in custody, so the owner holds that much
		// less than the vault records.
		let retained = op.retained_redistribution();
		let held = op.vault().collateral.checked_sub(&retained).ok_or(DispatchError::Corruption)?;

		let (collateral, shortfall) = T::CollateralAssets::slash(
			collateral_id.clone(),
			&HoldReason::VaultCollateral.into(),
			&owner,
			held,
		);
		if !shortfall.is_zero() {
			defensive!("vault collateral hold fell short of the recorded amount");
			return Err(DispatchError::Corruption);
		}

		let outcome = Self::settle_liquidation(
			op,
			&keeper,
			&collateral_id,
			&stable_id,
			branch,
			jit,
			snapshot,
			collateral,
			retained,
		)?;

		Self::deposit_event(Event::VaultLiquidated {
			collateral_id,
			stable_id,
			owner,
			keeper,
			outcome,
		});
		Ok(())
	}

	/// Leaves the redistribution account holding exactly `leg` on the liquidated vault's behalf.
	///
	/// `retained` is what it already holds for the vault. A larger leg takes the difference from
	/// `credit` and holds it; a smaller one slashes the excess back into `credit`. Either way one
	/// custody operation replaces the round trip through the owner's account. The credit that
	/// comes back funds the other legs.
	pub(crate) fn reconcile_redistribution_custody(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		mut credit: CollateralCreditOf<T>,
		retained: BalanceOf<T>,
		leg: BalanceOf<T>,
	) -> Result<CollateralCreditOf<T>, DispatchError> {
		let custody = Self::redistribution_account(collateral_id, stable_id);
		let reason = HoldReason::VaultCollateral.into();
		match leg.cmp(&retained) {
			Ordering::Equal => {},
			Ordering::Greater => {
				let top_up = leg.saturating_sub(retained);
				ensure!(top_up <= credit.peek(), Error::<T>::InvalidLiquidationPlan);
				Self::resolve_collateral(&custody, credit.extract(top_up))?;
				T::CollateralAssets::hold(collateral_id.clone(), &reason, &custody, top_up)?;
			},
			Ordering::Less => {
				let excess = retained.saturating_sub(leg);
				let (released, shortfall) =
					T::CollateralAssets::slash(collateral_id.clone(), &reason, &custody, excess);
				if !shortfall.is_zero() {
					defensive!("redistribution custody fell short of the retained share");
					return Err(DispatchError::Corruption);
				}
				if let Err(released) = credit.subsume(released) {
					// Both credits are the market's collateral, so the merge cannot fail.
					drop(released);
					return Err(DispatchError::Corruption);
				}
			},
		}
		Ok(credit)
	}

	/// Finalizes the vault and pays the owner's surplus as free balance.
	///
	/// The redistribution leg is already in custody: see
	/// [`Self::reconcile_redistribution_custody`].
	pub(crate) fn settle_liquidation_custody(
		op: VaultOp<T>,
		redistribution: DebtCollateral<BalanceOf<T>>,
		owner_collateral: CollateralCreditOf<T>,
	) -> DispatchResult {
		let owner = op.owner().clone();
		Self::resolve_collateral(&owner, owner_collateral)?;
		op.finish_liquidation(redistribution)
	}

	fn settle_liquidation(
		op: VaultOp<T>,
		keeper: &T::AccountId,
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		branch: BranchSnapshot,
		jit: JitTerms<BalanceOf<T>>,
		snapshot: LiquidationSnapshot<BalanceOf<T>>,
		collateral: CollateralCreditOf<T>,
		retained: BalanceOf<T>,
	) -> Result<LiquidationOutcome<BalanceOf<T>>, DispatchError> {
		debug_assert!(snapshot.config.offset_penalty <= snapshot.config.redistribution_penalty);
		let collateral_total =
			collateral.peek().checked_add(&retained).ok_or(Error::<T>::ArithmeticOverflow)?;
		debug_assert_eq!(collateral_total, op.vault().collateral);
		let quoter = LiquidationQuoter::<T> {
			keeper,
			collateral_id,
			stable_id,
			pool: T::StabilityPool::quote(collateral_id, stable_id, branch),
			snapshot: &snapshot,
			collateral_total,
		};
		let LiquidationQuote { plan, jit_preservation } = quoter.plan_liquidation(jit)?;

		// Custody settles first, so the credit left over is exactly what the other legs and the
		// owner share.
		let collateral = Self::reconcile_redistribution_custody(
			collateral_id,
			stable_id,
			collateral,
			retained,
			plan.collateral.redistribution,
		)?;
		let seized_out = plan
			.seized
			.checked_sub(&plan.collateral.redistribution)
			.ok_or(Error::<T>::InvalidLiquidationPlan)?;
		let (seized, owner_surplus) = collateral.split(seized_out);
		debug_assert_eq!(owner_surplus.peek(), plan.owner_surplus);
		let (keeper_collateral, mut resolution) = seized.split(plan.keeper_reward);
		Self::settle_keeper_legs(
			keeper,
			stable_id,
			&plan,
			jit,
			jit_preservation,
			keeper_collateral,
			&mut resolution,
		)?;
		Self::settle_pool_legs(collateral_id, stable_id, branch, &plan, &mut resolution)?;

		// Every leg but the owner's surplus has left the seized credit.
		if let Err(resolution) = resolution.drop_zero() {
			drop(resolution);
			return Err(DispatchError::Corruption);
		}
		let leg = |debt, collateral| DebtCollateral { debt, collateral };
		let touch = op.liquidation_touch()?;
		let outcome = LiquidationOutcome {
			active_pool: leg(plan.debt.active_pool, plan.collateral.active_pool),
			keeper_jit: leg(plan.debt.keeper_jit, plan.collateral.keeper_jit),
			pending_pool: leg(plan.debt.pending_pool, plan.collateral.pending_pool),
			redistribution: leg(plan.debt.redistribution, plan.collateral.redistribution),
			keeper_reward: plan.keeper_reward,
			owner_surplus: owner_surplus.peek(),
			touch,
		};
		Self::settle_liquidation_custody(op, outcome.redistribution, owner_surplus)?;
		Ok(outcome)
	}

	// Withdraws the keeper's JIT stablecoin and pays the reward and the JIT share as one deposit:
	// the deposit planning checked, so it cannot fail on the keeper's account state. The withdrawn
	// stablecoin burns here: it is the debt the JIT leg cancels.
	fn settle_keeper_legs(
		keeper: &T::AccountId,
		stable_id: &StableIdOf<T>,
		plan: &LiquidationPlan<BalanceOf<T>>,
		jit: JitTerms<BalanceOf<T>>,
		jit_preservation: Preservation,
		mut keeper_collateral: CollateralCreditOf<T>,
		resolution: &mut CollateralCreditOf<T>,
	) -> DispatchResult {
		debug_assert_eq!(keeper_collateral.peek(), plan.keeper_reward);
		if plan.debt.keeper_jit.is_zero() {
			debug_assert!(plan.collateral.keeper_jit.is_zero());
		} else {
			debug_assert!(plan.collateral.keeper_jit >= jit.min_collateral_out);
			let credit = T::StableAssets::withdraw(
				stable_id.clone(),
				keeper,
				plan.debt.keeper_jit,
				Precision::Exact,
				jit_preservation,
				Fortitude::Polite,
			)?;
			assert!(
				credit.peek() == plan.debt.keeper_jit,
				"an exact withdrawal moves the whole debt"
			);
			// Dropping the credit burns it: the JIT leg cancelled exactly this debt.
			drop(credit);
		}
		if let Err(jit_share) =
			keeper_collateral.subsume(resolution.extract(plan.collateral.keeper_jit))
		{
			// Both halves came from the seized credit, so the merge cannot fail; refuse the
			// settlement and let the dispatch roll back.
			drop(jit_share);
			return Err(DispatchError::Corruption);
		}
		Self::resolve_collateral(keeper, keeper_collateral)
	}

	// Hands both pool legs to the Stability Pool in one exact call, which keeps them atomic, and
	// burns the stablecoin the pool gave up. A plan without pool debt carries no pool collateral
	// and makes no call.
	fn settle_pool_legs(
		collateral_id: &CollateralIdOf<T>,
		stable_id: &StableIdOf<T>,
		branch: BranchSnapshot,
		plan: &LiquidationPlan<BalanceOf<T>>,
		resolution: &mut CollateralCreditOf<T>,
	) -> DispatchResult {
		if plan.debt.active_pool.is_zero() && plan.debt.pending_pool.is_zero() {
			debug_assert!(plan.collateral.active_pool.is_zero());
			debug_assert!(plan.collateral.pending_pool.is_zero());
			return Ok(());
		}
		let active_collateral = resolution.extract(plan.collateral.active_pool);
		let pending_collateral = resolution.extract(plan.collateral.pending_pool);
		if plan.debt.active_pool.is_zero() {
			debug_assert!(active_collateral.peek().is_zero());
		}
		if plan.debt.pending_pool.is_zero() {
			debug_assert!(pending_collateral.peek().is_zero());
		}
		let burned = T::StabilityPool::offset(
			collateral_id,
			stable_id,
			branch,
			OffsetLegs { active: plan.debt.active_pool, pending: plan.debt.pending_pool },
			OffsetLegs { active: active_collateral, pending: pending_collateral },
		)?;
		let pool_debt = plan
			.debt
			.active_pool
			.checked_add(&plan.debt.pending_pool)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		// The pool must give up exactly the debt it cancels, or the supply would drift.
		let given_up = burned.as_ref().map_or_else(Zero::zero, |credit| credit.peek());
		ensure!(given_up == pool_debt, DispatchError::Corruption);
		// Dropping the credit burns it: the offset cancelled exactly this debt.
		drop(burned);
		Ok(())
	}

	// Whether a planned JIT trade executes: its collateral share must clear the keeper's floor and
	// reach the keeper's account. The reward and the share settle as one deposit, so the check is
	// that deposit.
	fn jit_leg_executes(
		collateral_id: &CollateralIdOf<T>,
		keeper: &T::AccountId,
		plan: &LiquidationPlan<BalanceOf<T>>,
		jit: JitTerms<BalanceOf<T>>,
	) -> Result<bool, DispatchError> {
		if plan.debt.keeper_jit.is_zero() {
			return Ok(true);
		}
		if plan.collateral.keeper_jit < jit.min_collateral_out {
			return Ok(false);
		}
		let intake = plan
			.keeper_reward
			.checked_add(&plan.collateral.keeper_jit)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		Ok(Self::keeper_can_be_paid(collateral_id, keeper, intake))
	}

	// Whether the keeper's account could take a payout of already-existing collateral.
	pub(crate) fn keeper_can_be_paid(
		collateral_id: &CollateralIdOf<T>,
		keeper: &T::AccountId,
		amount: BalanceOf<T>,
	) -> bool {
		if amount.is_zero() {
			return true;
		}
		T::CollateralAssets::can_deposit(collateral_id.clone(), keeper, amount, Provenance::Extant) ==
			DepositConsequence::Success
	}

	// Limits keeper JIT to stablecoin that the keeper can burn. The limit uses residual debt, the
	// allowance, and reducible balance. An allowance or funding shortfall below the market minimum
	// skips JIT so an optional contribution cannot block liquidation. The minimum never binds on
	// the system's residual ask, which the keeper did not select: a small residual still executes
	// against a minimum-clearing allowance and funding.
	fn size_jit(
		keeper: &T::AccountId,
		stable_id: &StableIdOf<T>,
		config: &LiquidationConfig<BalanceOf<T>>,
		jit: JitTerms<BalanceOf<T>>,
		remaining: BalanceOf<T>,
	) -> Result<(BalanceOf<T>, Preservation), DispatchError> {
		if remaining.is_zero() || jit.max_stable.is_zero() {
			return Ok((Zero::zero(), Preservation::Preserve));
		}
		if jit.max_stable < config.minimum_jit_contribution {
			return Ok((Zero::zero(), Preservation::Preserve));
		}
		let target = remaining.min(jit.max_stable);
		let (funded, preservation) =
			reducible_debit::<T::StableAssets, _>(stable_id.clone(), keeper, target);
		if funded.is_zero() {
			return Ok((Zero::zero(), preservation));
		}
		if funded < target && funded < config.minimum_jit_contribution {
			// The keeper's funding shortfall, not the system ask, sized this dust contribution.
			return Ok((Zero::zero(), preservation));
		}
		Ok((funded, preservation))
	}

	fn resolve_collateral(
		recipient: &T::AccountId,
		credit: CollateralCreditOf<T>,
	) -> DispatchResult {
		let Err(credit) = credit.drop_zero() else { return Ok(()) };
		T::CollateralAssets::resolve(recipient, credit).map_err(|credit| {
			drop(credit);
			Error::<T>::CollateralPayoutFailed.into()
		})
	}
}

impl<T: Config> LiquidationQuoter<'_, T> {
	// Builds the waterfall plan and prunes the keeper legs that cannot execute, retrying once
	// without JIT. Pool custody is a branch-registration invariant, so no pool leg needs
	// liquidation-time pruning.
	fn plan_liquidation(
		&self,
		jit: JitTerms<BalanceOf<T>>,
	) -> Result<LiquidationQuote<BalanceOf<T>>, DispatchError> {
		let quote = self.quote_payable(jit)?;
		if Pallet::<T>::jit_leg_executes(self.collateral_id, self.keeper, &quote.plan, jit)? {
			return Ok(quote);
		}
		let retried = self.quote_payable(JitTerms { max_stable: Zero::zero(), ..jit })?;
		debug_assert!(retried.plan.debt.keeper_jit.is_zero());
		Ok(retried)
	}

	// Quotes debt, converts that exact waterfall split into a collateral-conserving plan, and plans
	// compensation out when the keeper cannot receive it: the reward is the one leg paid to an
	// account the protocol does not control, and an unpaid keeper is preferable to an unsafe vault
	// left in the market. Everything here is read-only, so the caller can discard a candidate
	// without unwinding state.
	fn quote_payable(
		&self,
		jit: JitTerms<BalanceOf<T>>,
	) -> Result<LiquidationQuote<BalanceOf<T>>, DispatchError> {
		let config = &self.snapshot.config;
		let (debt, jit_preservation) = self.size_debt(jit)?;
		let plan = plan(self.collateral_total, debt, self.snapshot.price, config)
			.ok_or(Error::<T>::ArithmeticOverflow)?;
		if Pallet::<T>::keeper_can_be_paid(self.collateral_id, self.keeper, plan.keeper_reward) {
			return Ok(LiquidationQuote { plan, jit_preservation });
		}
		let plan = plan.without_keeper_reward(config).ok_or(Error::<T>::ArithmeticOverflow)?;
		debug_assert!(plan.keeper_reward.is_zero());
		Ok(LiquidationQuote { plan, jit_preservation })
	}

	// Sizes debt in liquidation priority order: active pool, keeper JIT, pending pool, and
	// redistribution. A path receives only debt that higher-priority capital did not cover.
	// The reads are quotes, not reservations: nothing below touches the pool before
	// `offset` re-validates them exactly, so a stale quote fails the liquidation instead
	// of over-drawing.
	fn size_debt(
		&self,
		jit: JitTerms<BalanceOf<T>>,
	) -> Result<(LiquidationSplit<BalanceOf<T>>, Preservation), DispatchError> {
		let Self { keeper, stable_id, snapshot, .. } = *self;
		let pool = self.pool.as_ref();
		let active_pool = pool
			.map_or_else(Zero::zero, |pool| T::StabilityPool::quote_active(pool, snapshot.debt));
		ensure!(active_pool <= snapshot.debt, Error::<T>::InvalidLiquidationPlan);
		let mut remaining = snapshot.debt.saturating_sub(active_pool);
		let (keeper_jit, preservation) =
			Pallet::<T>::size_jit(keeper, stable_id, &snapshot.config, jit, remaining)?;
		remaining.saturating_reduce(keeper_jit);
		let pending_pool = match pool {
			Some(pool) if !remaining.is_zero() => {
				T::StabilityPool::quote_pending(pool, remaining, active_pool)
			},
			_ => Zero::zero(),
		};
		ensure!(pending_pool <= remaining, Error::<T>::InvalidLiquidationPlan);
		remaining.saturating_reduce(pending_pool);
		Ok((
			LiquidationSplit { active_pool, keeper_jit, pending_pool, redistribution: remaining },
			preservation,
		))
	}
}

// Returns one path's penalty-adjusted value and rounds it up. Thus, a fractional unit cannot
// decrease the configured penalty for that path.
fn penalized_debt<Balance: FixedPointOperand + AtLeast32BitUnsigned>(
	debt: Balance,
	penalty: Permill,
) -> Option<Balance> {
	debt.checked_add(&penalty.mul_ceil(debt))
}

// Limits borrower loss to the penalty-adjusted value of resolved debt. All offset debt uses
// `offset_penalty`, while redistributed debt uses `redistribution_penalty`. The caller also limits
// seizure to the collateral held.
fn max_seizable_collateral<Balance: FixedPointOperand + AtLeast32BitUnsigned>(
	debt: LiquidationSplit<Balance>,
	price: FixedU128,
	config: &LiquidationConfig<Balance>,
) -> Option<Balance> {
	let total_debt = debt.checked_total()?;
	let offset_debt = total_debt.checked_sub(&debt.redistribution)?;
	let value = penalized_debt(offset_debt, config.offset_penalty)?
		.checked_add(&penalized_debt(debt.redistribution, config.redistribution_penalty)?)?;
	collateral_for_value_ceil(value, price)
}

// Computes keeper compensation inside the seized collateral. The minimum rule keeps the reward
// within the configured cap, the penalty funding it, and the available collateral.
fn keeper_reward<Balance: FixedPointOperand + AtLeast32BitUnsigned>(
	seized: Balance,
	penalty_budget: Balance,
	price: FixedU128,
	config: &LiquidationConfig<Balance>,
) -> Option<Balance> {
	let flat = collateral_for_value_ceil(config.keeper_flat_compensation_value, price)?;
	let cap = collateral_for_value_ceil(config.keeper_compensation_cap_value, price)?;
	let percent = config.keeper_percent_compensation.mul_floor(seized);
	Some(seized.min(cap).min(flat.checked_add(&percent)?).min(penalty_budget))
}

// Allocates resolution collateral by penalty-adjusted debt. Floor division prevents
// over-allocation. If redistributed debt exists, it receives the remainder. Otherwise, the last
// nonzero offset path receives it. This preserves exact collateral conservation.
fn allocate_collateral<Balance: FixedPointOperand + AtLeast32BitUnsigned>(
	resolution: Balance,
	debt: LiquidationSplit<Balance>,
	config: &LiquidationConfig<Balance>,
) -> Option<LiquidationSplit<Balance>> {
	let penalized = LiquidationSplit {
		active_pool: penalized_debt(debt.active_pool, config.offset_penalty)?,
		keeper_jit: penalized_debt(debt.keeper_jit, config.offset_penalty)?,
		pending_pool: penalized_debt(debt.pending_pool, config.offset_penalty)?,
		redistribution: penalized_debt(debt.redistribution, config.redistribution_penalty)?,
	};
	let total = penalized.checked_total()?;
	if total.is_zero() {
		return Some(LiquidationSplit::zero());
	}
	let share = |debt| mul_div_floor(resolution, debt, total);
	let mut collateral = LiquidationSplit {
		active_pool: share(penalized.active_pool)?,
		keeper_jit: share(penalized.keeper_jit)?,
		pending_pool: share(penalized.pending_pool)?,
		redistribution: share(penalized.redistribution)?,
	};
	let allocated = collateral.checked_total()?;
	let remainder = resolution.checked_sub(&allocated)?;
	let last = if !debt.redistribution.is_zero() {
		&mut collateral.redistribution
	} else if !debt.pending_pool.is_zero() {
		&mut collateral.pending_pool
	} else if !debt.keeper_jit.is_zero() {
		&mut collateral.keeper_jit
	} else {
		&mut collateral.active_pool
	};
	*last = last.checked_add(&remainder)?;
	Some(collateral)
}

// Builds a liquidation plan that conserves collateral. Keeper compensation comes from seized
// collateral before path allocation, so configured penalties measure total borrower loss. Unseized
// collateral remains owner surplus.
fn plan<Balance: FixedPointOperand + AtLeast32BitUnsigned>(
	total_collateral: Balance,
	debt: LiquidationSplit<Balance>,
	price: FixedU128,
	config: &LiquidationConfig<Balance>,
) -> Option<LiquidationPlan<Balance>> {
	let max_seizable = max_seizable_collateral(debt, price, config)?;
	let seized = total_collateral.min(max_seizable);
	let owner_surplus = total_collateral.checked_sub(&seized)?;
	// The penalty a full seizure holds above the debt it resolves, which is all the keeper is
	// ever paid from. A vault can owe less than the `minimum_debt` its market sized the
	// compensation against, since a market may raise that floor after the vault opened, so the
	// vault in hand caps the reward rather than the configuration.
	let penalty_budget =
		max_seizable.checked_sub(&collateral_for_value_ceil(debt.checked_total()?, price)?)?;
	let keeper_reward = keeper_reward(seized, penalty_budget, price, config)?;
	let resolution = seized.checked_sub(&keeper_reward)?;
	let collateral = allocate_collateral(resolution, debt, config)?;
	Some(LiquidationPlan { debt, collateral, seized, keeper_reward, owner_surplus })
}

/// Quotes the recovery reward as if a Stability Pool covered all debt.
///
/// The debt split is not known at entry. The pool's `offset_penalty` also caps keeper fees.
/// The caller checks whether to pay the reward.
pub(crate) fn final_recovery_keeper_reward<Balance: FixedPointOperand + AtLeast32BitUnsigned>(
	collateral: Balance,
	debt: Balance,
	price: FixedU128,
	config: &LiquidationConfig<Balance>,
) -> Option<Balance> {
	let debt = LiquidationSplit { active_pool: debt, ..LiquidationSplit::zero() };
	Some(plan(collateral, debt, price, config)?.keeper_reward)
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame::deps::sp_runtime::traits::One;

	fn config() -> LiquidationConfig<u128> {
		LiquidationConfig {
			offset_penalty: Permill::from_percent(5),
			keeper_flat_compensation_value: 0,
			keeper_percent_compensation: Permill::zero(),
			keeper_compensation_cap_value: 0,
			minimum_jit_contribution: 100,
			redistribution_penalty: Permill::from_percent(10),
		}
	}

	// This mixed case protects penalty allocation and conservation when floor division creates a
	// remainder.
	#[test]
	fn mixed_split_is_pro_rata_to_penalized_debt_and_exact() {
		let debt = LiquidationSplit {
			active_pool: 500,
			keeper_jit: 200,
			pending_pool: 100,
			redistribution: 200,
		};
		let plan = plan(600, debt, FixedU128::from_rational(2, 1), &config()).unwrap();
		assert_eq!(plan.seized, 530);
		assert_eq!(plan.owner_surplus, 70);
		assert_eq!(
			plan.collateral,
			LiquidationSplit {
				active_pool: 262,
				keeper_jit: 105,
				pending_pool: 52,
				redistribution: 111,
			}
		);
		assert_eq!(plan.collateral.checked_total(), Some(530));
	}

	// Keeper compensation must reduce path collateral because it is part of the borrower's total
	// loss.
	#[test]
	fn keeper_reward_is_deducted_before_allocation() {
		let mut policy = config();
		policy.keeper_flat_compensation_value = 10;
		policy.keeper_percent_compensation = Permill::from_percent(1);
		policy.keeper_compensation_cap_value = 10_000;
		let debt = LiquidationSplit {
			active_pool: 500,
			keeper_jit: 0,
			pending_pool: 0,
			redistribution: 0,
		};
		// The reward 15 = 10 flat + floor(1% of 525) comes out of the seized
		// lot before allocation — the penalty is gross of keeper compensation,
		// so the pool receives 510, not 525.
		let plan = plan(600, debt, FixedU128::one(), &policy).unwrap();
		assert_eq!(plan.seized, 525);
		assert_eq!(plan.keeper_reward, 15);
		assert_eq!(plan.owner_surplus, 75);
		assert_eq!(plan.collateral.active_pool, 510);
	}

	// A vault that opened under looser terms than the market now configures must still leave the
	// pool its principal cover. The penalty the vault itself carries is all the keeper can take.
	#[test]
	fn keeper_reward_never_outgrows_the_penalty_seized() {
		let mut policy = config();
		policy.keeper_flat_compensation_value = 100;
		policy.keeper_percent_compensation = Permill::from_percent(10);
		policy.keeper_compensation_cap_value = 10_000;
		let debt = LiquidationSplit {
			active_pool: 500,
			keeper_jit: 0,
			pending_pool: 0,
			redistribution: 0,
		};
		// The configured 152 = 100 flat + floor(10% of 525) is larger than the 25 penalty this
		// vault seizes, so the keeper takes the 25 and the pool still receives its whole 500.
		let plan = plan(600, debt, FixedU128::one(), &policy).unwrap();
		assert_eq!(plan.seized, 525);
		assert_eq!(plan.keeper_reward, 25);
		assert_eq!(plan.collateral.active_pool, 500);
	}

	// Each reward bound must independently cap keeper compensation so it cannot exceed seized
	// collateral, the penalty funding it, or policy limits.
	#[test]
	fn keeper_reward_takes_the_binding_minimum() {
		let reward = |flat: u128, percent: Permill, cap: u128, budget: u128, seized: u128| {
			let mut policy = config();
			policy.keeper_flat_compensation_value = flat;
			policy.keeper_percent_compensation = percent;
			policy.keeper_compensation_cap_value = cap;
			keeper_reward(seized, budget, FixedU128::one(), &policy)
		};
		let percent_per_mille = Permill::from_rational(1u32, 1_000u32);
		// Flat plus percent binds: 100 + floor(0.1% of 584) = 100.
		assert_eq!(reward(100, percent_per_mille, 10_000, 10_000, 584), Some(100));
		// The cap binds: flat 5_000 clamped to 300.
		assert_eq!(reward(5_000, Permill::zero(), 300, 10_000, 584), Some(300));
		// The seized lot binds: everything else is larger.
		assert_eq!(reward(5_000, Permill::zero(), 10_000, 10_000, 584), Some(584));
		// The penalty this seizure carries binds: everything else is larger.
		assert_eq!(reward(5_000, Permill::zero(), 10_000, 29, 584), Some(29));
		// Percent contributes above the flat: 100 + floor(10% of 500) = 150.
		assert_eq!(reward(100, Permill::from_percent(10), 10_000, 10_000, 500), Some(150));
	}

	// The final-recovery entry reward on every bound it can take.
	#[test]
	fn final_recovery_reward_takes_the_binding_minimum() {
		let mut terms = config();
		terms.keeper_flat_compensation_value = 10;
		terms.keeper_percent_compensation = Permill::from_percent(1);
		terms.keeper_compensation_cap_value = 10_000;
		let one = FixedU128::one();
		let cases = [
			// Flat plus percent binds: 10 + floor(1% of 525) = 15.
			(600, 500, one, terms, 15),
			// Below par the seized lot is the whole collateral: 10 + floor(1% of 400) = 14.
			(400, 500, one, terms, 14),
			// The cap binds.
			(600, 500, one, LiquidationConfig { keeper_compensation_cap_value: 12, ..terms }, 12),
			// The penalty this vault carries (25) binds on terms sized for a larger debt.
			(
				600,
				500,
				one,
				LiquidationConfig {
					keeper_flat_compensation_value: 100,
					keeper_percent_compensation: Permill::from_percent(10),
					..terms
				},
				25,
			),
			// Values convert at the price: flat ceil(10 / 0.1) = 100, plus floor(1% of 1_000).
			(1_000, 501, FixedU128::from_rational(1, 10), terms, 110),
		];
		for (collateral, debt, price, policy, expected) in cases {
			assert_eq!(
				final_recovery_keeper_reward(collateral, debt, price, &policy),
				Some(expected)
			);
		}
	}

	// Separate penalties must make redistribution harsher than an offset. Zero penalties must
	// preserve debt-value parity.
	#[test]
	fn max_seizable_prices_the_two_debt_kinds_apart() {
		let seizable = |offset: u128, redistribution: u128, policy: &LiquidationConfig<u128>| {
			let debt = LiquidationSplit {
				active_pool: offset,
				keeper_jit: 0,
				pending_pool: 0,
				redistribution,
			};
			max_seizable_collateral(debt, FixedU128::one(), policy)
		};
		// All-offset debt carries the milder 5% penalty, all-redistribution
		// the harsher 10%: 1_050 against 1_100 of value at par.
		assert_eq!(seizable(1_000, 0, &config()), Some(1_050));
		assert_eq!(seizable(0, 1_000, &config()), Some(1_100));
		// Zero penalties seize exactly the debt value.
		let mut zero = config();
		zero.offset_penalty = Permill::zero();
		zero.redistribution_penalty = Permill::zero();
		assert_eq!(seizable(600, 400, &zero), Some(1_000));
	}

	// A zero-debt vault cannot reach liquidation, but this defensive case prevents an accidental
	// collateral seizure.
	#[test]
	fn zero_debt_plan_seizes_nothing() {
		let debt =
			LiquidationSplit { active_pool: 0, keeper_jit: 0, pending_pool: 0, redistribution: 0 };
		let plan = plan(600, debt, FixedU128::one(), &config()).unwrap();
		assert_eq!(plan.seized, 0);
		assert_eq!(plan.keeper_reward, 0);
		assert_eq!(plan.owner_surplus, 600);
		assert_eq!(plan.collateral.checked_total(), Some(0));
	}
}
