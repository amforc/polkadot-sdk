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
		Accumulators, OrdinaryStep, RecoveryPricing, RecoveryStep, Regime, StepAction,
	};
	use alloc::vec::Vec;
	use frame::{
		deps::{
			frame_support::{
				storage::{with_storage_layer, with_transaction, TransactionOutcome},
				traits::{
					fungibles::{self, Balanced as FungiblesBalanced},
					tokens::{Fortitude, Precision, Preservation},
					OnUnbalanced, Time,
				},
			},
			sp_runtime::{
				traits::{SaturatedConversion, Saturating, Zero},
				FixedPointNumber, FixedU128,
			},
		},
		prelude::*,
	};
	use pusd_primitives::{
		recovery_pricing, BranchMode, BranchModeProvider, ProvidePrice, RedemptionAllocation,
		RedemptionStepSnapshot, VaultBadDebtInterface, VaultRedemptionInterface,
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

	pub type SnapshotOf<T> =
		RedemptionStepSnapshot<<T as frame_system::Config>::AccountId, BalanceOf<T>>;

	pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

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
		type StableAssets: fungibles::Mutate<
				Self::AccountId,
				AssetId = Self::StableAssetId,
				Balance: FixedPointOperand,
			> + FungiblesBalanced<Self::AccountId>;

		type Oracle: ProvidePrice<AssetId = Self::CollateralAssetId, Moment = MomentOf<Self>>;

		/// Vaults owns ordering and state so redemptions cannot fork a local queue.
		type Vaults: VaultRedemptionInterface<
				Self::AccountId,
				Self::CollateralAssetId,
				Self::StableAssetId,
				BalanceOf<Self>,
			> + VaultBadDebtInterface<
				Self::CollateralAssetId,
				Self::StableAssetId,
				BalanceOf<Self>,
				StableCreditOf<Self>,
			>;

		type BranchMode: BranchModeProvider<Self::CollateralAssetId, Self::StableAssetId>;

		/// Cover is read at settlement time; the fund is not reserved per vault.
		type InsuranceFundAccount: Get<Self::AccountId>;

		type FeeHandler: OnUnbalanced<StableCreditOf<Self>>;

		/// Fee decay assumes this moment is expressed in milliseconds.
		type TimeProvider: Time;

		type ManagerOrigin: EnsureOrigin<Self::RuntimeOrigin, Success = ()>;

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
		/// The branch base rate moved after an ordinary redemption.
		RedemptionBaseRateUpdated {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			old_base_rate: FixedU128,
			new_base_rate: FixedU128,
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
		/// The branch is frozen; redemptions are disabled.
		BranchFrozen,
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
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
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
			T::ManagerOrigin::ensure_origin(origin)?;
			ensure!(
				T::BranchMode::is_registered(&collateral_id, &stable_id),
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

		/// Branch-mode, config, price, and decayed-fee-rate setup shared by
		/// execution and preview, so a new precondition or a fee-formula change
		/// cannot land in one path without also reaching the other.
		fn redemption_preamble(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			max_pusd_in: BalanceOf<T>,
		) -> Result<RedemptionPreamble<BalanceOf<T>, MomentOf<T>>, Error<T>> {
			let mode =
				T::BranchMode::mode(collateral_id, stable_id).ok_or(Error::<T>::InvalidBranch)?;
			ensure!(!matches!(mode, BranchMode::Frozen), Error::<T>::BranchFrozen);
			let config = RedemptionConfigs::<T>::get(collateral_id, stable_id)
				.ok_or(Error::<T>::InvalidBranch)?;
			ensure!(
				max_pusd_in >= config.minimum_redemption_amount,
				Error::<T>::BelowMinimumRedemptionAmount
			);
			let price = T::Oracle::provide_price(collateral_id)
				.map_err(|_| Error::<T>::OracleUnavailable)?
				.price;
			ensure!(!price.is_zero(), Error::<T>::OracleUnavailable);
			let now = T::TimeProvider::now();
			let state = RedemptionStates::<T>::get(collateral_id, stable_id);
			let decayed = Self::decayed_base_rate(&state, &config, now);
			let fee_rate =
				fees::fee_rate(decayed, config.redemption_fee_floor, config.redemption_fee_ceiling);
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
				state.base_rate,
			);
			Ok(steps)
		}

		/// Classify a prepared target so the barrier/redeemability ladder is defined
		/// once and cannot drift between execution (`run_loop`) and preview.
		fn classify(snap: &SnapshotOf<T>, price: FixedU128) -> StepAction {
			if snap.kind.is_final_recovery() {
				return StepAction::Recovery;
			}
			let redeemable = matches!(
				pusd_primitives::collateralization_ratio(snap.collateral, snap.debt, price),
				Some(cr) if cr >= FixedU128::one()
			);
			if redeemable {
				StepAction::Redeem
			} else if snap.kind.is_dormant() {
				// Dormant is a hard barrier; an active underwater target can be skipped.
				StepAction::Stop
			} else {
				StepAction::Skip
			}
		}

		/// Recovery stops at one FIFO head so a call cannot silently cross into a
		/// different recovery price and Insurance Fund snapshot.
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
				let Some(target) =
					T::Vaults::next_redemption_target(collateral_id, stable_id, cursor.as_ref())
				else {
					break;
				};
				let snap = T::Vaults::prepare_redemption_step(
					collateral_id.clone(),
					stable_id.clone(),
					target.owner.clone(),
				)?;
				steps = steps.saturating_add(1);
				match Self::classify(&snap, price) {
					StepAction::Recovery => {
						if let Some(step) = Self::recovery_step(
							collateral_id,
							stable_id,
							&snap,
							price,
							acc.remaining,
							redeemer,
							recipient,
							config,
						)? {
							acc.apply_recovery(snap.owner.clone(), &step);
						}
						// The next recovery head may have a different price/fund split.
						break;
					},
					StepAction::Stop => break,
					StepAction::Skip => {
						cursor = Some(target.owner.clone());
						continue;
					},
					StepAction::Redeem => match Self::ordinary_step(
						collateral_id,
						stable_id,
						&snap,
						price,
						fee_rate,
						acc.remaining,
						redeemer,
						recipient,
					)? {
						// The cursor is intentionally left as-is on a successful redeem: a
						// drained vault leaves the rate index, so the next lookup advances
						// without bypassing any newly created Dormant/FinalRecovery barrier.
						Some(step) => acc.apply_ordinary(&step),
						None => break,
					},
				}
			}
			Ok(steps)
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

		fn ordinary_step(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			snap: &SnapshotOf<T>,
			price: FixedU128,
			fee_rate: FixedU128,
			budget: BalanceOf<T>,
			redeemer: &T::AccountId,
			recipient: &T::AccountId,
		) -> Result<Option<OrdinaryStep<BalanceOf<T>>>, DispatchError> {
			let Some(step) = Self::price_ordinary(snap, price, fee_rate, budget) else {
				return Ok(None);
			};
			let total_in = step.debt.saturating_add(step.fee);
			Self::burn_and_apply(
				collateral_id,
				stable_id,
				&snap.owner,
				redeemer,
				recipient,
				step.debt,
				step.collateral_out,
				total_in,
			)?;
			Ok(Some(step))
		}

		/// Shared by execution and preview to keep recovery pricing identical.
		fn price_recovery(
			stable_id: &T::StableAssetId,
			snap: &SnapshotOf<T>,
			price: FixedU128,
			budget: BalanceOf<T>,
			config: &RedemptionConfigOf<T>,
		) -> Option<RecoveryPricing<BalanceOf<T>>> {
			let cr = pusd_primitives::collateralization_ratio(snap.collateral, snap.debt, price)?;
			if cr >= FixedU128::one() {
				let bonus = recovery_pricing::recovery_bonus(
					cr,
					config.final_recovery_bonus_buffer,
					snap.redistribution_penalty,
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

		/// Guarded redeemer-funded recovery burn shared by both regimes. Returns
		/// `false` — caller stops with `Ok(None)` — when there is nothing to cancel.
		fn burn_recovery_debt(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			owner: &T::AccountId,
			redeemer: &T::AccountId,
			recipient: &T::AccountId,
			debt: BalanceOf<T>,
			collateral_out: BalanceOf<T>,
		) -> Result<bool, DispatchError> {
			if debt.is_zero() {
				return Ok(false);
			}
			Self::burn_and_apply(
				collateral_id,
				stable_id,
				owner,
				redeemer,
				recipient,
				debt,
				collateral_out,
				debt,
			)?;
			Ok(true)
		}

		fn recovery_step(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			snap: &SnapshotOf<T>,
			price: FixedU128,
			budget: BalanceOf<T>,
			redeemer: &T::AccountId,
			recipient: &T::AccountId,
			config: &RedemptionConfigOf<T>,
		) -> Result<Option<RecoveryStep<BalanceOf<T>>>, DispatchError> {
			let Some(pricing) = Self::price_recovery(stable_id, snap, price, budget, config) else {
				return Ok(None);
			};
			let Some(split) = pricing.split else {
				if !Self::burn_recovery_debt(
					collateral_id,
					stable_id,
					&snap.owner,
					redeemer,
					recipient,
					pricing.debt,
					pricing.collateral_out,
				)? {
					return Ok(None);
				}
				return Ok(Some(RecoveryStep {
					burned: pricing.debt,
					collateral_out: pricing.collateral_out,
					debt_settled: pricing.debt,
					regime: Regime::RecoveryBonus,
				}));
			};
			// Full IF cover means no redeemer-funded burn is needed.
			if split.market_cancel_debt.is_zero() {
				let residual = Self::settle_residual_via_if(collateral_id, stable_id, &snap.owner)?;
				return Ok(Some(RecoveryStep {
					burned: Zero::zero(),
					collateral_out: Zero::zero(),
					debt_settled: residual,
					regime: Regime::InsuranceAdjusted,
				}));
			}
			if !Self::burn_recovery_debt(
				collateral_id,
				stable_id,
				&snap.owner,
				redeemer,
				recipient,
				pricing.debt,
				pricing.collateral_out,
			)? {
				return Ok(None);
			}
			let mut debt_settled = pricing.debt;
			// IF residuals burn only after all externally cancellable debt is gone.
			if pricing.debt == split.market_cancel_debt && !split.effective_cover.is_zero() {
				let residual = Self::settle_residual_via_if(collateral_id, stable_id, &snap.owner)?;
				debt_settled = debt_settled.saturating_add(residual);
			}
			Ok(Some(RecoveryStep {
				burned: pricing.debt,
				collateral_out: pricing.collateral_out,
				debt_settled,
				regime: Regime::InsuranceAdjusted,
			}))
		}

		/// Fee credit is routed, not burned, so issuance only falls by cancelled debt.
		fn burn_and_apply(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
			owner: &T::AccountId,
			redeemer: &T::AccountId,
			recipient: &T::AccountId,
			debt_to_cancel: BalanceOf<T>,
			collateral_out: BalanceOf<T>,
			total_in: BalanceOf<T>,
		) -> DispatchResult {
			let credit = <T::StableAssets as FungiblesBalanced<_>>::withdraw(
				stable_id.clone(),
				redeemer,
				total_in,
				Precision::Exact,
				Preservation::Preserve,
				Fortitude::Polite,
			)
			.map_err(|_| Error::<T>::InsufficientPusdBalance)?;
			let (debt_credit, fee_credit) = credit.split(debt_to_cancel);
			// Dropping the credit is the debt-cancelling burn.
			drop(debt_credit);
			if fee_credit.peek().is_zero() {
				drop(fee_credit);
			} else {
				T::FeeHandler::on_unbalanced(fee_credit);
			}
			T::Vaults::apply_redemption(
				collateral_id.clone(),
				stable_id.clone(),
				owner.clone(),
				recipient.clone(),
				RedemptionAllocation {
					debt_to_cancel,
					collateral_to_redeemer: collateral_out,
					fee_collateral_retained: Zero::zero(),
				},
			)
		}

		/// Cover is intentionally unreserved, so settlement reads live balance.
		fn insurance_fund_available(stable_id: &T::StableAssetId) -> BalanceOf<T> {
			<T::StableAssets as fungibles::Inspect<_>>::reducible_balance(
				stable_id.clone(),
				&T::InsuranceFundAccount::get(),
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
				&T::InsuranceFundAccount::get(),
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
			let residual = T::Vaults::settle_recovery_residual(
				collateral_id.clone(),
				stable_id.clone(),
				owner.clone(),
			)
			.map_err(|_| Error::<T>::RecoverySettlementFailed)?;
			if residual.is_zero() {
				return Ok(residual);
			}
			let credit = Self::insurance_fund_withdraw(stable_id, residual)
				.map_err(|_| Error::<T>::InsuranceFundBurnFailed)?;
			let surplus = <T::Vaults as VaultBadDebtInterface<_, _, _, _>>::heal(
				collateral_id.clone(),
				stable_id.clone(),
				credit,
			)
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

		// Post-loop settlement emits both redemption events and updates the base
		// rate from the full execution context; the inputs are irreducible without
		// splitting one atomic finalize into several partial passes.
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
			old_base_rate: FixedU128,
		) {
			if !acc.ordinary_debt.is_zero() {
				let fraction =
					FixedU128::checked_from_rational(acc.ordinary_debt, branch_debt_before)
						.unwrap_or_else(FixedU128::one);
				let new_base = fees::increased_base_rate(
					decayed,
					fraction,
					config.base_rate_increase_divisor,
					config.base_rate_floor,
					config.base_rate_ceiling,
				);
				RedemptionStates::<T>::insert(
					collateral_id,
					stable_id,
					RedemptionState { base_rate: new_base, last_fee_operation: now },
				);
				if new_base != old_base_rate {
					Self::deposit_event(Event::RedemptionBaseRateUpdated {
						collateral_id: collateral_id.clone(),
						stable_id: stable_id.clone(),
						old_base_rate,
						new_base_rate: new_base,
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

		fn decayed_base_rate(
			state: &RedemptionState<MomentOf<T>>,
			config: &RedemptionConfigOf<T>,
			now: MomentOf<T>,
		) -> FixedU128 {
			let elapsed = now.saturating_sub(state.last_fee_operation).saturated_into::<u64>();
			let period = config.base_rate_decay_period.saturated_into::<u64>();
			fees::decay_base_rate(state.base_rate, elapsed, period)
				.max(config.base_rate_floor)
				.min(config.base_rate_ceiling)
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
				let Some(target) =
					T::Vaults::next_redemption_target(collateral_id, stable_id, cursor.as_ref())
				else {
					break;
				};
				let Ok(snap) = T::Vaults::prepare_redemption_step(
					collateral_id.clone(),
					stable_id.clone(),
					target.owner.clone(),
				) else {
					break;
				};
				steps = steps.saturating_add(1);
				match Self::classify(&snap, price) {
					StepAction::Recovery => {
						if let Some(step) =
							Self::preview_recovery_step(stable_id, config, &snap, price, remaining)
						{
							detail.push(step);
						}
						break;
					},
					StepAction::Stop => break,
					StepAction::Skip => {
						cursor = Some(target.owner);
						continue;
					},
					StepAction::Redeem => {
						let Some(step) = Self::preview_ordinary_step(&snap, price, rate, remaining)
						else {
							break;
						};
						let drained = step.debt_cancellable >= snap.debt;
						remaining = remaining.saturating_sub(step.pusd_in);
						detail.push(step);
						// A drained Dormant is a barrier the preview cannot cross (it does not
						// mutate to remove it), so stop rather than re-walking it.
						if !drained || snap.kind.is_dormant() {
							break;
						}
						cursor = Some(target.owner);
					},
				}
			}
			(detail, steps, false)
		}

		fn preview_ordinary_step(
			snap: &SnapshotOf<T>,
			price: FixedU128,
			rate: FixedU128,
			budget: BalanceOf<T>,
		) -> Option<RedemptionPreviewStepOf<T>> {
			let step = Self::price_ordinary(snap, price, rate, budget)?;
			Some(RedemptionPreviewStep {
				target: snap.owner.clone(),
				kind: snap.kind,
				debt_cancellable: step.debt,
				collateral_out: step.collateral_out,
				fee_pusd: step.fee,
				pusd_in: step.debt.saturating_add(step.fee),
			})
		}

		fn preview_recovery_step(
			stable_id: &T::StableAssetId,
			config: &RedemptionConfigOf<T>,
			snap: &SnapshotOf<T>,
			price: FixedU128,
			budget: BalanceOf<T>,
		) -> Option<RedemptionPreviewStepOf<T>> {
			let pricing = Self::price_recovery(stable_id, snap, price, budget, config)?;
			Some(RedemptionPreviewStep {
				target: snap.owner.clone(),
				kind: snap.kind,
				debt_cancellable: pricing.debt,
				collateral_out: pricing.collateral_out,
				fee_pusd: Zero::zero(),
				pusd_in: pricing.debt,
			})
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

	fn apply_recovery(&mut self, owner: AccountId, step: &RecoveryStep<Balance>) {
		self.remaining = self.remaining.saturating_sub(step.burned);
		self.debt_settled = self.debt_settled.saturating_add(step.debt_settled);
		self.recovery_burned = self.recovery_burned.saturating_add(step.burned);
		self.recovery_collateral = self.recovery_collateral.saturating_add(step.collateral_out);
		self.recovery_owner = Some((owner, step.regime));
	}
}

#[cfg(feature = "runtime-benchmarks")]
pub trait BenchmarkHelper<CollateralId, StableId, AccountId, Balance> {
	fn setup_redeemable_branch(vaults: u32) -> (CollateralId, StableId, AccountId, Balance);
}
