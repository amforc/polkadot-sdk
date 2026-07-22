//! # Redemptions Pallet
//!
//! Exchanges stable assets for collateral from the lowest-rate eligible vaults.
//!
//! ## Pallet API
//!
//! See the [`pallet`] module for the pallet's configuration, calls, storage, events, and errors.
//!
//! ## Overview
//!
//! Redemptions are executed against the ordering and vault state owned by `pallet-vaults`. Ordinary
//! redemptions walk dormant and active vaults, charge a market fee, and burn the redeemed stable
//! asset. Final-recovery vaults are settled first using recovery pricing and insurance cover.
//!
//! Each market stores its own redemption configuration and dynamic fee state. Market lifecycle is
//! synchronized through [`pusd_primitives::OnBranchLifecycle`].

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod fees;
pub mod types;
pub mod weights;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

#[cfg(feature = "try-runtime")]
mod try_state;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

pub use pallet::*;
pub use pusd_primitives;
pub use types::{
	RecoveryOffsetQuote, RecoveryRegime, RedemptionConfig, RedemptionPreview,
	RedemptionPreviewStep, RedemptionState,
};
pub use weights::WeightInfo;

pub(crate) const LOG_TARGET: &str = "runtime::redemptions";

/// Runtime-supplied benchmark setup.
///
/// Redemptions delegates setup because the pallet cannot create markets or vaults itself.
#[cfg(feature = "runtime-benchmarks")]
pub trait BenchmarkHelper<CollateralId, StableId, AccountId, Balance> {
	/// Creates a market with `vaults` redeemable vaults and funds a redeemer.
	fn setup_redeemable_branch(vaults: u32) -> (CollateralId, StableId, AccountId, Balance);
}

#[frame::pallet]
pub mod pallet {
	use super::*;
	use crate::{
		fees,
		types::{
			Accumulators, OffsetDecision, OrdinaryStep, RecoveryOffsetQuote, RecoveryPricing,
			RecoveryStep, RedeemMode, RedemptionConfig, RedemptionPreamble, RedemptionPreview,
			RedemptionPreviewStep, RedemptionState, StepAction, StepOutcome, WalkResult,
		},
	};
	use alloc::vec::Vec;
	use frame::{
		deps::{
			frame_support::storage::{with_transaction, TransactionOutcome},
			sp_runtime::{
				traits::{Convert, Saturating, Zero},
				FixedPointNumber, FixedU128,
			},
		},
		prelude::*,
		traits::{
			fungibles::{self, Balanced as FungiblesBalanced},
			tokens::{Fortitude, Precision, Preservation},
			EnsureOriginWithArg, OnUnbalanced, Time,
		},
	};
	use pusd_primitives::{
		debit_preservation, recovery_pricing, reducible_debit, ProvidePrice,
		RecoveryOffsetInterface, RecoveryOffsetResult, RedemptionSettlement,
		RedemptionStepSnapshot, VaultInterface,
	};

	/// Balance type used by stable assets and vault accounting.
	pub type BalanceOf<T> = <<T as Config>::StableAssets as fungibles::Inspect<
		<T as frame_system::Config>::AccountId,
	>>::Balance;

	/// Collateral identifier exposed by [`Config::Vaults`].
	pub type CollateralIdOf<T> = <<T as Config>::Vaults as VaultInterface>::CollateralId;

	/// Stable asset identifier exposed by [`Config::StableAssets`].
	pub type StableIdOf<T> = <<T as Config>::StableAssets as fungibles::Inspect<
		<T as frame_system::Config>::AccountId,
	>>::AssetId;

	/// UNIX time in milliseconds.
	pub use pusd_primitives::Millis;

	/// Stable-asset credit consumed by the pallet.
	pub type StableCreditOf<T> =
		fungibles::Credit<<T as frame_system::Config>::AccountId, <T as Config>::StableAssets>;

	/// Market redemption configuration used by the runtime.
	pub type RedemptionConfigOf<T> = RedemptionConfig<BalanceOf<T>>;

	/// Result returned by [`Pallet::preview_redeem`].
	pub type RedemptionPreviewOf<T> =
		RedemptionPreview<<T as frame_system::Config>::AccountId, BalanceOf<T>>;

