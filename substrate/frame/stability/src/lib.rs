//! # Stability Pool Pallet

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod types;
pub mod weights;

mod dispatchable_impls;
mod interfaces;
mod math;
mod pending;
#[cfg(feature = "try-runtime")]
mod try_state;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub use pallet::*;
pub use weights::WeightInfo;

#[frame::pallet]
pub mod pallet {
	use super::*;
	use crate::{
		dispatchable_impls::ClaimKind,
		types::{Deposit, PoolState, PoolSums, RecoveryOffsetSource, StabilityPoolConfig},
	};
	use frame::{
		deps::frame_support::{
			storage::with_storage_layer,
			traits::{fungibles, EnsureOriginWithArg},
			PalletId,
		},
		prelude::*,
	};
	use pallet_linked_list::SortedListInterface;
	use pusd_primitives::{
		BranchModeProvider, OnBranchLifecycle, RecoveryOffsetInterface, StableListId,
	};

	pub type BalanceOf<T> = <<T as Config>::StableAssets as fungibles::Inspect<
		<T as frame_system::Config>::AccountId,
	>>::Balance;

	pub type StableCreditOf<T> =
		fungibles::Credit<<T as frame_system::Config>::AccountId, <T as Config>::StableAssets>;

	pub type DepositOf<T> = Deposit<BalanceOf<T>, BlockNumberFor<T>>;

	pub type PoolStateOf<T> = PoolState<BalanceOf<T>>;

	pub type StabilityPoolConfigOf<T> = StabilityPoolConfig<BalanceOf<T>, BlockNumberFor<T>>;

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

		/// The stablecoin a market mints; the pool holds and burns it.
		type StableAssetId: Parameter + Member + Ord + MaxEncodedLen;

		/// Product-sum math needs a fungibles balance type that can enter
		/// fixed-point calculations without lossy adapters.
		type StableAssets: fungibles::Inspect<
				Self::AccountId,
				AssetId = Self::StableAssetId,
				Balance: FixedPointOperand,
			> + fungibles::Mutate<Self::AccountId>
			+ fungibles::Balanced<Self::AccountId>;

		/// Collateral surface the pool receives offset gains on and pays
		/// depositor claims from.
		type CollateralAssets: fungibles::Inspect<
				Self::AccountId,
				AssetId = Self::CollateralAssetId,
				Balance = BalanceOf<Self>,
			> + fungibles::Mutate<Self::AccountId>;

		/// Branch operating-mode source of truth (point it at the vault
		/// pallet). Frozen branches reject every value-moving pool operation
		/// (SPEC.md §8.1); Safety Mode turns withdrawals two-step.
		type BranchModes: BranchModeProvider<Self::CollateralAssetId, Self::StableAssetId>;

		/// Shared `FinalRecovery` settlement pricing and execution (point it
		/// at the redemptions pallet, which owns that pricing) — recovery
		/// offsets can never diverge from recovery-redemption pricing
		/// (SPEC.md §12 invariant 9).
		type RecoveryOffsets: RecoveryOffsetInterface<
			CollateralId = Self::CollateralAssetId,
			StableId = Self::StableAssetId,
			AccountId = Self::AccountId,
			Balance = BalanceOf<Self>,
		>;

		/// Backing store for the per-branch pending-deposit FIFO: the
		/// runtime's shared `pallet-linked-list` instance, addressed through
		/// the `StableListId::StabilityPending` variant.
		type PendingLists: SortedListInterface<
			StableListId<Self::CollateralAssetId, Self::StableAssetId>,
			Self::AccountId,
			Priority = FixedU128,
		>;

		/// Authorizes [`Pallet::set_stability_pool_config`] for the market
		/// given as argument. Point this at the market's admin authority
		/// (e.g. vaults' `EnsureBranchFullAdmin`) and compose a governance
		/// override with `EitherOf`.
		type UpdateOrigin: EnsureOriginWithArg<
			Self::RuntimeOrigin,
			(Self::CollateralAssetId, Self::StableAssetId),
			Success = (),
		>;

