#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod fees;
pub mod types;
pub mod weights;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub use pallet::*;
pub use types::{
	RecoveryRegime, RedemptionConfig, RedemptionPreview, RedemptionPreviewStep, RedemptionState,
};
pub use weights::WeightInfo;

use frame::deps::sp_runtime::FixedU128;
use types::RecoveryRegime as Regime;

pub(crate) const LOG_TARGET: &str = "runtime::redemptions";

struct OrdinaryStep<Balance> {
	debt: Balance,
	collateral_out: Balance,
	fee: Balance,
}

struct RecoveryStep<Balance> {
	burned: Balance,
	collateral_out: Balance,
	// Insurance Fund residuals settle debt without redeemer pUSD.
	debt_settled: Balance,
	regime: Regime,
}

/// Shared by execution and preview so `FinalRecovery` pricing cannot drift.
struct RecoveryPricing<Balance> {
	debt: Balance,
	collateral_out: Balance,
	split: Option<pusd_primitives::InsuranceAdjusted<Balance>>,
}

/// Per-target loop decision, shared by execution (`run_loop`) and preview
/// (`simulate_walk`) so the barrier/redeemability ladder lives in one place.
enum StepAction {
	Recovery,
	Redeem,
	Skip,
	Stop,
}

/// What one vault-side `redeem_step` did, escaped from the pricing closure by
/// `&mut` capture and consumed by `run_loop` to steer the walk.
enum StepOutcome<Balance> {
	/// An ordinary (or dormant-target) vault was redeemed at face value.
	Redeemed(OrdinaryStep<Balance>),
	/// A `FinalRecovery` vault was (partially) settled. `settle_residual`
	/// defers the Insurance-Fund settlement to the loop: it re-enters the
	/// vault pallet, which the in-flight step must not do.
	Recovery { step: RecoveryStep<Balance>, settle_residual: bool },
	/// Unredeemable target skipped; the cursor advances past it.
	Skipped,
	/// A barrier or an unpriceable target ended the walk.
	Stopped,
}

struct Accumulators<Balance, AccountId> {
	remaining: Balance,
	debt_settled: Balance,
	ordinary_debt: Balance,
	ordinary_collateral: Balance,
	ordinary_fee: Balance,
	recovery_burned: Balance,
	recovery_collateral: Balance,
	// Recovery stops after one FIFO head, so one owner/regime is enough.
	recovery_owner: Option<(AccountId, Regime)>,
}

/// Shared validation + fee-rate setup for `do_redeem` and `simulate`, so the
/// preview can never diverge from execution. Execution surfaces the typed
/// error; preview collapses it to `None`.
struct RedemptionPreamble<Balance, Moment> {
	config: RedemptionConfig<Balance, Moment>,
	state: RedemptionState<Moment>,
	price: FixedU128,
	now: Moment,
	decayed: FixedU128,
	fee_rate: FixedU128,
}

#[frame::pallet]
pub mod pallet {
	use super::*;
	use crate::{
		fees,
		types::{RedemptionConfig, RedemptionPreview, RedemptionPreviewStep, RedemptionState},
		weights::WeightInfo,
		Accumulators, OrdinaryStep, RecoveryPricing, RecoveryStep, Regime, StepAction, StepOutcome,
	};
	use alloc::vec::Vec;
	use frame::{
		deps::{
			frame_support::{
				storage::{with_storage_layer, with_transaction, TransactionOutcome},
				traits::{
					fungibles::{self, Balanced as FungiblesBalanced},
					tokens::{Fortitude, Precision, Preservation},
					EnsureOriginWithArg, OnUnbalanced, Time,
				},
			},
			sp_runtime::{
				traits::{Convert, SaturatedConversion, Saturating, Zero},
				FixedPointNumber, FixedU128,
			},
		},
		prelude::*,
	};
	use pusd_primitives::{
		recovery_pricing, ProvidePrice, RecoveryOffsetInterface, RecoveryOffsetOutcome,
		RecoveryOffsetQuote, RedemptionAllocation, RedemptionStepSnapshot, VaultInterface,
	};

	pub type BalanceOf<T> = <<T as Config>::StableAssets as fungibles::Inspect<
		<T as frame_system::Config>::AccountId,
	>>::Balance;

	pub type MomentOf<T> = <<T as Config>::TimeProvider as Time>::Moment;

	pub type StableCreditOf<T> =
		fungibles::Credit<<T as frame_system::Config>::AccountId, <T as Config>::StableAssets>;

	pub type RedemptionConfigOf<T> = RedemptionConfig<BalanceOf<T>, MomentOf<T>>;

	pub type RedemptionPreviewOf<T> =
		RedemptionPreview<<T as frame_system::Config>::AccountId, BalanceOf<T>>;

	pub type RedemptionPreviewStepOf<T> =
		RedemptionPreviewStep<<T as frame_system::Config>::AccountId, BalanceOf<T>>;

	pub type SnapshotOf<T> = RedemptionStepSnapshot<BalanceOf<T>>;

	/// One priced step: the vault-facing allocation (the `redeem_step`-closure
	/// return value) plus the loop-facing outcome it implies.
	type StepDecision<T> = (Option<RedemptionAllocation<BalanceOf<T>>>, StepOutcome<BalanceOf<T>>);

	/// Version 0: a pallet added to a live chain starts with on-chain version 0,
	/// so any higher declared version would demand a version-set migration.
	pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The collateral asset a market borrows against; a market is one
		/// `(collateral, stable)` pair.
		type CollateralAssetId: Parameter + Member + Ord + MaxEncodedLen;

		/// The stablecoin a market mints; redemptions burn it against collateral.
		type StableAssetId: Parameter + Member + Ord + MaxEncodedLen;