	/// One step returned by [`Pallet::preview_redeem`].
	pub type RedemptionPreviewStepOf<T> =
		RedemptionPreviewStep<<T as frame_system::Config>::AccountId, BalanceOf<T>>;

	/// Vault snapshot used to price a redemption step.
	pub type SnapshotOf<T> = RedemptionStepSnapshot<BalanceOf<T>>;

	/// One priced step: the vault-facing settlement plus the loop-facing outcome it implies.
	type StepDecision<T> =
		(Option<RedemptionSettlement<StableCreditOf<T>, BalanceOf<T>>>, StepOutcome<BalanceOf<T>>);

	pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Multi-asset system used to inspect, withdraw, and burn stable assets.
		type StableAssets: fungibles::Inspect<
				Self::AccountId,
				AssetId: Parameter + Member + Ord + MaxEncodedLen,
				Balance: FixedPointOperand,
			> + FungiblesBalanced<Self::AccountId>;

		/// Provides the value of each collateral asset in stable units.
		type Oracle: ProvidePrice<AssetId = CollateralIdOf<Self>>;

		/// Owns market state, vault ordering, and atomic settlement.
		type Vaults: VaultInterface<
			CollateralId: Parameter + Member + Ord + MaxEncodedLen,
			StableId = StableIdOf<Self>,
			AccountId = Self::AccountId,
			Balance = BalanceOf<Self>,
			StableCredit = StableCreditOf<Self>,
		>;

		/// Maps each stablecoin to the account holding its insurance cover.
		/// Cover is read at settlement time — nothing is reserved per vault —
		/// and per-stable accounts keep one coin's cover from settling another
		/// coin's bad debt.
		type InsuranceFundAccount: Convert<StableIdOf<Self>, Self::AccountId>;

		/// Destination for redemption fees.
		type FeeHandler: OnUnbalanced<StableCreditOf<Self>>;

		/// Provides UNIX time in milliseconds.
		type TimeProvider: Time<Moment = Millis>;

		/// Authorizes [`Pallet::set_redemption_config`] for the market given
		/// as argument. Point this at the market's admin authority (e.g.
		/// vaults' `EnsureBranchFullAdmin`) and compose a governance override
		/// with `EitherOf`.
		type UpdateOrigin: EnsureOriginWithArg<
			Self::RuntimeOrigin,
			(CollateralIdOf<Self>, StableIdOf<Self>),
			Success = (),
		>;

		/// Redemption configuration seeded when a market is registered.
		type DefaultRedemptionConfig: Get<RedemptionConfigOf<Self>>;

		/// Maximum number of vaults a redemption may visit.
		#[pallet::constant]
		type MaxRedemptionSteps: Get<u32>;

		/// Weights for dispatchable calls.
		type WeightInfo: weights::WeightInfo;

