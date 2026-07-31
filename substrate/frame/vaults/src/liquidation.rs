//! Atomic liquidation planning and settlement.
//!
//! Whenever a rounding direction matters, it favors the system: seizure
//! rounds up against the owner, and offset collateral slices round down
//! against their recipients.

use crate::{
	context::VaultOp,
	pallet::{
		BalanceOf, CollateralCreditOf, CollateralIdOf, Config, Error, Event, HoldReason, Pallet,
		StableIdOf,
	},
	types::{DebtCollateral, JitTerms, LiquidationConfig, LiquidationOutcome},
};
use frame::{
	arithmetic::{
		AtLeast32BitUnsigned, CheckedAdd, FixedPointOperand, FixedU128, Permill, Saturating, Zero,
	},
	deps::frame_support::transactional,
	prelude::*,
	traits::{
		fungibles::{
			Balanced as FungiblesBalanced, BalancedHold as FungiblesBalancedHold,
			Mutate as FungiblesMutate, MutateHold as FungiblesMutateHold,
		},
		tokens::{Fortitude, Precision, Preservation},
	},
};
use pusd_primitives::{
	mul_div_floor, recovery_pricing::collateral_for_value_ceil, reducible_debit,
	StabilityOffsetSession, StabilityPoolOffsetApi,
};

type StabilitySessionOf<T> = <<T as Config>::StabilityPool as StabilityPoolOffsetApi<
	CollateralIdOf<T>,
	StableIdOf<T>,
	BalanceOf<T>,
	CollateralCreditOf<T>,
>>::Session;

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

struct LiquidationSettlement<Credit, Balance> {
	redistribution_collateral: Credit,
	owner_surplus: Credit,
	outcome: LiquidationOutcome<Balance>,
}

