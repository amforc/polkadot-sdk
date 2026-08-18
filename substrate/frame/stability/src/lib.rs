//! # Stability Pool Pallet

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod types;
pub mod weights;

mod dispatchable_impls;
mod interfaces;
mod math;
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
		types::{
			Deposit, Leg, PoolState, PoolSums, RecoveryOffsetSource, StabilityPool,
			StabilityPoolConfig,
		},
	};
	use frame::{
		deps::frame_support::{
			traits::{
				fungibles,
				fungibles::{Balanced as _, Inspect as _},
				tokens::{Fortitude, Precision, Preservation},
				EnsureOriginWithArg, OnUnbalanced, Time,
			},
			PalletId,
		},
		prelude::*,
	};
	use pusd_primitives::{BranchModeProvider, Millis, OnBranchLifecycle, RecoveryOffsetInterface};

	pub type BalanceOf<T> = <<T as Config>::StableAssets as fungibles::Inspect<
		<T as frame_system::Config>::AccountId,
	>>::Balance;

	/// Collateral identifier exposed by [`Config::CollateralAssets`].
	pub type CollateralIdOf<T> = <<T as Config>::CollateralAssets as fungibles::Inspect<
		<T as frame_system::Config>::AccountId,
	>>::AssetId;

	/// Stable asset identifier exposed by [`Config::StableAssets`].
	pub type StableIdOf<T> = <<T as Config>::StableAssets as fungibles::Inspect<
		<T as frame_system::Config>::AccountId,
	>>::AssetId;

	pub type StableCreditOf<T> =
		fungibles::Credit<<T as frame_system::Config>::AccountId, <T as Config>::StableAssets>;

	pub type CollateralCreditOf<T> =
		fungibles::Credit<<T as frame_system::Config>::AccountId, <T as Config>::CollateralAssets>;

	pub type DepositOf<T> = Deposit<BalanceOf<T>>;

	pub type PoolStateOf<T> = PoolState<BalanceOf<T>>;

	pub type StabilityPoolConfigOf<T> = StabilityPoolConfig<BalanceOf<T>>;

	pub type StabilityPoolOf<T> = StabilityPool<BalanceOf<T>>;

	pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Product-sum math needs a fungibles balance type that can enter
		/// fixed-point calculations without lossy adapters.
		type StableAssets: fungibles::Inspect<
				Self::AccountId,
				AssetId: Parameter + Member + Ord + MaxEncodedLen,
				Balance: FixedPointOperand,
			> + fungibles::Mutate<Self::AccountId>
			+ fungibles::Balanced<Self::AccountId>;

		/// Collateral surface the pool receives offset gains on and pays depositor claims from.
		type CollateralAssets: fungibles::Mutate<
				Self::AccountId,
				AssetId: Parameter + Member + Ord + MaxEncodedLen,
				Balance = BalanceOf<Self>,
			> + fungibles::Balanced<Self::AccountId>;

		/// Time source the entry delay and the Safety-Mode withdrawal delay
		/// are measured against.
		type TimeProvider: Time<Moment = Millis>;

		/// Branch operating-mode source of truth (point it at the vault
		/// pallet). Frozen branches reject every value-moving pool operation
		/// (SPEC.md §8.1); Safety Mode turns withdrawals two-step.
		type BranchModes: BranchModeProvider<CollateralIdOf<Self>, StableIdOf<Self>>;

		/// Shared `FinalRecovery` settlement pricing and execution (point it
		/// at the redemptions pallet, which owns that pricing) — recovery
		/// offsets can never diverge from recovery-redemption pricing
		/// (SPEC.md §12 invariant 9).
		type RecoveryOffsets: RecoveryOffsetInterface<
			CollateralId = CollateralIdOf<Self>,
			AccountId = Self::AccountId,
			Balance = BalanceOf<Self>,
			Credit = StableCreditOf<Self>,
		>;

		/// TODO: Doc
		type StableDustHandler: OnUnbalanced<StableCreditOf<Self>>;

		/// TODO: Doc
		type CollateralDustHandler: OnUnbalanced<CollateralCreditOf<Self>>;

		/// Authorizes [`Pallet::set_stability_pool_config`] for the market
		/// given as argument. Point this at the market's admin authority
		/// (e.g. vaults' `EnsureBranchFullAdmin`) and compose a governance
		/// override with `EitherOf`.
		type UpdateOrigin: EnsureOriginWithArg<
			Self::RuntimeOrigin,
			(CollateralIdOf<Self>, StableIdOf<Self>),
			Success = (),
		>;

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
			NMapKey<Twox64Concat, CollateralIdOf<T>>,
			NMapKey<Twox64Concat, StableIdOf<T>>,
			NMapKey<Blake2_128Concat, T::AccountId>,
		),
		DepositOf<T>,
		OptionQuery,
	>;

	/// The registered markets' pools: governance parameters and hot
	/// accounting state in one record, so the pieces can never partially
	/// exist. Existence of the row is what "registered" means.
	#[pallet::storage]
	pub type Pools<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		CollateralIdOf<T>,
		Twox64Concat,
		StableIdOf<T>,
		StabilityPoolOf<T>,
		OptionQuery,
	>;

	/// Historical and current `S`/`G` sums of both legs, keyed by
	/// `(leg, epoch, scale)`. The [`Leg::Pending`] rows are the
	/// backstop's own `P`/`S` domain — pending deposits are consumed pro-rata
	/// through it — and carry a structurally zero `g_yield`, kept only so both
	/// legs share one realization implementation. Rows may be pruned only when
	/// no snapshot on their leg references them.
	#[pallet::storage]
	pub type PoolSumsStore<T: Config> = StorageNMap<
		_,
		(
			NMapKey<Twox64Concat, CollateralIdOf<T>>,
			NMapKey<Twox64Concat, StableIdOf<T>>,
			NMapKey<Twox64Concat, Leg>,
			NMapKey<Twox64Concat, u32>,
			NMapKey<Twox64Concat, u32>,
		),
		PoolSums,
		ValueQuery,
	>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Stablecoin entered the pool (SPEC.md §6.6). `used_for_recovery`
		/// was burned immediately against a `FinalRecovery` vault;
		/// `pending_amount` queued behind the entry delay.
		DepositReceived {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
			used_for_recovery: BalanceOf<T>,
			pending_amount: BalanceOf<T>,
		},
		/// A pending deposit passed its entry delay and became active on the
		/// next touch of its row.
		PendingDepositActivated {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// A Safety-Mode withdrawal request was created or replaced.
		WithdrawalRequested {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
			executable_at: Millis,
		},
		/// Active stablecoin left the pool. `amount` is what was actually
		/// taken, which may be less than requested.
		WithdrawalExecuted {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Realized collateral gains were paid out (SPEC.md §6.10).
		CollateralClaimed {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Realized stablecoin yield was paid out (SPEC.md §6.10).
		YieldClaimed {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Branch yield was distributed to active depositors through `G`
		/// (SPEC.md §6.3).
		YieldDistributed {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			amount: BalanceOf<T>,
		},
		/// Claimable yield was moved into the active deposit (SPEC.md §6.11).
		YieldCompounded {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Active-pool stablecoin was burned against liquidation debt
		/// (SPEC.md §7.1). `epoch`/`scale` are the post-offset coordinates.
		PoolOffsetApplied {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			debt_burned: BalanceOf<T>,
			collateral_gain: BalanceOf<T>,
			epoch: u32,
			scale: u32,
		},
		/// Pending deposits were consumed pro-rata as the last-resort
		/// liquidation backstop (SPEC.md §7.2). `epoch`/`scale` are the
		/// post-offset PENDING accumulator coordinates.
		PendingDepositOffsetApplied {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			debt_burned: BalanceOf<T>,
			collateral_gain: BalanceOf<T>,
			epoch: u32,
			scale: u32,
		},
		/// Stablecoin was burned against the `FinalRecovery` FIFO head at
		/// the shared settlement pricing (SPEC.md §7.3 / §7.4). The
		/// `source` distinguishes active-pool capital (gains through `S`)
		/// from an incoming deposit (gains credited directly).
		RecoveryOffsetApplied {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			debt_burned: BalanceOf<T>,
			collateral_gain: BalanceOf<T>,
			source: RecoveryOffsetSource,
		},
		/// Governance replaced a market's stability-pool config.
		StabilityPoolConfigUpdated { collateral_id: CollateralIdOf<T>, stable_id: StableIdOf<T> },
		/// Market teardown swept the pool account's residual dust to
		/// [`Config::StableDustHandler`] / [`Config::CollateralDustHandler`].
		DustSwept {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			stable_amount: BalanceOf<T>,
			collateral_amount: BalanceOf<T>,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// No stability pool is registered for this market.
		PoolNotRegistered,
		/// The deposit is below the branch `minimum_deposit`.
		DepositTooSmall,
		/// The caller has no deposit row on this market.
		DepositNotFound,
		/// The requested amount is zero.
		ZeroAmount,
		/// The withdrawal resolved to zero stablecoin: an exhausted request,
		/// or no active deposit to draw from.
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
		/// A debt reservation could not be settled exactly.
		OffsetSettlementFailed,
		/// No `FinalRecovery` vault is queued on this market.
		RecoveryVaultNotFound,
		/// The `FinalRecovery` head is below par (`CR < 100%`): deposits
		/// are rejected and recovery offsets unavailable — discounted
		/// settlement stays exclusive to the explicit redemption pathway.
		RecoveryOffsetBelowPar,
		/// The recovery offset resolved to zero burnable stablecoin (empty
		/// active pool, the §6.5 floor, or a zero request).
		NoRecoveryOffsetPerformed,
		/// The supplied stability-pool config is internally inconsistent.
		InvalidStabilityPoolConfig,
		/// The `precision` pair is frozen at registration: deposits left
		/// behind a scale boundary realize against the factor that was live
		/// when the boundary was crossed, so changing it would misprice them.
		AccumulatorParamsImmutable,
		/// The pool still holds depositor rows; the branch cannot be removed.
		PoolNotEmpty,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		#[cfg(feature = "try-runtime")]
		fn try_state(_: BlockNumberFor<T>) -> Result<(), frame::try_runtime::TryRuntimeError> {
			crate::try_state::do_try_state::<T>()
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Supply `amount` stablecoin to the market's stability pool. The
		/// funds queue as a pending deposit until `entry_delay` has passed,
		/// and fold into the active pool on the next touch of the row — one
		/// of the owner's own calls, or anyone's [`Pallet::poke_deposit`]. A
		/// second deposit merges into the existing pending amount and
		/// restarts its delay.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::deposit())]
		pub fn deposit(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_deposit(who, collateral_id, stable_id, amount)
		}

		/// Safety Mode: create or replace a withdrawal request for up to
		/// `amount` active stablecoin, executable `safety_withdrawal_delay`
		/// after the request.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::request_withdraw())]
		pub fn request_withdraw(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_request_withdraw(who, collateral_id, stable_id, amount)
		}

		/// Withdraw up to `amount` active stablecoin — immediately in Normal
		/// Mode, against a matured withdrawal request in Safety Mode. Takes
		/// `min(amount, active)` rather than failing when the active deposit
		/// shrank since the caller last looked. A `None` recipient pays the
		/// caller.
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::withdraw())]
		pub fn withdraw(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			amount: BalanceOf<T>,
			recipient: Option<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let recipient = recipient.unwrap_or_else(|| who.clone());
			Self::do_withdraw(who, collateral_id, stable_id, amount, recipient)
		}

		/// Pay the caller's realized collateral gains out; a `None` recipient
		/// pays the caller.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::claim_collateral())]
		pub fn claim_collateral(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			recipient: Option<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let recipient = recipient.unwrap_or_else(|| who.clone());
			Self::do_claim(who, collateral_id, stable_id, recipient, ClaimKind::Collateral)
		}

		/// Pay the caller's realized stablecoin yield out; a `None` recipient
		/// pays the caller. Yield stays claimable — never offsettable — until
		/// explicitly compounded.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::claim_yield())]
		pub fn claim_yield(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			recipient: Option<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let recipient = recipient.unwrap_or_else(|| who.clone());
			Self::do_claim(who, collateral_id, stable_id, recipient, ClaimKind::Yield)
		}

		/// Permissionlessly burn active pool stablecoin against the current
		/// `FinalRecovery` FIFO head at the shared recovery-settlement
		/// pricing (SPEC.md §7.3). Active depositors receive the priced
		/// collateral through `S`, exactly like an ordinary liquidation
		/// offset. Available whenever the head is at or above par.
		#[pallet::call_index(5)]
		#[pallet::weight(T::WeightInfo::offset_recovery())]
		pub fn offset_recovery(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			max_stable_in: BalanceOf<T>,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			Self::do_offset_recovery(collateral_id, stable_id, max_stable_in)
		}

		/// Move up to `amount` of the caller's claimable yield into the
		/// active deposit, where it starts absorbing offsets and earning
		/// gains immediately (SPEC.md §6.11). Yield never becomes
		/// offsettable without this explicit step.
		#[pallet::call_index(6)]
		#[pallet::weight(T::WeightInfo::compound_yield())]
		pub fn compound_yield(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_compound_yield(who, collateral_id, stable_id, amount)
		}

		/// Permissionlessly realize `owner`'s deposit against the current
		/// accumulators, without moving value, and fold in a matured pending
		/// deposit. A matured pending deposit needs a touch to fold in; past
		/// the entry delay the move is mechanical, so any caller may supply
		/// that touch.
		#[pallet::call_index(7)]
		#[pallet::weight(T::WeightInfo::poke_deposit())]
		pub fn poke_deposit(
			origin: OriginFor<T>,
			owner: T::AccountId,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			Self::do_poke_deposit(owner, collateral_id, stable_id)
		}

		/// Replace a market's stability-pool parameters. The `precision`
		/// pair must match the stored values — see
		/// [`Error::AccumulatorParamsImmutable`].
		///
		/// Call indices 0-7 are reserved for the user-facing deposit
		/// lifecycle so calls can land milestone by milestone without
		/// renumbering.
		#[pallet::call_index(8)]
		#[pallet::weight(T::WeightInfo::set_stability_pool_config())]
		pub fn set_stability_pool_config(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			config: StabilityPoolConfigOf<T>,
		) -> DispatchResult {
			T::UpdateOrigin::ensure_origin(origin, &(collateral_id.clone(), stable_id.clone()))?;
			Self::do_set_stability_pool_config(collateral_id, stable_id, config)
		}
	}

	impl<T: Config> Pallet<T> {
		/// Per-market account holding the pool's stablecoin (active and
		/// pending deposits plus undistributed yield) and collateral
		/// (unclaimed gains).
		pub fn pool_account(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
		) -> T::AccountId {
			pusd_primitives::market_sub_account(T::PalletId::get(), collateral_id, stable_id)
		}

		/// Sweep the pool account's residual balances to the dust handlers,
		/// returning the swept amounts.
		///
		/// Only called with no depositor rows left, so whatever remains is
		/// unattributable flooring residue. It must not stay behind: the
		/// balance↔totals invariant holds as an equality, so a re-registered
		/// pair starting from fresh zero totals would inherit a divergence.
		fn sweep_dust(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			pool_account: &T::AccountId,
		) -> Result<(BalanceOf<T>, BalanceOf<T>), DispatchError> {
			let stable_dust = T::StableAssets::balance(stable_id.clone(), pool_account);
			if !stable_dust.is_zero() {
				let credit = T::StableAssets::withdraw(
					stable_id.clone(),
					pool_account,
					stable_dust,
					Precision::Exact,
					Preservation::Expendable,
					Fortitude::Polite,
				)?;
				T::StableDustHandler::on_unbalanced(credit);
			}
			let collateral_dust = T::CollateralAssets::balance(collateral_id.clone(), pool_account);
			if !collateral_dust.is_zero() {
				let credit = T::CollateralAssets::withdraw(
					collateral_id.clone(),
					pool_account,
					collateral_dust,
					Precision::Exact,
					Preservation::Expendable,
					Fortitude::Polite,
				)?;
				T::CollateralDustHandler::on_unbalanced(credit);
			}
			debug_assert!(T::StableAssets::balance(stable_id.clone(), pool_account).is_zero());
			debug_assert!(
				T::CollateralAssets::balance(collateral_id.clone(), pool_account).is_zero()
			);
			Ok((stable_dust, collateral_dust))
		}
	}

	impl<T: Config> OnBranchLifecycle<CollateralIdOf<T>, StableIdOf<T>> for Pallet<T> {
		/// One pool per `(collateral, stablecoin)` market, so every
		/// registration carries its own parameters.
		type RegistrationConfig = StabilityPoolConfigOf<T>;

		fn on_registered(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			_stablecoin_markets: u32,
			config: Self::RegistrationConfig,
		) -> DispatchResult {
			ensure!(config.is_valid(), Error::<T>::InvalidStabilityPoolConfig);
			Pools::<T>::insert(collateral_id, stable_id, StabilityPoolOf::<T>::fresh(config));
			for leg in Leg::ALL {
				PoolSumsStore::<T>::insert(
					(collateral_id, stable_id, leg, 0u32, 0u32),
					PoolSums::default(),
				);
			}

			// A provider reference keeps the sub-account alive across
			// zero-balance moments without depositing an existential deposit.
			let pool_account = Self::pool_account(collateral_id, stable_id);
			if frame_system::Pallet::<T>::providers(&pool_account) == 0 {
				frame_system::Pallet::<T>::inc_providers(&pool_account);
			}
			Ok(())
		}

		fn on_deregistered(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
			_remaining_stablecoin_markets: u32,
		) -> DispatchResult {
			// Depositor rows are the user-funds guard: active, pending, and
			// claimable value all live on them. Vaults rolls the whole
			// `remove_branch` back on this error, so a market admin cannot
			// strand depositor funds. With no rows left, any residual pool
			// balance is unattributable flooring dust, swept to the runtime's
			// dust handlers below.
			ensure!(
				Deposits::<T>::iter_prefix((collateral_id.clone(), stable_id.clone()))
					.next()
					.is_none(),
				Error::<T>::PoolNotEmpty
			);

			let pool_account = Self::pool_account(collateral_id, stable_id);
			let (stable_amount, collateral_amount) =
				Self::sweep_dust(collateral_id, stable_id, &pool_account)?;
			if !stable_amount.is_zero() || !collateral_amount.is_zero() {
				Self::deposit_event(Event::DustSwept {
					collateral_id: collateral_id.clone(),
					stable_id: stable_id.clone(),
					stable_amount,
					collateral_amount,
				});
			}

			Pools::<T>::remove(collateral_id, stable_id);
			// Safe to clear wholesale: without deposit rows, no snapshot on
			// either leg can reference a sums row.
			let removal = PoolSumsStore::<T>::clear_prefix(
				(collateral_id.clone(), stable_id.clone()),
				u32::MAX,
				None,
			);
			debug_assert!(removal.maybe_cursor.is_none());

			if frame_system::Pallet::<T>::providers(&pool_account) > 0 {
				// The sweep zeroed the market's own assets, but unrelated
				// tokens a third party parked here may still hold consumer
				// references; the account then just stays, which is harmless.
				let _ = frame_system::Pallet::<T>::dec_providers(&pool_account);
			}
			Ok(())
		}

		/// One-unit thresholds and no delays, so nothing in the pool becomes
		/// the binding constraint of a benchmark. `p_min * scale_factor` lands
		/// exactly at one, the tightest rescale [`PoolPrecision::is_valid`]
		/// accepts.
		#[cfg(feature = "runtime-benchmarks")]
		fn benchmark_registration_config(_stablecoin_markets: u32) -> Self::RegistrationConfig {
			StabilityPoolConfig {
				minimum_deposit: BalanceOf::<T>::one(),
				minimum_active_pool_balance: BalanceOf::<T>::one(),
				entry_delay: 0,
				safety_withdrawal_delay: 0,
				precision: crate::types::PoolPrecision {
					p_min: FixedU128::from_inner(1_000_000_000),
					scale_factor: 1_000_000_000,
				},
				yield_share: Permill::from_percent(75),
			}
		}
	}
}