		/// See [`crate::BenchmarkHelper`].
		#[cfg(feature = "runtime-benchmarks")]
		type BenchmarkHelper: crate::BenchmarkHelper<
			CollateralIdOf<Self>,
			StableIdOf<Self>,
			Self::AccountId,
			BalanceOf<Self>,
		>;
	}

	/// Redemption parameters keyed by collateral and stable asset.
	///
	/// A row exists exactly while the corresponding Vaults market is registered.
	#[pallet::storage]
	pub type RedemptionConfigs<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		CollateralIdOf<T>,
		Twox64Concat,
		StableIdOf<T>,
		RedemptionConfigOf<T>,
		OptionQuery,
	>;

	/// Dynamic redemption fee state keyed by collateral and stable asset.
	#[pallet::storage]
	pub type RedemptionStates<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		CollateralIdOf<T>,
		Twox64Concat,
		StableIdOf<T>,
		RedemptionState,
		ValueQuery,
	>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// One or more ordinary (or dormant-target) vaults were redeemed.
		OrdinaryRedemptionExecuted {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Account that paid the stable assets.
			redeemer: T::AccountId,
			/// Account that received collateral.
			recipient: T::AccountId,
			/// Stable assets burned against vault debt.
			pusd_burned: BalanceOf<T>,
			/// Collateral paid to the recipient.
			collateral_out: BalanceOf<T>,
			/// Stable-asset fee charged to the redeemer.
			fee_pusd: BalanceOf<T>,
			/// Number of vaults visited.
			steps: u32,
		},
		/// A `FinalRecovery` vault was (partially) settled.
		RecoveryRedemptionExecuted {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Account that paid the stable assets.
			redeemer: T::AccountId,
			/// Account that received collateral.
			recipient: T::AccountId,
			/// Owner of the settled vault.
			vault_owner: T::AccountId,
			/// Stable assets burned against vault debt.
			pusd_burned: BalanceOf<T>,
			/// Collateral paid to the recipient.
			collateral_out: BalanceOf<T>,
			/// Pricing regime applied to the settlement.
			regime: RecoveryRegime,
		},
		/// The branch dynamic fee moved after an ordinary redemption.
		RedemptionDynamicFeeUpdated {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Dynamic fee before the redemption.
			old_dynamic_fee: FixedU128,
			/// Dynamic fee after the redemption.
			new_dynamic_fee: FixedU128,
		},
		/// Governance replaced a market's redemption config.
		RedemptionConfigUpdated {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
		},
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
		/// A recovery-offset payment is denominated in a coin other than the
		/// named market's stablecoin.
		RecoveryOffsetCoinMismatch,
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
			crate::try_state::do_try_state::<T>()
		}
	}

	#[pallet::view_functions]
	impl<T: Config> Pallet<T> {
		/// Runs the real redemption loop against live vault state and rolls
		/// everything back; see [`Pallet::simulate`] for why that is sound.
		pub fn preview_redeem(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
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

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Swaps up to `max_pusd_in` stable assets for collateral.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the redeemer.
		///
		/// Redemption targets are visited from the cheapest borrow rate upward. `max_steps` caps
		/// how many vaults the walk may touch; zero uses [`Config::MaxRedemptionSteps`]. Weight is
		/// charged for the cap and refunded to the number of steps actually taken.
		///
		/// `min_collateral_out` is the redeemer's slippage floor. Partial fills scale it pro-rata
		/// to the stable assets actually spent.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::redeem(Pallet::<T>::effective_step_cap(*max_steps)))]
		pub fn redeem(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			max_pusd_in: BalanceOf<T>,
			min_collateral_out: BalanceOf<T>,
			recipient: T::AccountId,
			max_steps: u32,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let steps = Self::do_redeem(
				&who,
				&collateral_id,
				&stable_id,
				max_pusd_in,
				min_collateral_out,
				&recipient,
				max_steps,
			)?;
			Ok(Some(T::WeightInfo::redeem(steps)).into())
		}

		/// Replaces a market's redemption configuration.
		///
		/// ## Dispatch Origin
		///
		/// Must satisfy [`Config::UpdateOrigin`] for the market.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::set_redemption_config())]
		pub fn set_redemption_config(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
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

		/// Config, price, and decayed-fee-rate setup shared by execution and
		/// preview, so a new precondition or a fee-formula change cannot land in
		/// one path without also reaching the other.
		fn redemption_preamble(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			max_pusd_in: BalanceOf<T>,
		) -> Result<RedemptionPreamble<BalanceOf<T>>, Error<T>> {
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
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			max_pusd_in: BalanceOf<T>,
			min_collateral_out: BalanceOf<T>,
			recipient: &T::AccountId,
			max_steps: u32,
		) -> Result<u32, DispatchError> {
			let RedemptionPreamble { config, state, price, now, decayed, fee_rate } =
				Self::redemption_preamble(collateral_id, stable_id, max_pusd_in)?;
			let branch_debt_before = T::Vaults::branch_debt(collateral_id, stable_id);
			let step_cap = Self::effective_step_cap(max_steps);

			let mut acc = Accumulators::default();
			let walk = Self::run_loop(
				&RedeemMode::Execute { redeemer, recipient },
				collateral_id,
				stable_id,
				price,
				fee_rate,
				step_cap,
				max_pusd_in,
				&config,
				&mut acc,
			)?;

			ensure!(!acc.debt_settled.is_zero(), Error::<T>::NoRedeemableVault);
			let spent = max_pusd_in.saturating_sub(walk.remaining);
			let scaled_min = fees::scale_floor(min_collateral_out, spent, max_pusd_in);
			ensure!(acc.collateral_out() >= scaled_min, Error::<T>::SlippageExceeded);

			Self::finalize(
				redeemer,
				collateral_id,
				stable_id,
				recipient,
				&acc,
				walk.steps,
				decayed,
				branch_debt_before,
				&config,
				now,
				state.dynamic_fee,
			);
			Ok(walk.steps)
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

		/// Walk the Vaults-owned redemption ordering once, executing one priced
		/// step per target. Execution and preview run this same loop —
		/// [`RedeemMode`] only selects funding and recipients — so the barrier
		/// ladder, cursor rules, and caps cannot diverge between the two.
		///
		/// A skipped underwater target advances the carried cursor. A successful
		/// redeem keeps that cursor because a drained vault leaves the index
		/// (the next lookup advances without bypassing any newly created
		/// Dormant/FinalRecovery barrier), while a partial fill must be found
		/// again. Priority targets always preempt the cursor inside
		/// [`VaultInterface::next_redemption_target`].
		pub(crate) fn run_loop(
			mode: &RedeemMode<'_, T::AccountId>,
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			price: FixedU128,
			fee_rate: FixedU128,
			step_cap: u32,
			max_pusd_in: BalanceOf<T>,
			config: &RedemptionConfigOf<T>,
			acc: &mut Accumulators<BalanceOf<T>, T::AccountId>,
		) -> Result<WalkResult<BalanceOf<T>>, DispatchError> {
			let mut remaining = max_pusd_in;
			let mut steps = 0u32;
			let mut cursor: Option<T::AccountId> = None;
			let mut truncated = false;
			while !remaining.is_zero() {
				if steps >= step_cap {
					truncated = true;
					break;
				}
				let Some((owner, _)) =
					T::Vaults::next_redemption_target(collateral_id, stable_id, cursor.as_ref())
				else {
					break;
				};
				let mut outcome = StepOutcome::Stopped;
				T::Vaults::redeem_step(
					collateral_id,
					stable_id,
					&owner,
					mode.step_recipient(&owner),
					|snap| {
						let (allocation, decision) = Self::execute_step(
							mode, stable_id, &snap, price, fee_rate, remaining, config,
						)?;
						outcome = decision;
						Ok(allocation)
					},
				)?;
				// Barrier stops count too: their target was visited and touched.
				steps = steps.saturating_add(1);
				match outcome {
					StepOutcome::Stopped => break,
					StepOutcome::Skipped => cursor = Some(owner),
					StepOutcome::Redeemed(step) => {
						remaining = remaining.saturating_sub(step.debt.saturating_add(step.fee));
						acc.apply_ordinary(&owner, &step);
					},
					StepOutcome::Recovery { step, settle_residual } => {
						// The residual settlement re-enters the vault pallet, so
						// it must run after the in-flight step committed.
						let residual = if settle_residual {
							Self::settle_residual_via_if(collateral_id, stable_id, &owner)?
						} else {
							Zero::zero()
						};
						remaining = remaining.saturating_sub(step.burned);
						acc.apply_recovery(owner, &step, residual);
						break;
					},
				}
			}
			Ok(WalkResult { remaining, steps, truncated })
		}

		/// Classify, price, and fund one step from inside the vault-side
		/// `redeem_step` closure. Returns the loop outcome plus the settlement for
		/// the vault; a `None` settlement persists the touch without redeeming.
		fn execute_step(
			mode: &RedeemMode<'_, T::AccountId>,
			stable_id: &StableIdOf<T>,
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
					let Some(mut step) = Self::price_ordinary(snap, price, fee_rate, budget) else {
						return Ok((None, StepOutcome::Stopped));
					};
					let need = step.debt.saturating_add(step.fee);
					let (funded, preservation) = Self::fundable_budget(mode, stable_id, need)?;
					if funded < need {
						// Reprice once at the preserving limit; pricing keeps
						// the new need at or below the budget it is given.
						let Some(repriced) = Self::price_ordinary(snap, price, fee_rate, funded)
						else {
							return Ok((None, StepOutcome::Stopped));
						};
						step = repriced;
					}
					let total_in = step.debt.saturating_add(step.fee);
					debug_assert!(total_in <= funded);
					let debt_payment =
						Self::fund_redemption(mode, stable_id, step.debt, total_in, preservation)?;
					let settlement = RedemptionSettlement {
						debt_payment,
						collateral_to_recipient: step.collateral_out,
					};
					Ok((Some(settlement), StepOutcome::Redeemed(step)))
				},
				StepAction::Recovery => {
					Self::recovery_decision(mode, stable_id, snap, price, budget, config)
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
			Some(OrdinaryStep { status: snap.status, debt, collateral_out, fee })
		}

		/// Shared by execution and preview to keep recovery pricing identical.
		fn price_recovery(
			stable_id: &StableIdOf<T>,
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
				return Some(RecoveryPricing::RecoveryBonus { debt, collateral_out, bonus });
			}
			let collateral_value = price.saturating_mul_int(snap.collateral);
			// Cover the fund can pay without dusting itself: sizing the
			// shortfall debit now keeps the residual settlement exactly
			// burnable later.
			let shortfall = snap.debt.saturating_sub(collateral_value);
			let cover = Self::insurance_fund_cover(stable_id, shortfall);
			let split = recovery_pricing::insurance_adjusted(snap.debt, collateral_value, cover);
			let debt = split.market_cancel_debt.min(budget);
			let collateral_out =
				recovery_pricing::recovery_rate_collateral_out(debt, split.recovery_rate, price)
					.min(snap.collateral);
			Some(RecoveryPricing::InsuranceAdjusted { debt, collateral_out, split })
		}

		/// The budget the paying side can fund toward a step needing `need`:
		/// `need` itself when fully fundable, otherwise the preserving limit for
		/// the caller to reprice its step at, once. A debit capped by the payer's
		/// whole balance is a genuine shortfall, not a dead zone, and errors.
		/// Simulation is always fully funded: preview quotes the market side,
		/// not any particular payer's wallet.
		fn fundable_budget(
			mode: &RedeemMode<'_, T::AccountId>,
			stable_id: &StableIdOf<T>,
			need: BalanceOf<T>,
		) -> Result<(BalanceOf<T>, Preservation), Error<T>> {
			let RedeemMode::Execute { redeemer, .. } = mode else {
				// The preservation is inert: simulation issues, never withdraws.
				return Ok((need, Preservation::Expendable));
			};
			let (funded, preservation) =
				reducible_debit::<T::StableAssets, _>(stable_id.clone(), redeemer, need);
			if funded < need {
				ensure!(
					preservation == Preservation::Preserve,
					Error::<T>::InsufficientPusdBalance
				);
			}
			Ok((funded, preservation))
		}

		fn recovery_decision(
			mode: &RedeemMode<'_, T::AccountId>,
			stable_id: &StableIdOf<T>,
			snap: &SnapshotOf<T>,
			price: FixedU128,
			budget: BalanceOf<T>,
			config: &RedemptionConfigOf<T>,
		) -> Result<StepDecision<T>, DispatchError> {
			let Some(mut pricing) = Self::price_recovery(stable_id, snap, price, budget, config)
			else {
				return Ok((None, StepOutcome::Stopped));
			};
			let preservation = if pricing.debt().is_zero() {
				None
			} else {
				let (funded, preservation) =
					Self::fundable_budget(mode, stable_id, pricing.debt())?;
				if funded < pricing.debt() {
					pricing = pricing.rebudget(snap.debt, snap.collateral, price, funded);
				}
				debug_assert!(pricing.debt() <= funded);
				Some(preservation)
			};
			let debt = pricing.debt();
			let settle_residual = pricing.settles_residual();
			if debt.is_zero() && !settle_residual {
				return Ok((None, StepOutcome::Stopped));
			}
			// Full IF cover needs no redeemer-funded burn; committing the touch
			// still unlocks the post-step residual settlement.
			let debt_payment = match preservation {
				Some(preservation) => {
					Some(Self::fund_redemption(mode, stable_id, debt, debt, preservation)?)
				},
				None => None,
			};
			let step = RecoveryStep {
				status: snap.status,
				burned: debt,
				collateral_out: pricing.collateral_out(),
				regime: pricing.regime(),
			};
			let settlement = debt_payment.map(|debt_payment| RedemptionSettlement {
				debt_payment,
				collateral_to_recipient: pricing.collateral_out(),
			});
			Ok((settlement, StepOutcome::Recovery { step, settle_residual }))
		}

		/// Fund `total_in` stable units for one step — withdrawn from the
		/// redeemer with the preservation sized by [`Self::fundable_budget`], or
		/// issued unbacked when simulating — return the debt payment to the
		/// vault step, and route the rest as the fee.
		fn fund_redemption(
			mode: &RedeemMode<'_, T::AccountId>,
			stable_id: &StableIdOf<T>,
			debt: BalanceOf<T>,
			total_in: BalanceOf<T>,
			preservation: Preservation,
		) -> Result<StableCreditOf<T>, DispatchError> {
			debug_assert!(debt <= total_in);
			let credit = match mode {
				RedeemMode::Execute { redeemer, .. } => {
					<T::StableAssets as FungiblesBalanced<_>>::withdraw(
						stable_id.clone(),
						redeemer,
						total_in,
						Precision::Exact,
						preservation,
						Fortitude::Polite,
					)?
				},
				// Unbacked issuance is sound only because `simulate` runs inside
				// a transaction its caller always rolls back.
				RedeemMode::Simulate => {
					<T::StableAssets as FungiblesBalanced<_>>::issue(stable_id.clone(), total_in)
				},
			};
			debug_assert_eq!(credit.peek(), total_in);
			let (debt_credit, fee_credit) = credit.split(debt);
			T::FeeHandler::on_unbalanced(fee_credit);
			Ok(debt_credit)
		}

		/// The shortfall cover the fund can pay without dusting itself. Cover
		/// is intentionally unreserved, so pricing and settlement read the
		/// live account.
		fn insurance_fund_cover(
			stable_id: &StableIdOf<T>,
			shortfall: BalanceOf<T>,
		) -> BalanceOf<T> {
			reducible_debit::<T::StableAssets, _>(
				stable_id.clone(),
				&T::InsuranceFundAccount::convert(stable_id.clone()),
				shortfall,
			)
			.0
		}

		/// Healing verifies the vault residual and IF burn matched.
		fn settle_residual_via_if(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			owner: &T::AccountId,
		) -> Result<BalanceOf<T>, DispatchError> {
			let residual = T::Vaults::settle_recovery_residual(collateral_id, stable_id, owner)
				.map_err(|_| Error::<T>::RecoverySettlementFailed)?;
			if residual.is_zero() {
				return Ok(residual);
			}
			// Re-read the fund: fees routed earlier in this call may have
			// changed it since pricing. `Expendable` only on a full drain, so
			// the withdrawal itself fails — mapped to the domain error — when
			// the fund cannot pay the residual exactly.
			let account = T::InsuranceFundAccount::convert(stable_id.clone());
			let preservation =
				debit_preservation::<T::StableAssets, _>(stable_id.clone(), &account, residual);
			let credit = <T::StableAssets as FungiblesBalanced<_>>::withdraw(
				stable_id.clone(),
				&account,
				residual,
				Precision::Exact,
				preservation,
				Fortitude::Polite,
			)
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
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			recipient: &T::AccountId,
			acc: &Accumulators<BalanceOf<T>, T::AccountId>,
			steps: u32,
			decayed: FixedU128,
			branch_debt_before: BalanceOf<T>,
			config: &RedemptionConfigOf<T>,
			now: Millis,
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
			state: &RedemptionState,
			config: &RedemptionConfigOf<T>,
			now: Millis,
		) -> FixedU128 {
			let elapsed = now.saturating_sub(state.last_fee_operation);
			let period = config.dynamic_fee_decay_period;
			fees::decay_dynamic_fee(state.dynamic_fee, elapsed, period)
				.max(config.dynamic_fee_floor)
				.min(config.dynamic_fee_ceiling)
		}

		/// Runs the real execution loop — settlements applied, Insurance-Fund
		/// residuals settled — with issued (unbacked) credit standing in for the
		/// redeemer, so preview and execution cannot diverge. Only the caller's
		/// unconditional rollback makes the issuance and the mutations sound.
		fn simulate(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			max_pusd_in: BalanceOf<T>,
		) -> Option<RedemptionPreviewOf<T>> {
			let RedemptionPreamble { config, price, fee_rate, .. } =
				Self::redemption_preamble(collateral_id, stable_id, max_pusd_in).ok()?;
			let mut acc = Accumulators::default();
			acc.detail = Some(Vec::new());
			let walk = Self::run_loop(
				&RedeemMode::Simulate,
				collateral_id,
				stable_id,
				price,
				fee_rate,
				T::MaxRedemptionSteps::get(),
				max_pusd_in,
				&config,
				&mut acc,
			)
			.ok()?;
			let steps_detail = acc.detail.take().unwrap_or_default();
			if steps_detail.is_empty() {
				return None;
			}
			Some(RedemptionPreview { steps_detail, steps: walk.steps, truncated: walk.truncated })
		}

		/// Touch-only `redeem_step`: capture the post-touch snapshot, apply
		/// nothing, so the recipient (never paid) is irrelevant and `owner`
		/// stands in. Callers roll the touch back via a surrounding transaction.
		fn touch_snapshot(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			owner: &T::AccountId,
		) -> Result<Option<SnapshotOf<T>>, DispatchError> {
			let mut captured = None;
			T::Vaults::redeem_step(collateral_id, stable_id, owner, owner, |snap| {
				captured = Some(snap);
				Ok(None)
			})?;
			Ok(captured)
		}
	}

	impl<T: Config> Pallet<T> {
		/// Config + oracle price for the recovery-offset paths. No fee state:
		/// offsets are fee-free and leave the dynamic fee untouched.
		fn offset_preamble(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
		) -> Result<(RedemptionConfigOf<T>, FixedU128), DispatchError> {
			let config = RedemptionConfigs::<T>::get(collateral_id, stable_id)
				.ok_or(Error::<T>::InvalidBranch)?;
			let price = T::Oracle::provide_price(collateral_id)
				.map_err(|_| Error::<T>::OracleUnavailable)?;
			ensure!(!price.is_zero(), Error::<T>::OracleUnavailable);
			Ok((config, price))
		}

		/// The queued priority head, only when it is a `FinalRecovery` vault.
		/// Shared by the offset quote and execution so the head gate cannot
		/// diverge.
		fn final_recovery_head(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
		) -> Option<T::AccountId> {
			let (owner, status) =
				T::Vaults::next_redemption_target(collateral_id, stable_id, None)?;
			status.is_final_recovery().then_some(owner)
		}

		/// Size a recovery offset against the FIFO head via a touch-only
		/// `redeem_step`; the caller wraps this in a rolled-back transaction.
		/// Prices through [`Self::price_recovery`], the same function that
		/// prices recovery redemptions, so the two can never diverge.
		fn quote_recovery_offset(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			max_debt_to_cancel: BalanceOf<T>,
		) -> Result<RecoveryOffsetQuote<BalanceOf<T>>, DispatchError> {
			// Target first, preamble second: the dominant path (no
			// `FinalRecovery` head queued) then skips the config and oracle
			// reads entirely.
			let Some(owner) = Self::final_recovery_head(collateral_id, stable_id) else {
				return Ok(RecoveryOffsetQuote::NoTarget);
			};
			let (config, price) = Self::offset_preamble(collateral_id, stable_id)?;
			let Some(snap) = Self::touch_snapshot(collateral_id, stable_id, &owner)? else {
				return Ok(RecoveryOffsetQuote::NoTarget);
			};
			let pricing =
				Self::price_recovery(stable_id, &snap, price, max_debt_to_cancel, &config);
			Ok(match Self::classify_offset(pricing) {
				OffsetDecision::NoTarget => RecoveryOffsetQuote::NoTarget,
				OffsetDecision::BelowPar => RecoveryOffsetQuote::BelowPar,
				OffsetDecision::Cancellable { debt, .. } => RecoveryOffsetQuote::Available { debt },
			})
		}

		/// Classify a priced head for the offset surface. Both the quote and
		/// the execution map through this one function so they can never
		/// diverge: offsets are restricted to the recovery-bonus regime, and
		/// a zero-sized burn is no target rather than `Available { debt: 0 }`.
		fn classify_offset(
			pricing: Option<RecoveryPricing<BalanceOf<T>>>,
		) -> OffsetDecision<BalanceOf<T>> {
			match pricing {
				None => OffsetDecision::NoTarget,
				Some(RecoveryPricing::InsuranceAdjusted { .. }) => OffsetDecision::BelowPar,
				Some(RecoveryPricing::RecoveryBonus { debt, .. }) if debt.is_zero() => {
					OffsetDecision::NoTarget
				},
				Some(RecoveryPricing::RecoveryBonus { debt, collateral_out, .. }) => {
					OffsetDecision::Cancellable { debt, collateral_out }
				},
			}
		}

		/// Quote the head's capacity for up to `max_debt_to_cancel`. Rolled
		/// back because sizing an accurate quote touches vault state; no
		/// state changes. A view surface (like [`Pallet::preview_redeem`]),
		/// not part of the [`RecoveryOffsetInterface`] execution seam.
		pub fn preview_recovery_offset(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			max_debt_to_cancel: BalanceOf<T>,
		) -> Result<RecoveryOffsetQuote<BalanceOf<T>>, DispatchError> {
			with_transaction(|| {
				let quote =
					Self::quote_recovery_offset(collateral_id, stable_id, max_debt_to_cancel);
				TransactionOutcome::Rollback(quote)
			})
		}
	}

	impl<T: Config> RecoveryOffsetInterface for Pallet<T> {
		type CollateralId = CollateralIdOf<T>;
		type StableId = StableIdOf<T>;
		type AccountId = T::AccountId;
		type Balance = BalanceOf<T>;
		type Credit = StableCreditOf<T>;

		fn execute_recovery_offset(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			payment: StableCreditOf<T>,
			collateral_recipient: &T::AccountId,
		) -> Result<(RecoveryOffsetResult<BalanceOf<T>>, StableCreditOf<T>), DispatchError> {
			// A wrong-coin payment is a caller wiring bug: refuse loudly
			// instead of settling in whatever market the coin names. The
			// caller's rollback unwinds the dropped payment.
			ensure!(payment.asset() == *stable_id, Error::<T>::RecoveryOffsetCoinMismatch);
			let Some(owner) = Self::final_recovery_head(collateral_id, stable_id) else {
				return Ok((RecoveryOffsetResult::NoTarget, payment));
			};
			let (config, price) = Self::offset_preamble(collateral_id, stable_id)?;
			// The credit is the budget: the burn can never exceed what the
			// caller funded, so no payer sizing happens on this side.
			let budget = payment.peek();

			with_transaction(|| {
				let mut payment = payment;
				// `NoTarget` unless the closure refuses with something more specific.
				// A successful settlement records its collateral result here while the
				// remaining payment credit becomes the caller's change.
				let mut refusal = RecoveryOffsetResult::NoTarget;
				let mut applied_collateral = None;
				let step = T::Vaults::redeem_step(
					collateral_id,
					stable_id,
					&owner,
					collateral_recipient,
					|snap| {
						let pricing =
							Self::price_recovery(stable_id, &snap, price, budget, &config);
						match Self::classify_offset(pricing) {
							OffsetDecision::NoTarget => Ok(None),
							OffsetDecision::BelowPar => {
								refusal = RecoveryOffsetResult::BelowPar;
								Ok(None)
							},
							OffsetDecision::Cancellable { debt, collateral_out } => {
								let debt_payment = payment.extract(debt);
								debug_assert_eq!(debt_payment.peek(), debt);
								applied_collateral = Some(collateral_out);
								Ok(Some(RedemptionSettlement {
									debt_payment,
									collateral_to_recipient: collateral_out,
								}))
							},
						}
					},
				);
				match step {
					Err(error) => TransactionOutcome::Rollback(Err(error)),
					Ok(()) => match applied_collateral {
						Some(collateral_out) => TransactionOutcome::Commit(Ok((
							RecoveryOffsetResult::Applied { collateral_out },
							payment,
						))),
						None => TransactionOutcome::Rollback(Ok((refusal, payment))),
					},
				}
			})
		}
	}

	impl<T: Config> pusd_primitives::OnBranchLifecycle<CollateralIdOf<T>, StableIdOf<T>> for Pallet<T> {
		fn on_registered(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
		) -> DispatchResult {
			let config = T::DefaultRedemptionConfig::get();
			ensure!(config.is_valid(), Error::<T>::InvalidRedemptionConfig);
			RedemptionConfigs::<T>::insert(collateral_id, stable_id, config);
			Ok(())
		}

		fn on_deregistered(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
		) -> DispatchResult {
			RedemptionConfigs::<T>::remove(collateral_id, stable_id);
			RedemptionStates::<T>::remove(collateral_id, stable_id);
			Ok(())
		}
	}
}