		/// Pricing math needs a fungibles balance type that can enter fixed-point
		/// calculations without lossy adapters.
		type StableAssets: fungibles::Inspect<
				Self::AccountId,
				AssetId = Self::StableAssetId,
				Balance: FixedPointOperand,
			> + FungiblesBalanced<Self::AccountId>;

		type Oracle: ProvidePrice<AssetId = Self::CollateralAssetId>;

		/// Vaults owns ordering and state so redemptions cannot fork a local queue.
		type Vaults: VaultInterface<
			CollateralId = Self::CollateralAssetId,
			StableId = Self::StableAssetId,
			AccountId = Self::AccountId,
			Balance = BalanceOf<Self>,
			Credit = StableCreditOf<Self>,
		>;

		/// Maps each stablecoin to the account holding its insurance cover.
		/// Cover is read at settlement time — nothing is reserved per vault —
		/// and per-stable accounts keep one coin's cover from settling another
		/// coin's bad debt.
		type InsuranceFundAccount: Convert<Self::StableAssetId, Self::AccountId>;

		/// Destination for redemption fees.
		type FeeHandler: OnUnbalanced<StableCreditOf<Self>>;

		/// Fee decay assumes this moment is expressed in milliseconds.
		type TimeProvider: Time;

		/// Authorizes [`Pallet::set_redemption_config`] for the market given
		/// as argument. Point this at the market's admin authority (e.g.
		/// vaults' `EnsureBranchFullAdmin`) and compose a governance override
		/// with `EitherOf`.
		type UpdateOrigin: EnsureOriginWithArg<
			Self::RuntimeOrigin,
			(Self::CollateralAssetId, Self::StableAssetId),
			Success = (),
		>;

		type DefaultRedemptionConfig: Get<RedemptionConfigOf<Self>>;

		#[pallet::constant]
		type MaxRedemptionSteps: Get<u32>;

		type WeightInfo: WeightInfo;