impl<T: Config> Pallet<T> {
	#[transactional]
	pub(crate) fn do_liquidate(
		keeper: T::AccountId,
		collateral_id: CollateralIdOf<T>,
		stable_id: StableIdOf<T>,
		owner: T::AccountId,
		jit: JitTerms<BalanceOf<T>>,
	) -> DispatchResult {
		let op = VaultOp::<T>::load_priced(collateral_id.clone(), stable_id.clone(), &owner)?;
		let snapshot = op.liquidation_snapshot()?;
		let held = op.vault().collateral;

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

		let settlement =
			T::StabilityPool::with_offset_session(&collateral_id, &stable_id, |pool| {
				Self::settle_with_pool(&keeper, &stable_id, jit, snapshot, collateral, pool)
			})?;
		let LiquidationSettlement { redistribution_collateral, owner_surplus, outcome } =
			settlement;
		Self::settle_liquidation_custody(
			op,
			outcome.redistribution,
			redistribution_collateral,
			owner_surplus,
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

	/// Removes the liquidated vault and settles collateral custody: the
	/// redistribution credit parks under the market's redistribution-account
	/// hold, the owner credit resolves free. Shared with the mock's
	/// redistribution harness so the custody sequence cannot drift.
	pub(crate) fn settle_liquidation_custody(
		mut op: VaultOp<T>,
		redistribution: DebtCollateral<BalanceOf<T>>,
		redistribution_collateral: CollateralCreditOf<T>,
		owner_collateral: CollateralCreditOf<T>,
	) -> DispatchResult {
		debug_assert_eq!(redistribution_collateral.peek(), redistribution.collateral);
		let owner = op.owner().clone();
		op.apply_liquidation(redistribution)?;

		if redistribution.collateral.is_zero() {
			drop(redistribution_collateral);
		} else {
			let redistribution_account =
				Self::redistribution_account(op.collateral_id(), op.stable_id());
			Self::resolve_collateral(&redistribution_account, redistribution_collateral)?;
			T::CollateralAssets::hold(
				op.collateral_id().clone(),
				&HoldReason::VaultCollateral.into(),
				&redistribution_account,
				redistribution.collateral,
			)?;
		}
		Self::resolve_collateral(&owner, owner_collateral)?;
		op.commit_exempt()
	}

	fn settle_with_pool(
		keeper: &T::AccountId,
		stable_id: &StableIdOf<T>,
		jit: JitTerms<BalanceOf<T>>,
		snapshot: LiquidationSnapshot<BalanceOf<T>>,
		collateral: CollateralCreditOf<T>,
		pool: &mut StabilitySessionOf<T>,
	) -> Result<LiquidationSettlement<CollateralCreditOf<T>, BalanceOf<T>>, DispatchError> {
		debug_assert!(snapshot.config.offset_penalty <= snapshot.config.redistribution_penalty);
		let (debt, jit_preservation) = Self::size_debt(keeper, stable_id, &snapshot, jit, pool)?;
		let plan = plan(collateral.peek(), debt, snapshot.price, &snapshot.config)
			.ok_or(Error::<T>::ArithmeticOverflow)?;

		let (seized, owner_surplus) = collateral.split(plan.seized);
		debug_assert_eq!(owner_surplus.peek(), plan.owner_surplus);
		let (keeper_reward, mut resolution) = seized.split(plan.keeper_reward);
		Self::resolve_collateral(keeper, keeper_reward)?;

		if !plan.debt.active_pool.is_zero() {
			let active_collateral = resolution.extract(plan.collateral.active_pool);
			pool.settle_active(active_collateral)?;
		}

		Self::apply_jit(
			keeper,
			stable_id,
			jit,
			plan.debt.keeper_jit,
			jit_preservation,
			plan.collateral.keeper_jit,
			&mut resolution,
		)?;

		if !plan.debt.pending_pool.is_zero() {
			let pending_collateral = resolution.extract(plan.collateral.pending_pool);
			pool.settle_pending(pending_collateral)?;
		}

		debug_assert_eq!(resolution.peek(), plan.collateral.redistribution);
		let redistribution_collateral = resolution;
		let outcome = LiquidationOutcome {
			active_pool: DebtCollateral {
				debt: plan.debt.active_pool,
				collateral: plan.collateral.active_pool,
			},
			keeper_jit: DebtCollateral {
				debt: plan.debt.keeper_jit,
				collateral: plan.collateral.keeper_jit,
			},
			pending_pool: DebtCollateral {
				debt: plan.debt.pending_pool,
				collateral: plan.collateral.pending_pool,
			},
			redistribution: DebtCollateral {
				debt: plan.debt.redistribution,
				collateral: plan.collateral.redistribution,
			},
			keeper_reward: plan.keeper_reward,
			owner_surplus: owner_surplus.peek(),
		};
		Ok(LiquidationSettlement { redistribution_collateral, owner_surplus, outcome })
	}

	/// Sizes the debt waterfall in resolution order: active pool capital,
	/// keeper JIT, pending deposits, then redistribution for what remains.
	fn size_debt(
		keeper: &T::AccountId,
		stable_id: &StableIdOf<T>,
		snapshot: &LiquidationSnapshot<BalanceOf<T>>,
		jit: JitTerms<BalanceOf<T>>,
		pool: &mut StabilitySessionOf<T>,
	) -> Result<(LiquidationSplit<BalanceOf<T>>, Preservation), DispatchError> {
		let active_pool = pool.reserve_active(snapshot.debt);
		ensure!(active_pool <= snapshot.debt, Error::<T>::InvalidLiquidationPlan);
		let mut remaining = snapshot.debt.saturating_sub(active_pool);
		let (keeper_jit, preservation) =
			Self::size_jit(keeper, stable_id, &snapshot.config, jit, remaining)?;
		remaining.saturating_reduce(keeper_jit);
		let pending_pool = pool.reserve_pending(remaining);
		ensure!(pending_pool <= remaining, Error::<T>::InvalidLiquidationPlan);
		remaining.saturating_reduce(pending_pool);
		Ok((
			LiquidationSplit { active_pool, keeper_jit, pending_pool, redistribution: remaining },
			preservation,
		))
	}

	/// Sizes the keeper's JIT contribution, bounded by the allowance, the
	/// remaining debt, and the keeper's reducible stablecoin balance. A
	/// system ask below `minimum_jit_contribution` skips JIT; a nonzero
	/// keeper-side contribution below it is rejected rather than clamped.
	fn size_jit(
		keeper: &T::AccountId,
		stable_id: &StableIdOf<T>,
		config: &LiquidationConfig<BalanceOf<T>>,
		jit: JitTerms<BalanceOf<T>>,
		remaining: BalanceOf<T>,
	) -> Result<(BalanceOf<T>, Preservation), DispatchError> {
		if remaining.is_zero() ||
			remaining < config.minimum_jit_contribution ||
			jit.max_stable.is_zero()
		{
			return Ok((Zero::zero(), Preservation::Preserve));
		}
		let target = remaining.min(jit.max_stable);
		let (funded, preservation) =
			reducible_debit::<T::StableAssets, _>(stable_id.clone(), keeper, target);
		if funded.is_zero() {
			return Ok((Zero::zero(), preservation));
		}
		ensure!(funded >= config.minimum_jit_contribution, Error::<T>::JitBelowMinimum);
		Ok((funded, preservation))
	}

	/// Burns the keeper's JIT stablecoin and pays out its collateral slice.
	/// The `min_collateral_out` floor applies only to an executed JIT trade.
	fn apply_jit(
		keeper: &T::AccountId,
		stable_id: &StableIdOf<T>,
		jit: JitTerms<BalanceOf<T>>,
		debt: BalanceOf<T>,
		preservation: Preservation,
		collateral_amount: BalanceOf<T>,
		resolution: &mut CollateralCreditOf<T>,
	) -> DispatchResult {
		if debt.is_zero() {
			return Ok(());
		}
		ensure!(collateral_amount >= jit.min_collateral_out, Error::<T>::JitSlippageExceeded);
		T::StableAssets::burn_from(
			stable_id.clone(),
			keeper,
			debt,
			preservation,
			Precision::Exact,
			Fortitude::Polite,
		)?;
		let jit_credit = resolution.extract(collateral_amount);
		Self::resolve_collateral(keeper, jit_credit)?;
		Ok(())
	}

	fn resolve_collateral(
		recipient: &T::AccountId,
		credit: CollateralCreditOf<T>,
	) -> DispatchResult {
		let credit = match credit.drop_zero() {
			Ok(()) => return Ok(()),
			Err(credit) => credit,
		};
		T::CollateralAssets::resolve(recipient, credit).map_err(|credit| {
			drop(credit);
			Error::<T>::CollateralPayoutFailed.into()
		})
	}
}

/// The stablecoin value a resolution path is entitled to: `debt * (1 +
/// penalty)`. Rounding up matches the seizure cap, which is the sum of these,
/// so the cap and the shares that divide it are the same numbers.
fn penalty_weight<Balance: FixedPointOperand + AtLeast32BitUnsigned>(
	debt: Balance,
	penalty: Permill,
) -> Option<Balance> {
	debt.checked_add(&penalty.mul_ceil(debt))
}

/// The most collateral a liquidation may seize, at the two prices the debt
/// resolves at: debt an offset cancels carries `offset_penalty`, debt pushed
/// onto other vaults the harsher `redistribution_penalty`.
///
/// `ceil((offset_debt * (1 + λ) + redistributed_debt * (1 + ρ)) / price)`.
/// The caller clamps to the collateral actually held.
fn max_seizable_collateral<Balance: FixedPointOperand + AtLeast32BitUnsigned>(
	debt: LiquidationSplit<Balance>,
	price: FixedU128,
	config: &LiquidationConfig<Balance>,
) -> Option<Balance> {
	let total_debt = debt.checked_total()?;
	let offset_debt = total_debt.checked_sub(&debt.redistribution)?;
	let value = penalty_weight(offset_debt, config.offset_penalty)?
		.checked_add(&penalty_weight(debt.redistribution, config.redistribution_penalty)?)?;
	collateral_for_value_ceil(value, price)
}

/// Keeper compensation paid from the seized collateral:
/// `min(seized, cap, flat + percent)`, with the stablecoin-denominated flat
/// and cap converted to collateral at the liquidation price.
fn keeper_reward<Balance: FixedPointOperand + AtLeast32BitUnsigned>(
	seized: Balance,
	price: FixedU128,
	config: &LiquidationConfig<Balance>,
) -> Option<Balance> {
	let flat = collateral_for_value_ceil(config.keeper_flat_compensation_value, price)?;
	let cap = collateral_for_value_ceil(config.keeper_compensation_cap_value, price)?;
	let percent = config.keeper_percent_compensation.mul_floor(seized);
	Some(seized.min(cap).min(flat.checked_add(&percent)?))
}

/// One floored pro-rata share of the resolution lot per path, weighted by
/// penalty-weighted debt. The flooring remainder goes to redistribution when
/// redistributed debt exists, otherwise to the last non-zero offset path, so
/// the lot is allocated exactly once.
fn allocate_collateral<Balance: FixedPointOperand + AtLeast32BitUnsigned>(
	resolution: Balance,
	debt: LiquidationSplit<Balance>,
	config: &LiquidationConfig<Balance>,
) -> Option<LiquidationSplit<Balance>> {
	let weights = LiquidationSplit {
		active_pool: penalty_weight(debt.active_pool, config.offset_penalty)?,
		keeper_jit: penalty_weight(debt.keeper_jit, config.offset_penalty)?,
		pending_pool: penalty_weight(debt.pending_pool, config.offset_penalty)?,
		redistribution: penalty_weight(debt.redistribution, config.redistribution_penalty)?,
	};
	let total = weights.checked_total()?;
	if total.is_zero() {
		return Some(LiquidationSplit {
			active_pool: Balance::zero(),
			keeper_jit: Balance::zero(),
			pending_pool: Balance::zero(),
			redistribution: Balance::zero(),
		});
	}
	let share = |weight| mul_div_floor(resolution, weight, total);
	let mut collateral = LiquidationSplit {
		active_pool: share(weights.active_pool)?,
		keeper_jit: share(weights.keeper_jit)?,
		pending_pool: share(weights.pending_pool)?,
		redistribution: share(weights.redistribution)?,
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

/// Sizes one liquidation: seizure clamped to the collateral held, keeper
/// compensation off the top (the penalties are gross of it), the remainder
/// allocated by penalty weight, and everything unseized left as owner
/// surplus.
fn plan<Balance: FixedPointOperand + AtLeast32BitUnsigned>(
	total_collateral: Balance,
	debt: LiquidationSplit<Balance>,
	price: FixedU128,
	config: &LiquidationConfig<Balance>,
) -> Option<LiquidationPlan<Balance>> {
	let max_seizable = max_seizable_collateral(debt, price, config)?;
	let seized = total_collateral.min(max_seizable);
	let owner_surplus = total_collateral.checked_sub(&seized)?;
	let keeper_reward = keeper_reward(seized, price, config)?;
	let resolution = seized.checked_sub(&keeper_reward)?;
	let collateral = allocate_collateral(resolution, debt, config)?;
	Some(LiquidationPlan { debt, collateral, seized, keeper_reward, owner_surplus })
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

	#[test]
	fn mixed_split_is_penalty_weighted_and_allocated_exactly() {
		let debt = LiquidationSplit {
			active_pool: 500,
			keeper_jit: 200,
			pending_pool: 100,
			redistribution: 200,
		};
		// The paths weigh 525/210/105/220, so 1_060 of value seizes 530 of the
		// 600 held at price 2. The floored shares 262/105/52/110 sum to 529;
		// the 1 remainder follows the redistributed debt.
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

	#[test]
	fn keeper_reward_is_deducted_before_allocation() {
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
		// The reward 152 = 100 flat + floor(10% of 525) comes out of the
		// seized lot before allocation — the penalty is gross of keeper
		// compensation, so the pool receives 373, not 525.
		let plan = plan(600, debt, FixedU128::one(), &policy).unwrap();
		assert_eq!(plan.seized, 525);
		assert_eq!(plan.keeper_reward, 152);
		assert_eq!(plan.owner_surplus, 75);
		assert_eq!(plan.collateral.active_pool, 373);
	}

	#[test]
	fn keeper_reward_takes_the_binding_minimum() {
		let reward = |flat: u128, percent: Permill, cap: u128, seized: u128| {
			let mut policy = config();
			policy.keeper_flat_compensation_value = flat;
			policy.keeper_percent_compensation = percent;
			policy.keeper_compensation_cap_value = cap;
			keeper_reward(seized, FixedU128::one(), &policy)
		};
		// Flat plus percent binds: 100 + floor(0.1% of 584) = 100.
		assert_eq!(reward(100, Permill::from_rational(1u32, 1_000u32), 10_000, 584), Some(100));
		// The cap binds: flat 5_000 clamped to 300.
		assert_eq!(reward(5_000, Permill::zero(), 300, 584), Some(300));
		// The seized lot binds: everything else is larger.
		assert_eq!(reward(5_000, Permill::zero(), 10_000, 584), Some(584));
		// Percent contributes above the flat: 100 + floor(10% of 500) = 150.
		assert_eq!(reward(100, Permill::from_percent(10), 10_000, 500), Some(150));
	}

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

	// Unreachable through the extrinsic — a zero-debt vault has no defined CR
	// and is never liquidatable — but pins the defensive arm: nothing seized,
	// the whole lot returned as surplus.
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
