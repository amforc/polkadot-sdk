//! # Vaults
//!
//! Vaults engine for the pUSD protocol. Users lock
//! collateral, mint pUSD, and pick a per-vault annual borrow rate. Redemptions
//! walk the rate index tail-first (lower-rate-first), with a `FinalRecovery`
//! FIFO served before the rate index for last-eligible-vault settlement.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod helpers;
mod interfaces;
mod math;
mod recovery;
pub mod types;
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
	DebtPayment, FrozenReason, FrozenState, RedistributionSnapshot, Vault, VaultDebt, VaultListId,
	VaultStatus,
};
pub use weights::WeightInfo;

/// Runtime-supplied benchmark hooks. The pallet's `Config` only exposes
/// oracle reads (`ProvidePrice`), clock reads (`Time`), and hold-only
/// collateral mutation; the helper fills the write side. The hint-repair
/// budget is read directly from `T::VaultLists::repair_budget()` so it can
/// never drift from what the linked-list pallet enforces.
#[cfg(feature = "runtime-benchmarks")]
pub trait BenchmarkHelper<CollateralId, StableId, AccountId, Balance> {
	/// Must be hold-capable for [`HoldReason::VaultCollateral`].
	fn collateral_asset_id() -> CollateralId;
	/// The coin the benchmark market mints. Must be owner-mintable by the
	/// pallet (the runtime grants the pallet issuer rights).
	fn stable_asset_id() -> StableId;
	fn mint_collateral(collateral_id: CollateralId, who: &AccountId, amount: Balance);
	fn mint_stable(stable_id: StableId, who: &AccountId, amount: Balance);
	fn set_oracle_price(
		collateral_id: CollateralId,
		stable_id: StableId,
		price: frame::arithmetic::FixedU128,
	);
	fn advance_time(ms: u64);
	/// A distinct synthetic `(collateral, stable)` market for prefilling the
	/// branch registry up to `MaxBranches`.
	fn synth_market(seed: u32) -> (CollateralId, StableId);
}

pub(crate) const LOG_TARGET: &str = "runtime::vaults";

/// Convenience macro mirroring `pallet-linked-list`'s log helper.
#[macro_export]
macro_rules! log {
	($level:tt, $pattern:expr $(, $values:expr)* $(,)?) => {
		frame::log::$level!(
			target: $crate::LOG_TARGET,
			concat!("[{:?}] [{}] ", $pattern),
			<frame_system::Pallet<T>>::block_number(),
			<$crate::Pallet::<T> as frame::traits::PalletInfoAccess>::name()
			$(, $values)*
		)
	};
}

#[frame::pallet]
pub mod pallet {
	use super::*;
	use crate::{
		helpers, recovery,
		types::{AdminLevel, BranchAdminInfo, BranchAdmins, BranchConfigGuard},
	};
	use frame::{
		prelude::*,
		traits::{
			fungibles::{
				Balanced as FungiblesBalanced, Inspect as FungiblesInspect,
				Mutate as FungiblesMutate, MutateHold as FungiblesMutateHold,
			},
			Consideration, EnsureOriginWithArg, Footprint, Time,
		},
	};
	use pallet_linked_list::{Position, PriorityProvider, SortedListInterface};
	use pusd_primitives::{BranchModeProvider, OnBranchLifecycle, OnBranchYield, ProvidePrice};

	pub type BalanceOf<T> = <<T as Config>::CollateralAssets as FungiblesInspect<
		<T as frame_system::Config>::AccountId,
	>>::Balance;

	/// Pallet-local time unit: UNIX milliseconds. All vault accounting is done
	/// in concrete `u64` milliseconds rather than a generic `Moment`; the time
	/// provider's `Moment` is pinned to `Millis` via [`Config::TimeProvider`].
	pub type Millis = u64;

	pub type StableCreditOf<T> =
		fungibles::Credit<<T as frame_system::Config>::AccountId, <T as Config>::StableAssets>;

	pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Outer hold-reason type. Must convert from the pallet's
		/// [`HoldReason`] enum so we can hold collateral on user accounts.
		type RuntimeHoldReason: From<HoldReason>;

		/// Identifier for collateral assets.
		type CollateralAssetId: Parameter + Member + Ord + MaxEncodedLen;

		/// Identifier for stable assets.
		type StableAssetId: Parameter + Member + Ord + MaxEncodedLen;