		#[cfg(feature = "runtime-benchmarks")]
		type BenchmarkHelper: crate::BenchmarkHelper<
			Self::CollateralAssetId,
			Self::StableAssetId,
			Self::AccountId,
			BalanceOf<Self>,
		>;
	}

	#[pallet::storage]
	pub type RedemptionConfigs<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		T::CollateralAssetId,
		Twox64Concat,
		T::StableAssetId,
		RedemptionConfigOf<T>,
		OptionQuery,
	>;

	#[pallet::storage]
	pub type RedemptionStates<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		T::CollateralAssetId,
		Twox64Concat,
		T::StableAssetId,
		RedemptionState<MomentOf<T>>,
		ValueQuery,
	>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// One or more ordinary (or dormant-target) vaults were redeemed.
		OrdinaryRedemptionExecuted {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			redeemer: T::AccountId,
			recipient: T::AccountId,
			pusd_burned: BalanceOf<T>,
			collateral_out: BalanceOf<T>,
			fee_pusd: BalanceOf<T>,
			steps: u32,
		},
		/// A `FinalRecovery` vault was (partially) settled.
		RecoveryRedemptionExecuted {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			redeemer: T::AccountId,
			recipient: T::AccountId,
			vault_owner: T::AccountId,
			pusd_burned: BalanceOf<T>,
			collateral_out: BalanceOf<T>,
			regime: RecoveryRegime,
		},
		/// The branch dynamic fee moved after an ordinary redemption.
		RedemptionDynamicFeeUpdated {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			old_dynamic_fee: FixedU128,
			new_dynamic_fee: FixedU128,
		},
		/// Governance replaced a market's redemption config.
		RedemptionConfigUpdated { collateral_id: T::CollateralAssetId, stable_id: T::StableAssetId },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// `max_pusd_in` is below the branch `minimum_redemption_amount`.
		BelowMinimumRedemptionAmount,
		/// No redeemable vault made any progress.
		NoRedeemableVault,
		/// Output collateral fell short of `min_collateral_out`.
		SlippageExceeded,
		/// The redeemer cannot cover the pUSD the redemption requires.
		InsufficientPusdBalance,
		/// No branch / redemption config is registered for this collateral.
		InvalidBranch,
		/// The oracle returned no usable price.
		OracleUnavailable,
		/// The vault pallet rejected a `FinalRecovery` settlement.
		RecoverySettlementFailed,
		/// Burning the Insurance-Fund residual failed.
		InsuranceFundBurnFailed,
		/// The supplied redemption config is internally inconsistent.
		InvalidRedemptionConfig,
		/// No `FinalRecovery` vault is queued, or nothing is cancellable
		/// against the head.
		NoRecoveryTarget,
		/// The `FinalRecovery` head is below par (`CR < 100%`): recovery
		/// offsets are restricted to the recovery-bonus regime.
		RecoveryOffsetBelowPar,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn integrity_test() {
			// A zero cap makes `effective_step_cap(0)` zero: the walk never
			// runs and every redeem fails with `NoRedeemableVault`.
			assert!(T::MaxRedemptionSteps::get() > 0, "`MaxRedemptionSteps` must be > 0");
			// An invalid default would reject every market registration at
			// runtime; under permissionless creation that bricks the pallet.
			assert!(
				T::DefaultRedemptionConfig::get().is_valid(),
				"`DefaultRedemptionConfig` must satisfy `RedemptionConfig::is_valid`"
			);
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(_: BlockNumberFor<T>) -> Result<(), frame::try_runtime::TryRuntimeError> {
			Self::do_try_state()
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Swap up to `max_pusd_in` pUSD for collateral at face value, walking
		/// redemption targets from the cheapest borrow rate upward.
		///
		/// `max_steps` caps how many vaults the walk may touch; `0` means the
		/// runtime's [`Config::MaxRedemptionSteps`] ceiling. Weight is charged
		/// up front for the whole cap and refunded to the steps actually
		/// taken, so a small redemption can bound the fee it must be able to
		/// pre-pay, and a large one can bound how far past the cheapest
		/// vaults it is willing to sweep.
		///
		/// `min_collateral_out` is the redeemer's slippage floor; partial
		/// fills scale it pro-rata to the pUSD actually spent.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::redeem(Pallet::<T>::effective_step_cap(*max_steps)))]
		pub fn redeem(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			max_pusd_in: BalanceOf<T>,
			min_collateral_out: BalanceOf<T>,
			recipient: T::AccountId,
			max_steps: u32,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let steps = with_storage_layer(|| {
				Self::do_redeem(
					&who,
					&collateral_id,
					&stable_id,
					max_pusd_in,
					min_collateral_out,
					&recipient,
					max_steps,
				)
			})?;
			Ok(Some(T::WeightInfo::redeem(steps)).into())
		}

		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::set_redemption_config())]
		pub fn set_redemption_config(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			config: RedemptionConfigOf<T>,
		) -> DispatchResult {
			T::UpdateOrigin::ensure_origin(origin, &(collateral_id.clone(), stable_id.clone()))?;
			ensure!(
				RedemptionConfigs::<T>::contains_key(&collateral_id, &stable_id),
				Error::<T>::InvalidBranch
			);
			ensure!(config.is_valid(), Error::<T>::InvalidRedemptionConfig);
			RedemptionConfigs::<T>::insert(&collateral_id, &stable_id, config);
			Self::deposit_event(Event::RedemptionConfigUpdated { collateral_id, stable_id });
			Ok(())
		}
	}

	#[pallet::view_functions]
	impl<T: Config> Pallet<T> {
		/// Rolled back because preparing an accurate snapshot can touch vault state.
		pub fn preview_redeem(
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			max_pusd_in: BalanceOf<T>,
		) -> Option<RedemptionPreviewOf<T>> {
			with_transaction(|| {
				let preview = Self::simulate(&collateral_id, &stable_id, max_pusd_in);
				TransactionOutcome::Rollback(Ok::<_, DispatchError>(preview))
			})
			.ok()
			.flatten()
		}
	}

	impl<T: Config> Pallet<T> {
		/// `0` preserves the default "use the runtime ceiling" behavior.
		pub(crate) fn effective_step_cap(max_steps: u32) -> u32 {
			let ceiling = T::MaxRedemptionSteps::get();
			if max_steps == 0 {
				ceiling
			} else {
				max_steps.min(ceiling)
			}
		}

		#[cfg(feature = "try-runtime")]
		pub(crate) fn do_try_state() -> Result<(), frame::try_runtime::TryRuntimeError> {
			// Every write path validates before inserting; a stored config
			// failing here means a path skipped the shared validation.
			for (_collateral_id, _stable_id, config) in RedemptionConfigs::<T>::iter() {
				if !config.is_valid() {
					return Err("stored redemption config fails `is_valid`".into());
				}
			}
			let now = T::TimeProvider::now();
			for (collateral_id, stable_id, state) in RedemptionStates::<T>::iter() {
				// Configs are the registration proxy (seeded on registration,
				// removed on deregistration); fee state must never outlive them.
				if !RedemptionConfigs::<T>::contains_key(&collateral_id, &stable_id) {
					return Err("redemption fee state row without a config row".into());
				}
				if state.last_fee_operation > now {
					return Err("`last_fee_operation` is ahead of now".into());
				}
			}
			Ok(())
		}

		/// Config, price, and decayed-fee-rate setup shared by execution and
		/// preview, so a new precondition or a fee-formula change cannot land in
		/// one path without also reaching the other.
		fn redemption_preamble(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			max_pusd_in: BalanceOf<T>,
		) -> Result<RedemptionPreamble<BalanceOf<T>, MomentOf<T>>, Error<T>> {
			let config = RedemptionConfigs::<T>::get(collateral_id, stable_id)
				.ok_or(Error::<T>::InvalidBranch)?;
			ensure!(
				max_pusd_in >= config.minimum_redemption_amount,
				Error::<T>::BelowMinimumRedemptionAmount
			);
			let price = T::Oracle::provide_price(collateral_id)
				.map_err(|_| Error::<T>::OracleUnavailable)?;
			ensure!(!price.is_zero(), Error::<T>::OracleUnavailable);
			let now = T::TimeProvider::now();
			let state = RedemptionStates::<T>::get(collateral_id, stable_id);
			let decayed = Self::decayed_dynamic_fee(&state, &config, now);
			let fee_rate = fees::fee_rate(decayed, config.base_fee, config.fee_ceiling);
			Ok(RedemptionPreamble { config, state, price, now, decayed, fee_rate })
		}

		fn do_redeem(
			redeemer: &T::AccountId,
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			max_pusd_in: BalanceOf<T>,
			min_collateral_out: BalanceOf<T>,
			recipient: &T::AccountId,
			max_steps: u32,
		) -> Result<u32, DispatchError> {
			let RedemptionPreamble { config, state, price, now, decayed, fee_rate } =
				Self::redemption_preamble(collateral_id, stable_id, max_pusd_in)?;
			let branch_debt_before = T::Vaults::branch_debt(collateral_id, stable_id);
			let step_cap = Self::effective_step_cap(max_steps);

			let mut acc = Accumulators::new(max_pusd_in);
			let steps = Self::run_loop(
				redeemer,
				collateral_id,
				stable_id,
				recipient,
				price,
				fee_rate,
				step_cap,
				&config,
				&mut acc,
			)?;

			ensure!(!acc.debt_settled.is_zero(), Error::<T>::NoRedeemableVault);
			let spent = max_pusd_in.saturating_sub(acc.remaining);
			let scaled_min = fees::scale_floor(min_collateral_out, spent, max_pusd_in);
			ensure!(acc.collateral_out() >= scaled_min, Error::<T>::SlippageExceeded);

			Self::finalize(
				redeemer,
				collateral_id,
				stable_id,
				recipient,
				&acc,
				steps,
				decayed,
				branch_debt_before,
				&config,
				now,
				state.dynamic_fee,
			);
			Ok(steps)
		}

		/// Classify a prepared target so the barrier/redeemability ladder is defined
		/// once and cannot drift between execution (`run_loop`) and preview.
		fn classify(snap: &SnapshotOf<T>, price: FixedU128) -> StepAction {
			if snap.status.is_final_recovery() {
				return StepAction::Recovery;
			}
			let redeemable = matches!(
				pusd_primitives::collateralization_ratio(snap.collateral, snap.debt, price),
				Some(cr) if cr >= FixedU128::one()
			);
			if redeemable {
				StepAction::Redeem
			} else if snap.status.is_dormant() {
				// Dormant is a hard barrier; an active underwater target can be skipped.
				StepAction::Stop
			} else {
				StepAction::Skip
			}
		}

		/// Recovery stops at one FIFO head so a call cannot silently cross into a
		/// different recovery price and Insurance Fund snapshot.
		///
		/// The cursor advances past skipped (underwater) targets so one call
		/// walks the rate index linearly: without it, every drained vault would
		/// restart the lookup at the index head and burn the remaining step
		/// budget re-visiting the same unredeemable prefix.
		fn run_loop(
			redeemer: &T::AccountId,
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			recipient: &T::AccountId,
			price: FixedU128,
			fee_rate: FixedU128,
			step_cap: u32,
			config: &RedemptionConfigOf<T>,
			acc: &mut Accumulators<BalanceOf<T>, T::AccountId>,
		) -> Result<u32, DispatchError> {
			let mut steps = 0u32;
			let mut cursor: Option<T::AccountId> = None;
			while steps < step_cap && !acc.remaining.is_zero() {
				let Some((owner, _)) =
					T::Vaults::next_redemption_target(collateral_id, stable_id, cursor.as_ref())
				else {
					break;
				};
				let budget = acc.remaining;
				let mut outcome = StepOutcome::Stopped;
				T::Vaults::redeem_step(collateral_id, stable_id, &owner, recipient, |snap| {
					let (allocation, decision) = Self::execute_step(
						stable_id, redeemer, &snap, price, fee_rate, budget, config,
					)?;
					outcome = decision;
					Ok(allocation)
				})?;
				steps = steps.saturating_add(1);
				match outcome {
					StepOutcome::Stopped => break,
					StepOutcome::Skipped => cursor = Some(owner),
					// The cursor is intentionally left as-is on a successful redeem: a
					// drained vault leaves the rate index, so the next lookup advances
					// without bypassing any newly created Dormant/FinalRecovery barrier.
					StepOutcome::Redeemed(step) => acc.apply_ordinary(&step),
					StepOutcome::Recovery { step, settle_residual } => {
						let residual = if settle_residual {
							Self::settle_residual_via_if(collateral_id, stable_id, &owner)?
						} else {
							Zero::zero()
						};
						acc.apply_recovery(owner, &step, residual);
						// The next recovery head may have a different price/fund split.
						break;
					},
				}
			}
			Ok(steps)
		}

		/// Classify, price, and fund one step from inside the vault-side
		/// `redeem_step` closure. Returns the loop outcome plus the allocation for
		/// the vault; a `None` allocation persists the touch without redeeming.
		fn execute_step(
			stable_id: &T::StableAssetId,
			redeemer: &T::AccountId,
			snap: &SnapshotOf<T>,
			price: FixedU128,
			fee_rate: FixedU128,
			budget: BalanceOf<T>,
			config: &RedemptionConfigOf<T>,
		) -> Result<StepDecision<T>, DispatchError> {
			match Self::classify(snap, price) {
				StepAction::Stop => Ok((None, StepOutcome::Stopped)),
				StepAction::Skip => Ok((None, StepOutcome::Skipped)),
				StepAction::Redeem => {
					let Some(step) = Self::price_ordinary(snap, price, fee_rate, budget) else {
						return Ok((None, StepOutcome::Stopped));
					};
					let total_in = step.debt.saturating_add(step.fee);
					Self::burn_redeemer_pusd(stable_id, redeemer, step.debt, total_in)?;
					let allocation = RedemptionAllocation {
						debt_to_cancel: step.debt,
						collateral_to_recipient: step.collateral_out,
					};
					Ok((Some(allocation), StepOutcome::Redeemed(step)))
				},
				StepAction::Recovery => {
					Self::recovery_decision(stable_id, redeemer, snap, price, budget, config)
				},
			}
		}

		/// Shared by execution and preview to keep ordinary pricing identical
		/// (mirrors [`Self::price_recovery`]).
		fn price_ordinary(
			snap: &SnapshotOf<T>,
			price: FixedU128,
			fee_rate: FixedU128,
			budget: BalanceOf<T>,
		) -> Option<OrdinaryStep<BalanceOf<T>>> {
			let debt = snap.debt.min(fees::max_debt_for_budget(budget, fee_rate));
			if debt.is_zero() {
				return None;
			}
			let collateral_out =
				recovery_pricing::collateral_for_value(debt, price).min(snap.collateral);
			let fee = fees::fee_pusd(debt, fee_rate);
			Some(OrdinaryStep { debt, collateral_out, fee })
		}

		/// Shared by execution and preview to keep recovery pricing identical.
		fn price_recovery(
			stable_id: &T::StableAssetId,
			snap: &SnapshotOf<T>,
			price: FixedU128,
			budget: BalanceOf<T>,
			config: &RedemptionConfigOf<T>,
		) -> Option<RecoveryPricing<BalanceOf<T>>> {
			// `snap.redistribution_penalty` is only consulted by `FinalRecovery`
			// pricing, so a non-recovery snapshot cannot be priced here.
			if !snap.status.is_final_recovery() {
				return None;
			}
			let redistribution_penalty = snap.redistribution_penalty;
			let cr = pusd_primitives::collateralization_ratio(snap.collateral, snap.debt, price)?;
			if cr >= FixedU128::one() {
				let bonus = recovery_pricing::recovery_bonus(
					cr,
					config.final_recovery_bonus_buffer,
					redistribution_penalty,
				);
				let debt = snap.debt.min(budget);
				let collateral_out =
					recovery_pricing::recovery_bonus_collateral_out(debt, bonus, price)
						.min(snap.collateral);
				return Some(RecoveryPricing { debt, collateral_out, split: None });
			}
			let collateral_value = price.saturating_mul_int(snap.collateral);
			let split = recovery_pricing::insurance_adjusted(
				snap.debt,
				collateral_value,
				Self::insurance_fund_available(stable_id),
			);
			let debt = split.market_cancel_debt.min(budget);
			let collateral_out =
				recovery_pricing::recovery_rate_collateral_out(debt, split.recovery_rate, price)
					.min(snap.collateral);
			Some(RecoveryPricing { debt, collateral_out, split: Some(split) })
		}

		fn recovery_decision(
			stable_id: &T::StableAssetId,
			redeemer: &T::AccountId,
			snap: &SnapshotOf<T>,
			price: FixedU128,
			budget: BalanceOf<T>,
			config: &RedemptionConfigOf<T>,
		) -> Result<StepDecision<T>, DispatchError> {
			let Some(pricing) = Self::price_recovery(stable_id, snap, price, budget, config) else {
				return Ok((None, StepOutcome::Stopped));
			};
			// Full IF cover means no redeemer-funded burn is needed; the loop
			// settles the residual once this (touch-only) step has committed.
			if pricing.split.as_ref().is_some_and(|split| split.market_cancel_debt.is_zero()) {
				let step = RecoveryStep {
					burned: Zero::zero(),
					collateral_out: Zero::zero(),
					debt_settled: Zero::zero(),
					regime: Regime::InsuranceAdjusted,
				};
				return Ok((None, StepOutcome::Recovery { step, settle_residual: true }));
			}
			Self::fund_recovery(stable_id, redeemer, &pricing)
		}

		/// Redeemer-funded recovery burn shared by both regimes; stops the loop —
		/// applying nothing — when there is nothing to cancel. The regime and the
		/// residual-settlement decision both follow from `pricing.split`.
		fn fund_recovery(
			stable_id: &T::StableAssetId,
			redeemer: &T::AccountId,
			pricing: &RecoveryPricing<BalanceOf<T>>,
		) -> Result<StepDecision<T>, DispatchError> {
			if pricing.debt.is_zero() {
				return Ok((None, StepOutcome::Stopped));
			}
			let (regime, settle_residual) = match &pricing.split {
				None => (Regime::RecoveryBonus, false),
				// IF residuals burn only after all externally cancellable debt is gone.
				Some(split) => (
					Regime::InsuranceAdjusted,
					pricing.debt == split.market_cancel_debt && !split.effective_cover.is_zero(),
				),
			};
			Self::burn_redeemer_pusd(stable_id, redeemer, pricing.debt, pricing.debt)?;
			let step = RecoveryStep {
				burned: pricing.debt,
				collateral_out: pricing.collateral_out,
				debt_settled: pricing.debt,
				regime,
			};
			let allocation = RedemptionAllocation {
				debt_to_cancel: pricing.debt,
				collateral_to_recipient: pricing.collateral_out,
			};
			Ok((Some(allocation), StepOutcome::Recovery { step, settle_residual }))
		}

		/// Withdraw `total_in` pUSD from the redeemer, burn the `debt` portion
		/// (dropping the credit is the debt-cancelling burn), and route the rest
		/// as the fee — so issuance only falls by cancelled debt. `Expendable` so
		/// a redeemer can spend their entire pUSD balance. The vault-side debt
		/// cancel happens in the surrounding `redeem_step`, atomically with this
		/// burn.
		fn burn_redeemer_pusd(
			stable_id: &T::StableAssetId,
			redeemer: &T::AccountId,
			debt: BalanceOf<T>,
			total_in: BalanceOf<T>,
		) -> DispatchResult {
			let credit = <T::StableAssets as FungiblesBalanced<_>>::withdraw(
				stable_id.clone(),
				redeemer,
				total_in,
				Precision::Exact,
				Preservation::Expendable,
				Fortitude::Polite,
			)
			.map_err(|_| Error::<T>::InsufficientPusdBalance)?;
			let (debt_credit, fee_credit) = credit.split(debt);
			drop(debt_credit);
			if fee_credit.peek().is_zero() {
				drop(fee_credit);
			} else {
				T::FeeHandler::on_unbalanced(fee_credit);
			}
			Ok(())
		}

		/// Cover is intentionally unreserved, so settlement reads live balance.
		fn insurance_fund_available(stable_id: &T::StableAssetId) -> BalanceOf<T> {
			<T::StableAssets as fungibles::Inspect<_>>::reducible_balance(
				stable_id.clone(),
				&T::InsuranceFundAccount::convert(stable_id.clone()),
				Preservation::Expendable,
				Fortitude::Polite,
			)
		}

		fn insurance_fund_withdraw(
			stable_id: &T::StableAssetId,
			amount: BalanceOf<T>,
		) -> Result<StableCreditOf<T>, DispatchError> {
			<T::StableAssets as FungiblesBalanced<_>>::withdraw(
				stable_id.clone(),
				&T::InsuranceFundAccount::convert(stable_id.clone()),
				amount,
				Precision::Exact,
				Preservation::Expendable,
				Fortitude::Polite,
			)
		}

		/// Healing verifies the vault residual and IF burn matched.
		fn settle_residual_via_if(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			owner: &T::AccountId,
		) -> Result<BalanceOf<T>, DispatchError> {
			let residual = T::Vaults::settle_recovery_residual(collateral_id, stable_id, owner)
				.map_err(|_| Error::<T>::RecoverySettlementFailed)?;
			if residual.is_zero() {
				return Ok(residual);
			}
			let credit = Self::insurance_fund_withdraw(stable_id, residual)
				.map_err(|_| Error::<T>::InsuranceFundBurnFailed)?;
			let surplus = T::Vaults::heal(collateral_id, stable_id, credit)
				.map_err(|_| Error::<T>::InsuranceFundBurnFailed)?;
			if !surplus.peek().is_zero() {
				log::error!(
					target: crate::LOG_TARGET,
					"insurance heal left surplus settling residual {residual:?}"
				);
				drop(surplus);
				return Err(Error::<T>::InsuranceFundBurnFailed.into());
			}
			drop(surplus);
			Ok(residual)
		}

		// Post-loop settlement emits both redemption events and updates the
		// dynamic fee from the full execution context; the inputs are irreducible
		// without splitting one atomic finalize into several partial passes.
		#[allow(clippy::too_many_arguments)]
		fn finalize(
			redeemer: &T::AccountId,
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			recipient: &T::AccountId,
			acc: &Accumulators<BalanceOf<T>, T::AccountId>,
			steps: u32,
			decayed: FixedU128,
			branch_debt_before: BalanceOf<T>,
			config: &RedemptionConfigOf<T>,
			now: MomentOf<T>,
			old_dynamic_fee: FixedU128,
		) {
			if !acc.ordinary_debt.is_zero() {
				let fraction =
					FixedU128::checked_from_rational(acc.ordinary_debt, branch_debt_before)
						.unwrap_or_else(FixedU128::one);
				let new_fee = fees::increased_dynamic_fee(
					decayed,
					fraction,
					config.dynamic_fee_increase_divisor,
					config.dynamic_fee_floor,
					config.dynamic_fee_ceiling,
				);
				RedemptionStates::<T>::insert(
					collateral_id,
					stable_id,
					RedemptionState { dynamic_fee: new_fee, last_fee_operation: now },
				);
				if new_fee != old_dynamic_fee {
					Self::deposit_event(Event::RedemptionDynamicFeeUpdated {
						collateral_id: collateral_id.clone(),
						stable_id: stable_id.clone(),
						old_dynamic_fee,
						new_dynamic_fee: new_fee,
					});
				}
				Self::deposit_event(Event::OrdinaryRedemptionExecuted {
					collateral_id: collateral_id.clone(),
					stable_id: stable_id.clone(),
					redeemer: redeemer.clone(),
					recipient: recipient.clone(),
					pusd_burned: acc.ordinary_debt,
					collateral_out: acc.ordinary_collateral,
					fee_pusd: acc.ordinary_fee,
					steps,
				});
			}
			if let Some((vault_owner, regime)) = acc.recovery_owner.clone() {
				Self::deposit_event(Event::RecoveryRedemptionExecuted {
					collateral_id: collateral_id.clone(),
					stable_id: stable_id.clone(),
					redeemer: redeemer.clone(),
					recipient: recipient.clone(),
					vault_owner,
					pusd_burned: acc.recovery_burned,
					collateral_out: acc.recovery_collateral,
					regime,
				});
			}
		}

		fn decayed_dynamic_fee(
			state: &RedemptionState<MomentOf<T>>,
			config: &RedemptionConfigOf<T>,
			now: MomentOf<T>,
		) -> FixedU128 {
			let elapsed = now.saturating_sub(state.last_fee_operation).saturated_into::<u64>();
			let period = config.dynamic_fee_decay_period.saturated_into::<u64>();
			fees::decay_dynamic_fee(state.dynamic_fee, elapsed, period)
				.max(config.dynamic_fee_floor)
				.min(config.dynamic_fee_ceiling)
		}

		/// Mirrors execution pricing, but lets touched vault snapshots roll back.
		fn simulate(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			max_pusd_in: BalanceOf<T>,
		) -> Option<RedemptionPreviewOf<T>> {
			let RedemptionPreamble { config, price, fee_rate, .. } =
				Self::redemption_preamble(collateral_id, stable_id, max_pusd_in).ok()?;
			let (steps_detail, steps, truncated) = Self::simulate_walk(
				collateral_id,
				stable_id,
				&config,
				price,
				fee_rate,
				max_pusd_in,
			);
			if steps_detail.is_empty() {
				return None;
			}
			let mut total_pusd_in = BalanceOf::<T>::zero();
			let mut total_collateral_out = BalanceOf::<T>::zero();
			let mut total_fee_pusd = BalanceOf::<T>::zero();
			for step in &steps_detail {
				total_pusd_in = total_pusd_in.saturating_add(step.pusd_in);
				total_collateral_out = total_collateral_out.saturating_add(step.collateral_out);
				total_fee_pusd = total_fee_pusd.saturating_add(step.fee_pusd);
			}
			Some(RedemptionPreview {
				steps_detail,
				total_pusd_in,
				total_collateral_out,
				total_fee_pusd,
				steps,
				truncated,
			})
		}

		/// Mirrors loop barriers so previews cannot suggest an unreachable route.
		fn simulate_walk(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			config: &RedemptionConfigOf<T>,
			price: FixedU128,
			rate: FixedU128,
			max_pusd_in: BalanceOf<T>,
		) -> (Vec<RedemptionPreviewStepOf<T>>, u32, bool) {
			let cap = T::MaxRedemptionSteps::get();
			let mut detail = Vec::new();
			let mut remaining = max_pusd_in;
			let mut cursor: Option<T::AccountId> = None;
			let mut steps = 0u32;
			while !remaining.is_zero() {
				if steps >= cap {
					return (detail, steps, true);
				}
				let Some((owner, _)) =
					T::Vaults::next_redemption_target(collateral_id, stable_id, cursor.as_ref())
				else {
					break;
				};
				// A touch-only step: capture the post-touch snapshot, apply nothing,
				// so the recipient (never paid) is irrelevant and `owner` stands in.
				// The preview's surrounding transaction rolls the touch back.
				let mut captured = None;
				let touch =
					T::Vaults::redeem_step(collateral_id, stable_id, &owner, &owner, |snap| {
						captured = Some(snap);
						Ok(None)
					});
				let (Ok(_), Some(snap)) = (touch, captured) else { break };
				steps = steps.saturating_add(1);
				match Self::classify(&snap, price) {
					StepAction::Recovery => {
						if let Some(step) = Self::preview_recovery_step(
							stable_id, config, &owner, &snap, price, remaining,
						) {
							detail.push(step);
						}
						break;
					},
					StepAction::Stop => break,
					StepAction::Skip => {
						cursor = Some(owner);
						continue;
					},
					StepAction::Redeem => {
						let Some(step) =
							Self::preview_ordinary_step(&owner, &snap, price, rate, remaining)
						else {
							break;
						};
						let drained = step.debt_cancellable >= snap.debt;
						remaining = remaining.saturating_sub(step.pusd_in);
						detail.push(step);
						// A drained Dormant is a barrier the preview cannot cross (it does not
						// mutate to remove it), so stop rather than re-walking it.
						if !drained || snap.status.is_dormant() {
							break;
						}
						cursor = Some(owner);
					},
				}
			}
			(detail, steps, false)
		}

		fn preview_ordinary_step(
			owner: &T::AccountId,
			snap: &SnapshotOf<T>,
			price: FixedU128,
			rate: FixedU128,
			budget: BalanceOf<T>,
		) -> Option<RedemptionPreviewStepOf<T>> {
			let step = Self::price_ordinary(snap, price, rate, budget)?;
			Some(RedemptionPreviewStep {
				target: owner.clone(),
				status: snap.status,
				debt_cancellable: step.debt,
				collateral_out: step.collateral_out,
				fee_pusd: step.fee,
				pusd_in: step.debt.saturating_add(step.fee),
			})
		}

		fn preview_recovery_step(
			stable_id: &T::StableAssetId,
			config: &RedemptionConfigOf<T>,
			owner: &T::AccountId,
			snap: &SnapshotOf<T>,
			price: FixedU128,
			budget: BalanceOf<T>,
		) -> Option<RedemptionPreviewStepOf<T>> {
			let pricing = Self::price_recovery(stable_id, snap, price, budget, config)?;
			Some(RedemptionPreviewStep {
				target: owner.clone(),
				status: snap.status,
				debt_cancellable: pricing.debt,
				collateral_out: pricing.collateral_out,
				fee_pusd: Zero::zero(),
				pusd_in: pricing.debt,
			})
		}
	}

	impl<T: Config> Pallet<T> {
		/// Config + oracle price for the recovery-offset paths. No fee state:
		/// offsets are fee-free and leave the dynamic fee untouched.
		fn offset_preamble(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
		) -> Result<(RedemptionConfigOf<T>, FixedU128), DispatchError> {
			let config = RedemptionConfigs::<T>::get(collateral_id, stable_id)
				.ok_or(Error::<T>::InvalidBranch)?;
			let price = T::Oracle::provide_price(collateral_id)
				.map_err(|_| Error::<T>::OracleUnavailable)?;
			ensure!(!price.is_zero(), Error::<T>::OracleUnavailable);
			Ok((config, price))
		}

		/// Size a recovery offset against the FIFO head via a touch-only
		/// `redeem_step`; the caller wraps this in a rolled-back transaction.
		/// Prices through [`Self::price_recovery`], the same function that
		/// prices recovery redemptions, so the two can never diverge.
		fn quote_recovery_offset(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			max_debt_to_cancel: BalanceOf<T>,
		) -> Result<RecoveryOffsetQuote<BalanceOf<T>>, DispatchError> {
			// Target first, preamble second: the dominant path (no
			// `FinalRecovery` head queued) then skips the config and oracle
			// reads entirely.
			let Some((owner, status)) =
				T::Vaults::next_redemption_target(collateral_id, stable_id, None)
			else {
				return Ok(RecoveryOffsetQuote::NoTarget);
			};
			if !status.is_final_recovery() {
				return Ok(RecoveryOffsetQuote::NoTarget);
			}
			let (config, price) = Self::offset_preamble(collateral_id, stable_id)?;
			// Touch-only, so the recipient (never paid) is irrelevant and `owner`
			// stands in.
			let mut captured = None;
			T::Vaults::redeem_step(collateral_id, stable_id, &owner, &owner, |snap| {
				captured = Some(snap);
				Ok(None)
			})?;
			let Some(snap) = captured else {
				return Ok(RecoveryOffsetQuote::NoTarget);
			};
			let Some(pricing) =
				Self::price_recovery(stable_id, &snap, price, max_debt_to_cancel, &config)
			else {
				return Ok(RecoveryOffsetQuote::NoTarget);
			};
			if pricing.split.is_some() {
				return Ok(RecoveryOffsetQuote::BelowPar);
			}
			Ok(RecoveryOffsetQuote::Available { debt: pricing.debt })
		}
	}

	impl<T: Config> RecoveryOffsetInterface for Pallet<T> {
		type CollateralId = T::CollateralAssetId;
		type StableId = T::StableAssetId;
		type AccountId = T::AccountId;
		type Balance = BalanceOf<T>;

		/// Rolled back because sizing an accurate quote touches vault state.
		fn preview_recovery_offset(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			max_debt_to_cancel: BalanceOf<T>,
		) -> Result<RecoveryOffsetQuote<BalanceOf<T>>, DispatchError> {
			with_transaction(|| {
				let quote =
					Self::quote_recovery_offset(collateral_id, stable_id, max_debt_to_cancel);
				TransactionOutcome::Rollback(quote)
			})
		}

		fn apply_recovery_offset(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			payer: &T::AccountId,
			collateral_recipient: &T::AccountId,
			max_debt_to_cancel: BalanceOf<T>,
		) -> Result<RecoveryOffsetOutcome<T::AccountId, BalanceOf<T>>, DispatchError> {
			let Some((owner, status)) =
				T::Vaults::next_redemption_target(collateral_id, stable_id, None)
			else {
				return Err(Error::<T>::NoRecoveryTarget.into());
			};
			ensure!(status.is_final_recovery(), Error::<T>::NoRecoveryTarget);
			let (config, price) = Self::offset_preamble(collateral_id, stable_id)?;

			let mut outcome = None;
			T::Vaults::redeem_step(
				collateral_id,
				stable_id,
				&owner,
				collateral_recipient,
				|snap| {
					let pricing =
						Self::price_recovery(stable_id, &snap, price, max_debt_to_cancel, &config)
							.ok_or(Error::<T>::NoRecoveryTarget)?;
					ensure!(pricing.split.is_none(), Error::<T>::RecoveryOffsetBelowPar);
					ensure!(!pricing.debt.is_zero(), Error::<T>::NoRecoveryTarget);
					// The same burn as a recovery redemption with a zero fee
					// portion: offsets are fee-free settlement. A drained head
					// flips to Dormant inside the vault step itself.
					Self::burn_redeemer_pusd(stable_id, payer, pricing.debt, pricing.debt)?;
					outcome = Some(RecoveryOffsetOutcome {
						vault_owner: owner.clone(),
						debt_cancelled: pricing.debt,
						collateral_out: pricing.collateral_out,
					});
					Ok(Some(RedemptionAllocation {
						debt_to_cancel: pricing.debt,
						collateral_to_recipient: pricing.collateral_out,
					}))
				},
			)?;
			// The closure always sets the outcome before returning an
			// allocation; reaching here without one is a vault-side breach.
			outcome.ok_or_else(|| Error::<T>::NoRecoveryTarget.into())
		}
	}

	impl<T: Config> pusd_primitives::OnBranchLifecycle<T::CollateralAssetId, T::StableAssetId>
		for Pallet<T>
	{
		fn on_registered(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
		) -> DispatchResult {
			let config = T::DefaultRedemptionConfig::get();
			ensure!(config.is_valid(), Error::<T>::InvalidRedemptionConfig);
			RedemptionConfigs::<T>::insert(collateral_id, stable_id, config);
			Ok(())
		}

		fn on_deregistered(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
		) -> DispatchResult {
			RedemptionConfigs::<T>::remove(collateral_id, stable_id);
			RedemptionStates::<T>::remove(collateral_id, stable_id);
			Ok(())
		}
	}
}

