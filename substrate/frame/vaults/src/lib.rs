//! # Vaults
//!
//! Vaults engine for the pUSD protocol. Users lock
//! collateral, mint pUSD, and pick a per-vault annual borrow rate. Redemptions
//! walk the rate index tail-first (lower-rate-first), with a `FinalRecovery`
//! FIFO served before the rate index for last-eligible-vault settlement.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod context;
mod dispatchable_impls;
mod interfaces;
mod math;
mod recovery;
pub mod types;
mod utility_impls;
pub mod weights;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

#[cfg(feature = "try-runtime")]
mod try_state;

#[cfg(test)]
pub mod mock;

#[cfg(test)]
mod tests;

pub use pallet::*;
pub use pusd_primitives;
pub use types::{
	BranchConfig, BranchConfigUpdate, BranchDebt, BranchMode, BranchStakes, BranchState,
	FrozenReason, FrozenState, RedistributionSnapshot, Vault, VaultDebt, VaultListId, VaultStatus,
};
pub use weights::WeightInfo;

/// Runtime-supplied benchmark hooks. The pallet's `Config` only exposes
/// oracle reads (`ProvidePrice`), clock reads (`Time`), and hold-only
/// collateral mutation; the helper fills the write side. The hint-repair
/// budget is read directly from `T::VaultLists::repair_budget()` so it can
/// never drift from what the linked-list pallet enforces.
#[cfg(feature = "runtime-benchmarks")]
pub trait BenchmarkHelper<CollateralId, StableId, AccountId, Balance> {
	fn collateral_asset_id() -> CollateralId;
	fn stable_asset_id() -> StableId;
	fn mint_collateral(collateral_id: CollateralId, who: &AccountId, amount: Balance);
	fn mint_stable(stable_id: StableId, who: &AccountId, amount: Balance);
	fn set_oracle_price(collateral_id: CollateralId, price: frame::arithmetic::FixedU128);
	fn clear_oracle_price(collateral_id: CollateralId);
	fn advance_time(ms: u64);
}

#[frame::pallet]
pub mod pallet {
	use super::*;
	use crate::{
		context::BranchOp,
		recovery,
		types::{AdminLevel, AssetRoleUsage, BranchAdmins, BranchConfigGuard},
	};
	use frame::{
		prelude::*,
		traits::{
			fungibles::{
				Balanced as FungiblesBalanced, Inspect as FungiblesInspect,
				Mutate as FungiblesMutate, MutateHold as FungiblesMutateHold,
			},
			Consideration, EnsureOriginWithArg, Footprint, OriginTrait, Time,
		},
	};
	use pallet_linked_list::{Position, PriorityProvider, SortedListInterface};
	use pusd_primitives::{OnBranchLifecycle, OnBranchYield, ProvidePrice};

	pub type BalanceOf<T> = <<T as Config>::CollateralAssets as FungiblesInspect<
		<T as frame_system::Config>::AccountId,
	>>::Balance;

	/// Collateral identifier exposed by [`Config::CollateralAssets`].
	pub type CollateralIdOf<T> = <<T as Config>::CollateralAssets as FungiblesInspect<
		<T as frame_system::Config>::AccountId,
	>>::AssetId;

	/// Stablecoin identifier exposed by [`Config::StableAssets`].
	pub type StableIdOf<T> = <<T as Config>::StableAssets as FungiblesInspect<
		<T as frame_system::Config>::AccountId,
	>>::AssetId;

	/// Protocol time unit: UNIX milliseconds. All vault accounting is done in
	/// concrete `u64` milliseconds rather than a generic `Moment`; the time
	/// provider's `Moment` is pinned to `Millis` via [`Config::TimeProvider`].
	pub use pusd_primitives::Millis;

	pub type StableCreditOf<T> =
		fungibles::Credit<<T as frame_system::Config>::AccountId, <T as Config>::StableAssets>;
	pub type CollateralCreditOf<T> =
		fungibles::Credit<<T as frame_system::Config>::AccountId, <T as Config>::CollateralAssets>;

	/// The [`Branches`] record, instantiated for the runtime.
	pub type BranchOf<T> = crate::types::Branch<
		PalletsOriginOf<T>,
		<T as frame_system::Config>::AccountId,
		BalanceOf<T>,
		<T as Config>::Consideration,
	>;

	/// The runtime's origin-caller type. Market admins are stored as origin callers, so a
	/// governance track or collective can administer a market.
	pub type PalletsOriginOf<T> =
		<<T as frame_system::Config>::RuntimeOrigin as OriginTrait>::PalletsOrigin;

	pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Outer hold-reason type. Must convert from the pallet's
		/// [`HoldReason`] enum so we can hold collateral on user accounts.
		type RuntimeHoldReason: From<HoldReason>;