		/// Whether the collateral asset and the stablecoin asset denote the same
		/// underlying asset. The pallet mints stablecoins permissionlessly, so a
		/// coin trusted as some market's collateral would let its owner mint
		/// unbacked collateral; `register_branch` rejects any such collision. The
		/// two id types are distinct, so only the runtime can relate them.
		fn is_same_asset(
			collateral_id: &Self::CollateralAssetId,
			stable_id: &Self::StableAssetId,
		) -> bool;

		/// Multi-asset collateral implementation. Balance must be a
		/// [`FixedPointOperand`] so the pallet's `FixedU128`-based math can
		/// operate on it directly without round-tripping through `u128`.
		type CollateralAssets: FungiblesMutateHold<
			Self::AccountId,
			AssetId = Self::CollateralAssetId,
			Balance: FixedPointOperand,
			Reason = Self::RuntimeHoldReason,
		>;

		/// Multi-asset stable issuance, so one instance can mint several coins.
		/// Shares its `Balance` type with the collateral surface.
		type StableAssets: FungiblesMutate<
				Self::AccountId,
				AssetId = Self::StableAssetId,
				Balance = BalanceOf<Self>,
			> + FungiblesBalanced<Self::AccountId>;

		/// The oracle pricing each collateral asset in the protocol's common
		/// numéraire (USD). Issued coins are treated as $1-pegged at par, so the
		/// price is keyed by collateral alone, not by the `(collateral, stable)`
		/// market: every coin backed by a given collateral reads the same feed.
		type Oracle: ProvidePrice<AssetId = Self::CollateralAssetId, Moment = Millis>;

		/// Market-aware sink for the SP share of minted yield. Implemented by
		/// `pallet-stability-pool` in production. Must consume the credit and
		/// either resolve it (`Balanced::resolve`) or pair it against a
		/// rescind so the imbalance nets to zero. The coin is carried by the
		/// credit (`Credit::asset()`).
		type SpYieldSink: OnBranchYield<
			Self::CollateralAssetId,
			Self::StableAssetId,
			StableCreditOf<Self>,
		>;

		/// Fraction of newly minted pUSD fees routed to `SpYieldSink`. The
		/// remainder is forwarded to `FeeHandler`.
		type SpYieldShare: Get<Permill>;

		/// Runtime-configured destination for the residual (non-SP) share of
		/// minted pUSD fees.
		type FeeHandler: OnUnbalanced<StableCreditOf<Self>>;

		/// Market lifecycle hook: `register_branch` calls `on_registered` so
		/// siblings seed their own per-market rows, and `remove_branch` calls
		/// `on_deregistered` so they tear those rows down again.
		type OnBranchLifecycle: OnBranchLifecycle<Self::CollateralAssetId, Self::StableAssetId>;

		/// Time provider for fee accrual. Its `Moment` is pinned to [`Millis`].
		type TimeProvider: Time<Moment = Millis>;

		/// Origin authorising market creation, keyed by the stable asset. The
		/// default `EnsureAssetOwner` admits the stable asset's owner
		/// (`Some(depositor)`, who locks the creation [`Config::Consideration`])
		/// and Root (`None`, deposit-free). Stable-asset ownership is required
		/// because a market can `mint_into` that coin, bypassing its issuer check.
		type CreateOrigin: EnsureOriginWithArg<
			Self::RuntimeOrigin,
			Self::StableAssetId,
			Success = Option<Self::AccountId>,
		>;

		/// Refundable deposit a non-Root creator locks for a market's lifetime,
		/// returned by `remove_branch`.
		type Consideration: Consideration<Self::AccountId, Footprint>;

		/// Governance envelope bounding every permissionless market's config.
		type BranchConfigGuard: Get<BranchConfigGuard<BalanceOf<Self>>>;

		/// Governance origin owning the systemic per-collateral limits
		/// ([`GlobalDebtCeiling`]) and the `force_freeze`/`force_remove` kill
		/// switch — the hard backstop beneath the permissionless per-market
		/// ceilings, distinct from any per-market admin.
		type GlobalManagerOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// Sorted-DLL backing the per-market rate index and FinalRecovery FIFO.
		/// Configured by the runtime to point at `pallet-linked-list` with
		/// `ListId = VaultListId<Self::CollateralAssetId, Self::StableAssetId>`,
		/// `ItemId = Self::AccountId`, `Priority = FixedU128`.
		type VaultLists: SortedListInterface<
			VaultListId<Self::CollateralAssetId, Self::StableAssetId>,
			Self::AccountId,
			Priority = FixedU128,
		>;

		/// Pallet-derived redistribution holding account (collateral parking
		/// during liquidation handoff).
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// Maximum registered collateral branches.
		#[pallet::constant]
		type MaxBranches: Get<u32> + Get<Option<u32>>;

