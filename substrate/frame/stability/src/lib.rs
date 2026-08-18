//! # Stability Pool
//!
//! Cancels the debt of liquidated vaults with depositor stablecoin, and pays the seized collateral
//! to those depositors.
//!
//! ## Overview
//!
//! A market is one collateral asset paired with one stablecoin. Every market has one pool.
//! `pallet-vaults` creates the pool when it registers the market, and removes the pool when it
//! removes the market.
//!
//! Depositors supply stablecoin to a pool. The liquidation engine spends that stablecoin to cancel
//! the debt of unhealthy vaults, and gives the pool the collateral of those vaults in exchange. The
//! collateral is worth more than the debt it cancels, so a depositor holds less stablecoin and more
//! collateral after each liquidation, for a net gain. The pool also takes a share of the interest
//! and the fees that the market collects, and pays that share to depositors as stablecoin yield.
//!
//! Depositors keep control of their funds. They leave with [`Pallet::withdraw`] and collect their
//! gains with [`Pallet::claim_collateral`] and [`Pallet::claim_yield`].
//!
//! ### The deposit lifecycle
//!
//! New stablecoin does not become active at once. It waits out `entry_delay` as a *pending
//! deposit*, and earns no yield while it waits. The delay stops an account from joining the pool
//! just before a liquidation, taking the collateral bonus, and leaving again.
//!
//! A pending deposit still carries risk. When the active pool cannot absorb a liquidation in full,
//! the liquidation engine takes the remainder from all pending deposits, in proportion to their
//! size. Pending capital is the last-resort backstop of the market, and it cannot avoid a
//! liquidation by waiting.
//!
//! A matured pending deposit becomes active on the next write to its row. Any account can supply
//! that write with [`Pallet::poke_deposit`], so a depositor never depends on another party.
//!
//! ### Gains
//!
//! Every write to a deposit row *realizes* it: the row settles its share of all liquidations and
//! all yield since its last write. Losses reduce the active deposit. Gains become claimable
//! balances, which stay on the row until the depositor asks for them.
//!
//! Claimable yield is stablecoin, but it is not part of the active deposit. It absorbs no
//! liquidations and earns nothing. [`Pallet::compound_yield`] moves it into the active deposit,
//! where it starts to absorb liquidations and to earn gains. Only the depositor can make that
//! move.
//!
//! ### Operating modes
//!
//! [`Config::BranchModes`] reports the operating mode of a market:
//!
//! - `Normal`: all calls are available, and a withdrawal pays out at once.
//! - `Safety`: a withdrawal needs a prior [`Pallet::request_withdraw`] and a wait of
//!   `safety_withdrawal_delay`. The wait keeps the pool available to absorb liquidations while the
//!   market is under stress.
//! - `Frozen`: every call that moves value fails. A market with no usable price is frozen too,
//!   because the mode provider fails closed.
//!
//! ### Recovery offsets
//!
//! A vault in `FinalRecovery` is wound down at a settlement price that `pallet-redemptions` owns.
//! Any account can call [`Pallet::offset_recovery`] to burn active pool stablecoin against the
//! head of that queue at the same price. Depositors receive the collateral, as they do from a
//! liquidation.
//!
//! [`Pallet::deposit`] runs the same settlement on the incoming stablecoin before the remainder
//! becomes a pending deposit. The settled part never enters the pool, and its collateral goes
//! straight to the depositor. A head below par rejects the deposit, because settlement at a
//! discount stays exclusive to the explicit redemption path.
//!
//! ## Pallet API
//!
//! See the [`pallet`] module for more information about the interfaces this pallet exposes,
//! including its configuration trait, dispatchables, storage items, events and errors.
//!
//! ## Low Level / Implementation Details
//!
//! ### Design goals
//!
//! A liquidation must cost the same whether the pool holds ten depositors or ten million. The
//! pallet therefore never iterates over depositors. A liquidation writes a fixed number of global
//! values, and each depositor derives its own share from those values on its next touch.
//!
//! ### Product-sum accounting
//!
//! Each pool keeps three global accumulators:
//!
//! - `P`, the fraction of a deposit that survives all liquidations so far;
//! - `S`, the collateral paid per unit of deposit;
//! - `G`, the stablecoin yield paid per unit of deposit.
//!
//! A deposit stores the values of `P`, `S` and `G` that were current when it was last written.
//! The distance between the stored values and the live values is the loss and the gains of that
//! deposit. The `math` module holds the formulas.
//!
//! `P` only falls. A large liquidation drives it towards zero and costs the smaller deposits their
//! precision. Two counters protect against this:
//!
//! - `scale` increases when `P` falls below `p_min`. `P` is multiplied back up by `scale_factor`,
//!   and a later read divides by that factor once per boundary crossed.
//! - `epoch` increases when a liquidation empties the pool. `P` returns to one. A deposit from an
//!   earlier epoch compounds to zero and keeps only the gains it had already earned.
//!
//! ### The two legs
//!
//! Active deposits and pending deposits each own a `P` and `S` pair, stored in [`PoolSumsStore`]
//! under a [`types::Leg`] key. Both legs run the same code. Pending deposits earn no yield, so
//! their `G` is always zero.
//!
//! ### Rounding
//!
//! Every payout to a user rounds down. The remainder stays inside the pool totals, so the pool
//! account balance always equals the sum of what the pool owes. Market teardown sweeps whatever is
//! left to [`Config::StableDustHandler`] and [`Config::CollateralDustHandler`].

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

	/// Balance of both the stablecoin and the collateral surface.
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

	/// Stablecoin taken out of issuance, and not yet placed back into it.
	pub type StableCreditOf<T> =
		fungibles::Credit<<T as frame_system::Config>::AccountId, <T as Config>::StableAssets>;

	/// Collateral taken out of issuance, and not yet placed back into it.
	pub type CollateralCreditOf<T> =
		fungibles::Credit<<T as frame_system::Config>::AccountId, <T as Config>::CollateralAssets>;

	/// One row of [`Deposits`].
	pub type DepositOf<T> = Deposit<BalanceOf<T>>;

	/// The accounting state of one pool.
	pub type PoolStateOf<T> = PoolState<BalanceOf<T>>;

	/// The governance parameters of one pool.
	pub type StabilityPoolConfigOf<T> = StabilityPoolConfig<BalanceOf<T>>;

	/// One row of [`Pools`].
	pub type StabilityPoolOf<T> = StabilityPool<BalanceOf<T>>;

	/// The storage layout this pallet writes.
	pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The stablecoin surface the pool takes deposits in and pays yield from.
		type StableAssets: fungibles::Inspect<
				Self::AccountId,
				AssetId: Parameter + Member + Ord + MaxEncodedLen,
				Balance: FixedPointOperand,
			> + fungibles::Mutate<Self::AccountId>
			+ fungibles::Balanced<Self::AccountId>;

		/// The collateral surface the pool receives gains on and pays claims from.
		type CollateralAssets: fungibles::Mutate<
				Self::AccountId,
				AssetId: Parameter + Member + Ord + MaxEncodedLen,
				Balance = BalanceOf<Self>,
			> + fungibles::Balanced<Self::AccountId>;

		/// The clock that the entry delay and the Safety-Mode withdrawal delay are measured
		/// against.
		type TimeProvider: Time<Moment = Millis>;

		/// The source of truth for the operating mode of a market.
		type BranchModes: BranchModeProvider<CollateralIdOf<Self>, StableIdOf<Self>>;

		/// Settlement pricing and execution for vaults in `FinalRecovery`.
		type RecoveryOffsets: RecoveryOffsetInterface<
			CollateralId = CollateralIdOf<Self>,
			AccountId = Self::AccountId,
			Balance = BalanceOf<Self>,
			Credit = StableCreditOf<Self>,
		>;

		/// Where the stablecoin left in a pool account at market teardown goes.
		type StableDustHandler: OnUnbalanced<StableCreditOf<Self>>;

		/// Where the collateral left in a pool account at market teardown goes. See
		/// [`Config::StableDustHandler`].
		type CollateralDustHandler: OnUnbalanced<CollateralCreditOf<Self>>;

		/// Authorizes [`Pallet::set_stability_pool_config`] for the market given as the argument.
		type UpdateOrigin: EnsureOriginWithArg<
			Self::RuntimeOrigin,
			(CollateralIdOf<Self>, StableIdOf<Self>),
			Success = (),
		>;

		/// The seed the per-market pool accounts are derived from. See [`Pallet::pool_account`].
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// Weight information for the calls of this pallet.
		type WeightInfo: WeightInfo;
	}

	/// What one account holds in one market.
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

	/// The pool of one registered market.
	///
	/// The row carries the governance parameters and the accounting state together, so neither can
	/// exist without the other. A market is registered exactly while its row exists.
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

	/// The `S` and `G` sums of one leg at one `(epoch, scale)` coordinate.
	///
	/// A deposit realizes against the row its snapshot points at, so a row may only be removed once
	/// no snapshot on its leg still refers to it.
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
		/// Stablecoin entered the pool. `used_for_recovery` settled against a `FinalRecovery`
		/// vault at once; `pending_amount` waits out the entry delay.
		DepositReceived {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			depositor: T::AccountId,
			amount: BalanceOf<T>,
			used_for_recovery: BalanceOf<T>,
			pending_amount: BalanceOf<T>,
		},
		/// A pending deposit passed its entry delay and joined the active pool.
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
		/// Active stablecoin cancelled liquidation debt. `epoch` and `scale` are the coordinates
		/// the active pool holds after the offset.
		PoolOffsetApplied {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			debt_burned: BalanceOf<T>,
			collateral_gain: BalanceOf<T>,
			epoch: u32,
			scale: u32,
		},
		/// Pending deposits cancelled liquidation debt as the last-resort backstop, in proportion
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
		/// Stablecoin settled against the head of the `FinalRecovery` queue. `source` says whether
		/// the stablecoin came from the active pool, which takes the collateral through `S`, or
		/// from an incoming deposit, which takes it directly.
		RecoveryOffsetApplied {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			debt_burned: BalanceOf<T>,
			collateral_gain: BalanceOf<T>,
			source: RecoveryOffsetSource,
		},
		/// Governance replaced the pool parameters of a market.
		StabilityPoolConfigUpdated { collateral_id: CollateralIdOf<T>, stable_id: StableIdOf<T> },
		/// Market teardown swept the amounts left in the pool account to the dust handlers.
		DustSwept {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			stable_amount: BalanceOf<T>,
			collateral_amount: BalanceOf<T>,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// This market has no stability pool. Check that the market is registered.
		PoolNotRegistered,
		/// The deposit is below the `minimum_deposit` of the market. Deposit more.
		DepositTooSmall,
		/// The caller holds no deposit in this market.
		DepositNotFound,
		/// The requested amount is zero.
		ZeroAmount,
		/// The withdrawal resolved to zero stablecoin, because the request is used up or the
		/// active deposit is empty.
		NoActiveDeposit,
		/// A Safety-Mode withdrawal needs a prior [`Pallet::request_withdraw`].
		WithdrawalRequestMissing,
		/// The withdrawal request has not passed its `safety_withdrawal_delay` yet. Wait longer.
		SafetyWithdrawalDelayActive,
		/// The market is frozen, so no call may change the risk the pool carries.
		BranchFrozen,
		/// The caller has no claimable collateral.
		NoClaimableCollateral,
		/// The caller has no claimable yield.
		NoClaimableYield,
		/// The caller has no claimable yield to compound.
		NoYieldToCompound,
		/// The offset would move `P` across more scale boundaries than the pallet supports, or it
		/// would overflow the accumulator math. This needs a `minimum_active_pool_balance` far too
		/// small for the size of the pool. Raise that parameter.
		UnsupportedOffsetPrecision,
		/// The pool could not burn the exact debt the caller reserved. Read the reducible amounts
		/// again and retry.
		OffsetSettlementFailed,
		/// This market has no vault in `FinalRecovery`.
		RecoveryVaultNotFound,
		/// The head of the `FinalRecovery` queue is below par, so the pool rejects deposits and
		/// offers no recovery offset. Settlement at a discount stays exclusive to the redemption
		/// path.
		RecoveryOffsetBelowPar,
		/// The recovery offset would burn no stablecoin, because the active pool is empty, the
		/// post-offset floor blocks it, or the request is zero.
		NoRecoveryOffsetPerformed,
		/// The pool parameters contradict each other. See
		/// [`types::StabilityPoolConfig::is_valid`].
		InvalidStabilityPoolConfig,
		/// The `precision` parameters cannot change after registration. A deposit left behind a
		/// scale boundary realizes against the factor that was live when it crossed that boundary,
		/// so a new factor would misprice it.
		AccumulatorParamsImmutable,
		/// The pool still holds deposits, so the market cannot be removed. Ask the depositors to
		/// withdraw first.
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
		/// The amount settles against the head of the `FinalRecovery` queue first, if the market
		/// has one. That part never enters the pool, and its collateral becomes claimable at once.
		/// The rest queues as a pending deposit and joins the active pool once `entry_delay` has
		/// passed and something writes the row again. A later deposit merges into the pending
		/// amount and restarts the delay for the whole of it.
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

		/// Announce a withdrawal of up to `amount` active stablecoin.
		///
		/// ## Dispatch Origin
		///
		/// The dispatch origin of this call must be signed.
		///
		/// ## Details
		///
		/// In Safety Mode the request becomes executable `safety_withdrawal_delay` later, and a
		/// new request replaces any earlier one. In Normal Mode a withdrawal needs no announcement,
		/// so the call withdraws to the caller instead.
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
		/// Normal Mode pays out at once. Safety Mode needs a [`Pallet::request_withdraw`] that has
		/// passed its delay.
		///
		/// The call pays out the smaller of `amount` and the active deposit. A liquidation between
		/// the last read and this call therefore shrinks the payout instead of failing it. Set
		/// `recipient` to send the stablecoin elsewhere; `None` pays the caller.
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
		/// Set `recipient` to send the collateral elsewhere; `None` pays the caller.
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
		/// Claimable yield absorbs no liquidations until [`Pallet::compound_yield`] moves it into
		/// the active deposit. Set `recipient` to send the stablecoin elsewhere; `None` pays the
		/// caller.
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
		/// The settlement uses the price that `pallet-redemptions` owns, and the active depositors
		/// receive the collateral through `S`, as they do from a liquidation. The call is available
		/// while the head of the queue is at or above par.
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
		/// The moved amount starts to absorb liquidations and to earn gains at once. Yield never
		/// becomes exposed to liquidations without this call.
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

		/// Realize the deposit of `owner` and activate a matured pending deposit of theirs.
		///
		/// ## Dispatch Origin
		///
		/// The dispatch origin of this call must be signed. Any account may call it.
		///
		/// ## Details
		///
		/// The call moves no value. Past the entry delay, activation is mechanical and its outcome
		/// does not depend on who asks for it, so a depositor never has to wait for their own next
		/// call.
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

		/// Replace the pool parameters of a market.
		///
		/// ## Dispatch Origin
		///
		/// The dispatch origin of this call must satisfy [`Config::UpdateOrigin`] for the market.
		///
		/// ## Details
		///
		/// The `precision` parameters must equal the stored ones. See
		/// [`Error::AccumulatorParamsImmutable`].
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
		/// The account that holds everything a market's pool owes.
		///
		/// It holds stablecoin for the active deposits, the pending deposits and the undistributed
		/// yield, and collateral for the unclaimed gains.
		pub fn pool_account(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
		) -> T::AccountId {
			pusd_primitives::market_sub_account(T::PalletId::get(), collateral_id, stable_id)
		}

		/// Sends everything left in a pool account to the dust handlers, and returns the amounts
		/// sent.
		///
		/// The caller runs this only once no deposit rows remain, so whatever is left belongs to
		/// nobody. It cannot stay: the pool account balance must equal the pool totals, so a
		/// market registered again on the same pair would start from totals it cannot match.
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
		/// One pool per market, so every registration carries its own parameters.
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
			// refer to a sums row.
			let removal = PoolSumsStore::<T>::clear_prefix(
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

		/// One-unit thresholds and no delays, so nothing in the pool becomes the binding
		/// constraint of a benchmark. `p_min * scale_factor` lands exactly at one, the tightest
		/// rescale that [`types::PoolPrecision::is_valid`] accepts.
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