		/// Multi-asset collateral implementation. Balance must be a
		/// [`FixedPointOperand`] so the pallet's `FixedU128`-based math can
		/// operate on it directly without round-tripping through `u128`.
		type CollateralAssets: FungiblesMutateHold<
				Self::AccountId,
				AssetId: Parameter + Member + Ord + MaxEncodedLen,
				Balance: FixedPointOperand,
				Reason = Self::RuntimeHoldReason,
			> + fungibles::BalancedHold<Self::AccountId>;

		/// Multi-asset stable issuance, so one instance can mint several coins.
		/// Shares its `Balance` type with the collateral surface.
		type StableAssets: FungiblesMutate<
				Self::AccountId,
				AssetId: Parameter + Member + Ord + MaxEncodedLen,
				Balance = BalanceOf<Self>,
			> + FungiblesBalanced<Self::AccountId>;

		/// Converts a stable-asset id into the collateral-id namespace. The
		/// converted id is the canonical key under which [`AssetRoles`] tracks
		/// the coin, so a stablecoin can never be trusted as collateral (and vice
		/// versa) anywhere in the registry.
		type StableToCollateralId: Convert<StableIdOf<Self>, CollateralIdOf<Self>>;

		/// The oracle pricing each collateral asset in the protocol's common
		/// numéraire (USD). Issued coins are treated as $1-pegged at par, so the
		/// price is keyed by collateral alone, not by the `(collateral, stable)`
		/// market: every coin backed by a given collateral reads the same feed.
		type Oracle: ProvidePrice<AssetId = CollateralIdOf<Self>>;

		/// Destination for minted pUSD fees (branch interest and upfront fees).
		/// The credit carries the coin (`Credit::asset()`), so a runtime can
		/// route revenue per stablecoin. Receives what [`Config::YieldHook`]
		/// leaves.
		type FeeHandler: OnUnbalanced<StableCreditOf<Self>>;

		/// Takes the Stability Pool's share of every stable-coin credit the
		/// engine mints for a market (branch interest and upfront fees); the
		/// remainder goes to [`Config::FeeHandler`]. Runtimes without a pool
		/// use `()`.
		type YieldHook: OnBranchYield<CollateralIdOf<Self>, StableIdOf<Self>, StableCreditOf<Self>>;

		/// Market lifecycle hook: `register_branch` calls `on_registered` so
		/// siblings seed their own per-market rows, and `remove_branch` calls
		/// `on_deregistered` so they tear those rows down again.
		type OnBranchLifecycle: OnBranchLifecycle<CollateralIdOf<Self>, StableIdOf<Self>>;

		/// Time provider for fee accrual. Its `Moment` is pinned to [`Millis`].
		type TimeProvider: Time<Moment = Millis>;

		/// Origin authorising market creation, keyed by the stable asset. The
		/// default `EnsureAssetOwner` admits the stable asset's owner
		/// (`Some(depositor)`, who locks the creation [`Config::Consideration`])
		/// and Root (`None`, deposit-free). Stable-asset ownership is required
		/// because a market can `mint_into` that coin, bypassing its issuer check.
		type CreateOrigin: EnsureOriginWithArg<
			Self::RuntimeOrigin,
			StableIdOf<Self>,
			Success = Option<Self::AccountId>,
		>;

		/// Refundable deposit a non-Root creator locks for a market's lifetime,
		/// returned by `remove_branch`.
		type Consideration: Consideration<Self::AccountId, Footprint>;

		/// Governance envelope bounding every permissionless market's config.
		type BranchConfigGuard: Get<BranchConfigGuard<BalanceOf<Self>>>;

		/// Governance origin owning the systemic per-collateral limits
		/// ([`CollateralRisks`]) and able to freeze or remove any market via
		/// `set_governance_frozen`/`remove_branch`, bypassing its admins — the
		/// hard backstop beneath the permissionless per-market ceilings,
		/// distinct from any per-market admin.
		type GlobalManagerOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// Sorted-DLL backing the per-market rate index and FinalRecovery FIFO.
		/// Configured by the runtime to point at `pallet-linked-list` with
		/// `ListId = VaultListId<CollateralIdOf<Self>, StableIdOf<Self>>`,
		/// `ItemId = Self::AccountId`, `Priority = FixedU128`.
		type VaultLists: SortedListInterface<
			VaultListId<CollateralIdOf<Self>, StableIdOf<Self>>,
			Self::AccountId,
			Priority = FixedU128,
		>;

		/// Pallet-derived redistribution holding account (collateral parking
		/// during liquidation handoff).
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// Maximum weight the `on_idle` refresh walk may consume out of a
		/// block's leftover weight. `None` skips the walk entirely; vaults
		/// then refresh only when an extrinsic touches them.
		#[pallet::constant]
		type IdleMaxRefreshWeight: Get<Option<Weight>>;

		/// Weight metadata.
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

	/// Hold reason used to lock collateral against the vault owner's
	/// account.
	#[pallet::composite_enum]
	pub enum HoldReason {
		/// Held collateral backing an open vault.
		VaultCollateral,
		/// The refundable deposit a market creator locks via the
		/// [`Config::Consideration`].
		MarketCreationDeposit,
	}