		/// Config every newly registered branch starts with.
		type DefaultStabilityPoolConfig: Get<StabilityPoolConfigOf<Self>>;

		/// Weight-safety bound on pending-deposit FIFO entries one
		/// liquidation backstop call may consume; not a governance risk
		/// parameter (SPEC.md §5.3).
		#[pallet::constant]
		type MaxPendingOffsetIterations: Get<u32>;

		/// Seed for the per-market pool sub-accounts.
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		type WeightInfo: WeightInfo;
	}

	/// Per-branch depositor rows (SPEC.md §5.4).
	#[pallet::storage]
	pub type Deposits<T: Config> = StorageNMap<
		_,
		(
			NMapKey<Twox64Concat, T::CollateralAssetId>,
			NMapKey<Twox64Concat, T::StableAssetId>,
			NMapKey<Blake2_128Concat, T::AccountId>,
		),
		DepositOf<T>,
		OptionQuery,
	>;

	/// Branch pool totals and current product-sum coordinates.
	#[pallet::storage]
	pub type PoolStates<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		T::CollateralAssetId,
		Twox64Concat,
		T::StableAssetId,
		PoolStateOf<T>,
		OptionQuery,
	>;

	/// Historical and current `S`/`G` sums, keyed by `(epoch, scale)`. Rows
	/// may be pruned only when no deposit snapshot references them.
	#[pallet::storage]
	pub type PoolSumsStore<T: Config> = StorageNMap<
		_,
		(
			NMapKey<Twox64Concat, T::CollateralAssetId>,
			NMapKey<Twox64Concat, T::StableAssetId>,
			NMapKey<Twox64Concat, u32>,
			NMapKey<Twox64Concat, u32>,
		),
		PoolSums,
		OptionQuery,
	>;

	/// Per-branch deposit, withdrawal, and accumulator parameters.
	#[pallet::storage]
	pub type StabilityPoolConfigs<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		T::CollateralAssetId,
		Twox64Concat,
		T::StableAssetId,
		StabilityPoolConfigOf<T>,
		OptionQuery,
	>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Stablecoin entered the pool (SPEC.md §6.6). `used_for_recovery`
		/// was burned immediately against a `FinalRecovery` vault;
		/// `pending_amount` queued behind the entry delay.
		DepositReceived {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
			used_for_recovery: BalanceOf<T>,
			pending_amount: BalanceOf<T>,
		},
		/// A pending deposit passed its entry delay and became active.
		PendingDepositActivated {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// A withdrawal request was created or replaced (SPEC.md §6.9).
		WithdrawalRequested {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
			executable_at: BlockNumberFor<T>,
		},
		/// Active stablecoin left the pool. `amount` is what was actually
		/// taken, which may be less than requested.
		WithdrawalExecuted {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			depositor: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Realized collateral gains were paid out (SPEC.md §6.10).
		CollateralClaimed {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			depositor: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Realized stablecoin yield was paid out (SPEC.md §6.10).
		YieldClaimed {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			depositor: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Branch yield was distributed to active depositors through `G`
		/// (SPEC.md §6.3).
		YieldDistributed {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			amount: BalanceOf<T>,
		},
		/// Claimable yield was moved into the active deposit (SPEC.md §6.11).
		YieldCompounded {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Active-pool stablecoin was burned against liquidation debt
		/// (SPEC.md §7.1). `epoch`/`scale` are the post-offset coordinates.
		PoolOffsetApplied {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			debt_burned: BalanceOf<T>,
			collateral_gain: BalanceOf<T>,
			epoch: u32,
			scale: u32,
		},
		/// Pending deposits were consumed as the last-resort liquidation
		/// backstop (SPEC.md §7.2).
		PendingDepositOffsetApplied {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			debt_burned: BalanceOf<T>,
			collateral_gain: BalanceOf<T>,
			iterations: u32,
		},
		/// Stablecoin was burned against the `FinalRecovery` FIFO head at
		/// the shared settlement pricing (SPEC.md §7.3 / §7.4). The
		/// `source` distinguishes active-pool capital (gains through `S`)
		/// from an incoming deposit (gains credited directly).
		RecoveryOffsetApplied {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			debt_burned: BalanceOf<T>,
			collateral_gain: BalanceOf<T>,
			source: RecoveryOffsetSource,
		},
		/// Governance replaced a market's stability-pool config.
		StabilityPoolConfigUpdated {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// No stability pool is registered for this market.
		BranchNotRegistered,
		/// The deposit is below the branch `minimum_deposit`.
		DepositTooSmall,
		/// The caller has no deposit row on this market.
		DepositNotFound,
		/// The deposit row has no pending amount to activate.
		NoPendingDeposit,
		/// The pending deposit has not passed its entry delay yet.
		PendingDepositNotMatured,
		/// The operation would withdraw zero stablecoin: a zero amount, an
		/// exhausted request, or no active deposit to draw from.
		NoActiveDeposit,
		/// Safety Mode withdrawals need a prior [`Pallet::request_withdraw`].
		WithdrawalRequestMissing,
		/// The withdrawal request has not passed the Safety delay yet.
		SafetyWithdrawalDelayActive,
		/// The branch is halted; no user operation may change pool risk.
		BranchFrozen,
		/// No realized collateral gains to claim.
		NoClaimableCollateral,
		/// No realized stablecoin yield to claim.
		NoClaimableYield,
		/// No realized claimable yield to compound.
		NoYieldToCompound,
		/// The offset would push `P` across more scale boundaries than
		/// supported (`new_total / total < 1e-18`) or overflow the
		/// accumulator math; only reachable with a misconfigured
		/// `minimum_active_pool_balance` on a gigantic pool.
		UnsupportedOffsetPrecision,
		/// Withdrawing the offset debt from the pool account failed —
		/// storage corruption, not a user error (the balance identity
		/// guarantees cover).
		StablecoinBurnFailed,
		/// No `FinalRecovery` vault is queued on this market.
		RecoveryVaultNotFound,
		/// The `FinalRecovery` head is below par (`CR < 100%`): deposits
		/// are rejected and recovery offsets unavailable — discounted
		/// settlement stays exclusive to the explicit redemption pathway.
		RecoveryOffsetBelowPar,
		/// The recovery offset resolved to zero burnable stablecoin (empty
		/// active pool, the §6.5 floor, or a zero request).
		NoRecoveryOffsetPerformed,
		/// The settlement executed against different figures than it was
		/// sized with — storage corruption, not a user error (quote and
		/// execution share one oracle price and block).
		InvalidRecoveryOffsetSnapshot,
		/// The pending-deposit FIFO and the deposit rows disagree — storage
		/// corruption, not a user error.
		PendingFifoInvariantBroken,
		/// The supplied stability-pool config is internally inconsistent.
		InvalidStabilityPoolConfig,
		/// `p_min` and `scale_factor` are frozen at registration: deposits
		/// left behind a scale boundary realize against the factor that was
		/// live when the boundary was crossed, so changing it would misprice
		/// them.
		AccumulatorParamsImmutable,
		/// The pool still holds depositor rows; the branch cannot be removed.
		PoolNotEmpty,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn integrity_test() {
			// A zero cap would make the pending-deposit liquidation backstop
			// a permanent no-op while still advertising the interface.
			assert!(
				T::MaxPendingOffsetIterations::get() > 0,
				"`MaxPendingOffsetIterations` must be > 0"
			);
			// An invalid default would reject every market registration at
			// runtime; under permissionless creation that bricks the pallet.
			assert!(
				T::DefaultStabilityPoolConfig::get().is_valid(),
				"`DefaultStabilityPoolConfig` must satisfy `StabilityPoolConfig::is_valid`"
			);
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(_: BlockNumberFor<T>) -> Result<(), frame::try_runtime::TryRuntimeError> {
			crate::try_state::do_try_state::<T>()
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Supply `amount` stablecoin to the market's stability pool. The
		/// funds queue as a pending deposit until `entry_delay_blocks` have
		/// passed, then activate on the next touch (or via
		/// [`Pallet::activate_deposit`]). A second deposit merges into the
		/// existing pending amount and restarts its delay.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::deposit())]
		pub fn deposit(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			with_storage_layer(|| Self::do_deposit(who, collateral_id, stable_id, amount))
		}

		/// Activate the caller's matured pending deposit, making it
		/// offsettable and yield-earning.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::activate_deposit())]
		pub fn activate_deposit(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			with_storage_layer(|| Self::do_activate_deposit(who, collateral_id, stable_id))
		}

		/// Create or replace a withdrawal request for up to `amount` active
		/// stablecoin. Only load-bearing in Safety Mode, where withdrawals
		/// must wait `safety_withdrawal_delay` from the request; Normal-Mode
		/// withdrawals execute directly via [`Pallet::withdraw`].
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::request_withdraw())]
		pub fn request_withdraw(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			with_storage_layer(|| Self::do_request_withdraw(who, collateral_id, stable_id, amount))
		}

		/// Withdraw up to `amount` active stablecoin to `recipient` —
		/// immediately in Normal Mode, against a matured withdrawal request
		/// in Safety Mode. Takes `min(amount, active)` rather than failing
		/// when the active deposit shrank since the caller last looked.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::withdraw())]
		pub fn withdraw(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			amount: BalanceOf<T>,
			recipient: T::AccountId,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			with_storage_layer(|| {
				Self::do_withdraw(who, collateral_id, stable_id, amount, recipient)
			})
		}

		/// Pay the caller's realized collateral gains to `recipient`.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::claim_collateral())]
		pub fn claim_collateral(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			recipient: T::AccountId,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			with_storage_layer(|| {
				Self::do_claim(who, collateral_id, stable_id, recipient, ClaimKind::Collateral)
			})
		}

		/// Pay the caller's realized stablecoin yield to `recipient`. Yield
		/// stays claimable — never offsettable — until explicitly compounded.
		#[pallet::call_index(5)]
		#[pallet::weight(T::WeightInfo::claim_yield())]
		pub fn claim_yield(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			recipient: T::AccountId,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			with_storage_layer(|| {
				Self::do_claim(who, collateral_id, stable_id, recipient, ClaimKind::Yield)
			})
		}

		/// Permissionlessly burn active pool stablecoin against the current
		/// `FinalRecovery` FIFO head at the shared recovery-settlement
		/// pricing (SPEC.md §7.3). Active depositors receive the priced
		/// collateral through `S`, exactly like an ordinary liquidation
		/// offset. Available whenever the head is at or above par.
		#[pallet::call_index(7)]
		#[pallet::weight(T::WeightInfo::offset_recovery())]
		pub fn offset_recovery(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			max_stable_in: BalanceOf<T>,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			with_storage_layer(|| Self::do_offset_recovery(collateral_id, stable_id, max_stable_in))
		}

		/// Move up to `amount` of the caller's claimable yield into the
		/// active deposit, where it starts absorbing offsets and earning
		/// gains immediately (SPEC.md §6.11). Yield never becomes
		/// offsettable without this explicit step.
		#[pallet::call_index(6)]
		#[pallet::weight(T::WeightInfo::compound_yield())]
		pub fn compound_yield(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			with_storage_layer(|| Self::do_compound_yield(who, collateral_id, stable_id, amount))
		}

		/// Permissionlessly realize `owner`'s deposit against the current
		/// accumulators, without moving value. Never activates a matured
		/// pending deposit — that choice stays with the owner.
		#[pallet::call_index(9)]
		#[pallet::weight(T::WeightInfo::poke_deposit())]
		pub fn poke_deposit(
			origin: OriginFor<T>,
			owner: T::AccountId,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			with_storage_layer(|| Self::do_poke_deposit(owner, collateral_id, stable_id))
		}

		/// Replace a market's stability-pool parameters. The accumulator
		/// precision parameters (`p_min`, `scale_factor`) must match the
		/// stored values — see [`Error::AccumulatorParamsImmutable`].
		///
		/// Call indices 0-7 are reserved for the user-facing deposit
		/// lifecycle so calls can land milestone by milestone without
		/// renumbering.
		#[pallet::call_index(8)]
		#[pallet::weight(T::WeightInfo::set_stability_pool_config())]
		pub fn set_stability_pool_config(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			config: StabilityPoolConfigOf<T>,
		) -> DispatchResult {
			T::UpdateOrigin::ensure_origin(origin, &(collateral_id.clone(), stable_id.clone()))?;
			let existing = StabilityPoolConfigs::<T>::get(&collateral_id, &stable_id)
				.ok_or(Error::<T>::BranchNotRegistered)?;
			ensure!(config.is_valid(), Error::<T>::InvalidStabilityPoolConfig);
			ensure!(config.p_min == existing.p_min, Error::<T>::AccumulatorParamsImmutable);
			ensure!(
				config.scale_factor == existing.scale_factor,
				Error::<T>::AccumulatorParamsImmutable
			);
			StabilityPoolConfigs::<T>::insert(&collateral_id, &stable_id, config);
			Self::deposit_event(Event::StabilityPoolConfigUpdated { collateral_id, stable_id });
			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Per-market account holding the pool's stablecoin (active and
		/// pending deposits plus undistributed yield) and collateral
		/// (unclaimed gains).
		pub fn pool_account(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
		) -> T::AccountId {
			pusd_primitives::market_sub_account(T::PalletId::get(), collateral_id, stable_id)
		}
	}

	impl<T: Config> OnBranchLifecycle<T::CollateralAssetId, T::StableAssetId> for Pallet<T> {
		fn on_registered(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
		) -> DispatchResult {
			let config = T::DefaultStabilityPoolConfig::get();
			ensure!(config.is_valid(), Error::<T>::InvalidStabilityPoolConfig);
			PoolStates::<T>::insert(collateral_id, stable_id, PoolStateOf::<T>::fresh());
			PoolSumsStore::<T>::insert((collateral_id, stable_id, 0u32, 0u32), PoolSums::default());
			StabilityPoolConfigs::<T>::insert(collateral_id, stable_id, config);

			// A provider reference keeps the sub-account alive across
			// zero-balance moments without depositing an existential deposit.
			let pool_account = Self::pool_account(collateral_id, stable_id);
			if frame_system::Pallet::<T>::providers(&pool_account) == 0 {
				frame_system::Pallet::<T>::inc_providers(&pool_account);
			}
			Ok(())
		}

		fn on_deregistered(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
		) -> DispatchResult {
			// Depositor rows are the user-funds guard: active, pending, and
			// claimable value all live on them. Vaults rolls the whole
			// `remove_branch` back on this error, so a market admin cannot
			// strand depositor funds. With no rows left, any residual pool
			// totals are unattributable flooring dust; it stays in the
			// (henceforth unreferenced) pool account.
			ensure!(
				Deposits::<T>::iter_prefix((collateral_id.clone(), stable_id.clone()))
					.next()
					.is_none(),
				Error::<T>::PoolNotEmpty
			);
			PoolStates::<T>::remove(collateral_id, stable_id);
			StabilityPoolConfigs::<T>::remove(collateral_id, stable_id);
			// Safe to clear wholesale: without deposit rows, no snapshot can
			// reference a sums row.
			let removal = PoolSumsStore::<T>::clear_prefix(
				(collateral_id.clone(), stable_id.clone()),
				u32::MAX,
				None,
			);
			debug_assert!(removal.maybe_cursor.is_none());

			let pool_account = Self::pool_account(collateral_id, stable_id);
			if frame_system::Pallet::<T>::providers(&pool_account) > 0 {
				// May refuse while dust keeps consumer references alive;
				// the account then just stays around, which is harmless.
				let _ = frame_system::Pallet::<T>::dec_providers(&pool_account);
			}
			Ok(())
		}
	}
}
