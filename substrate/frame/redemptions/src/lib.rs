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
//! Redemption configuration and dynamic fee state are stored per stablecoin, shared by every
//! collateral market issuing it: the fee nudges how much of the coin is redeemed, whichever
//! collateral the redeemer used. Market lifecycle is synchronized through
//! [`pusd_primitives::OnBranchLifecycle`], which refcounts those rows.

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
	RecoveryOffsetQuote, RecoveryRegime, RedemptionConfig, RedemptionQuote, RedemptionState,
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
			Accumulators, OffsetDecision, OrdinaryStep, PricedStep, RecoveryOffsetQuote,
			RecoveryPricing, RecoveryStep, RedemptionConfig, RedemptionPreamble, RedemptionQuote,
			RedemptionState, StepAction, StepOutcome, WalkResult,
		},
	};
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
	pub type RedemptionQuoteOf<T> = RedemptionQuote<BalanceOf<T>>;

	/// Vault snapshot used to price a redemption step.
	pub type SnapshotOf<T> = RedemptionStepSnapshot<BalanceOf<T>>;

	/// One priced step: the vault-facing settlement plus the loop-facing outcome it implies.
	type StepDecision<T> =
		(Option<RedemptionSettlement<StableCreditOf<T>, BalanceOf<T>>>, StepOutcome<BalanceOf<T>>);

	/// The fixed inputs one redemption walk threads through every step.
	pub(crate) struct WalkContext<'a, T: Config> {
		pub(crate) redeemer: &'a T::AccountId,
		pub(crate) collateral_id: &'a CollateralIdOf<T>,
		pub(crate) stable_id: &'a StableIdOf<T>,
		pub(crate) recipient: &'a T::AccountId,
		pub(crate) price: FixedU128,
		pub(crate) config: &'a RedemptionConfigOf<T>,
	}

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

		/// Authorizes [`Pallet::set_redemption_config`] for the stablecoin given
		/// as argument. One config governs every collateral market issuing that
		/// coin, so this must be the coin's own authority (e.g. whatever vaults
		/// accepts as its `CreateOrigin`) rather than any single market's admin.
		type UpdateOrigin: EnsureOriginWithArg<Self::RuntimeOrigin, StableIdOf<Self>>;

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

	/// Redemption parameters keyed by stable asset.
	///
	/// One config governs every collateral market issuing the coin: the fee
	/// nudges how much of the coin is redeemed, whichever collateral backs it.
	/// A row exists exactly while at least one Vaults market issues it.
	#[pallet::storage]
	pub type RedemptionConfigs<T: Config> =
		StorageMap<_, Twox64Concat, StableIdOf<T>, RedemptionConfigOf<T>, OptionQuery>;

	/// Dynamic redemption fee state keyed by stable asset. Shared with
	/// [`RedemptionConfigs`], so redeeming against one collateral raises the fee
	/// on every other collateral issuing the same coin.
	#[pallet::storage]
	pub type RedemptionStates<T: Config> =
		StorageMap<_, Twox64Concat, StableIdOf<T>, RedemptionState, ValueQuery>;

	/// Number of registered Vaults markets issuing each stable asset.
	///
	/// [`RedemptionConfigs`] is seeded when this reaches one and removed when it
	/// falls back to zero, so a market leaving never wipes fee state its
	/// siblings are still using.
	#[pallet::storage]
	pub type MarketCounts<T: Config> = StorageMap<_, Twox64Concat, StableIdOf<T>, u32, ValueQuery>;

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
		/// The stablecoin's dynamic fee moved after an ordinary redemption.
		RedemptionDynamicFeeUpdated {
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Dynamic fee before the redemption.
			old_dynamic_fee: FixedU128,
			/// Dynamic fee after the redemption.
			new_dynamic_fee: FixedU128,
		},
		/// Governance replaced a stablecoin's redemption config.
		RedemptionConfigUpdated {
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// `max_stable_in` is below the branch `minimum_redemption_amount`.
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
		/// Quotes the market-side result of cancelling up to `max_stable_in` of debt.
		///
		/// `max_stable_in` and the fee have the same meaning as in
		/// [`Pallet::redeem`]: the quote's `stable_in` is the debt it would
		/// cancel plus the fee that much redemption would raise the rate to.
		///
		/// `max_steps` has the same meaning as in [`Pallet::redeem`]: zero uses
		/// [`Config::MaxRedemptionSteps`]. The quote projects pending vault
		/// updates without applying them and does not inspect a redeemer's
		/// wallet, so execution against a wallet that cannot cover
		/// `stable_in` fills less. Use `min_collateral_out` on execution to
		/// protect against state changes after the quote.
		pub fn preview_redeem(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			max_stable_in: BalanceOf<T>,
			max_steps: u32,
		) -> Option<RedemptionQuoteOf<T>> {
			Self::quote_redeem(&collateral_id, &stable_id, max_stable_in, max_steps)
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Cancels up to `max_stable_in` of vault debt, paying the redeemer collateral for it.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the redeemer.
		///
		/// `max_stable_in` is the debt the redeemer is willing to cancel, **not** its total spend:
		/// the redemption fee is charged on top, so the redeemer needs `max_stable_in` plus the fee
		/// and the walk is bounded by what its balance covers at both.
		///
		/// The fee is charged once for the whole redemption, at the rate this redemption itself
		/// raises the dynamic accelerator to — a large redemption after a quiet period pays the
		/// rate it causes, not the decayed one it arrived at.
		///
		/// Redemption targets are visited from the cheapest borrow rate upward. `max_steps` caps
		/// how many vaults the walk may touch; zero uses [`Config::MaxRedemptionSteps`]. Weight is
		/// charged for the cap and refunded to the number of steps actually taken.
		///
		/// `min_collateral_out` is the redeemer's slippage floor. Partial fills scale it pro-rata
		/// to the debt actually cancelled.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::redeem(Pallet::<T>::effective_step_cap(*max_steps)))]
		pub fn redeem(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			max_stable_in: BalanceOf<T>,
			min_collateral_out: BalanceOf<T>,
			recipient: T::AccountId,
			max_steps: u32,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let steps = Self::do_redeem(
				&who,
				&collateral_id,
				&stable_id,
				max_stable_in,
				min_collateral_out,
				&recipient,
				max_steps,
			)?;
			Ok(Some(T::WeightInfo::redeem(steps)).into())
		}

		/// Replaces a stablecoin's redemption configuration, for every collateral
		/// market issuing it.
		///
		/// ## Dispatch Origin
		///
		/// Must satisfy [`Config::UpdateOrigin`] for the stablecoin.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::set_redemption_config())]
		pub fn set_redemption_config(
			origin: OriginFor<T>,
			stable_id: StableIdOf<T>,
			config: RedemptionConfigOf<T>,
		) -> DispatchResult {
			T::UpdateOrigin::ensure_origin(origin, &stable_id)?;
			ensure!(RedemptionConfigs::<T>::contains_key(&stable_id), Error::<T>::InvalidBranch);
			ensure!(config.is_valid(), Error::<T>::InvalidRedemptionConfig);
			RedemptionConfigs::<T>::insert(&stable_id, config);
			Self::deposit_event(Event::RedemptionConfigUpdated { stable_id });
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
		/// quoting, so a new precondition or a fee-formula change cannot land in
		/// one path without also reaching the other.
		fn redemption_preamble(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			max_stable_in: BalanceOf<T>,
		) -> Result<RedemptionPreamble<BalanceOf<T>>, Error<T>> {
			let config = RedemptionConfigs::<T>::get(stable_id).ok_or(Error::<T>::InvalidBranch)?;
			ensure!(
				max_stable_in >= config.minimum_redemption_amount,
				Error::<T>::BelowMinimumRedemptionAmount
			);
			let price = T::Oracle::provide_price(collateral_id)
				.map_err(|_| Error::<T>::OracleUnavailable)?;
			ensure!(!price.is_zero(), Error::<T>::OracleUnavailable);
			let now = T::TimeProvider::now();
			let state = RedemptionStates::<T>::get(stable_id);
			let decayed = Self::decayed_dynamic_fee(&state, &config, now);
			let stablecoin_debt = T::Vaults::stablecoin_debt(stable_id);
			Ok(RedemptionPreamble { config, state, price, now, decayed, stablecoin_debt })
		}

		/// The rate charged on a redemption that cancels `redeemed` debt: the
		/// accelerator is raised first, and the redemption then pays the rate it
		/// caused. Shared by the funding bound, the charge, and the quote so no
		/// two of them can price differently.
		fn charged_fee_rate(
			preamble: &RedemptionPreamble<BalanceOf<T>>,
			redeemed: BalanceOf<T>,
		) -> FixedU128 {
			let raised = Self::raised_dynamic_fee(preamble, redeemed);
			fees::fee_rate(raised, preamble.config.base_fee, preamble.config.fee_ceiling)
		}

		fn do_redeem(
			redeemer: &T::AccountId,
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			max_stable_in: BalanceOf<T>,
			min_collateral_out: BalanceOf<T>,
			recipient: &T::AccountId,
			max_steps: u32,
		) -> Result<u32, DispatchError> {
			let preamble = Self::redemption_preamble(collateral_id, stable_id, max_stable_in)?;
			let step_cap = Self::effective_step_cap(max_steps);
			let ctx = WalkContext {
				redeemer,
				collateral_id,
				stable_id,
				recipient,
				price: preamble.price,
				config: &preamble.config,
			};

			let first_target = T::Vaults::next_redemption_target(collateral_id, stable_id, None);
			ensure!(first_target.is_some(), Error::<T>::NoRedeemableVault);
			let recovery_first =
				matches!(&first_target, Some((_, status)) if status.is_final_recovery());
			let spendable = Self::spendable_stable(stable_id, redeemer);
			let debt_budget = if recovery_first {
				// Recovery is fee-free and ends the walk, so every spendable
				// unit may cancel debt.
				max_stable_in.min(spendable)
			} else {
				Self::ordinary_debt_budget(&preamble, max_stable_in, spendable)
			};
			// A wallet that cannot fund the market's own minimum, fee included,
			// has no redemption to make. Saying so beats bounding the walk to
			// nothing and reporting it as an empty queue.
			if !recovery_first {
				ensure!(
					debt_budget >= preamble.config.minimum_redemption_amount,
					Error::<T>::InsufficientPusdBalance
				);
			}

			let mut acc = Accumulators::default();
			let walk = Self::run_loop(&ctx, step_cap, debt_budget, first_target, &mut acc)?;

			ensure!(!acc.debt_settled().is_zero(), Error::<T>::NoRedeemableVault);

			let fee_rate = Self::charged_fee_rate(&preamble, acc.ordinary_debt);
			let fee = fees::fee_pusd(acc.ordinary_debt, fee_rate);
			Self::charge_fee(stable_id, redeemer, fee)?;

			// The caller's floor was quoted against the debt it asked to cancel,
			// so a partial fill scales it by the debt actually cancelled.
			let redeemed = debt_budget.saturating_sub(walk.remaining);
			let scaled_min = fees::scale_floor(min_collateral_out, redeemed, max_stable_in);
			ensure!(acc.collateral_out() >= scaled_min, Error::<T>::SlippageExceeded);

			Self::finalize(&ctx, &preamble, &acc, walk.steps, fee);
			Ok(walk.steps)
		}

		/// Most debt the wallet can cancel while still paying the post-increase
		/// fee that same amount induces. `max_stable_in` caps debt, not the fee
		/// paid on top.
		fn ordinary_debt_budget(
			preamble: &RedemptionPreamble<BalanceOf<T>>,
			max_stable_in: BalanceOf<T>,
			spendable: BalanceOf<T>,
		) -> BalanceOf<T> {
			let max_debt = max_stable_in.min(preamble.stablecoin_debt);
			fees::max_debt_for_budget(spendable, max_debt, |debt| {
				fees::fee_pusd(debt, Self::charged_fee_rate(preamble, debt))
			})
		}

		/// Everything the redeemer could spend on this redemption, fee included.
		fn spendable_stable(stable_id: &StableIdOf<T>, redeemer: &T::AccountId) -> BalanceOf<T> {
			let (spendable, _) = reducible_debit::<T::StableAssets, _>(
				stable_id.clone(),
				redeemer,
				BalanceOf::<T>::max_value(),
			);
			spendable
		}

		/// Take the whole redemption's fee in one debit and route it to the fee
		/// handler.
		///
		/// The budget reserved room for this at a rate at or above the one
		/// charged, so the balance is there; what it cannot rule out is the fee
		/// landing in the minimum-balance dead zone, where an exact debit would
		/// strand a sub-minimum remainder. That case reverts the whole redemption
		/// rather than mangling the amount, and the redeemer retries with a
		/// smaller `max_stable_in`.
		fn charge_fee(
			stable_id: &StableIdOf<T>,
			redeemer: &T::AccountId,
			fee: BalanceOf<T>,
		) -> DispatchResult {
			if fee.is_zero() {
				return Ok(());
			}
			let (funded, preservation) =
				reducible_debit::<T::StableAssets, _>(stable_id.clone(), redeemer, fee);
			ensure!(funded >= fee, Error::<T>::InsufficientPusdBalance);
			let credit = <T::StableAssets as FungiblesBalanced<_>>::withdraw(
				stable_id.clone(),
				redeemer,
				fee,
				Precision::Exact,
				preservation,
				Fortitude::Polite,
			)?;
			debug_assert_eq!(credit.peek(), fee);
			T::FeeHandler::on_unbalanced(credit);
			Ok(())
		}

		/// Classify a prepared target so the barrier/redeemability ladder is defined
		/// once and cannot drift between execution and quoting.
		fn classify(snap: &SnapshotOf<T>, price: FixedU128) -> StepAction {
			if snap.status.is_final_recovery() {
				return StepAction::Recovery;
			}
			let redeemable = matches!(
				pusd_primitives::collateralization_ratio(&snap.position(), price),
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
		/// step per target.
		///
		/// A skipped underwater target advances the carried cursor. A successful
		/// redeem keeps that cursor because a drained vault leaves the index
		/// (the next lookup advances without bypassing any newly created
		/// Dormant/FinalRecovery barrier), while a partial fill must be found
		/// again. Priority targets always preempt the cursor inside
		/// [`VaultInterface::next_redemption_target`].
		pub(crate) fn run_loop(
			ctx: &WalkContext<'_, T>,
			step_cap: u32,
			debt_budget: BalanceOf<T>,
			first_target: Option<(T::AccountId, pusd_primitives::VaultStatus)>,
			acc: &mut Accumulators<BalanceOf<T>, T::AccountId>,
		) -> Result<WalkResult<BalanceOf<T>>, DispatchError> {
			let mut remaining = debt_budget;
			let mut steps = 0u32;
			let mut cursor: Option<T::AccountId> = None;
			let mut next = first_target;
			loop {
				if steps >= step_cap {
					break;
				}
				let target = next.take().or_else(|| {
					T::Vaults::next_redemption_target(
						ctx.collateral_id,
						ctx.stable_id,
						cursor.as_ref(),
					)
				});
				let Some((owner, status)) = target else {
					break;
				};
				// A zero-budget recovery can still settle a fully insured
				// residual. Ordinary targets cannot make progress without debt.
				if remaining.is_zero() && !status.is_final_recovery() {
					break;
				}
				let mut outcome = StepOutcome::Stopped;
				T::Vaults::redeem_step(
					ctx.collateral_id,
					ctx.stable_id,
					&owner,
					ctx.recipient,
					|snap| {
						let (allocation, decision) = Self::execute_step(ctx, &snap, remaining)?;
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
						remaining = remaining.saturating_sub(step.debt);
						acc.apply_ordinary(&step);
					},
					StepOutcome::Recovery { step, settle_residual } => {
						// The residual settlement re-enters the vault pallet, so
						// it must run after the in-flight step committed.
						let residual = if settle_residual {
							Self::settle_residual_via_if(ctx.collateral_id, ctx.stable_id, &owner)?
						} else {
							Zero::zero()
						};
						remaining = remaining.saturating_sub(step.burned);
						acc.apply_recovery(owner, &step, residual);
						break;
					},
				}
			}
			Ok(WalkResult { remaining, steps })
		}

		/// Classify and price one step without funding or moving anything.
		/// Shared by execution ([`Self::execute_step`]) and quoting
		/// ([`Self::quote_redeem`]) so the classify→price ladder — including the
		/// "zero-sized step with nothing to settle" stop — cannot drift between
		/// them.
		fn price_step(
			stable_id: &StableIdOf<T>,
			snap: &SnapshotOf<T>,
			price: FixedU128,
			budget: BalanceOf<T>,
			config: &RedemptionConfigOf<T>,
		) -> PricedStep<BalanceOf<T>> {
			match Self::classify(snap, price) {
				StepAction::Stop => PricedStep::Stop,
				StepAction::Skip => PricedStep::Skip,
				StepAction::Redeem => match Self::price_ordinary(snap, price, budget) {
					Some(step) => PricedStep::Redeem(step),
					None => PricedStep::Stop,
				},
				StepAction::Recovery => {
					let Some(pricing) =
						Self::price_recovery(stable_id, snap, price, budget, config)
					else {
						return PricedStep::Stop;
					};
					if pricing.debt().is_zero() && !pricing.settles_residual() {
						return PricedStep::Stop;
					}
					PricedStep::Recovery(pricing)
				},
			}
		}

		/// Fund one priced step from inside the vault-side `redeem_step`
		/// closure. Returns the loop outcome plus the settlement for the vault;
		/// a `None` settlement persists the touch without redeeming.
		fn execute_step(
			ctx: &WalkContext<'_, T>,
			snap: &SnapshotOf<T>,
			budget: BalanceOf<T>,
		) -> Result<StepDecision<T>, DispatchError> {
			match Self::price_step(ctx.stable_id, snap, ctx.price, budget, ctx.config) {
				PricedStep::Stop => Ok((None, StepOutcome::Stopped)),
				PricedStep::Skip => Ok((None, StepOutcome::Skipped)),
				PricedStep::Redeem(mut step) => {
					let (funded, preservation) =
						Self::fundable_budget(ctx.stable_id, ctx.redeemer, step.debt)?;
					if funded < step.debt {
						// Reprice once at the preserving limit; pricing keeps
						// the new debt at or below the budget it is given.
						let Some(repriced) = Self::price_ordinary(snap, ctx.price, funded) else {
							return Ok((None, StepOutcome::Stopped));
						};
						step = repriced;
					}
					debug_assert!(step.debt <= funded);
					let debt_payment = Self::fund_redemption(
						ctx.stable_id,
						ctx.redeemer,
						step.debt,
						preservation,
					)?;
					let settlement = RedemptionSettlement {
						debt_payment,
						collateral_to_recipient: step.collateral_out,
					};
					Ok((Some(settlement), StepOutcome::Redeemed(step)))
				},
				PricedStep::Recovery(pricing) => Self::recovery_decision(ctx, snap, pricing),
			}
		}

		/// Shared by execution and quoting to keep ordinary pricing identical
		/// (mirrors [`Self::price_recovery`]).
		/// `budget` is debt the step may cancel, not stable the redeemer may
		/// spend: the fee rides on top and is charged once for the whole walk.
		fn price_ordinary(
			snap: &SnapshotOf<T>,
			price: FixedU128,
			budget: BalanceOf<T>,
		) -> Option<OrdinaryStep<BalanceOf<T>>> {
			let debt = snap.debt.min(budget);
			if debt.is_zero() {
				return None;
			}
			let collateral_out =
				recovery_pricing::collateral_for_value(debt, price).min(snap.collateral);
			Some(OrdinaryStep { debt, collateral_out })
		}

		/// Shared by execution and quoting to keep recovery pricing identical.
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
			let cr = pusd_primitives::collateralization_ratio(&snap.position(), price)?;
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
		fn fundable_budget(
			stable_id: &StableIdOf<T>,
			redeemer: &T::AccountId,
			need: BalanceOf<T>,
		) -> Result<(BalanceOf<T>, Preservation), Error<T>> {
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

		/// Fund a priced `FinalRecovery` step, repricing once if the redeemer
		/// cannot cover it in full.
		fn recovery_decision(
			ctx: &WalkContext<'_, T>,
			snap: &SnapshotOf<T>,
			mut pricing: RecoveryPricing<BalanceOf<T>>,
		) -> Result<StepDecision<T>, DispatchError> {
			let preservation = if pricing.debt().is_zero() {
				None
			} else {
				let (funded, preservation) =
					Self::fundable_budget(ctx.stable_id, ctx.redeemer, pricing.debt())?;
				if funded < pricing.debt() {
					pricing = pricing.rebudget(snap, ctx.price, funded);
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
					Some(Self::fund_redemption(ctx.stable_id, ctx.redeemer, debt, preservation)?)
				},
				None => None,
			};
			let step = RecoveryStep {
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

		/// Withdraw one step's debt payment from the redeemer. The fee is not
		/// taken here: it is a function of the whole walk's redeemed debt, so
		/// [`Self::charge_fee`] takes it once at the end.
		fn fund_redemption(
			stable_id: &StableIdOf<T>,
			redeemer: &T::AccountId,
			debt: BalanceOf<T>,
			preservation: Preservation,
		) -> Result<StableCreditOf<T>, DispatchError> {
			let credit = <T::StableAssets as FungiblesBalanced<_>>::withdraw(
				stable_id.clone(),
				redeemer,
				debt,
				Precision::Exact,
				preservation,
				Fortitude::Polite,
			)?;
			debug_assert_eq!(credit.peek(), debt);
			Ok(credit)
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

		/// Post-loop settlement: emits both redemption events and updates the
		/// dynamic fee from the walk's outcome.
		fn finalize(
			ctx: &WalkContext<'_, T>,
			preamble: &RedemptionPreamble<BalanceOf<T>>,
			acc: &Accumulators<BalanceOf<T>, T::AccountId>,
			steps: u32,
			fee: BalanceOf<T>,
		) {
			if !acc.ordinary_debt.is_zero() {
				let new_fee = Self::raised_dynamic_fee(preamble, acc.ordinary_debt);
				RedemptionStates::<T>::insert(
					ctx.stable_id,
					RedemptionState { dynamic_fee: new_fee, last_fee_operation: preamble.now },
				);
				if new_fee != preamble.state.dynamic_fee {
					Self::deposit_event(Event::RedemptionDynamicFeeUpdated {
						stable_id: ctx.stable_id.clone(),
						old_dynamic_fee: preamble.state.dynamic_fee,
						new_dynamic_fee: new_fee,
					});
				}
				Self::deposit_event(Event::OrdinaryRedemptionExecuted {
					collateral_id: ctx.collateral_id.clone(),
					stable_id: ctx.stable_id.clone(),
					redeemer: ctx.redeemer.clone(),
					recipient: ctx.recipient.clone(),
					pusd_burned: acc.ordinary_debt,
					collateral_out: acc.ordinary_collateral,
					fee_pusd: fee,
					steps,
				});
			}
			if let Some(recovery) = &acc.recovery {
				Self::deposit_event(Event::RecoveryRedemptionExecuted {
					collateral_id: ctx.collateral_id.clone(),
					stable_id: ctx.stable_id.clone(),
					redeemer: ctx.redeemer.clone(),
					recipient: ctx.recipient.clone(),
					vault_owner: recovery.owner.clone(),
					pusd_burned: recovery.burned,
					collateral_out: recovery.collateral_out,
					regime: recovery.regime,
				});
			}
		}

		/// The dynamic fee after `redeemed` debt is cancelled against the
		/// preamble's stablecoin-wide debt. Monotonic in `redeemed`, which is
		/// what lets the walk bound its funding with the rate for the largest
		/// redemption it could make.
		fn raised_dynamic_fee(
			preamble: &RedemptionPreamble<BalanceOf<T>>,
			redeemed: BalanceOf<T>,
		) -> FixedU128 {
			// Redeeming the whole coin's debt (or a stale-zero denominator)
			// saturates the fraction rather than dividing by zero.
			let fraction = FixedU128::checked_from_rational(redeemed, preamble.stablecoin_debt)
				.unwrap_or_else(FixedU128::one);
			fees::increased_dynamic_fee(
				preamble.decayed,
				fraction,
				preamble.config.dynamic_fee_increase_divisor,
				preamble.config.dynamic_fee_floor,
				preamble.config.dynamic_fee_ceiling,
			)
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

		/// Build a read-only market quote from projected post-touch snapshots.
		fn quote_redeem(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			max_stable_in: BalanceOf<T>,
			max_steps: u32,
		) -> Option<RedemptionQuoteOf<T>> {
			let preamble =
				Self::redemption_preamble(collateral_id, stable_id, max_stable_in).ok()?;
			let step_cap = Self::effective_step_cap(max_steps);
			let (mut quote, ordinary_debt, recovered) =
				Self::quote_walk(collateral_id, stable_id, &preamble, max_stable_in, step_cap)?;

			// Same two-phase pricing as execution: the walk cancels debt, then the
			// fee is charged once at the rate that much redemption raises.
			quote.fee =
				fees::fee_pusd(ordinary_debt, Self::charged_fee_rate(&preamble, ordinary_debt));
			quote.stable_in = quote.stable_in.saturating_add(quote.fee);

			(recovered || !quote.stable_in.is_zero()).then_some(quote)
		}

		/// The quote's projection walk: price targets until the budget, the step
		/// cap, or a barrier stops it. Returns the accumulated quote, the
		/// ordinary (fee-bearing) debt, and whether a recovery head settled;
		/// `None` when a target cannot be projected.
		fn quote_walk(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			preamble: &RedemptionPreamble<BalanceOf<T>>,
			max_stable_in: BalanceOf<T>,
			step_cap: u32,
		) -> Option<(RedemptionQuoteOf<T>, BalanceOf<T>, bool)> {
			// Debt cancelled by ordinary steps, which is what the fee prices; a
			// recovery head settles fee-free.
			let mut ordinary_debt: BalanceOf<T> = Zero::zero();
			// A fully-covered recovery head is a real quote at zero `stable_in`.
			let mut recovered = false;
			let mut quote = RedemptionQuoteOf::<T>::default();
			let mut targets = T::Vaults::redemption_quote_targets(collateral_id, stable_id);
			let mut carried: Option<(T::AccountId, SnapshotOf<T>)> = None;

			loop {
				let remaining = max_stable_in.saturating_sub(quote.stable_in);
				if remaining.is_zero() {
					break;
				}
				if quote.steps >= step_cap {
					quote.truncated = true;
					break;
				}

				let (owner, snap) = match carried.take() {
					Some(revisit) => revisit,
					None => {
						let Some(owner) = targets.next() else {
							break;
						};
						let snap = T::Vaults::project_redemption_snapshot(
							collateral_id,
							stable_id,
							&owner,
						)
						.ok()?;
						(owner, snap)
					},
				};
				quote.steps = quote.steps.saturating_add(1);

				match Self::price_step(
					stable_id,
					&snap,
					preamble.price,
					remaining,
					&preamble.config,
				) {
					PricedStep::Stop => break,
					PricedStep::Skip => {},
					PricedStep::Recovery(pricing) => {
						quote.stable_in = quote.stable_in.saturating_add(pricing.debt());
						quote.collateral_out =
							quote.collateral_out.saturating_add(pricing.collateral_out());
						// Quoted even at zero `stable_in`: a fully-covered head
						// still settles (the Insurance Fund pays the residual),
						// so execution succeeds costing and paying the redeemer
						// nothing. A recovery head always ends the walk.
						recovered = true;
						break;
					},
					PricedStep::Redeem(step) => {
						ordinary_debt = ordinary_debt.saturating_add(step.debt);
						carried = Self::quote_ordinary_step(&mut quote, owner, &snap, &step);
					},
				}
			}

			Some((quote, ordinary_debt, recovered))
		}

		/// Fold one priced ordinary step into the quote. Returns the carried
		/// revisit target when the step leaves vault debt behind: a partial fill
		/// must be priced again against the reduced snapshot before the walk
		/// moves on.
		fn quote_ordinary_step(
			quote: &mut RedemptionQuoteOf<T>,
			owner: T::AccountId,
			snap: &SnapshotOf<T>,
			step: &OrdinaryStep<BalanceOf<T>>,
		) -> Option<(T::AccountId, SnapshotOf<T>)> {
			quote.stable_in = quote.stable_in.saturating_add(step.debt);
			quote.collateral_out = quote.collateral_out.saturating_add(step.collateral_out);
			let debt_left = snap.debt.saturating_sub(step.debt);
			if debt_left.is_zero() {
				return None;
			}
			Some((
				owner,
				RedemptionStepSnapshot {
					status: snap.status,
					debt: debt_left,
					collateral: snap.collateral.saturating_sub(step.collateral_out),
					redistribution_penalty: snap.redistribution_penalty,
				},
			))
		}
	}

	impl<T: Config> Pallet<T> {
		/// Config + oracle price for the recovery-offset paths. No fee state:
		/// offsets are fee-free and leave the dynamic fee untouched.
		fn offset_preamble(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
		) -> Result<(RedemptionConfigOf<T>, FixedU128), DispatchError> {
			let config = RedemptionConfigs::<T>::get(stable_id).ok_or(Error::<T>::InvalidBranch)?;
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

		/// Quote the head's capacity for up to `max_debt_to_cancel`, sized
		/// against the projected FIFO head snapshot. A read-only view surface,
		/// not part of the [`RecoveryOffsetInterface`] execution seam. Prices
		/// through [`Self::price_recovery`], the same function that prices
		/// recovery redemptions, so the two can never diverge.
		pub fn preview_recovery_offset(
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
			let snap = T::Vaults::project_redemption_snapshot(collateral_id, stable_id, &owner)?;
			let pricing =
				Self::price_recovery(stable_id, &snap, price, max_debt_to_cancel, &config);
			Ok(match Self::classify_offset(pricing) {
				OffsetDecision::NoTarget => RecoveryOffsetQuote::NoTarget,
				OffsetDecision::BelowPar => RecoveryOffsetQuote::BelowPar,
				OffsetDecision::Cancellable { debt, .. } => RecoveryOffsetQuote::Available { debt },
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

	/// Config and fee state are per-stablecoin but markets are registered per
	/// `(collateral, stable)` pair, so both writes are refcounted: the first
	/// market issuing a coin seeds them, the last one to leave clears them, and
	/// everything in between leaves a live fee state alone.
	impl<T: Config> pusd_primitives::OnBranchLifecycle<CollateralIdOf<T>, StableIdOf<T>> for Pallet<T> {
		fn on_registered(_: &CollateralIdOf<T>, stable_id: &StableIdOf<T>) -> DispatchResult {
			MarketCounts::<T>::try_mutate(stable_id, |count| {
				// One increment per registered market, so a `u32` cannot wrap.
				*count = count.saturating_add(1);
				if *count > 1 {
					return Ok(());
				}
				let config = T::DefaultRedemptionConfig::get();
				ensure!(config.is_valid(), Error::<T>::InvalidRedemptionConfig);
				RedemptionConfigs::<T>::insert(stable_id, config);
				Ok(())
			})
		}

		fn on_deregistered(_: &CollateralIdOf<T>, stable_id: &StableIdOf<T>) -> DispatchResult {
			MarketCounts::<T>::mutate_exists(stable_id, |maybe| {
				// Vaults only deregisters markets it registered, so the count
				// cannot already be zero here.
				let count = maybe.get_or_insert_default();
				*count = count.saturating_sub(1);
				if !count.is_zero() {
					return;
				}
				maybe.take();
				RedemptionConfigs::<T>::remove(stable_id);
				RedemptionStates::<T>::remove(stable_id);
			});
			Ok(())
		}
	}
}
