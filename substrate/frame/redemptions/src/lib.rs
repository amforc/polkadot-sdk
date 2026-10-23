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
//! Redemptions are executed against the ordering and vault state owned by the vault engine
//! ([`Config::Vaults`]). Ordinary redemptions walk dormant and active vaults, charge a fee,
//! and burn the redeemed stable asset. Final-recovery vaults are settled first using recovery
//! pricing and insurance cover.
//!
//! Redemption configuration and dynamic fee state are stored per stablecoin, shared by every
//! collateral branch issuing it: the fee nudges how much of the coin is redeemed, whichever
//! collateral the redeemer used. Branch lifecycle is synchronized through
//! [`stablecoin_primitives::OnBranchLifecycle`].

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod fees;
mod recovery;
mod redemption;
pub mod types;
pub mod weights;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

// The mock checks these invariants after each run, so `test-utils` needs them as well.
#[cfg(any(feature = "try-runtime", feature = "test-utils", test))]
mod try_state;

#[cfg(any(feature = "test-utils", test))]
pub mod mock;

#[cfg(test)]
mod tests;

pub use pallet::*;
pub use types::{RecoveryRegime, RedemptionConfig, RedemptionInfo, RedemptionTerms};
pub use weights::WeightInfo;

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
	use frame::{
		deps::sp_runtime::{traits::Convert, FixedU128},
		prelude::*,
		traits::{
			fungibles::{self, Balanced as FungiblesBalanced},
			EnsureOriginWithArg, OnUnbalanced, Time,
		},
	};
	use stablecoin_primitives::{ProvidePrice, RedemptionStepSnapshot, VaultInterface};
	use types::RedemptionState;

	/// Balance type used by stable assets and vault accounting.
	pub type BalanceOf<T> =
		<<T as Config>::StableAssets as fungibles::Inspect<AccountIdFor<T>>>::Balance;

	/// Collateral identifier exposed by [`Config::Vaults`].
	pub type CollateralIdOf<T> = <<T as Config>::Vaults as VaultInterface>::CollateralId;

	/// Stable asset identifier exposed by [`Config::StableAssets`].
	pub type StableIdOf<T> =
		<<T as Config>::StableAssets as fungibles::Inspect<AccountIdFor<T>>>::AssetId;

	/// UNIX time in milliseconds.
	pub use stablecoin_primitives::Millis;

	/// Stable-asset credit consumed by the pallet.
	pub type StableCreditOf<T> = fungibles::Credit<AccountIdFor<T>, <T as Config>::StableAssets>;

	/// Per-stablecoin redemption configuration used by the runtime.
	pub type RedemptionConfigOf<T> = RedemptionConfig<BalanceOf<T>>;

	/// Per-stablecoin redemption config and dynamic fee state used by the runtime.
	pub type RedemptionInfoOf<T> = RedemptionInfo<BalanceOf<T>>;

	/// Vault snapshot used to price a redemption step.
	pub(crate) type SnapshotOf<T> = RedemptionStepSnapshot<BalanceOf<T>>;

	/// On-chain storage version of this pallet. No migration has run yet.
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

		/// Owns branch state, vault ordering, and atomic settlement.
		type Vaults: VaultInterface<
			CollateralId: Parameter + Member + Ord + MaxEncodedLen,
			StableId = StableIdOf<Self>,
			AccountId = Self::AccountId,
			Balance = BalanceOf<Self>,
			StableCredit = StableCreditOf<Self>,
		>;

		/// Maps each stablecoin to its Insurance Fund account.
		///
		/// Separate accounts prevent one stablecoin from covering another stablecoin's vault debt.
		type InsuranceFundAccount: Convert<StableIdOf<Self>, Self::AccountId>;

		/// Destination for redemption fees.
		type FeeHandler: OnUnbalanced<StableCreditOf<Self>>;

		/// Provides UNIX time in milliseconds.
		///
		/// Must be a clock collators cannot steer, such as the relay chain's timestamp, rather
		/// than the parachain's own `pallet_timestamp`. The dynamic fee decays with this clock,
		/// so a block author that sets its own time can fast-forward the decay and redeem below
		/// the fee the redemption volume should cost.
		type TimeProvider: Time<Moment = Millis>;

		/// Authorizes [`Pallet::set_redemption_config`] for the stablecoin given
		/// as argument. One config governs every collateral branch issuing that
		/// coin, so this must be the coin's own authority (e.g. the origin the
		/// vault engine accepts to create a branch on the coin) rather than any
		/// single branch's admin.
		type UpdateOrigin: EnsureOriginWithArg<Self::RuntimeOrigin, StableIdOf<Self>>;

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

	/// Redemption config and dynamic fee state keyed by stable asset.
	///
	/// One row governs every collateral branch issuing the coin: the fee nudges
	/// how much of the coin is redeemed, whichever collateral backs it, so
	/// redeeming against one collateral raises the fee on every other. A row
	/// exists exactly while at least one branch issues the coin.
	#[pallet::storage]
	pub type Stablecoins<T: Config> =
		StorageMap<_, Twox64Concat, StableIdOf<T>, RedemptionInfoOf<T>, OptionQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// An ordinary redemption settled against one or more ordinary (or dormant-target) vaults.
		///
		/// The vault engine reports each settled vault and the recipient. `stable_burned` and
		/// `collateral_out` total those reports, so a reader need not attribute them to this
		/// settlement. The redeemer paid `stable_burned` plus `fee`.
		OrdinaryRedemptionExecuted {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Account that paid the stable assets.
			redeemer: T::AccountId,
			/// Stable assets burned against vault debt.
			stable_burned: BalanceOf<T>,
			/// Collateral paid to the recipient.
			collateral_out: BalanceOf<T>,
			/// Stable-asset fee charged to the redeemer.
			fee: BalanceOf<T>,
			/// Number of vaults visited, including those that settled nothing.
			steps: u32,
		},
		/// A `FinalRecovery` vault was (partially) settled.
		///
		/// The vault engine reports the settled vault and the recipient. The debt it reports
		/// cancelled is `stable_burned` plus `insurance_cover`.
		RecoveryRedemptionExecuted {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Account that paid the stable assets.
			redeemer: T::AccountId,
			/// Stable assets the redeemer burned against vault debt.
			stable_burned: BalanceOf<T>,
			/// Insurance Fund cover burned against the same vault's debt, in
			/// the same settlement. Nonzero only for a full below-par fill.
			insurance_cover: BalanceOf<T>,
			/// Collateral paid to the recipient.
			collateral_out: BalanceOf<T>,
			/// Pricing regime applied to the settlement.
			regime: RecoveryRegime,
		},
		/// An ordinary redemption rewrote the stablecoin's dynamic fee state.
		RedemptionDynamicFeeUpdated {
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Dynamic fee the redemption started from, decayed to its block.
			old_dynamic_fee: FixedU128,
			/// Dynamic fee after the redemption.
			new_dynamic_fee: FixedU128,
		},
		/// Governance replaced a stablecoin's redemption config.
		RedemptionConfigUpdated {
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Config in force before the update.
			old_config: RedemptionConfigOf<T>,
			/// Config in force from now on.
			new_config: RedemptionConfigOf<T>,
		},
		/// The stablecoin's first branch seeded its redemption config and a zero dynamic fee.
		RedemptionConfigSet {
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Config the first branch supplied.
			config: RedemptionConfigOf<T>,
		},
		/// The stablecoin's last branch left, clearing its redemption config and fee state.
		RedemptionConfigCleared {
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// `max_stable_to_spend` cannot buy the stablecoin's `minimum_redemption_amount` of debt.
		BelowMinimumRedemptionAmount,
		/// No redeemable vault made any progress.
		NoRedeemableVault,
		/// The `FinalRecovery` head is above ICR: call `exit_final_recovery`, then redeem.
		FinalRecoveryExitRequired,
		/// Output collateral fell short of `min_collateral_out`.
		SlippageExceeded,
		/// The redeemer cannot cover the stable asset the redemption requires.
		InsufficientStableBalance,
		/// No redemption config exists for this stablecoin.
		StablecoinNotRegistered,
		/// The oracle returned no usable price.
		OracleUnavailable,
		/// Withdrawing the Insurance Fund cover for a below-par settlement failed.
		InsuranceFundWithdrawFailed,
		/// The supplied redemption config is internally inconsistent.
		InvalidRedemptionConfig,
		/// The stablecoin's first branch must supply a redemption config.
		RedemptionConfigRequired,
		/// Only a stablecoin's first branch supplies a redemption config.
		RedemptionConfigNotExpected,
		/// An internal invariant broke. The dispatch is refused and rolled back.
		Unexpected,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn integrity_test() {
			// A zero cap makes `effective_step_cap(0)` zero: the walk never
			// runs and every redeem fails with `NoRedeemableVault`.
			assert!(T::MaxRedemptionSteps::get() > 0, "`MaxRedemptionSteps` must be > 0");
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(_: BlockNumberFor<T>) -> Result<(), frame::try_runtime::TryRuntimeError> {
			crate::try_state::do_try_state::<T>()
		}
	}

	#[pallet::view_functions]
	impl<T: Config> Pallet<T> {
		/// Returns the fee rate at which an ordinary redemption of the stablecoin starts now.
		///
		/// The rate is `min(base_fee + dynamic fee, fee_ceiling)`, with the dynamic fee decayed
		/// to now and nothing persisted. A redemption raises the rate while it cancels debt, so
		/// every size pays at least this rate. A dry run of [`Pallet::redeem`] gives the result
		/// of a given size, and its event carries the amounts.
		///
		/// Returns `None` when no branch issues the stablecoin.
		pub fn current_fee_rate(stable_id: StableIdOf<T>) -> Option<FixedU128> {
			let RedemptionInfo { config, fee_state } = Stablecoins::<T>::get(&stable_id)?;
			let dynamic_fee = fee_state.dynamic_fee_at(T::TimeProvider::now(), &config);
			Some(fees::fee_rate(dynamic_fee, config.base_fee, config.fee_ceiling))
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Cancels vault debt and pays collateral to `recipient`.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the redeemer.
		///
		/// `terms.max_stable_to_spend` caps the total cost, fee included; the walk cancels the
		/// most debt whose sum with its fee fits it, and never more than the redeemer can pay
		/// while staying at or above the stablecoin's minimum balance. The fee is the integral
		/// of the fee rate over the cancelled debt (see [`RedemptionConfig`]).
		///
		/// `terms.min_collateral_out` is the slippage floor for the full budget, scaled pro rata
		/// on a partial fill. A fee rise between quote and execution can fail it like a price
		/// move.
		///
		/// Targets are visited from the cheapest borrow rate upward. `max_steps` caps the vaults
		/// touched; zero uses [`Config::MaxRedemptionSteps`]. Weight is charged for the cap and
		/// refunded to the steps taken.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::redeem(Pallet::<T>::effective_step_cap(*max_steps)))]
		pub fn redeem(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			terms: RedemptionTerms<BalanceOf<T>>,
			recipient: T::AccountId,
			max_steps: u32,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let steps =
				Self::do_redeem(&who, &collateral_id, &stable_id, terms, &recipient, max_steps)?;
			Ok(Some(T::WeightInfo::redeem(steps)).into())
		}

		/// Replaces a stablecoin's redemption configuration, for every collateral
		/// branch issuing it.
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
			let old_config = Stablecoins::<T>::try_mutate(&stable_id, |info| {
				let info = info.as_mut().ok_or(Error::<T>::StablecoinNotRegistered)?;
				ensure!(config.is_valid(), Error::<T>::InvalidRedemptionConfig);
				// The fee state stays: it is the decay anchor whichever bounds apply next.
				let old_config = info.config.clone();
				info.config = config.clone();
				Ok::<_, Error<T>>(old_config)
			})?;
			Self::deposit_event(Event::RedemptionConfigUpdated {
				stable_id,
				old_config,
				new_config: config,
			});
			Ok(())
		}
	}

	/// Whether the branch being registered is the one that seeds a stablecoin's
	/// row. The vault engine's counter includes the branch it is announcing, so
	/// the first one sees exactly one.
	///
	/// Every use of "first branch" resolves through here, so the rule the
	/// registration path enforces is the rule a caller builds its payload
	/// against.
	pub(crate) const fn is_first_market(stablecoin_markets: u32) -> bool {
		stablecoin_markets == 1
	}

	impl<T: Config> Pallet<T> {
		/// Reports a broken invariant: panics under debug assertions, logs otherwise, and returns
		/// the error that refuses the dispatch so the broken state is never built on.
		pub(crate) fn unexpected(proof: &'static str) -> Error<T> {
			defensive!(proof);
			Error::<T>::Unexpected
		}
	}

	/// The stablecoin's first branch seeds its [`Stablecoins`] row, and the
	/// last one to leave clears it.
	impl<T: Config>
		stablecoin_primitives::OnBranchLifecycle<CollateralIdOf<T>, StableIdOf<T>, T::AccountId>
		for Pallet<T>
	{
		/// One redemption policy governs every collateral branch issuing the
		/// coin, so only the first branch carries it. Later branches must pass
		/// `None` rather than restate a policy they do not own.
		type RegistrationConfig = Option<RedemptionConfigOf<T>>;

		fn on_registered(
			_: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			stablecoin_markets: u32,
			config: Self::RegistrationConfig,
			_funder: &T::AccountId,
		) -> DispatchResult {
			ensure!(
				stablecoin_markets > 0,
				Self::unexpected("the branch count includes the branch being announced")
			);
			let first_market = is_first_market(stablecoin_markets);
			// The engine's counter decides which branch seeds the policy; a stored policy is the
			// same fact recorded here, so the two disagreeing means one of them is corrupt.
			ensure!(
				Stablecoins::<T>::contains_key(stable_id) == !first_market,
				Self::unexpected("a stored redemption config disagrees with the branch count")
			);
			match (first_market, config) {
				(true, Some(config)) => {
					ensure!(config.is_valid(), Error::<T>::InvalidRedemptionConfig);
					let info = RedemptionInfo { config, fee_state: RedemptionState::default() };
					Stablecoins::<T>::insert(stable_id, &info);
					Self::deposit_event(Event::RedemptionConfigSet {
						stable_id: stable_id.clone(),
						config: info.config,
					});
					Ok(())
				},
				(false, None) => Ok(()),
				(true, None) => Err(Error::<T>::RedemptionConfigRequired.into()),
				(false, Some(_)) => Err(Error::<T>::RedemptionConfigNotExpected.into()),
			}
		}

		fn on_deregistered(
			_: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			remaining_stablecoin_markets: u32,
			_: &T::AccountId,
		) -> DispatchResult {
			ensure!(
				Stablecoins::<T>::contains_key(stable_id),
				Self::unexpected("a registered branch's stablecoin has no redemption config")
			);
			if remaining_stablecoin_markets == 0 {
				Stablecoins::<T>::remove(stable_id);
				Self::deposit_event(Event::RedemptionConfigCleared {
					stable_id: stable_id.clone(),
				});
			}
			Ok(())
		}

		#[cfg(feature = "runtime-benchmarks")]
		fn benchmark_registration_config(stablecoin_markets: u32) -> Self::RegistrationConfig {
			is_first_market(stablecoin_markets).then(crate::benchmarking::registration_config::<T>)
		}
	}
}