impl<Balance, AccountId> Accumulators<Balance, AccountId>
where
	Balance:
		frame::deps::sp_runtime::traits::Zero + frame::deps::sp_runtime::traits::Saturating + Copy,
{
	fn new(max_pusd_in: Balance) -> Self {
		Self {
			remaining: max_pusd_in,
			debt_settled: Balance::zero(),
			ordinary_debt: Balance::zero(),
			ordinary_collateral: Balance::zero(),
			ordinary_fee: Balance::zero(),
			recovery_burned: Balance::zero(),
			recovery_collateral: Balance::zero(),
			recovery_owner: None,
		}
	}

	fn collateral_out(&self) -> Balance {
		self.ordinary_collateral.saturating_add(self.recovery_collateral)
	}

	fn apply_ordinary(&mut self, step: &OrdinaryStep<Balance>) {
		self.remaining = self.remaining.saturating_sub(step.debt.saturating_add(step.fee));
		self.debt_settled = self.debt_settled.saturating_add(step.debt);
		self.ordinary_debt = self.ordinary_debt.saturating_add(step.debt);
		self.ordinary_collateral = self.ordinary_collateral.saturating_add(step.collateral_out);
		self.ordinary_fee = self.ordinary_fee.saturating_add(step.fee);
	}

	/// `residual` is the Insurance-Fund-settled debt the loop obtained after
	/// the step committed; it settles debt without consuming redeemer pUSD.
	fn apply_recovery(
		&mut self,
		owner: AccountId,
		step: &RecoveryStep<Balance>,
		residual: Balance,
	) {
		self.remaining = self.remaining.saturating_sub(step.burned);
		self.debt_settled =
			self.debt_settled.saturating_add(step.debt_settled).saturating_add(residual);
		self.recovery_burned = self.recovery_burned.saturating_add(step.burned);
		self.recovery_collateral = self.recovery_collateral.saturating_add(step.collateral_out);
		self.recovery_owner = Some((owner, step.regime));
	}
}

#[cfg(feature = "runtime-benchmarks")]
pub trait BenchmarkHelper<CollateralId, StableId, AccountId, Balance> {
	fn setup_redeemable_branch(vaults: u32) -> (CollateralId, StableId, AccountId, Balance);
}