	/// Source-of-truth vault rows, keyed by `(collateral_id, stable_id, owner)`.
	#[pallet::storage]
	pub type Vaults<T: Config> = StorageNMap<
		_,
		(
			NMapKey<Twox64Concat, CollateralIdOf<T>>,
			NMapKey<Twox64Concat, StableIdOf<T>>,
			NMapKey<Blake2_128Concat, T::AccountId>,
		),
		Vault<BalanceOf<T>>,
		OptionQuery,
	>;

	/// The authoritative market registry: the `(collateral, stable)` markets'
	/// config, hot state, admins, and creation deposit.
	#[pallet::storage]
	pub type Branches<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		CollateralIdOf<T>,
		Twox64Concat,
		StableIdOf<T>,
		BranchOf<T>,
		OptionQuery,
	>;

	/// Which side of a market each asset is used on, reference-counted per
	/// registered market.
	#[pallet::storage]
	pub type AssetRoles<T: Config> =
		StorageMap<_, Twox64Concat, CollateralIdOf<T>, AssetRoleUsage, OptionQuery>;

	/// Per-collateral systemic risk record: the [`Config::GlobalManagerOrigin`]-set
	/// hard cap on total debt across the collateral's markets, and the stored
	/// aggregate of that debt (`Σ BranchDebt::outstanding()`). Markets sharing a
	/// collateral share its concentration risk, so the cap binds their sum; a
	/// single market's per-branch ceiling cannot. The `outstanding` side is a
	/// derived index over [`Branches`], not a second ledger: maintained by
	/// the audited `commit_branch` boundary, default records removed,
	/// recomputed in full by `try_state`. Summing raw units across
	/// stablecoins assumes $1 par — see the TODO on `ensure_global_ceiling`.
	#[pallet::storage]
	pub type CollateralRisks<T: Config> = StorageMap<
		_,
		Twox64Concat,
		CollateralIdOf<T>,
		crate::types::CollateralRisk<BalanceOf<T>>,
		ValueQuery,
	>;

	/// Cursor of the `on_idle` refresh walk over [`Vaults`]: the key of the
	/// last row touched, resumed after on the next idle block. `None` restarts
	/// from the front of the map.
	#[pallet::storage]
	pub type IdleCursor<T: Config> =
		StorageValue<_, (CollateralIdOf<T>, StableIdOf<T>, T::AccountId), OptionQuery>;

