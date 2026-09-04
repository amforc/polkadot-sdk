//! # Stability Pool
//!
//! Provides market-specific stablecoin pools that cancel vault debt and allocate collateral and
//! yield to depositors.
//!
//! ## Overview
//!
//! Each pool belongs to one `(collateral_id, stable_id)` market. The market lifecycle creates and
//! removes its pool, which prevents pool state without a registered market.
//!
//! Depositors supply stablecoin that the liquidation system can burn to cancel vault debt. The pool
//! allocates the associated collateral to those depositors. Active deposits also receive the
//! configured share of market yield.
//!
//! Participation has risk. Debt cancellation reduces the stablecoin deposit, and the value of the
//! received collateral can change. The operating mode can also delay withdrawals.
//!
//! ### Deposit lifecycle
//!
//! A deposit has pending capital, active capital, or both. New capital waits at least `entry_delay`
//! before activation. This delay limits deposits made only to capture an expected liquidation
//! gain. A depositor can withdraw active capital only.
//!
//! Pending capital earns no yield. It can absorb debt after the liquidation system uses the active
//! pool and other earlier sources. Thus, the delay is not a risk-free period.
//!
//! Cohort deadlines keep activation work independent of the depositor count. The activation wait
//! is in `[entry_delay, 2 * entry_delay)`, unless `entry_delay` is zero. Activation does not
//! require an action from the depositor.
//!
//! ### Gains
//!
//! Liquidation offsets reduce deposit principal and add claimable collateral. Market yield adds
//! claimable stablecoin. A depositor collects these balances with [`Pallet::claim_collateral`] and
//! [`Pallet::claim_yield`].
//!
//! Claimable yield does not absorb debt or earn more yield. Only the depositor can use
//! [`Pallet::compound_yield`] to expose it to pool risk again.
//!
//! ### Operating modes
//!
//! [`Config::BranchModes`] reports the operating mode of a market:
//!
//! - `Normal` permits immediate withdrawals.
//! - `Safety` requires [`Pallet::request_withdraw`] and a `safety_withdrawal_delay`. The delay
//!   keeps capital available while the market is under stress.
//! - `Frozen` blocks value movement and cohort activation. Local settlement remains available so
//!   committed losses and gains do not depend on market recovery.
//!
//! The pallet applies `Frozen` rules when market risk information is unavailable. This rule
//! prevents value movement without current risk information.
//!
//! ### Recovery offsets
//!
//! [`Config::RecoveryOffsets`] owns the settlement policy for vaults in `FinalRecovery`. This
//! authority keeps the recovery price policy consistent for all callers.
//!
//! Any signed account can use [`Pallet::offset_recovery`] to settle active pool stablecoin against
//! the queue head when its price is at or above par. The active depositors receive the collateral.
//!
//! [`Pallet::deposit`] applies the same policy before it adds the unused stablecoin to the pool.
//! The depositor receives collateral for the settled part. A below-par queue head disables pool
//! recovery offsets and rejects deposits because discounted settlement belongs to the redemption
//! path.
//!
//! ## Pallet API
//!
//! The [`pallet`] module describes the configuration trait, dispatchable calls, storage items,
//! events, and errors.
//!
//! ## Low Level / Implementation Details
//!
//! ### Scalable accounting
//!
//! Liquidation cost must not increase with the depositor count. The pallet therefore updates
//! market accumulators and settles each deposit when an operation uses its row.
//!
//! `P` tracks the deposit fraction that remains. `S` tracks collateral per deposit unit, and `G`
//! tracks yield per active deposit unit. Active and pending capital have separate loss accumulators
//! because they have different risk priority.
//!
//! Scale and epoch boundaries protect precision when offsets make `P` small or consume a pool.
//! Governance cannot change precision parameters after registration because stored deposit
//! snapshots depend on them.
//!
//! ### Custody and rounding
//!
//! Debt cancellation and collateral allocation settle atomically. A stale quote or a custody
//! mismatch causes an error, so vault debt and pool balances cannot diverge.
//!
//! User payouts round down. Remainders stay in pool custody, which prevents the pool from owing
//! more than its account holds. Market removal sends unowned remainders to
//! [`Config::StableDustHandler`] and [`Config::CollateralDustHandler`].

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
			CohortCheckpoint, CohortId, Deposit, Leg, PoolState, PoolSums, RecoveryOffsetSource,
			StabilityPool, StabilityPoolConfig,
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

	/// Balance type shared by the stablecoin and collateral asset systems.
	pub type BalanceOf<T> = <<T as Config>::StableAssets as fungibles::Inspect<
		<T as frame_system::Config>::AccountId,
	>>::Balance;

	/// Collateral identifier from [`Config::CollateralAssets`].
	pub type CollateralIdOf<T> = <<T as Config>::CollateralAssets as fungibles::Inspect<
		<T as frame_system::Config>::AccountId,
	>>::AssetId;

	/// Stablecoin identifier from [`Config::StableAssets`].
	pub type StableIdOf<T> = <<T as Config>::StableAssets as fungibles::Inspect<
		<T as frame_system::Config>::AccountId,
	>>::AssetId;

	/// Stablecoin credit that the pallet must resolve or burn.
	pub type StableCreditOf<T> =
		fungibles::Credit<<T as frame_system::Config>::AccountId, <T as Config>::StableAssets>;

	/// Collateral credit that the pallet must resolve or burn.
	pub type CollateralCreditOf<T> =
		fungibles::Credit<<T as frame_system::Config>::AccountId, <T as Config>::CollateralAssets>;

	/// Deposit position of one account in one market.
	pub type DepositOf<T> = Deposit<BalanceOf<T>>;

	/// Activation checkpoint for one maturity cohort.
	pub type CohortCheckpointOf<T> = CohortCheckpoint<BalanceOf<T>>;

	/// Accounting state of one pool.
	pub type PoolStateOf<T> = PoolState<BalanceOf<T>>;

	/// Governance parameters of one pool.
	pub type StabilityPoolConfigOf<T> = StabilityPoolConfig<BalanceOf<T>>;

	/// Configuration and accounting state of one pool.
	pub type StabilityPoolOf<T> = StabilityPool<BalanceOf<T>>;

	/// Storage version of this pallet.
	pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Asset system for deposits, yield, and debt cancellation.
		type StableAssets: fungibles::Inspect<
				Self::AccountId,
				AssetId: Parameter + Member + Ord + MaxEncodedLen,
				Balance: FixedPointOperand,
			> + fungibles::Mutate<Self::AccountId>
			+ fungibles::Balanced<Self::AccountId>;

		/// Asset system for liquidation gains and collateral claims.
		type CollateralAssets: fungibles::Mutate<
				Self::AccountId,
				AssetId: Parameter + Member + Ord + MaxEncodedLen,
				Balance = BalanceOf<Self>,
			> + fungibles::Balanced<Self::AccountId>;

		/// Clock for the entry delay and the Safety-mode withdrawal delay.
		type TimeProvider: Time<Moment = Millis>;

		/// Authority for the operating mode of each market.
		type BranchModes: BranchModeProvider<CollateralIdOf<Self>, StableIdOf<Self>>;

		/// Price policy and settlement interface for vaults in `FinalRecovery`.
		type RecoveryOffsets: RecoveryOffsetInterface<
			CollateralId = CollateralIdOf<Self>,
			AccountId = Self::AccountId,
			Balance = BalanceOf<Self>,
			Credit = StableCreditOf<Self>,
		>;

		/// Destination for unowned stablecoin after market removal.
		type StableDustHandler: OnUnbalanced<StableCreditOf<Self>>;

		/// Destination for unowned collateral after market removal. See
		/// [`Config::StableDustHandler`].
		type CollateralDustHandler: OnUnbalanced<CollateralCreditOf<Self>>;

		/// Origin that can update the configuration of the specified market.
		type UpdateOrigin: EnsureOriginWithArg<
			Self::RuntimeOrigin,
			(CollateralIdOf<Self>, StableIdOf<Self>),
			Success = (),
		>;

		/// Seed for the pool account of each market. See [`Pallet::pool_account`].
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// Weight information for this pallet's dispatchable calls.
		type WeightInfo: WeightInfo;
	}

	/// Deposit position of each account in each market.
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

	/// Configuration and accounting state of each registered market.
	///
	/// One row contains both parts so configuration cannot exist without its accounting state. The
	/// row also proves that the market is registered.
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

	/// The `S` and `G` accumulator sums at each `(epoch, scale)` coordinate.
	///
	/// A row remains while a deposit snapshot can refer to it. This rule preserves unclaimed gains.
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

	/// Activation boundary of each completed maturity cohort.
	///
	/// The boundary preserves pending losses and subsequent active gains until all member rows
	/// settle. `members` counts the rows that still require the boundary.
	#[pallet::storage]
	pub type CohortCheckpoints<T: Config> = StorageNMap<
		_,
		(
			NMapKey<Twox64Concat, CollateralIdOf<T>>,
			NMapKey<Twox64Concat, StableIdOf<T>>,
			NMapKey<Twox64Concat, CohortId>,
		),
		CohortCheckpointOf<T>,
		OptionQuery,
	>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Stablecoin entered the pool. `used_for_recovery` settled `FinalRecovery` debt.
		/// `pending_amount` entered the pending deposit or the active pool when the delay was
		/// zero.
		DepositReceived {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
			used_for_recovery: BalanceOf<T>,
			pending_amount: BalanceOf<T>,
		},
		/// A maturity cohort passed its deadline, and the capital that remained joined the active
		/// pool. Member rows record their shares later. See [`Event::PendingDepositActivated`].
		CohortActivated {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			cohort: CohortId,
			deadline: Millis,
			amount: BalanceOf<T>,
		},
		/// A deposit row recorded the `amount` that became active at its cohort deadline.
		PendingDepositActivated {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// A Safety-mode withdrawal request was created or replaced.
		WithdrawalRequested {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
			executable_at: Millis,
		},
		/// Active stablecoin left the pool. `amount` is what the pool paid, which can be less than
		/// the caller asked for.
		WithdrawalExecuted {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Claimable collateral was paid out.
		CollateralClaimed {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Claimable stablecoin yield was paid out.
		YieldClaimed {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Market yield was shared out to the active depositors.
		YieldDistributed {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			amount: BalanceOf<T>,
		},
		/// Claimable yield moved into the active deposit.
		YieldCompounded {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Active stablecoin canceled liquidation debt. `epoch` and `scale` are the coordinates
		/// the active pool holds after the offset.
		PoolOffsetApplied {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			debt_burned: BalanceOf<T>,
			collateral_gain: BalanceOf<T>,
			epoch: u32,
			scale: u32,
		},
		/// Pending deposits canceled liquidation debt as the final pool backstop, in proportion
		/// to their size. `epoch` and `scale` are the coordinates the pending leg holds after the
		/// offset.
		PendingDepositOffsetApplied {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			debt_burned: BalanceOf<T>,
			collateral_gain: BalanceOf<T>,
			epoch: u32,
			scale: u32,
		},
		/// Stablecoin settled the head of the `FinalRecovery` queue. `source` identifies the
		/// capital that paid and therefore the owner of the collateral gain.
		RecoveryOffsetApplied {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			debt_burned: BalanceOf<T>,
			collateral_gain: BalanceOf<T>,
			source: RecoveryOffsetSource,
		},
		/// An authorized origin replaced the pool parameters of a market.
		StabilityPoolConfigUpdated { collateral_id: CollateralIdOf<T>, stable_id: StableIdOf<T> },
		/// Market removal sent unowned pool balances to the dust handlers.
		DustSwept {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			stable_amount: BalanceOf<T>,
			collateral_amount: BalanceOf<T>,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The market has no stability pool. Register the market before this call.
		PoolNotRegistered,
		/// The deposit is less than the market's `minimum_deposit`. Increase the deposit amount.
		DepositTooSmall,
		/// The caller holds no deposit in this market.
		DepositNotFound,
		/// The requested amount is zero.
		ZeroAmount,
		/// The withdrawal amount is zero because the request is used or the active deposit is
		/// empty.
		NoActiveDeposit,
		/// A Safety-mode withdrawal has no prior [`Pallet::request_withdraw`].
		WithdrawalRequestMissing,
		/// The withdrawal request has not completed its `safety_withdrawal_delay`.
		SafetyWithdrawalDelayActive,
		/// The market is frozen, and this call can move value or change pool risk.
		BranchFrozen,
		/// The caller has no claimable collateral.
		NoClaimableCollateral,
		/// The caller has no claimable yield.
		NoClaimableYield,
		/// The caller has no claimable yield to compound.
		NoYieldToCompound,
		/// The offset exceeds the scale limit or the accumulator range. Increase
		/// `minimum_active_pool_balance` relative to the pool size.
		UnsupportedOffsetPrecision,
		/// The pool cannot burn the exact quoted debt. Obtain new reducible amounts, and then
		/// retry.
		OffsetSettlementFailed,
		/// This market has no vault in `FinalRecovery`.
		RecoveryVaultNotFound,
		/// The head of the `FinalRecovery` queue is below par, so the pool rejects deposits and
		/// offers no recovery offset. Settlement at a discount stays exclusive to the redemption
		/// path.
		RecoveryOffsetBelowPar,
		/// The recovery offset is zero because the active pool is empty, the floor applies, or the
		/// request is zero.
		NoRecoveryOffsetPerformed,
		/// The pool parameters contradict each other. See
		/// [`types::StabilityPoolConfig::is_valid`].
		InvalidStabilityPoolConfig,
		/// The `precision` parameters differ from their registered values. Stored snapshots depend
		/// on those values and would produce incorrect amounts after a change.
		AccumulatorParamsImmutable,
		/// The pool still has deposit rows. Remove all user positions before market removal.
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
		/// Supply stablecoin to the stability pool of a market.
		///
		/// ## Dispatch Origin
		///
		/// The dispatch origin of this call must be signed.
		///
		/// ## Details
		///
		/// The call first applies [`Config::RecoveryOffsets`] to the queue head. The settled part
		/// gives claimable collateral to the depositor and does not enter the pool.
		///
		/// The remainder enters a maturity cohort. Its activation wait is in
		/// `[entry_delay, 2 * entry_delay)`, unless the delay is zero. A new deposit restarts the
		/// wait for all pending capital of the depositor.
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

		/// Request a withdrawal of up to `amount` active stablecoin.
		///
		/// ## Dispatch Origin
		///
		/// The dispatch origin of this call must be signed.
		///
		/// ## Details
		///
		/// Safety mode records a request that becomes valid after `safety_withdrawal_delay`. A new
		/// request replaces the current request.
		///
		/// Normal mode does not need a request, so this call immediately pays the caller.
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

		/// Withdraw up to `amount` active stablecoin.
		///
		/// ## Dispatch Origin
		///
		/// The dispatch origin of this call must be signed.
		///
		/// ## Details
		///
		/// Normal mode pays immediately. Safety mode requires a [`Pallet::request_withdraw`] that
		/// has completed its delay.
		///
		/// The call pays the smaller of `amount` and the current active deposit. This limit permits
		/// a successful withdrawal after a liquidation reduces the deposit.
		///
		/// `recipient` selects another beneficiary. `None` pays the caller.
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

		/// Pay out all claimable collateral of the caller.
		///
		/// ## Dispatch Origin
		///
		/// The dispatch origin of this call must be signed.
		///
		/// ## Details
		///
		/// `recipient` selects another beneficiary. `None` pays the caller.
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

		/// Pay out all claimable stablecoin yield of the caller.
		///
		/// ## Dispatch Origin
		///
		/// The dispatch origin of this call must be signed.
		///
		/// ## Details
		///
		/// Claimable yield does not absorb debt. [`Pallet::compound_yield`] can move it into the
		/// active deposit.
		///
		/// `recipient` selects another beneficiary. `None` pays the caller.
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

		/// Settle up to `max_stable_in` active pool stablecoin against the head of the
		/// `FinalRecovery` queue.
		///
		/// ## Dispatch Origin
		///
		/// The dispatch origin of this call must be signed. Any account may call it.
		///
		/// ## Details
		///
		/// [`Config::RecoveryOffsets`] supplies the price policy and executes the settlement. The
		/// queue head must be at or above par.
		///
		/// The call reduces recovery debt and gives the collateral to active depositors through
		/// `S`.
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

		/// Move up to `amount` claimable yield of the caller into the active deposit.
		///
		/// ## Dispatch Origin
		///
		/// The dispatch origin of this call must be signed.
		///
		/// ## Details
		///
		/// The moved amount immediately absorbs debt and earns gains. This call requires the
		/// depositor's authority because it exposes claimable yield to pool risk.
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

		/// Settle the recorded position of `owner` against the current pool state.
		///
		/// ## Dispatch Origin
		///
		/// The dispatch origin of this call must be signed. Any account may call it.
		///
		/// ## Details
		///
		/// The call moves no value and does not control activation. Permissionless settlement
		/// removes unused rows and checkpoint references when an owner becomes inactive.
		///
		/// This action lets checkpoint cleanup and market removal continue when accounts are
		/// inactive.
		#[pallet::call_index(7)]
		#[pallet::weight(T::WeightInfo::settle_deposit())]
		pub fn settle_deposit(
			origin: OriginFor<T>,
			owner: T::AccountId,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			Self::do_settle_deposit(owner, collateral_id, stable_id)
		}

		/// Replace the pool parameters of a market.
		///
		/// ## Dispatch Origin
		///
		/// The dispatch origin of this call must satisfy [`Config::UpdateOrigin`] for the market.
		///
		/// ## Details
		///
		/// The `precision` parameters must equal the registered values. Stored deposit snapshots
		/// depend on those values. See [`Error::AccumulatorParamsImmutable`].
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
		/// Returns the custody account of a market's pool.
		///
		/// The account backs active deposits, pending deposits, claimable yield, and unclaimed
		/// collateral.
		pub fn pool_account(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
		) -> T::AccountId {
			pusd_primitives::market_sub_account(T::PalletId::get(), collateral_id, stable_id)
		}

		/// Sends unowned balances to the dust handlers and returns the amounts sent.
		///
		/// Market removal requires no deposit rows. Balances that remain are rounding remainders or
		/// unrelated transfers, not user claims.
		///
		/// The sweep prevents a later registration of the same market from receiving those
		/// balances.
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
		/// Pool parameters required when the market is registered.
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

			// A provider reference keeps the pool account alive while it holds nothing, without
			// locking up an existential deposit.
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
			// Deposit rows are what guards user funds: active, pending and claimable value all
			// live on them. Vaults rolls the whole `remove_branch` back on this error, so a market
			// admin cannot strand depositor funds.
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
			// Safe to clear in full: with no deposit rows, no snapshot on either leg can still
			// refer to a sums row, and no row can reference a cohort checkpoint.
			let removal = PoolSumsStore::<T>::clear_prefix(
				(collateral_id.clone(), stable_id.clone()),
				u32::MAX,
				None,
			);
			debug_assert!(removal.maybe_cursor.is_none());
			let removal = CohortCheckpoints::<T>::clear_prefix(
				(collateral_id.clone(), stable_id.clone()),
				u32::MAX,
				None,
			);
			debug_assert!(removal.maybe_cursor.is_none());

			if frame_system::Pallet::<T>::providers(&pool_account) > 0 {
				// The sweep emptied the market's own assets, but a third party may have parked
				// another token here and left a consumer reference behind. The account then stays,
				// which does no harm.
				let _ = frame_system::Pallet::<T>::dec_providers(&pool_account);
			}
			Ok(())
		}

		/// Returns valid parameters that do not add unrelated limits to a benchmark.
		///
		/// `p_min * scale_factor` equals one, which is the maximum product that
		/// [`types::PoolPrecision::is_valid`] accepts.
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
