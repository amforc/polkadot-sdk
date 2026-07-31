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
//! [`pusd_primitives::OnBranchLifecycle`].

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod fees;
mod recovery;
mod redemption;
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
	RedemptionTerms,
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
	use crate::types::{RedemptionConfig, RedemptionQuote, RedemptionState, RedemptionTerms};
	use frame::{
		deps::sp_runtime::{traits::Convert, FixedU128},
		prelude::*,
		traits::{
			fungibles::{self, Balanced as FungiblesBalanced},
			EnsureOriginWithArg, OnUnbalanced, Time,
		},
	};
	use pusd_primitives::{ProvidePrice, RedemptionStepSnapshot, VaultInterface};

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
			stable_burned: BalanceOf<T>,
			/// Collateral paid to the recipient.
			collateral_out: BalanceOf<T>,
			/// Stable-asset fee charged to the redeemer.
			fee: BalanceOf<T>,
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
			stable_burned: BalanceOf<T>,
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
		/// The redeemer cannot cover the stable asset the redemption requires.
		InsufficientStableBalance,
		/// No redemption config exists for this stablecoin.
		StablecoinNotRegistered,
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
		/// [`Pallet::redeem`]: [`RedemptionQuote::debt_cancelled`] is the
		/// requested debt dimension, while [`RedemptionQuote::stable_in`]
		/// adds the fee that much redemption would raise the rate to.
		///
		/// `max_steps` has the same meaning as in [`Pallet::redeem`]: zero uses
		/// [`Config::MaxRedemptionSteps`]. The quote projects pending vault
		/// updates without applying them and does not inspect a redeemer's
		/// wallet, so execution against a wallet that cannot cover
		/// `stable_in` fills less. Use `min_collateral_out` on execution to
		/// protect against state changes after the quote.
		///
		/// Validation, oracle, and vault-projection failures are returned to the
		/// caller. An empty or blocked queue returns [`Error::NoRedeemableVault`].
		pub fn preview_redeem(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			max_stable_in: BalanceOf<T>,
			max_steps: u32,
		) -> Result<RedemptionQuoteOf<T>, DispatchError> {
			Self::quote_redeem(&collateral_id, &stable_id, max_stable_in, max_steps)
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Cancels up to `terms.max_stable_in` of vault debt, paying the redeemer collateral for
		/// it.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the redeemer.
		///
		/// `terms.max_stable_in` is the debt the redeemer is willing to cancel, **not** its total
		/// spend: the redemption fee is charged on top, so the redeemer needs
		/// `terms.max_stable_in` plus the fee and the walk is bounded by what its balance covers at
		/// both.
		///
		/// The fee is charged once for the whole redemption, at the rate this redemption itself
		/// raises the dynamic accelerator to — a large redemption after a quiet period pays the
		/// rate it causes, not the decayed one it arrived at.
		///
		/// Redemption targets are visited from the cheapest borrow rate upward. `max_steps` caps
		/// how many vaults the walk may touch; zero uses [`Config::MaxRedemptionSteps`]. Weight is
		/// charged for the cap and refunded to the number of steps actually taken.
		///
		/// `terms.min_collateral_out` is the redeemer's slippage floor. Partial fills scale it
		/// pro-rata to the debt actually cancelled.
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
			RedemptionConfigs::<T>::try_mutate(&stable_id, |registered| {
				let stored = registered.as_mut().ok_or(Error::<T>::StablecoinNotRegistered)?;
				ensure!(config.is_valid(), Error::<T>::InvalidRedemptionConfig);
				*stored = config;
				Ok::<_, DispatchError>(())
			})?;
			Self::deposit_event(Event::RedemptionConfigUpdated { stable_id });
			Ok(())
		}
	}

	/// Vaults supplies the authoritative stablecoin market count: the first
	/// market seeds these stablecoin-wide rows and the last one clears them.
	impl<T: Config> pusd_primitives::OnBranchLifecycle<CollateralIdOf<T>, StableIdOf<T>> for Pallet<T> {
		fn on_registered(
			_: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			stablecoin_markets: u32,
		) -> DispatchResult {
			ensure!(stablecoin_markets > 0, DispatchError::Corruption);
			if stablecoin_markets > 1 {
				ensure!(RedemptionConfigs::<T>::contains_key(stable_id), DispatchError::Corruption);
				return Ok(());
			}
			ensure!(!RedemptionConfigs::<T>::contains_key(stable_id), DispatchError::Corruption);
			let config = T::DefaultRedemptionConfig::get();
			ensure!(config.is_valid(), Error::<T>::InvalidRedemptionConfig);
			RedemptionConfigs::<T>::insert(stable_id, config);
			Ok(())
		}

		fn on_deregistered(
			_: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			remaining_stablecoin_markets: u32,
		) -> DispatchResult {
			ensure!(RedemptionConfigs::<T>::contains_key(stable_id), DispatchError::Corruption);
			if remaining_stablecoin_markets == 0 {
				RedemptionConfigs::<T>::remove(stable_id);
				RedemptionStates::<T>::remove(stable_id);
			}
			Ok(())
		}
	}
}