		/// Maximum vaults the `on_idle` cursor refreshes per block. Bounds
		/// idle-block weight regardless of branch count.
		#[pallet::constant]
		type MaxOnIdleVaultRefresh: Get<u32>;

		/// Weight metadata.
		type WeightInfo: weights::WeightInfo;

		/// See [`crate::BenchmarkHelper`].
		#[cfg(feature = "runtime-benchmarks")]
		type BenchmarkHelper: crate::BenchmarkHelper<
			Self::CollateralAssetId,
			Self::StableAssetId,
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
			NMapKey<Twox64Concat, T::CollateralAssetId>,
			NMapKey<Twox64Concat, T::StableAssetId>,
			NMapKey<Blake2_128Concat, T::AccountId>,
		),
		Vault<BalanceOf<T>>,
		OptionQuery,
	>;

	/// Per-market governance/risk parameters. The count gates `MaxBranches`;
	/// the collateral-major key lets the per-collateral risk fold prefix-iterate
	/// one collateral's markets.
	#[pallet::storage]
	pub type BranchConfigs<T: Config> = CountedStorageNMap<
		_,
		(NMapKey<Twox64Concat, T::CollateralAssetId>, NMapKey<Twox64Concat, T::StableAssetId>),
		BranchConfig<BalanceOf<T>>,
		OptionQuery,
		GetDefault,
		T::MaxBranches,
	>;

	/// Per-market hot accounting state. A `DoubleMap` (collateral-major) rather
	/// than an `NMap`: two keys, and the collateral-major outer key still lets a
	/// per-collateral risk fold prefix-iterate one collateral's markets.
	#[pallet::storage]
	pub type BranchStates<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		T::CollateralAssetId,
		Twox64Concat,
		T::StableAssetId,
		BranchState<T::AccountId, BalanceOf<T>>,
		OptionQuery,
		GetDefault,
		T::MaxBranches,
	>;

	/// Governance-set hard cap on total debt per collateral asset, in the
	/// collateral's own unit. Markets sharing a collateral share its
	/// concentration risk, so this caps the sum across them; a single market's
	/// per-branch ceiling cannot. The default of `0` doubles as the collateral
	/// allow-list: a collateral can host markets but cannot be borrowed against
	/// until [`Config::GlobalManagerOrigin`] sets a non-zero ceiling.
	#[pallet::storage]
	pub type GlobalDebtCeiling<T: Config> =
		StorageMap<_, Twox64Concat, T::CollateralAssetId, BalanceOf<T>, ValueQuery>;

	/// Per-market admins and the creation deposit. Present iff the market is
	/// registered; removed (and the deposit refunded) by `remove_branch`.
	#[pallet::storage]
	pub type BranchAdmin<T: Config> = StorageMap<
		_,
		Twox64Concat,
		(T::CollateralAssetId, T::StableAssetId),
		BranchAdminInfo<T::AccountId, T::Consideration>,
	>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A new vault was opened on the market.
		VaultOpened {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
		},
		/// A vault moved between `Active`, `Dormant`, and `FinalRecovery`.
		VaultStatusChanged {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			old_status: VaultStatus,
			new_status: VaultStatus,
		},
		/// The owner was appended to the market's `FinalRecovery` FIFO.
		FinalRecoveryEntered {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
		},
		/// The owner left the market's `FinalRecovery` FIFO.
		FinalRecoveryExited {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
		},
		/// Unbacked circulating debt was recorded against the market ledger.
		BadDebtRecorded {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			amount: BalanceOf<T>,
		},
		/// Recorded bad debt was burned away by an insurance credit.
		BadDebtHealed {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			amount: BalanceOf<T>,
		},
		/// Collateral moved from `from` onto the vault's hold.
		CollateralDeposited {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			from: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// Collateral left the vault's hold for `recipient`.
		CollateralWithdrawn {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// New pUSD was minted to `recipient` against the vault.
		Borrowed {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// `from` burned pUSD against the vault's debt (capped at outstanding).
		Repaid {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			from: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// The vault row was removed; remaining collateral went to `recipient`.
		VaultClosed {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			recipient: T::AccountId,
		},
		/// A touch folded pending interest into the vault's stored debt.
		InterestAccrued {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// An open / borrow / rate-change charged its upfront fee.
		UpfrontFeeCharged {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// The vault's annual borrow rate was re-set (rate index re-sorted).
		BorrowRateChanged {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			old_rate: FixedU128,
			new_rate: FixedU128,
		},
		/// The market entered or left `Frozen` mode.
		ModeChanged {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			old_mode: BranchMode,
			new_mode: BranchMode,
		},
		/// Governance updated one branch-config parameter.
		ParameterUpdated {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			update: BranchConfigUpdate<BalanceOf<T>>,
		},
		/// A new `(collateral, stable)` market was registered.
		BranchRegistered { collateral_id: T::CollateralAssetId, stable_id: T::StableAssetId },
		/// Governance set the per-collateral global debt ceiling.
		GlobalDebtCeilingSet { collateral_id: T::CollateralAssetId, ceiling: BalanceOf<T> },
		/// An empty market was removed; the creation deposit was refunded.
		BranchRemoved { collateral_id: T::CollateralAssetId, stable_id: T::StableAssetId },
		/// A market's admin (`full` or `emergency`) was reassigned.
		BranchAdminChanged {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			full_admin: T::AccountId,
			emergency_admin: T::AccountId,
		},
		/// A redemption cancelled vault debt in exchange for collateral.
		VaultRedeemed {
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			redeemer: T::AccountId,
			debt_cancelled: BalanceOf<T>,
			collateral_to_redeemer: BalanceOf<T>,
			fee_collateral_retained: BalanceOf<T>,
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
		/// Registering would exceed `MaxBranches`.
		TooManyBranches,
		/// The resulting debt would be non-zero but below `minimum_debt`.
		DebtBelowMinimum,
		/// The repayment would leave a non-zero remainder below
		/// `minimum_debt`; repay less, or repay in full.
		DebtWouldBecomeDust,
		/// The borrow would push branch principal above `debt_ceiling`.
		DebtCeilingExceeded,
		/// The borrow would push the collateral's total debt (summed across its
		/// markets, valued in the collateral unit) above `GlobalDebtCeiling`. A
		/// collateral with the default `0` ceiling cannot be borrowed against.
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
		/// The vault's collateralization ratio fails the gate for this
		/// operation (ICR on user ops, MCR on liquidation/recovery paths).
		UnsafeCollateralizationRatio,
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
		/// The `FinalRecovery` insertion sequence overflowed `u128`.
		FinalRecoverySequenceOverflow,
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
		/// The liquidation allocation pays out more collateral than held or
		/// offsets more debt than outstanding.
		InvalidLiquidationAllocation,
		/// The redemption allocation cancels more debt than outstanding or
		/// takes more collateral than held.
		InvalidRedemptionAllocation,
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
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn integrity_test() {
			assert!(<T::MaxBranches as Get<u32>>::get() > 0, "`MaxBranches` must be > 0");
		}

		fn on_idle(_block: BlockNumberFor<T>, remaining: Weight) -> Weight {
			helpers::on_idle_walk::<T>(remaining)
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
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
		) -> Option<FixedU128> {
			helpers::view_vault_cr::<T>(&collateral_id, &stable_id, &owner)
		}

		/// Derived lifecycle status of the vault.
		pub fn vault_status(
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
		) -> Option<VaultStatus> {
			helpers::view_vault_status::<T>(&collateral_id, &stable_id, &owner)
		}

		/// Market TCR, including aggregate interest accrued since the last
		/// update so off-chain observers see the value the runtime would
		/// compute on the next write.
		pub fn branch_tcr(
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
		) -> Option<FixedU128> {
			helpers::view_branch_tcr::<T>(&collateral_id, &stable_id)
		}

		/// Registered `(collateral, stable)` markets.
		pub fn branches() -> alloc::vec::Vec<(T::CollateralAssetId, T::StableAssetId)> {
			BranchConfigs::<T>::iter_keys().collect()
		}

		/// First `n` vault owners in actual redemption order: `FinalRecovery`
		/// FIFO first, then `dormant_redemption_target`, then the rate index
		/// tail-first.
		pub fn redemption_queue_head(
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			n: u32,
		) -> alloc::vec::Vec<T::AccountId> {
			helpers::redemption_targets::<T>(&collateral_id, &stable_id)
				.take(n as usize)
				.collect()
		}

		/// First `n` `FinalRecovery` owners in FIFO order.
		pub fn final_recovery_queue_head(
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			n: u32,
		) -> alloc::vec::Vec<T::AccountId> {
			recovery::queue_head::<T>(&collateral_id, &stable_id, n)
		}

		/// Rate-index insert hint for `rate` on the market.
		pub fn find_rate_position(
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			rate: FixedU128,
		) -> Position<T::AccountId> {
			T::VaultLists::find_position(&VaultListId::Rate(collateral_id, stable_id), rate)
		}

		/// Rate-index re-insert hint for moving the vault to `new_rate`. `None`
		/// if the vault is not in the rate index.
		pub fn find_re_insert_position(
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
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
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
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
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
		) -> Option<Position<T::AccountId>> {
			T::VaultLists::neighbors(&VaultListId::Rate(collateral_id, stable_id), &owner)
		}

		/// Total active-vault interest-bearing debt at rates strictly less
		/// than `rate`, walking at most `max_steps` vaults from the tail.
		/// Returns the partial sum when the cap stops the walk early.
		pub fn debt_in_front(
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			rate: FixedU128,
			max_steps: u32,
		) -> BalanceOf<T> {
			helpers::view_debt_in_front::<T>(&collateral_id, &stable_id, rate, max_steps)
		}

		/// Predict the upfront fee `open_vault` would charge for
		/// `(initial_debt, annual_rate)` against the current market state.
		pub fn predict_open_upfront_fee(
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			initial_debt: BalanceOf<T>,
			annual_rate: FixedU128,
		) -> BalanceOf<T> {
			helpers::predict_upfront_fee_open::<T>(
				&collateral_id,
				&stable_id,
				initial_debt,
				annual_rate,
			)
		}

		/// Predict the upfront fee `borrow` would charge.
		pub fn predict_borrow_upfront_fee(
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			debt_increase: BalanceOf<T>,
			maybe_new_rate: Option<FixedU128>,
		) -> BalanceOf<T> {
			helpers::predict_upfront_fee_borrow::<T>(
				&collateral_id,
				&stable_id,
				&owner,
				debt_increase,
				maybe_new_rate,
			)
		}

		/// Predict the upfront fee `change_rate` would charge — `0` when the
		/// cooldown has elapsed.
		pub fn predict_rate_change_upfront_fee(
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			new_rate: FixedU128,
		) -> BalanceOf<T> {
			helpers::predict_upfront_fee_rate_change::<T>(
				&collateral_id,
				&stable_id,
				&owner,
				new_rate,
			)
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Open a new vault.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::open_vault())]
		pub fn open_vault(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			initial_collateral: BalanceOf<T>,
			initial_debt: BalanceOf<T>,
			annual_rate: FixedU128,
			hint: Position<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			helpers::open_vault::<T>(
				who,
				collateral_id,
				stable_id,
				initial_collateral,
				initial_debt,
				annual_rate,
				hint,
			)
		}

		/// Permissionless deposit-into-vault: caller spends their own
		/// collateral to deposit into `(collateral_id, owner)`.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::deposit_collateral_for())]
		pub fn deposit_collateral_for(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let from = ensure_signed(origin)?;
			helpers::deposit_collateral_for::<T>(from, owner, collateral_id, stable_id, amount)
		}

		/// Withdraw collateral from caller's vault on `collateral_id`.
		/// `recipient` defaults to the caller when `None`.
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::withdraw_collateral())]
		pub fn withdraw_collateral(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			amount: BalanceOf<T>,
			recipient: Option<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let recipient = recipient.unwrap_or_else(|| who.clone());
			helpers::withdraw_collateral::<T>(who, collateral_id, stable_id, amount, recipient)
		}

		/// Borrow more pUSD from caller's vault, optionally adjusting the
		/// rate. May revive a `Dormant` vault. `recipient` of the minted pUSD
		/// defaults to the caller when `None`.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::borrow())]
		pub fn borrow(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			amount: BalanceOf<T>,
			maybe_new_rate: Option<FixedU128>,
			recipient: Option<T::AccountId>,
			hint: Position<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let recipient = recipient.unwrap_or_else(|| who.clone());
			helpers::borrow::<T>(
				who,
				collateral_id,
				stable_id,
				amount,
				maybe_new_rate,
				recipient,
				hint,
			)
		}

		/// Permissionless repay-into-vault. `amount` is capped at the
		/// outstanding debt. Repaying it all clears the debt but keeps the vault
		/// open as a zero-debt Dormant husk (call `close_vault` to reclaim the
		/// collateral); a husk with no collateral left is closed outright.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::repay_for())]
		pub fn repay_for(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let from = ensure_signed(origin)?;
			helpers::repay_for::<T>(from, owner, collateral_id, stable_id, amount)
		}

		/// Change the borrow rate of caller's vault.
		#[pallet::call_index(5)]
		#[pallet::weight(T::WeightInfo::change_rate())]
		pub fn change_rate(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			new_rate: FixedU128,
			hint: Position<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			helpers::change_rate::<T>(who, collateral_id, stable_id, new_rate, hint)
		}

		/// Close caller's vault and reclaim its collateral. The vault must have
		/// zero debt (repay it in full first).
		#[pallet::call_index(6)]
		#[pallet::weight(T::WeightInfo::close_vault())]
		pub fn close_vault(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			recipient: Option<T::AccountId>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			helpers::close_vault::<T>(who, collateral_id, stable_id, recipient)
		}

		/// Permissionless: refresh aggregate/vault interest and apply pending
		/// redistribution to `(collateral_id, owner)`.
		#[pallet::call_index(7)]
		#[pallet::weight(T::WeightInfo::poke())]
		pub fn poke(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			helpers::poke::<T>(owner, collateral_id, stable_id)
		}

		/// Permissionless: move an unsafe last-eligible vault into
		/// `FinalRecovery`.
		#[pallet::call_index(8)]
		#[pallet::weight(T::WeightInfo::enter_final_recovery())]
		pub fn enter_final_recovery(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			helpers::enter_final_recovery::<T>(owner, collateral_id, stable_id)
		}

		/// Permissionless: exit `FinalRecovery` once the fully-accrued vault CR
		/// is back above `MinimumCollateralizationRatio`. Caller supplies the
		/// rate-index `hint` used to reinsert in O(1).
		#[pallet::call_index(9)]
		#[pallet::weight(T::WeightInfo::exit_final_recovery())]
		pub fn exit_final_recovery(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			hint: Position<T::AccountId>,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			helpers::exit_final_recovery::<T>(owner, collateral_id, stable_id, hint)
		}

		/// Permissionless market creation. The stable asset's owner (or Root,
		/// deposit-free) opens a `(collateral, stable)` market with `full_admin`
		/// and `emergency_admin`, and a config inside the governance envelope.
		#[pallet::call_index(10)]
		#[pallet::weight(T::WeightInfo::register_branch())]
		pub fn create_branch(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			full_admin: T::AccountId,
			emergency_admin: T::AccountId,
			config: BranchConfig<BalanceOf<T>>,
		) -> DispatchResult {
			let depositor = T::CreateOrigin::ensure_origin(origin, &stable_id)?;
			helpers::create_branch::<T>(
				collateral_id,
				stable_id,
				BranchAdmins { full_admin, emergency_admin },
				config,
				depositor,
			)
		}

		#[pallet::call_index(11)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_minimum_collateralization_ratio(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			ratio: FixedU128,
		) -> DispatchResult {
			helpers::set_param::<T>(
				origin,
				collateral_id,
				stable_id,
				BranchConfigUpdate::MinimumCollateralizationRatio(ratio),
			)
		}

		#[pallet::call_index(12)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_initial_collateralization_ratio(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			ratio: FixedU128,
		) -> DispatchResult {
			helpers::set_param::<T>(
				origin,
				collateral_id,
				stable_id,
				BranchConfigUpdate::InitialCollateralizationRatio(ratio),
			)
		}

		#[pallet::call_index(13)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_safety_collateralization_ratio(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			ratio: FixedU128,
		) -> DispatchResult {
			helpers::set_param::<T>(
				origin,
				collateral_id,
				stable_id,
				BranchConfigUpdate::SafetyCollateralizationRatio(ratio),
			)
		}

		#[pallet::call_index(14)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_debt_ceiling(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			ceiling: BalanceOf<T>,
		) -> DispatchResult {
			helpers::set_param::<T>(
				origin,
				collateral_id,
				stable_id,
				BranchConfigUpdate::DebtCeiling(ceiling),
			)
		}

		#[pallet::call_index(15)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_minimum_debt(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			minimum_debt: BalanceOf<T>,
		) -> DispatchResult {
			helpers::set_param::<T>(
				origin,
				collateral_id,
				stable_id,
				BranchConfigUpdate::MinimumDebt(minimum_debt),
			)
		}

		#[pallet::call_index(16)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_minimum_collateral(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			minimum_collateral: BalanceOf<T>,
		) -> DispatchResult {
			helpers::set_param::<T>(
				origin,
				collateral_id,
				stable_id,
				BranchConfigUpdate::MinimumCollateral(minimum_collateral),
			)
		}

		#[pallet::call_index(17)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_borrow_rate_bounds(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			min_rate: FixedU128,
			max_rate: FixedU128,
		) -> DispatchResult {
			helpers::set_param::<T>(
				origin,
				collateral_id,
				stable_id,
				BranchConfigUpdate::BorrowRateBounds { min: min_rate, max: max_rate },
			)
		}

		#[pallet::call_index(18)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_upfront_fee_period(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			period: Millis,
		) -> DispatchResult {
			helpers::set_param::<T>(
				origin,
				collateral_id,
				stable_id,
				BranchConfigUpdate::UpfrontFeePeriod(period),
			)
		}

		#[pallet::call_index(19)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_rate_adjustment_cooldown(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			cooldown: Millis,
		) -> DispatchResult {
			helpers::set_param::<T>(
				origin,
				collateral_id,
				stable_id,
				BranchConfigUpdate::RateAdjustmentCooldown(cooldown),
			)
		}

		#[pallet::call_index(20)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_redistribution_penalty(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			penalty: Permill,
		) -> DispatchResult {
			helpers::set_param::<T>(
				origin,
				collateral_id,
				stable_id,
				BranchConfigUpdate::RedistributionPenalty(penalty),
			)
		}

		/// Freeze the market. Either admin tier may issue this — a defensive
		/// override.
		#[pallet::call_index(21)]
		#[pallet::weight(T::WeightInfo::enable_frozen_mode())]
		pub fn enable_frozen_mode(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
		) -> DispatchResult {
			helpers::ensure_branch_admin::<T>(
				origin,
				&collateral_id,
				&stable_id,
				AdminLevel::Emergency,
			)?;
			helpers::enable_frozen_mode::<T>(&collateral_id, &stable_id)
		}

		/// Permissionless: clear an oracle-induced `Frozen` state once the
		/// oracle is healthy again. No-op when the market is not frozen or is
		/// frozen for a non-oracle reason.
		#[pallet::call_index(22)]
		#[pallet::weight(T::WeightInfo::refresh_branch())]
		pub fn refresh_branch(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			helpers::refresh_branch::<T>(&collateral_id, &stable_id)
		}

		/// Full-admin: clear a governance-induced `Frozen` state. No-op when the
		/// market is not frozen or is frozen for a non-governance reason.
		/// Oracle-induced freezes must be cleared with `refresh_branch`.
		#[pallet::call_index(23)]
		#[pallet::weight(T::WeightInfo::clear_governance_frozen_mode())]
		pub fn clear_governance_frozen_mode(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
		) -> DispatchResult {
			helpers::ensure_branch_admin::<T>(
				origin,
				&collateral_id,
				&stable_id,
				AdminLevel::Full,
			)?;
			helpers::clear_governance_frozen_mode::<T>(&collateral_id, &stable_id)
		}

		/// Full-admin: remove an empty market, refunding the creation deposit and
		/// freeing the `MaxBranches` slot.
		#[pallet::call_index(24)]
		#[pallet::weight(T::WeightInfo::register_branch())]
		pub fn remove_branch(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
		) -> DispatchResult {
			helpers::ensure_branch_admin::<T>(
				origin,
				&collateral_id,
				&stable_id,
				AdminLevel::Full,
			)?;
			helpers::remove_branch::<T>(collateral_id, stable_id)
		}

		/// Full-admin: reassign the market's admins.
		#[pallet::call_index(25)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_branch_admins(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			full_admin: T::AccountId,
			emergency_admin: T::AccountId,
		) -> DispatchResult {
			helpers::ensure_branch_admin::<T>(
				origin,
				&collateral_id,
				&stable_id,
				AdminLevel::Full,
			)?;
			BranchAdmin::<T>::try_mutate(
				(&collateral_id, &stable_id),
				|maybe| -> Result<_, DispatchError> {
					let info = maybe.as_mut().ok_or(Error::<T>::UnknownCollateral)?;
					info.full_admin = full_admin.clone();
					info.emergency_admin = emergency_admin.clone();
					Ok(())
				},
			)?;
			Self::deposit_event(Event::BranchAdminChanged {
				collateral_id,
				stable_id,
				full_admin,
				emergency_admin,
			});
			Ok(())
		}

		/// Governance kill switch: freeze any market, bypassing its admins.
		#[pallet::call_index(26)]
		#[pallet::weight(T::WeightInfo::enable_frozen_mode())]
		pub fn force_freeze(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
		) -> DispatchResult {
			T::GlobalManagerOrigin::ensure_origin(origin)?;
			helpers::enable_frozen_mode::<T>(&collateral_id, &stable_id)
		}

		/// Governance kill switch: remove any empty market, bypassing its admins.
		#[pallet::call_index(27)]
		#[pallet::weight(T::WeightInfo::register_branch())]
		pub fn force_remove(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
		) -> DispatchResult {
			T::GlobalManagerOrigin::ensure_origin(origin)?;
			helpers::remove_branch::<T>(collateral_id, stable_id)
		}

		/// Permissionless: revive a `Dormant` vault whose fully-accrued debt is
		/// back at or above `MinimumDebt`, reinserting it into the rate index at
		/// the caller-supplied `hint`. Returns `Pays::No` on a successful flip.
		#[pallet::call_index(28)]
		#[pallet::weight(T::WeightInfo::activate_dormant())]
		pub fn activate_dormant(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			owner: T::AccountId,
			hint: Position<T::AccountId>,
		) -> DispatchResultWithPostInfo {
			let _ = ensure_signed(origin)?;
			helpers::activate_dormant::<T>(owner, collateral_id, stable_id, hint)?;
			Ok(Pays::No.into())
		}

		/// Set the per-collateral global debt ceiling, in the collateral's unit.
		/// `0` blocks borrowing against the collateral (the allow-list default).
		/// The systemic backstop beneath the per-market ceilings.
		#[pallet::call_index(29)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_global_debt_ceiling(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			ceiling: BalanceOf<T>,
		) -> DispatchResult {
			T::GlobalManagerOrigin::ensure_origin(origin)?;
			GlobalDebtCeiling::<T>::insert(&collateral_id, ceiling);
			Self::deposit_event(Event::GlobalDebtCeilingSet { collateral_id, ceiling });
			Ok(())
		}

		/// Permissionless: advance a market's autoline ceiling. Increases are gated
		/// by the configured `ceiling_ttl`; decreases apply immediately. No-op when
		/// the market's autoline is disabled (`ceiling_gap == 0`).
		#[pallet::call_index(30)]
		#[pallet::weight(T::WeightInfo::poke())]
		pub fn poke_ceiling(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;
			helpers::poke_ceiling::<T>(collateral_id, stable_id)
		}

		/// Full-admin: set the market's autoline headroom (`ceiling_gap`). `0`
		/// disables the autoline, pinning the borrow cap to the static `debt_ceiling`.
		#[pallet::call_index(31)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_ceiling_gap(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			ceiling_gap: BalanceOf<T>,
		) -> DispatchResult {
			helpers::set_param::<T>(
				origin,
				collateral_id,
				stable_id,
				BranchConfigUpdate::CeilingGap(ceiling_gap),
			)
		}

		/// Full-admin: set the minimum time between autoline ceiling increases
		/// (`ceiling_ttl`), the slow-up gate.
		#[pallet::call_index(32)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_ceiling_ttl(
			origin: OriginFor<T>,
			collateral_id: T::CollateralAssetId,
			stable_id: T::StableAssetId,
			ceiling_ttl: Millis,
		) -> DispatchResult {
			helpers::set_param::<T>(
				origin,
				collateral_id,
				stable_id,
				BranchConfigUpdate::CeilingTtl(ceiling_ttl),
			)
		}
	}

	/// `BranchModeProvider` implementation so other pallets can query the
	/// derived/persisted mode without depending on us at the trait surface.
	impl<T: Config> BranchModeProvider<T::CollateralAssetId, T::StableAssetId> for Pallet<T> {
		fn mode(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
		) -> Option<BranchMode> {
			helpers::current_mode::<T>(collateral_id, stable_id).ok()
		}
	}

	/// `PriorityProvider` so `pallet-linked-list` can read authoritative rates
	/// from us when relisting a drifted node.
	impl<T: Config>
		PriorityProvider<VaultListId<T::CollateralAssetId, T::StableAssetId>, T::AccountId>
		for Pallet<T>
	{
		type Priority = FixedU128;
		fn priority(
			list_id: &VaultListId<T::CollateralAssetId, T::StableAssetId>,
			item: &T::AccountId,
		) -> Option<FixedU128> {
			match list_id {
				VaultListId::Rate(collateral_id, stable_id) => {
					Vaults::<T>::get((collateral_id, stable_id, item)).map(|v| v.annual_rate)
				},
				VaultListId::FinalRecovery(..) => T::VaultLists::priority(list_id, item),
			}
		}
	}

	impl<T: Config> Pallet<T> {
		/// Per-market account holding that market's redistribution-pending
		/// collateral. Derived from the `(collateral, stable)` pair via a bounded
		/// hash preimage so a large asset-id pair cannot overflow the sub-account
		/// seed and collide across markets.
		pub fn redistribution_account(
			collateral_id: &T::CollateralAssetId,
			stable_id: &T::StableAssetId,
		) -> T::AccountId {
			let seed =
				frame::deps::sp_io::hashing::blake2_256(&(collateral_id, stable_id).encode());
			T::PalletId::get().into_sub_account_truncating(&seed[..24])
		}
	}
}