	/// Cursor of the `on_idle` refresh walk over [`Branches`]: the key of the
	/// last market reconciled, resumed after on the next idle block. `None`
	/// restarts from the front of the map.
	#[pallet::storage]
	pub type BranchIdleCursor<T: Config> =
		StorageValue<_, (CollateralIdOf<T>, StableIdOf<T>), OptionQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A new vault was opened on the market.
		VaultOpened {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
		},
		/// A vault moved between `Active`, `Dormant`, and `FinalRecovery`.
		VaultStatusChanged {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			old_status: VaultStatus,
			new_status: VaultStatus,
		},
		/// Unbacked circulating debt was recorded against the market ledger.
		BadDebtRecorded {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			amount: BalanceOf<T>,
		},
		/// Recorded bad debt was burned away by an insurance credit.
		BadDebtHealed {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			amount: BalanceOf<T>,
		},
		/// Collateral moved from `from` onto the vault's hold.
		CollateralDeposited {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			from: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Collateral left the vault's hold for `recipient`.
		CollateralWithdrawn {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// New pUSD was minted to `recipient` against the vault.
		Borrowed {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// `from` burned pUSD against the vault's debt (capped at outstanding).
		Repaid {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			from: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// The vault row was removed; remaining collateral went to `recipient`.
		VaultClosed {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			recipient: T::AccountId,
		},
		/// A touch folded pending interest into the vault's stored debt.
		InterestAccrued {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// An open / borrow / rate-change charged its upfront fee.
		UpfrontFeeCharged {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// The vault's annual borrow rate was re-set (rate index re-sorted).
		BorrowRateChanged {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			old_rate: FixedU128,
			new_rate: FixedU128,
		},
		/// The market entered or left `Frozen` mode.
		ModeChanged {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			old_mode: BranchMode,
			new_mode: BranchMode,
		},
		/// Governance updated one branch-config parameter.
		ParameterUpdated {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			update: BranchConfigUpdate<BalanceOf<T>>,
		},
		/// A new `(collateral, stable)` market was registered.
		BranchRegistered { collateral_id: CollateralIdOf<T>, stable_id: StableIdOf<T> },
		/// Governance set the per-collateral global debt ceiling.
		GlobalDebtCeilingSet { collateral_id: CollateralIdOf<T>, ceiling: BalanceOf<T> },
		/// An empty market was removed; the creation deposit was refunded.
		BranchRemoved { collateral_id: CollateralIdOf<T>, stable_id: StableIdOf<T> },
		/// A market's admin (`full` or `emergency`) was reassigned.
		BranchAdminChanged {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			full_admin: PalletsOriginOf<T>,
			emergency_admin: PalletsOriginOf<T>,
		},
		/// A redemption cancelled vault debt in exchange for collateral.
		VaultRedeemed {
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			recipient: T::AccountId,
			debt_cancelled: BalanceOf<T>,
			collateral_to_recipient: BalanceOf<T>,
			vault_annual_rate: FixedU128,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// No vault row exists for this `(collateral, owner)` pair.
		VaultNotFound,
		/// The owner already has a vault on this branch (one per pair).
		VaultAlreadyExists,
		/// The vault's lifecycle status does not admit this operation.
		InvalidVaultStatus,
		/// The operation is not available while the vault sits in the
		/// `FinalRecovery` FIFO.
		VaultInFinalRecovery,
		/// No branch is registered for this collateral asset.
		UnknownCollateral,
		/// The stable asset does not exist.
		UnknownStable,
		/// A market's stablecoin asset is already trusted as collateral (here or in
		/// a sibling market), or its collateral is already minted as a stablecoin.
		StableCollateralCollision,
		/// A branch for this collateral asset already exists.
		BranchAlreadyRegistered,
		/// The resulting debt would be non-zero but below `minimum_debt`.
		DebtBelowMinimum,
		/// The repayment would leave a non-zero remainder below
		/// `minimum_debt`; repay less, or repay in full.
		DebtWouldBecomeDust,
		/// The borrow would push branch principal above `debt_ceiling`.
		DebtCeilingExceeded,
		/// The borrow would push the collateral's total debt (summed across its
		/// markets, valued in the collateral unit) above the
		/// [`CollateralRisks`] ceiling. A collateral with the default `0`
		/// ceiling cannot be borrowed against.
		GlobalDebtCeilingExceeded,
		/// The caller is not a (sufficiently-privileged) admin of this market.
		NotBranchAdmin,
		/// The market still has debt, bad debt, stake, pending redistribution,
		/// locked collateral, or vault rows; it cannot be removed.
		MarketNotEmpty,
		/// The config sits outside the governance `BranchConfigGuard` envelope.
		ConfigOutsideEnvelope,
		/// The annual rate is outside the branch's configured bounds.
		RateOutOfBounds,
		/// The vault's collateralization ratio is too low for this operation
		/// (below ICR on user ops, below MCR on `exit_final_recovery`).
		UnsafeCollateralizationRatio,
		/// The vault's fully-accrued collateralization ratio is at or above
		/// MCR — too healthy to enter `FinalRecovery`.
		CollateralizationRatioTooHealthy,
		/// In Safety mode, the operation would lower the branch TCR.
		SafetyModeTcrWorsening,
		/// In Normal mode, the operation would drop the branch TCR below the
		/// safety threshold.
		WouldEnterSafetyMode,
		/// The branch is frozen (governance or oracle failure).
		BranchFrozen,
		/// The oracle returned no price for this collateral.
		OraclePriceNotAvailable,
		/// The oracle price is older than its validity window.
		OracleStale,
		/// The supplied rate-index position hint is stale beyond the repair
		/// budget; fetch a fresh hint and retry.
		InvalidPositionHints,
		/// The rate index and the vault rows disagree — storage corruption,
		/// not a user error.
		RateIndexInvariantBroken,
		/// The `FinalRecovery` FIFO and the vault rows disagree — storage
		/// corruption, not a user error.
		FinalRecoveryInvariantBroken,
		/// `enter_final_recovery` requires the candidate to be the branch's
		/// only remaining stake-bearer.
		NotLastEligibleVault,
		/// The vault holds less collateral than the operation needs.
		InsufficientCollateral,
		/// `close_vault` requires zero debt; repay the vault in full first.
		DebtOutstanding,
		/// A checked arithmetic operation overflowed.
		ArithmeticOverflow,
		/// An `Emergency`-tier admin tried a parameter change in the
		/// non-defensive (risk-increasing) direction.
		DefensiveActionNotDefensive,
		/// The liquidation settlement returns invalid collateral credits or
		/// offsets more debt than outstanding.
		InvalidLiquidationSettlement,
		/// The redemption settlement pays zero or in the wrong coin, cancels
		/// more debt than outstanding, or takes more collateral than held.
		InvalidRedemptionSettlement,
		/// Liquidating the branch's last stake-bearing vault would leave no
		/// redistribution recipients; it must go through `FinalRecovery`.
		LastVaultCannotBeLiquidated,
		/// Redistribution per-stake math overflowed; the liquidation cannot
		/// be finalized with these amounts.
		RedistributionWouldOverflow,
		/// The vault is not eligible for liquidation (fully-accrued CR at or
		/// above MCR).
		VaultNotLiquidatable,
		/// The branch's single `dormant_redemption_target` slot is already held
		/// by a different debt-bearing vault.
		DormantTargetOccupied,
		/// A borrow must mint a non-zero amount; use `change_rate` to adjust only
		/// the vault's rate.
		ZeroBorrowAmount,
		/// A collateral deposit must transfer a non-zero amount.
		ZeroDepositAmount,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_idle(_block: BlockNumberFor<T>, remaining: Weight) -> Weight {
			Self::on_idle_walk(remaining)
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(_: BlockNumberFor<T>) -> Result<(), frame::try_runtime::TryRuntimeError> {
			crate::try_state::do_try_state::<T>()
		}
	}

	/// View functions exposed to runtime API consumers (wallets, indexers).
	#[pallet::view_functions]
	impl<T: Config> Pallet<T> {
		/// Fully-accrued collateralization ratio of the vault.
		pub fn vault_cr(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
		) -> Option<FixedU128> {
			let vault = Vaults::<T>::get((&collateral_id, &stable_id, &owner))?;
			let state = Branches::<T>::get(&collateral_id, &stable_id)?.state;
			let now = T::TimeProvider::now();
			let price = T::Oracle::provide_price(&collateral_id).ok()?;
			let pending = Self::pending_touch_for(&vault, &state, now);
			let total_coll = vault.collateral.saturating_add(pending.collateral);
			let total_debt = pending.total_debt(&vault.debt);
			pusd_primitives::collateralization_ratio::<BalanceOf<T>>(total_coll, total_debt, price)
		}

		/// Derived lifecycle status of the vault.
		pub fn vault_status(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
		) -> Option<VaultStatus> {
			Vaults::<T>::contains_key((&collateral_id, &stable_id, &owner))
				.then(|| Self::vault_status_of(&collateral_id, &stable_id, &owner))
		}

		/// Market TCR, including aggregate interest accrued since the last
		/// update so off-chain observers see the value the runtime would
		/// compute on the next write.
		pub fn branch_tcr(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
		) -> Option<FixedU128> {
			let state = Branches::<T>::get(&collateral_id, &stable_id)?.state;
			let price = T::Oracle::provide_price(&collateral_id).ok()?;
			let now = T::TimeProvider::now();
			Self::compute_tcr(&state, price, now).ok()
		}

		/// First `n` vault owners in actual redemption order: `FinalRecovery`
		/// FIFO first, then `dormant_redemption_target`, then the rate index
		/// tail-first.
		pub fn redemption_queue_head(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			n: u32,
		) -> alloc::vec::Vec<T::AccountId> {
			Self::redemption_targets(&collateral_id, &stable_id)
				.map(|(owner, _kind)| owner)
				.take(n as usize)
				.collect()
		}

		/// First `n` `FinalRecovery` owners in FIFO order.
		pub fn final_recovery_queue_head(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			n: u32,
		) -> alloc::vec::Vec<T::AccountId> {
			recovery::queue_head::<T>(&collateral_id, &stable_id, n)
		}

		/// Rate-index insert hint for `rate` on the market.
		pub fn find_rate_position(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			rate: FixedU128,
		) -> Position<T::AccountId> {
			T::VaultLists::find_position(&VaultListId::Rate(collateral_id, stable_id), rate)
		}

		/// Rate-index re-insert hint for moving the vault to `new_rate`. `None`
		/// if the vault is not in the rate index.
		pub fn find_re_insert_position(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			new_rate: FixedU128,
		) -> Option<Position<T::AccountId>> {
			T::VaultLists::find_re_insert_position(
				&VaultListId::Rate(collateral_id, stable_id),
				&owner,
				new_rate,
			)
		}

		/// Steps the on-chain repair walk would take for `(rate, hint)` on the
		/// market.
		pub fn repair_steps_needed(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			rate: FixedU128,
			hint: Position<T::AccountId>,
		) -> u32 {
			T::VaultLists::repair_steps_needed(
				&VaultListId::Rate(collateral_id, stable_id),
				rate,
				hint,
			)
		}

		/// Current rate-index neighbors of the vault. `None` when the vault is
		/// not in the rate index.
		pub fn vault_rate_index_neighbors(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
		) -> Option<Position<T::AccountId>> {
			T::VaultLists::neighbors(&VaultListId::Rate(collateral_id, stable_id), &owner)
		}

		/// How much debt a redemption consumes before reaching `rate`: the
		/// projected entire debt of the dormant redemption target (drained
		/// first, whatever its rate), plus that of every listed vault at a
		/// rate strictly below it. Walks at most `max_steps` vaults —
		/// the dormant target counts as one — and returns the partial sum if
		/// the cap stops the walk.
		pub fn debt_in_front(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			rate: FixedU128,
			max_steps: u32,
		) -> BalanceOf<T> {
			let mut total = BalanceOf::<T>::zero();
			let Some(branch) = Branches::<T>::get(&collateral_id, &stable_id) else {
				return total;
			};
			let now = T::TimeProvider::now();
			let mut steps_left = max_steps;
			if let Some(target) = &branch.state.dormant_redemption_target {
				if steps_left == 0 {
					return total;
				}
				steps_left -= 1;
				if let Some(v) = Vaults::<T>::get((&collateral_id, &stable_id, target)) {
					let pending = Self::pending_touch_for(&v, &branch.state, now);
					total = total.saturating_add(pending.total_debt(&v.debt));
				}
			}
			let rate_list = VaultListId::Rate(collateral_id.clone(), stable_id.clone());
			let mut cursor = T::VaultLists::tail(&rate_list);
			for _ in 0..steps_left {
				let Some(o) = cursor else { break };
				let Some((priority, neighbors)) = T::VaultLists::node(&rate_list, &o) else {
					break;
				};
				if priority >= rate {
					break;
				}
				if let Some(v) = Vaults::<T>::get((&collateral_id, &stable_id, &o)) {
					let pending = Self::pending_touch_for(&v, &branch.state, now);
					total = total.saturating_add(pending.total_debt(&v.debt));
				}
				cursor = neighbors.prev;
			}
			total
		}

		/// Predict the upfront fee `open_vault` would charge for
		/// `(initial_debt, annual_rate)` against the current market state.
		pub fn predict_open_upfront_fee(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			initial_debt: BalanceOf<T>,
			annual_rate: FixedU128,
		) -> BalanceOf<T> {
			let Some(branch) = Branches::<T>::get(&collateral_id, &stable_id) else {
				return BalanceOf::<T>::zero();
			};
			let (config, mut state) = (branch.config, branch.state);
			let now = T::TimeProvider::now();
			let mut scratch = Self::open_scratch_row(&state, annual_rate, Zero::zero(), now);
			Self::apply_borrow_unchecked(
				&mut state,
				&config,
				&mut scratch,
				initial_debt,
				annual_rate,
				now,
			)
		}

		/// Predict the upfront fee `borrow` would charge.
		pub fn predict_borrow_upfront_fee(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			debt_increase: BalanceOf<T>,
			maybe_new_rate: Option<FixedU128>,
		) -> BalanceOf<T> {
			if debt_increase.is_zero() {
				return BalanceOf::<T>::zero();
			}
			let Some((config, mut state, mut vault)) =
				Self::predict_inputs(&collateral_id, &stable_id, &owner)
			else {
				return BalanceOf::<T>::zero();
			};
			let new_rate = maybe_new_rate.unwrap_or(vault.annual_rate);
			let now = T::TimeProvider::now();
			Self::apply_borrow_unchecked(
				&mut state,
				&config,
				&mut vault,
				debt_increase,
				new_rate,
				now,
			)
		}

		/// Predict the upfront fee `change_rate` would charge — `0` when the
		/// cooldown has elapsed.
		pub fn predict_rate_change_upfront_fee(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			new_rate: FixedU128,
		) -> BalanceOf<T> {
			let Some((config, mut state, mut vault)) =
				Self::predict_inputs(&collateral_id, &stable_id, &owner)
			else {
				return BalanceOf::<T>::zero();
			};
			let now = T::TimeProvider::now();
			Self::apply_rate_change(&mut state, &config, &mut vault, new_rate, now)
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Open a new vault.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::open_vault())]
		pub fn open_vault(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			initial_collateral: BalanceOf<T>,
			initial_debt: BalanceOf<T>,
			annual_rate: FixedU128,
			hint: Position<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_open_vault(
				who,
				collateral_id,
				stable_id,
				initial_collateral,
				initial_debt,
				annual_rate,
				hint,
			)
		}

		/// Permissionless deposit-into-vault: caller spends a non-zero amount of
		/// their own collateral to deposit into `(collateral_id, owner)`.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::deposit_collateral_for())]
		pub fn deposit_collateral_for(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let from = ensure_signed(origin)?;
			Self::do_deposit_collateral_for(from, owner, collateral_id, stable_id, amount)
		}

		/// Withdraw collateral from caller's vault on `collateral_id`.
		/// `recipient` defaults to the caller when `None`. Withdrawing the
		/// last collateral from a debt-free vault closes it outright.
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::withdraw_collateral())]
		pub fn withdraw_collateral(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			amount: BalanceOf<T>,
			recipient: Option<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let recipient = recipient.unwrap_or_else(|| who.clone());
			Self::do_withdraw_collateral(who, collateral_id, stable_id, amount, recipient)
		}

		/// Borrow a non-zero amount of pUSD from caller's vault, optionally
		/// adjusting the rate. Use `change_rate` to adjust the rate without
		/// borrowing. May revive a `Dormant` vault. `recipient` of the minted
		/// pUSD defaults to the caller when `None`.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::borrow())]
		pub fn borrow(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			amount: BalanceOf<T>,
			maybe_new_rate: Option<FixedU128>,
			recipient: Option<T::AccountId>,
			hint: Position<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let recipient = recipient.unwrap_or_else(|| who.clone());
			Self::do_borrow(who, collateral_id, stable_id, amount, maybe_new_rate, recipient, hint)
		}

		/// Permissionless repay-into-vault. `amount` is capped at the
		/// outstanding debt. Repaying it all clears the debt but keeps the vault
		/// open as a zero-debt Dormant husk (call `close_vault` to reclaim the
		/// collateral); a husk with no collateral left is closed outright.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::repay_for())]
		pub fn repay_for(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let from = ensure_signed(origin)?;
			Self::do_repay_for(from, owner, collateral_id, stable_id, amount)
		}

		/// Change the borrow rate of caller's vault.
		#[pallet::call_index(5)]
		#[pallet::weight(T::WeightInfo::change_rate())]
		pub fn change_rate(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			new_rate: FixedU128,
			hint: Position<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_change_rate(who, collateral_id, stable_id, new_rate, hint)
		}

		/// Close caller's vault and reclaim its collateral. The vault must have
		/// zero debt (repay it in full first).
		#[pallet::call_index(6)]
		#[pallet::weight(T::WeightInfo::close_vault())]
		pub fn close_vault(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			recipient: Option<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_close_vault(who, collateral_id, stable_id, recipient)
		}

		/// Permissionless: refresh aggregate/vault interest and apply pending
		/// redistribution to `(collateral_id, owner)`.
		#[pallet::call_index(7)]
		#[pallet::weight(T::WeightInfo::poke())]
		pub fn poke(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			BranchOp::<T>::refresh(collateral_id, stable_id, &owner)
		}

		/// Permissionless: move an unsafe last-eligible vault into
		/// `FinalRecovery`.
		#[pallet::call_index(8)]
		#[pallet::weight(T::WeightInfo::enter_final_recovery())]
		pub fn enter_final_recovery(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			Self::do_enter_final_recovery(owner, collateral_id, stable_id)
		}

		/// Permissionless: exit `FinalRecovery` once the fully-accrued vault CR
		/// is back above `MinimumCollateralizationRatio`. Caller supplies the
		/// rate-index `hint` used to reinsert in O(1).
		#[pallet::call_index(9)]
		#[pallet::weight(T::WeightInfo::exit_final_recovery())]
		pub fn exit_final_recovery(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			hint: Position<T::AccountId>,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			Self::do_exit_final_recovery(owner, collateral_id, stable_id, hint)
		}

		/// Permissionless market creation. The stable asset's owner (or Root,
		/// deposit-free) opens a `(collateral, stable)` market with the given
		/// `admins` and a config inside the governance envelope.
		#[pallet::call_index(10)]
		#[pallet::weight(T::WeightInfo::create_branch())]
		pub fn create_branch(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			admins: BranchAdmins<PalletsOriginOf<T>>,
			config: BranchConfig<BalanceOf<T>>,
		) -> DispatchResult {
			let depositor = T::CreateOrigin::ensure_origin(origin, &stable_id)?;
			Self::do_create_branch(collateral_id, stable_id, admins, config, depositor)
		}

		/// Admin: update one branch-config parameter. The required admin tier,
		/// the `Emergency`-only defensive-direction rule, and the governance
		/// envelope check are all derived from the `update` itself; see
		/// [`BranchConfigUpdate`].
		#[pallet::call_index(11)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_param(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			update: BranchConfigUpdate<BalanceOf<T>>,
		) -> DispatchResult {
			Self::do_set_param(origin, collateral_id, stable_id, update)
		}

		/// Set (`true`) or clear (`false`) the governance-induced `Frozen`
		/// state. Freezing is a defensive override: either admin tier may
		/// issue it, and so may [`Config::GlobalManagerOrigin`] as the
		/// governance kill switch that bypasses the market's admins.
		/// Unfreezing is `Full`-admin only. No-op when the market is already
		/// frozen (any reason) on freeze, or not governance-frozen on clear;
		/// oracle-induced freezes are cleared with `refresh_branch`.
		#[pallet::call_index(12)]
		#[pallet::weight(T::WeightInfo::set_governance_frozen())]
		pub fn set_governance_frozen(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			frozen: bool,
		) -> DispatchResult {
			if frozen {
				Self::ensure_branch_admin_or_manager(
					origin,
					&collateral_id,
					&stable_id,
					AdminLevel::Emergency,
				)?;
			} else {
				Self::ensure_branch_admin(origin, &collateral_id, &stable_id, AdminLevel::Full)?;
			}
			Self::do_set_governance_frozen(&collateral_id, &stable_id, frozen)
		}

		/// Permissionless: clear an oracle-induced `Frozen` state once the
		/// oracle is healthy again. No-op when the market is not frozen or is
		/// frozen for a non-oracle reason.
		#[pallet::call_index(13)]
		#[pallet::weight(T::WeightInfo::refresh_branch())]
		pub fn refresh_branch(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			Self::do_refresh_branch(&collateral_id, &stable_id)
		}

		/// Full-admin (or [`Config::GlobalManagerOrigin`], bypassing the market's
		/// admins): remove an empty market, refunding the creation deposit and
		/// releasing both assets' role references.
		#[pallet::call_index(14)]
		#[pallet::weight(T::WeightInfo::remove_branch())]
		pub fn remove_branch(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
		) -> DispatchResult {
			Self::ensure_branch_admin_or_manager(
				origin,
				&collateral_id,
				&stable_id,
				AdminLevel::Full,
			)?;
			Self::do_remove_branch(collateral_id, stable_id)
		}

		/// Full-admin: reassign the market's admins.
		#[pallet::call_index(15)]
		#[pallet::weight(T::WeightInfo::set_branch_admins())]
		pub fn set_branch_admins(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			admins: BranchAdmins<PalletsOriginOf<T>>,
		) -> DispatchResult {
			Self::ensure_branch_admin(origin, &collateral_id, &stable_id, AdminLevel::Full)?;
			Branches::<T>::try_mutate_exists(
				&collateral_id,
				&stable_id,
				|maybe| -> Result<(), DispatchError> {
					let branch = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
					branch.admins = admins.clone();
					Ok(())
				},
			)?;
			let BranchAdmins { full_admin, emergency_admin } = admins;
			Self::deposit_event(Event::BranchAdminChanged {
				collateral_id,
				stable_id,
				full_admin,
				emergency_admin,
			});
			Ok(())
		}

		/// Permissionless: revive a `Dormant` vault whose fully-accrued debt is
		/// back at or above `MinimumDebt`, reinserting it into the rate index at
		/// the caller-supplied `hint`. Returns `Pays::No` on a successful flip.
		#[pallet::call_index(16)]
		#[pallet::weight(T::WeightInfo::activate_dormant())]
		pub fn activate_dormant(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			hint: Position<T::AccountId>,
		) -> DispatchResultWithPostInfo {
			let _ = ensure_signed(origin)?;
			Self::do_activate_dormant(owner, collateral_id, stable_id, hint)?;
			Ok(Pays::No.into())
		}

		/// Set the per-collateral global debt ceiling, in the collateral's unit.
		/// `0` blocks borrowing against the collateral (the allow-list default).
		/// The systemic backstop beneath the per-market ceilings.
		#[pallet::call_index(17)]
		#[pallet::weight(T::WeightInfo::set_global_debt_ceiling())]
		pub fn set_global_debt_ceiling(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			ceiling: BalanceOf<T>,
		) -> DispatchResult {
			T::GlobalManagerOrigin::ensure_origin(origin)?;
			CollateralRisks::<T>::mutate_exists(&collateral_id, |maybe| {
				let mut risk = maybe.take().unwrap_or_default();
				risk.debt_ceiling = ceiling;
				*maybe = (risk != Default::default()).then_some(risk);
			});
			Self::deposit_event(Event::GlobalDebtCeilingSet { collateral_id, ceiling });
			Ok(())
		}

		/// Permissionless: advance a market's autoline ceiling. Increases are gated
		/// by the configured `ceiling_ttl`; decreases apply immediately. No-op when
		/// the market's autoline is disabled (`ceiling_gap == 0`).
		#[pallet::call_index(18)]
		#[pallet::weight(T::WeightInfo::poke_ceiling())]
		pub fn poke_ceiling(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			Self::do_poke_ceiling(collateral_id, stable_id)
		}
	}

	/// `PriorityProvider` so `pallet-linked-list` can read authoritative rates
	/// from us when relisting a drifted node.
	impl<T: Config> PriorityProvider<VaultListId<CollateralIdOf<T>, StableIdOf<T>>, T::AccountId>
		for Pallet<T>
	{
		type Priority = FixedU128;
		fn priority(
			list_id: &VaultListId<CollateralIdOf<T>, StableIdOf<T>>,
			item: &T::AccountId,
		) -> Option<FixedU128> {
			match list_id {
				VaultListId::Rate(collateral_id, stable_id) => {
					Vaults::<T>::get((collateral_id, stable_id, item)).map(|v| v.annual_rate)
				},
				// FIFO lists never drift: the stored priority (assigned once
				// at insertion) is authoritative. This also serves sibling
				// pallets' FIFO variants on the shared list instance.
				VaultListId::FinalRecovery(..) | VaultListId::StabilityPending(..) => {
					T::VaultLists::priority(list_id, item)
				},
			}
		}
	}

	impl<T: Config> Pallet<T> {
		/// Per-market account holding that market's redistribution-pending
		/// collateral.
		pub fn redistribution_account(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
		) -> T::AccountId {
			let seed =
				frame::deps::sp_io::hashing::blake2_256(&(collateral_id, stable_id).encode());
			T::PalletId::get().into_sub_account_truncating(&seed[..24])
		}
	}
}
