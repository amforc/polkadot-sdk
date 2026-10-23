//! # Vaults Pallet
//!
//! Creates collateral-backed stablecoin branches.
//!
//! ## Pallet API
//!
//! See the [`pallet`] module for the pallet's configuration, calls, storage, events, and errors.
//!
//! ## Overview
//!
//! A branch pairs one collateral asset with one stable asset. Each branch has its own risk limits
//! and administrators.
//!
//! A user opens a vault by locking collateral and borrowing the stable asset. Each vault has an
//! annual interest rate chosen by its owner. Interest is simple, not compounding: it accrues
//! linearly on the vault's principal only, never on interest already charged. It is added when the
//! branch or vault is updated.
//! Opening also takes a refundable storage deposit, settled
//! according to the runtime policy.
//!
//! Vaults are ordered by rate for redemptions. Lower-rate vaults are redeemed first. A final
//! recovery queue is served before the rate list when a branch has only one eligible vault left.
//!
//! A branch may enter safety mode when its total collateral ratio is low. It may also be frozen by
//! an administrator or when its oracle price is unavailable.

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

#[cfg(any(feature = "try-runtime", test))]
mod try_state;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

pub use pallet::*;
pub use stablecoin_primitives;
pub use types::{
	BoundViolation, BranchConfig, BranchConfigDefect, BranchConfigUpdate, BranchDebt, BranchMode,
	BranchState, DebtBreakdown, DebtCollateral, FrozenReason, FrozenState, LiquidationSettlement,
	LiquidationSnapshot, RedistributionAccumulators, RedistributionStakeTotals,
	StablecoinDebtState, Vault, VaultListId, VaultRecord, VaultStatus,
};
pub use weights::WeightInfo;

/// Runtime-specific benchmark fixtures and external-state mutation.
#[cfg(feature = "runtime-benchmarks")]
pub trait BenchmarkHelper<CollateralId, StableId, AccountId> {
	/// Returns the benchmark collateral.
	fn collateral_asset_id() -> CollateralId;
	/// Sets the oracle price of a collateral asset.
	fn set_oracle_price(collateral_id: CollateralId, price: frame::arithmetic::FixedU128);
	/// Moves the benchmark clock forward. [`Config::TimeProvider`] only reads the clock.
	fn advance_time(ms: u64);
	/// Idempotently creates collateral and any reference feed needed by the deposit policy.
	/// Benchmarks set the collateral price separately.
	///
	/// A runtime hook supports native and holder-backed assets that lack `fungibles::Create`.
	fn ensure_collateral_asset(collateral_id: CollateralId);
	/// Stages the costliest global debt-ceiling lookup after stablecoin creation.
	/// The runtime owns external inputs, such as PSM debt rows; local-only ceilings need no setup.
	fn stage_debt_ceiling(stable_id: StableId);
	/// Funds native asset-account and storage deposits; `Config` exposes no native currency.
	fn fund_native(who: &AccountId);
}

/// Benchmark-only asset creation and event inspection bounds.
#[cfg(feature = "runtime-benchmarks")]
pub trait BenchmarkConfig:
	Config<
	StableAssets: frame::traits::fungibles::Create<Self::AccountId, AssetId: From<u32>>,
	RuntimeEvent: TryInto<Event<Self>>,
>
{
}

#[cfg(feature = "runtime-benchmarks")]
impl<T> BenchmarkConfig for T where
	T: Config<
		StableAssets: frame::traits::fungibles::Create<T::AccountId, AssetId: From<u32>>,
		RuntimeEvent: TryInto<Event<T>>,
	>
{
}

#[frame::pallet]
pub mod pallet {
	use super::*;
	use crate::{
		context::VaultOp,
		recovery,
		types::{AdminLevel, BranchAdmins, BranchConfigBounds},
	};
	use alloc::{vec, vec::Vec};
	use frame::{
		prelude::*,
		traits::{
			fungibles::{
				Balanced as FungiblesBalanced, Inspect as FungiblesInspect,
				Mutate as FungiblesMutate, MutateHold as FungiblesMutateHold,
			},
			AccountTouch, AssetFootprint, Consideration, EnsureOriginWithArg, Footprint, Time,
		},
	};
	use linked_list_interface::{Position, PriorityProvider, SortedListInterface};
	use stablecoin_primitives::{
		collateralization_ratio, CollateralRatio, OnBranchLifecycle, OnBranchYield, ProvidePrice,
	};

	/// Balance type used by collateral and stable assets.
	pub type BalanceOf<T> =
		<<T as Config>::CollateralAssets as FungiblesInspect<AccountIdFor<T>>>::Balance;

	/// Collateral identifier exposed by [`Config::CollateralAssets`].
	pub type CollateralIdOf<T> =
		<<T as Config>::CollateralAssets as FungiblesInspect<AccountIdFor<T>>>::AssetId;

	/// Stable asset identifier exposed by [`Config::StableAssets`].
	pub type StableIdOf<T> =
		<<T as Config>::StableAssets as FungiblesInspect<AccountIdFor<T>>>::AssetId;

	/// Account lookup type used by administrator calls.
	pub type AccountIdLookupOf<T> = <<T as frame_system::Config>::Lookup as StaticLookup>::Source;

	/// UNIX time in milliseconds.
	pub use stablecoin_primitives::Millis;

	/// Lifecycle registration payload forwarded to [`Config::OnBranchLifecycle`].
	pub type RegistrationConfigOf<T> = <<T as Config>::OnBranchLifecycle as OnBranchLifecycle<
		CollateralIdOf<T>,
		StableIdOf<T>,
		AccountIdFor<T>,
	>>::RegistrationConfig;

	/// Stable-asset credit produced by the pallet.
	pub type StableCreditOf<T> = fungibles::Credit<AccountIdFor<T>, <T as Config>::StableAssets>;
	/// Collateral credit produced by the pallet.
	pub type CollateralCreditOf<T> =
		fungibles::Credit<AccountIdFor<T>, <T as Config>::CollateralAssets>;

	/// List identifier for one of this pallet's per-branch lists.
	pub type VaultListIdOf<T> = VaultListId<CollateralIdOf<T>, StableIdOf<T>>;

	/// Branch record used by the runtime.
	pub type BranchOf<T> =
		crate::types::Branch<AccountIdFor<T>, BalanceOf<T>, <T as Config>::BranchConsideration>;

	/// Stored vault record: the accounting row with its storage deposit.
	pub type VaultRecordOf<T> =
		crate::types::VaultRecord<BalanceOf<T>, <T as Config>::VaultConsideration>;

	/// The in-code storage version.
	pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Multi-asset system used to hold collateral.
		type CollateralAssets: FungiblesMutateHold<
				Self::AccountId,
				AssetId: Parameter + Member + Ord + MaxEncodedLen,
				Balance: FixedPointOperand,
				Reason: From<HoldReason>,
			> + fungibles::BalancedHold<Self::AccountId>;

		/// Multi-asset system used to mint and burn stable assets.
		///
		/// [`AccountTouch`] prepares the fee account to receive sub-minimum credits.
		type StableAssets: FungiblesMutate<
				Self::AccountId,
				AssetId: Parameter + Member + Ord + MaxEncodedLen,
				Balance = BalanceOf<Self>,
			> + FungiblesBalanced<Self::AccountId>
			+ AccountTouch<StableIdOf<Self>, Self::AccountId, Balance = BalanceOf<Self>>;

		/// Converts a stable asset ID into the collateral asset ID type.
		///
		/// The pallet uses the converted ID to stop one asset from being both collateral and
		/// stable.
		type StableToCollateralId: Convert<StableIdOf<Self>, CollateralIdOf<Self>>;

		/// Provides the value of each collateral asset in stable units.
		type Oracle: ProvidePrice<AssetId = CollateralIdOf<Self>>;

		/// Converts each stablecoin ID to its fee account.
		///
		/// This account receives stable-asset fees that remain after [`Config::YieldHook`]. Fee
		/// resolution must be exact because each fee credit backs a recorded liability.
		///
		/// The account must fund its asset-account deposit before registration of the first branch
		/// for that stablecoin.
		type FeeAccount: Convert<StableIdOf<Self>, Self::AccountId>;

		/// Receives the first share of interest and upfront fees.
		///
		/// The pallet resolves the remaining credit to [`Config::FeeAccount`].
		type YieldHook: OnBranchYield<CollateralIdOf<Self>, StableCreditOf<Self>>;

		/// Notifies other pallets when a branch is created or removed.
		type OnBranchLifecycle: OnBranchLifecycle<
			CollateralIdOf<Self>,
			StableIdOf<Self>,
			Self::AccountId,
		>;

		/// Provides UNIX time in milliseconds.
		type TimeProvider: Time<Moment = Millis>;

		/// Origin allowed to create a branch for a stable asset.
		///
		/// `Some(account)` charges that account the branch deposit. `None` creates the branch
		/// without a deposit.
		type CreateOrigin: EnsureOriginWithArg<
			Self::RuntimeOrigin,
			StableIdOf<Self>,
			Success = Option<Self::AccountId>,
		>;

		/// Refundable deposit charged for creating a branch.
		type BranchConsideration: Consideration<Self::AccountId, Footprint>;

		/// Refundable deposit charged for opening a vault.
		type VaultConsideration: Consideration<
			Self::AccountId,
			AssetFootprint<CollateralIdOf<Self>>,
		>;

		/// Runtime-owned, scale-independent limits for every branch.
		#[pallet::constant]
		type BranchConfigBounds: Get<BranchConfigBounds>;

		/// Origin allowed to override branch administrators.
		type ForceOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// Effective stablecoin-wide limit for new borrowing, in stable units.
		type GlobalDebtCeiling: Convert<StableIdOf<Self>, BalanceOf<Self>>;

		/// Sorted lists used for redemption order and final recovery.
		type VaultLists: SortedListInterface<
			VaultListId<CollateralIdOf<Self>, StableIdOf<Self>>,
			Self::AccountId,
			Priority = FixedU128,
		>;

		/// Pallet ID used to derive each branch's redistribution account.
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// Weights for calls.
		type WeightInfo: weights::WeightInfo;

		/// See [`crate::BenchmarkHelper`].
		#[cfg(feature = "runtime-benchmarks")]
		type BenchmarkHelper: crate::BenchmarkHelper<
			CollateralIdOf<Self>,
			StableIdOf<Self>,
			Self::AccountId,
		>;
	}

	/// Reason for holding funds.
	#[pallet::composite_enum]
	pub enum HoldReason {
		/// Collateral held for an open vault.
		VaultCollateral,
		/// Refundable deposit held while a branch exists.
		BranchCreationDeposit,
		/// Refundable deposit held while a vault exists.
		VaultCreationDeposit,
	}

	/// Authoritative vault state keyed by collateral, stable asset, and owner.
	#[pallet::storage]
	pub type Vaults<T: Config> = StorageNMap<
		_,
		(
			NMapKey<Blake2_128Concat, CollateralIdOf<T>>,
			NMapKey<Blake2_128Concat, StableIdOf<T>>,
			NMapKey<Blake2_128Concat, T::AccountId>,
		),
		VaultRecordOf<T>,
		OptionQuery,
	>;

	/// Authoritative branch state keyed by collateral and stable asset.
	///
	/// Each record contains the branch state, configuration, administrators, and creation deposit.
	#[pallet::storage]
	pub type Branches<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		CollateralIdOf<T>,
		Blake2_128Concat,
		StableIdOf<T>,
		BranchOf<T>,
		OptionQuery,
	>;

	/// Number of live branches issuing each stablecoin, keyed by the stablecoin's name in the
	/// collateral namespace. The entry is removed with its last branch.
	///
	/// This is the stable half of the role-exclusivity index: registration reads it to reject a
	/// stablecoin proposed as collateral, a second-key question [`Branches`] cannot answer with a
	/// bounded probe. The collateral half needs no index at all — [`Branches`] is keyed
	/// collateral-first, so "is this asset a collateral?" is one prefix probe on the registry.
	#[pallet::storage]
	pub type StablecoinMarkets<T: Config> =
		StorageMap<_, Blake2_128Concat, CollateralIdOf<T>, u32, OptionQuery>;

	/// Governance debt limit across every branch issuing one stable asset.
	///
	/// The limit uses that stablecoin's own units. A zero limit blocks new debt and is not stored.
	#[pallet::storage]
	pub type GlobalDebtCeilings<T: Config> =
		StorageMap<_, Blake2_128Concat, StableIdOf<T>, BalanceOf<T>, ValueQuery>;

	/// Fully accrued debt state across every branch issuing one stable asset.
	///
	/// A redemption consumer can read it as the denominator of the redeemed fraction, which
	/// prices the stablecoin's dynamic fee across all of its collateral branches at once. A zero
	/// record is not stored.
	///
	/// The record combines realized debt with the aggregate projection of
	/// unminted interest, keeping the fully accrued denominator available in
	/// O(1) without walking the uncapped branch registry.
	#[pallet::storage]
	pub type StablecoinDebt<T: Config> = StorageMap<
		_,
		Blake2_128Concat,
		StableIdOf<T>,
		crate::types::StablecoinDebtState<BalanceOf<T>>,
		ValueQuery,
	>;

	/// Accepts a signed origin matching a branch's stored full administrator.
	///
	/// Unknown branches and all other origins are rejected. A runtime may compose this with a
	/// governance origin using [`frame::traits::EitherOf`].
	pub struct EnsureBranchFullAdmin<T>(core::marker::PhantomData<T>);

	impl<T: Config> EnsureOriginWithArg<OriginFor<T>, (CollateralIdOf<T>, StableIdOf<T>)>
		for EnsureBranchFullAdmin<T>
	{
		type Success = ();

		fn try_origin(
			origin: OriginFor<T>,
			(collateral_id, stable_id): &(CollateralIdOf<T>, StableIdOf<T>),
		) -> Result<Self::Success, OriginFor<T>> {
			let Ok(who) = ensure_signed(origin.clone()) else { return Err(origin) };
			Pallet::<T>::ensure_branch_admin(&who, collateral_id, stable_id, AdminLevel::Full)
				.map(|_| ())
				.map_err(|_| origin)
		}

		#[cfg(feature = "runtime-benchmarks")]
		fn try_successful_origin(
			(collateral_id, stable_id): &(CollateralIdOf<T>, StableIdOf<T>),
		) -> Result<OriginFor<T>, ()> {
			let branch = Branches::<T>::get(collateral_id, stable_id).ok_or(())?;
			Ok(frame_system::RawOrigin::Signed(branch.admins.full_admin).into())
		}
	}

	/// Ceiling provider backed by the pallet's own [`GlobalDebtCeilings`] storage.
	///
	/// The default choice for [`Config::GlobalDebtCeiling`]: the stored value is the
	/// effective limit, exactly as [`Pallet::set_global_debt_ceiling`] wrote it.
	pub struct StoredCeiling<T>(core::marker::PhantomData<T>);

	impl<T: Config> Convert<StableIdOf<T>, BalanceOf<T>> for StoredCeiling<T> {
		fn convert(stable_id: StableIdOf<T>) -> BalanceOf<T> {
			GlobalDebtCeilings::<T>::get(&stable_id)
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A vault was opened.
		VaultOpened {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Vault owner.
			owner: T::AccountId,
			/// Collateral held from the owner.
			collateral: BalanceOf<T>,
			/// Stable assets minted to the owner.
			debt: BalanceOf<T>,
			/// Annual interest rate chosen by the owner, which places the vault in the redemption
			/// order.
			annual_rate: FixedU128,
		},
		/// A vault changed status.
		VaultStatusChanged {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Vault owner.
			owner: T::AccountId,
			/// Status before the change.
			old_status: VaultStatus,
			/// Status after the change.
			new_status: VaultStatus,
		},
		/// Collateral was added to a vault.
		CollateralDeposited {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Vault owner.
			owner: T::AccountId,
			/// Account that provided the collateral.
			from: T::AccountId,
			/// Collateral added.
			amount: BalanceOf<T>,
		},
		/// Collateral was removed from a vault.
		CollateralWithdrawn {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Vault owner.
			owner: T::AccountId,
			/// Account that received the collateral.
			recipient: T::AccountId,
			/// Collateral removed.
			amount: BalanceOf<T>,
		},
		/// Stable assets were borrowed from a vault.
		Borrowed {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Vault owner.
			owner: T::AccountId,
			/// Account that received the stable assets.
			recipient: T::AccountId,
			/// Stable assets borrowed.
			amount: BalanceOf<T>,
		},
		/// Stable assets were burned to repay a vault.
		Repaid {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Vault owner.
			owner: T::AccountId,
			/// Account that paid the stable assets.
			from: T::AccountId,
			/// Debt repaid, split into the interest and principal it removed.
			amount: DebtBreakdown<BalanceOf<T>>,
		},
		/// A vault was closed and its collateral was returned.
		VaultClosed {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Vault owner.
			owner: T::AccountId,
			/// Account that received the remaining collateral.
			recipient: T::AccountId,
			/// Collateral released to the recipient.
			collateral: BalanceOf<T>,
		},
		/// Accrued interest was added to a vault's debt.
		InterestAccrued {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Vault owner.
			owner: T::AccountId,
			/// Interest added.
			amount: BalanceOf<T>,
		},
		/// An upfront fee was charged to a vault.
		UpfrontFeeCharged {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Vault owner.
			owner: T::AccountId,
			/// Fee added to the vault's debt.
			amount: BalanceOf<T>,
		},
		/// A vault's annual interest rate changed.
		BorrowRateChanged {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Vault owner.
			owner: T::AccountId,
			/// Rate before the change.
			old_rate: FixedU128,
			/// Rate after the change.
			new_rate: FixedU128,
		},
		/// A branch changed mode.
		ModeChanged {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Mode before the change.
			old_mode: BranchMode,
			/// Mode after the change.
			new_mode: BranchMode,
		},
		/// A branch parameter changed.
		ParameterUpdated {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Parameter and the value it replaced.
			old_value: BranchConfigUpdate<BalanceOf<T>>,
			/// Parameter and new value.
			new_value: BranchConfigUpdate<BalanceOf<T>>,
		},
		/// A branch was created.
		BranchRegistered {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
		},
		/// The global debt limit for a stable asset changed.
		GlobalDebtCeilingSet {
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Stored global debt limit before the change. Zero when none was stored.
			old_ceiling: BalanceOf<T>,
			/// New global debt limit.
			new_ceiling: BalanceOf<T>,
		},
		/// An empty branch was removed.
		BranchRemoved {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
		},
		/// A branch's administrators changed.
		BranchAdminsChanged {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Administrators before the change.
			old_admins: BranchAdmins<T::AccountId>,
			/// Administrators after the change.
			new_admins: BranchAdmins<T::AccountId>,
		},
		/// A redemption exchanged stable debt for collateral.
		VaultRedeemed {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Vault owner.
			owner: T::AccountId,
			/// Account that received the collateral.
			recipient: T::AccountId,
			/// Debt removed from the vault, split into interest and principal.
			debt_cancelled: DebtBreakdown<BalanceOf<T>>,
			/// Collateral sent to the recipient.
			collateral_to_recipient: BalanceOf<T>,
			/// Vault rate used for the redemption fee.
			vault_annual_rate: FixedU128,
		},
		/// Aggregate interest accrued since the branch's last update was issued as yield.
		///
		/// Every write to a branch issues the interest accrued since the previous one, so this
		/// reports each increase of the stablecoin supply from interest.
		/// [`Event::InterestAccrued`] later adds one vault's share of issued interest to its
		/// debt row, and mints nothing.
		InterestIssued {
			/// Collateral asset ID.
			collateral_id: CollateralIdOf<T>,
			/// Stable asset ID.
			stable_id: StableIdOf<T>,
			/// Stablecoin issued to the yield hook.
			amount: BalanceOf<T>,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The requested vault does not exist.
		VaultNotFound,
		/// The owner already has a vault in this branch.
		VaultAlreadyExists,
		/// The vault is not in a valid state for this operation.
		InvalidVaultStatus,
		/// This operation is not allowed during final recovery.
		VaultInFinalRecovery,
		/// The collateral asset does not exist.
		UnknownCollateral,
		/// No branch is registered for this collateral and stable asset pair.
		BranchNotFound,
		/// The stable asset does not exist.
		UnknownStable,
		/// An asset would be used as both collateral and stable.
		StableCollateralCollision,
		/// This collateral and stable asset pair is already registered.
		BranchAlreadyRegistered,
		/// The branch creator cannot pay one minimum balance of the collateral asset.
		CustodySeedUnavailable,
		/// A fee credit backing a recorded liability failed to resolve.
		FeeResolutionFailed,
		/// The resulting debt is below the branch minimum.
		DebtBelowMinimum,
		/// The repayment would leave debt below the branch minimum.
		///
		/// Repay less or repay the full debt.
		DebtWouldBecomeDust,
		/// The borrow would exceed the branch debt limit.
		DebtCeilingExceeded,
		/// The borrow would exceed the global debt limit for this stablecoin.
		GlobalDebtCeilingExceeded,
		/// The caller does not have the required branch role.
		NotBranchAdmin,
		/// The branch still has vaults, collateral, or debt.
		BranchNotEmpty,
		/// The branch configuration contradicts itself.
		InvalidBranchConfig(crate::types::BranchConfigDefect),
		/// The branch configuration is outside the runtime's limits.
		ConfigOutsideEnvelope(crate::types::BoundViolation),
		/// The annual interest rate is outside the branch limits.
		RateOutOfBounds,
		/// The operation would leave the vault below its required collateral ratio.
		UnsafeCollateralizationRatio,
		/// The vault is too well collateralized to enter final recovery.
		CollateralizationRatioTooHealthy,
		/// The operation would lower the branch collateral ratio in safety mode.
		SafetyModeTcrWorsening,
		/// The operation would move the branch into safety mode.
		WouldEnterSafetyMode,
		/// The branch is frozen.
		BranchFrozen,
		/// The oracle has no valid price for this collateral asset.
		OraclePriceNotAvailable,
		/// The oracle price is too old. Reserved for runtime oracle adapters.
		OracleStale,
		/// The rate-list hint is too far from the correct position.
		///
		/// Fetch a new hint from the [`Config::VaultLists`] views for [`VaultListId::Rate`] and
		/// try again.
		InvalidPositionHints,
		/// The rate list does not match the vault records. This is storage corruption.
		RateIndexInvariantBroken,
		/// The final recovery queue does not match the vault records. This is storage corruption.
		FinalRecoveryInvariantBroken,
		/// The vault is not the branch's last eligible vault.
		NotLastEligibleVault,
		/// The vault does not have enough collateral.
		InsufficientCollateral,
		/// The vault still has debt.
		///
		/// Repay the full debt before closing it.
		DebtOutstanding,
		/// An arithmetic operation overflowed.
		ArithmeticOverflow,
		/// An emergency administrator tried to increase risk.
		DefensiveActionNotDefensive,
		/// A liquidation supplied invalid debt or collateral amounts.
		InvalidLiquidationSettlement,
		/// A redemption supplied invalid debt or collateral amounts.
		InvalidRedemptionSettlement,
		/// The last eligible vault cannot be liquidated.
		///
		/// Move it into final recovery instead.
		LastVaultCannotBeLiquidated,
		/// The liquidation would overflow redistribution accounting.
		RedistributionWouldOverflow,
		/// The vault is not eligible for liquidation.
		VaultNotLiquidatable,
		/// Collateral could not be delivered to its recipient.
		CollateralPayoutFailed,
		/// Another dormant vault is already first in the redemption queue.
		DormantTargetOccupied,
		/// The amount is zero.
		ZeroAmount,
		/// The vault has no debt to repay.
		NoDebt,
		/// The vault is below the ratio required to leave final recovery.
		CollateralizationRatioTooLow,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn integrity_test() {
			let bounds = T::BranchConfigBounds::get();
			let par = FixedU128::from_u32(1);
			// A liquidation floor under par admits branches whose vaults may owe more than their
			// collateral is worth before anyone can liquidate them.
			assert!(
				bounds.min_minimum_collateralization_ratio >= par,
				"`BranchConfigBounds::min_minimum_collateralization_ratio` must be at least 100%"
			);
			// Every branch keeps its liquidation ratio at or below its borrow and safety ratios,
			// so a liquidation floor above either floor silently replaces it.
			assert!(
				bounds.min_minimum_collateralization_ratio <=
					bounds.min_initial_collateralization_ratio,
				"`BranchConfigBounds` liquidation floor exceeds the borrow floor"
			);
			assert!(
				bounds.min_minimum_collateralization_ratio <=
					bounds.min_safety_collateralization_ratio,
				"`BranchConfigBounds` liquidation floor exceeds the safety floor"
			);
			// A zero cap forces every branch to lend interest-free, which is a misconfiguration
			// rather than a policy: borrowing would pay no yield and no upfront fee.
			assert!(
				!bounds.max_borrow_rate.is_zero(),
				"`BranchConfigBounds::max_borrow_rate` must be non-zero"
			);
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(_: BlockNumberFor<T>) -> Result<(), frame::try_runtime::TryRuntimeError> {
			crate::try_state::do_try_state::<T>()
		}
	}

	#[pallet::view_functions]
	impl<T: Config> Pallet<T> {
		/// Returns the vault's collateral ratio after pending updates.
		///
		/// Missing rows, oracle failure, and arithmetic failure are reported
		/// explicitly. A debt-free vault reports [`CollateralRatio::DebtFree`].
		pub fn vault_cr(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
		) -> Result<CollateralRatio, DispatchError> {
			let draft = Self::touched_vault_draft(&collateral_id, &stable_id, &owner)?;
			let price = T::Oracle::provide_price(&collateral_id)?;
			Ok(collateralization_ratio(&draft.vault.position(), price)?)
		}

		/// Returns the branch's total collateral ratio after pending interest.
		///
		/// Missing branches, oracle failure, and arithmetic failure are reported
		/// explicitly. A debt-free branch reports [`CollateralRatio::DebtFree`].
		pub fn branch_tcr(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
		) -> Result<CollateralRatio, DispatchError> {
			let state = Self::branch_of(&collateral_id, &stable_id)?.state;
			let price = T::Oracle::provide_price(&collateral_id)?;
			let now = T::TimeProvider::now();
			Self::compute_tcr(&state, price, now)
		}

		/// Returns the vault row its next touch would store, with its status.
		///
		/// Pending interest and redistribution are applied and nothing is persisted. The stored
		/// row still holds the amounts as of the last touch, so the difference is the pending
		/// part.
		pub fn vault_after_touch(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
		) -> Result<(Vault<BalanceOf<T>>, VaultStatus), DispatchError> {
			let draft = Self::touched_vault_draft(&collateral_id, &stable_id, &owner)?;
			Ok((draft.vault, draft.status))
		}

		/// Returns the branch state with aggregate interest accrued to now.
		///
		/// Nothing is persisted and no interest is issued. The carried interest fraction stays
		/// in the remainder here, while [`Self::branch_tcr`] rounds it up to one debt unit.
		pub fn branch_after_accrual(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
		) -> Result<BranchState<T::AccountId, BalanceOf<T>>, DispatchError> {
			let (_, state, _) = Self::accrued_branch_view(&collateral_id, &stable_id)?;
			Ok(state)
		}

		/// Returns a stablecoin's fully accrued debt across all of its branches, and the global
		/// limit new borrowing is checked against.
		pub fn stablecoin_debt_and_ceiling(
			stable_id: StableIdOf<T>,
		) -> (BalanceOf<T>, BalanceOf<T>) {
			let debt = Self::accrued_stablecoin_debt(&stable_id);
			(debt, T::GlobalDebtCeiling::convert(stable_id))
		}

		/// Returns up to `n` owners in redemption order.
		///
		/// Tiered with a cutoff: a `FinalRecovery` head (else a dormant target)
		/// gates the whole rate index, so only the gating target is returned and
		/// any `n > 0` yields one owner; otherwise active vaults from the lowest
		/// rate upward.
		pub fn redemption_queue(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			n: u32,
		) -> Vec<T::AccountId> {
			if n == 0 {
				return Vec::new();
			}
			match Self::priority_redemption_target(&collateral_id, &stable_id) {
				Some((owner, _)) => vec![owner],
				None => T::VaultLists::iter_from_tail(VaultListId::Rate(collateral_id, stable_id))
					.take(n.saturated_into())
					.collect(),
			}
		}

		/// Returns the debt redeemed before the given annual rate, and whether the walk stopped
		/// early.
		///
		/// Final recovery vaults are counted first (oldest first), then the dormant target, then
		/// rate-list vaults below `rate`. The walk visits at most `max_steps` vaults. When more
		/// remain, the sum is partial and the flag is `true`.
		pub fn debt_in_front(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			rate: FixedU128,
			max_steps: u32,
		) -> Result<(BalanceOf<T>, bool), DispatchError> {
			// All vault projections must use the same accrued branch state.
			let (_, accrued, now) = Self::accrued_branch_view(&collateral_id, &stable_id)?;
			let rate_list = VaultListId::Rate(collateral_id.clone(), stable_id.clone());
			let below_rate = T::VaultLists::iter_from_tail(rate_list.clone()).take_while(|owner| {
				T::VaultLists::priority(&rate_list, owner).is_some_and(|priority| priority < rate)
			});
			let mut in_front =
				T::VaultLists::iter_from_tail(recovery::list_id::<T>(&collateral_id, &stable_id))
					.chain(accrued.dormant_redemption_target.clone())
					.chain(below_rate);
			let mut total = BalanceOf::<T>::zero();
			for owner in in_front.by_ref().take(max_steps.saturated_into()) {
				let debt =
					Self::projected_vault_debt(&collateral_id, &stable_id, &owner, &accrued, now)?;
				total.ensure_add_assign(debt).map_err(|_| Error::<T>::ArithmeticOverflow)?;
			}
			Ok((total, in_front.next().is_some()))
		}

		/// Estimates the upfront fee for opening a vault.
		///
		/// Uses the same accrued branch draft and fee transition as execution.
		pub fn predict_open_upfront_fee(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			initial_debt: BalanceOf<T>,
			annual_rate: FixedU128,
		) -> Result<BalanceOf<T>, DispatchError> {
			let (config, mut state, now) = Self::accrued_branch_view(&collateral_id, &stable_id)?;
			ensure!(!state.is_frozen(), Error::<T>::BranchFrozen);
			Self::validate_rate(&config, annual_rate)?;
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

		/// Estimates the upfront fee for borrowing from a vault, changing its annual rate, or
		/// both.
		///
		/// Applies the same pending touch as execution before pricing the fee. A zero
		/// `debt_increase` with a new rate prices [`Pallet::change_rate`], which charges nothing
		/// once the rate-change cooldown has passed.
		pub fn predict_borrow_upfront_fee(
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			debt_increase: BalanceOf<T>,
			maybe_new_rate: Option<FixedU128>,
		) -> Result<BalanceOf<T>, DispatchError> {
			let mut draft = Self::touched_vault_draft(&collateral_id, &stable_id, &owner)?;
			ensure!(!draft.state.is_frozen(), Error::<T>::BranchFrozen);
			let new_rate = maybe_new_rate.unwrap_or(draft.vault.annual_rate);
			Self::validate_rate(&draft.config, new_rate)?;
			Self::apply_borrow_unchecked(
				&mut draft.state,
				&draft.config,
				&mut draft.vault,
				debt_increase,
				new_rate,
				draft.now,
			)
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Opens a vault and creates its initial debt.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the vault owner.
		///
		/// The branch must not be frozen. The collateral, debt, annual rate, and list hint must
		/// satisfy the branch limits. The collateral is held from the owner, and the stable asset
		/// is minted to the owner. Unused hint-repair steps are refunded after a successful open.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::open_vault(T::VaultLists::repair_budget()))]
		pub fn open_vault(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			initial_collateral: BalanceOf<T>,
			initial_debt: BalanceOf<T>,
			annual_rate: FixedU128,
			hint: Position<T::AccountId>,
		) -> DispatchResultWithPostInfo {
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

		/// Adds collateral to a vault.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the account providing the collateral.
		///
		/// Any account may add collateral for a vault owner. The amount must be non-zero. Deposits
		/// are allowed during final recovery and while the branch is frozen, because they need no
		/// price and only lower risk.
		///
		/// Dormant vaults cannot receive deposits: the owner must first borrow back above the
		/// branch's minimum debt.
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

		/// Removes collateral from the caller's vault.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the vault owner.
		///
		/// The amount must be non-zero and the vault must remain safe. `recipient` defaults to the
		/// owner. Removing all collateral from a debt-free vault closes it, so the call is charged
		/// the costlier of a withdrawal and a close.
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::withdraw_collateral().max(T::WeightInfo::close_vault()))]
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

		/// Borrows more stable assets from the caller's vault.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the vault owner.
		///
		/// The amount must be non-zero and all debt limits must hold. The call may also change the
		/// annual rate. `recipient` defaults to the owner.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::borrow()
			.max(T::WeightInfo::borrow_relocate(T::VaultLists::repair_budget()))
			.max(T::WeightInfo::borrow_activate(T::VaultLists::repair_budget())))]
		pub fn borrow(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			amount: BalanceOf<T>,
			maybe_new_rate: Option<FixedU128>,
			recipient: Option<T::AccountId>,
			hint: Position<T::AccountId>,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let recipient = recipient.unwrap_or_else(|| who.clone());
			Self::do_borrow(who, collateral_id, stable_id, amount, maybe_new_rate, recipient, hint)
		}

		/// Repays debt for a vault.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the account paying the stable assets.
		///
		/// Any account may repay for a vault owner. `None` settles the full payoff at execution.
		/// A `Some` amount must be non-zero, and the vault must carry debt.
		/// Repayment is allowed during final recovery, while frozen, and without a live price.
		/// A full payoff closes a vault with no collateral, including while frozen or without a
		/// live price. A vault with remaining collateral becomes debt-free and dormant.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::repay_for())]
		pub fn repay_for(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			amount: Option<BalanceOf<T>>,
		) -> DispatchResult {
			let from = ensure_signed(origin)?;
			Self::do_repay_for(from, owner, collateral_id, stable_id, amount)
		}

		/// Changes the annual rate of the caller's vault.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the vault owner.
		///
		/// The new rate must be within the branch limits. A change during the cooldown may charge
		/// an upfront fee. The hint gives the new place in the rate list.
		#[pallet::call_index(5)]
		#[pallet::weight(T::WeightInfo::change_rate()
			.max(T::WeightInfo::change_rate_relocate(T::VaultLists::repair_budget())))]
		pub fn change_rate(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			new_rate: FixedU128,
			hint: Position<T::AccountId>,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			Self::do_change_rate(who, collateral_id, stable_id, new_rate, hint)
		}

		/// Closes the caller's vault and returns its collateral.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the vault owner.
		///
		/// The vault must have no debt. `recipient` defaults to the owner.
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

		/// Applies pending interest and redistribution to a vault.
		///
		/// ## Dispatch Origin
		///
		/// May be called by any signed account.
		#[pallet::call_index(7)]
		#[pallet::weight(T::WeightInfo::poke())]
		pub fn poke(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
		) -> DispatchResult {
			ensure_signed(origin)?;
			VaultOp::<T>::refresh(collateral_id, stable_id, &owner)
		}

		/// Moves the last eligible vault into final recovery.
		///
		/// ## Dispatch Origin
		///
		/// May be called by any signed account.
		///
		/// The vault must be the branch's last eligible vault and must be below the minimum
		/// collateral ratio.
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

		/// Removes a safe vault from final recovery.
		///
		/// ## Dispatch Origin
		///
		/// May be called by any signed account.
		///
		/// The vault must meet the branch's minimum collateral ratio. It returns to the rate list
		/// if its debt is at least the branch minimum.
		#[pallet::call_index(9)]
		#[pallet::weight(T::WeightInfo::exit_final_recovery())]
		pub fn exit_final_recovery(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			hint: Position<T::AccountId>,
		) -> DispatchResult {
			ensure_signed(origin)?;
			Self::do_exit_final_recovery(owner, collateral_id, stable_id, hint)
		}

		/// Creates a branch for a collateral and stable asset pair.
		///
		/// ## Dispatch Origin
		///
		/// Must pass [`Config::CreateOrigin`] for the stable asset.
		///
		/// The assets must exist, the oracle must have a price, and the configuration must be
		/// structurally valid and within [`Config::BranchConfigBounds`]. A non-privileged creator
		/// pays a refundable deposit.
		///
		/// `lifecycle_config` is forwarded to [`Config::OnBranchLifecycle`]. Vaults does not
		/// interpret it: each handler validates and stores its own payload, and any hook error
		/// rolls the complete registration back.
		///
		/// Registration also charges one minimum balance of the collateral asset, which the
		/// branch's redistribution account carries until removal refunds it. A deposit-paying
		/// creator funds it. A privileged creation charges the named full administrator: that
		/// account never signed, so an unfunded admin fails with [`Error::CustodySeedUnavailable`].
		#[pallet::call_index(10)]
		#[pallet::weight(T::WeightInfo::create_branch())]
		pub fn create_branch(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			admins: BranchAdmins<AccountIdLookupOf<T>>,
			vault_config: BranchConfig<BalanceOf<T>>,
			lifecycle_config: RegistrationConfigOf<T>,
		) -> DispatchResult {
			let depositor = T::CreateOrigin::ensure_origin(origin, &stable_id)?;
			let admins = admins.try_map(T::Lookup::lookup)?;
			Self::do_create_branch(
				collateral_id,
				stable_id,
				admins,
				vault_config,
				lifecycle_config,
				depositor,
			)
		}

		/// Changes one branch parameter.
		///
		/// ## Dispatch Origin
		///
		/// Requires [`Config::ForceOrigin`] or a branch administrator with the required role.
		///
		/// Emergency administrators may only reduce risk. The full configuration must remain
		/// structurally valid and within [`Config::BranchConfigBounds`] for every origin.
		#[pallet::call_index(11)]
		#[pallet::weight(T::WeightInfo::set_param())]
		pub fn set_param(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			update: BranchConfigUpdate<BalanceOf<T>>,
		) -> DispatchResult {
			let level = Self::ensure_force_or_branch_admin(
				origin,
				&collateral_id,
				&stable_id,
				update.required_level(),
			)?;
			Self::do_set_param(collateral_id, stable_id, update, level)
		}

		/// Freezes or unfreezes a branch for an administrative reason.
		///
		/// ## Dispatch Origin
		///
		/// Freezing requires [`Config::ForceOrigin`] or a branch administrator. Unfreezing requires
		/// [`Config::ForceOrigin`] or the branch's full administrator.
		///
		/// The call does nothing if the requested administrative state is already satisfied. Oracle
		/// freezes are managed by [`Pallet::refresh_branch`].
		#[pallet::call_index(12)]
		#[pallet::weight(T::WeightInfo::set_governance_frozen())]
		pub fn set_governance_frozen(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			frozen: bool,
		) -> DispatchResult {
			// An emergency admin may pull the kill switch but not clear it.
			let required = if frozen { AdminLevel::Emergency } else { AdminLevel::Full };
			Self::ensure_force_or_branch_admin(origin, &collateral_id, &stable_id, required)?;
			Self::do_set_governance_frozen(&collateral_id, &stable_id, frozen)
		}

		/// Brings a branch up to date.
		///
		/// ## Dispatch Origin
		///
		/// May be called by any signed account.
		///
		/// The branch is frozen when its price is unavailable and unfrozen when the price returns.
		/// Administrative freezes are unchanged. An unfrozen branch applies its pending aggregate
		/// interest and issues it as yield; a frozen branch accrues nothing.
		#[pallet::call_index(13)]
		#[pallet::weight(T::WeightInfo::refresh_branch())]
		pub fn refresh_branch(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
		) -> DispatchResult {
			ensure_signed(origin)?;
			Self::do_refresh_branch(&collateral_id, &stable_id)
		}

		/// Removes an empty branch.
		///
		/// ## Dispatch Origin
		///
		/// Requires [`Config::ForceOrigin`] or the branch's full administrator.
		///
		/// The branch must have no vaults, collateral, or debt. Its creation deposit is refunded,
		/// and the redistribution custody seed returns to the original depositor, or to the current
		/// full administrator of a privileged branch. Rotating that admin therefore redirects the
		/// seed.
		#[pallet::call_index(14)]
		#[pallet::weight(T::WeightInfo::remove_branch())]
		pub fn remove_branch(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
		) -> DispatchResult {
			Self::ensure_force_or_branch_admin(
				origin,
				&collateral_id,
				&stable_id,
				AdminLevel::Full,
			)?;
			Self::do_remove_branch(collateral_id, stable_id)
		}

		/// Replaces a branch's administrators.
		///
		/// ## Dispatch Origin
		///
		/// Requires [`Config::ForceOrigin`] or the branch's full administrator.
		#[pallet::call_index(15)]
		#[pallet::weight(T::WeightInfo::set_branch_admins())]
		pub fn set_branch_admins(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			admins: BranchAdmins<AccountIdLookupOf<T>>,
		) -> DispatchResult {
			Self::ensure_force_or_branch_admin(
				origin,
				&collateral_id,
				&stable_id,
				AdminLevel::Full,
			)?;
			let admins = admins.try_map(T::Lookup::lookup)?;
			Self::do_set_branch_admins(collateral_id, stable_id, admins)
		}

		/// Returns a dormant vault to the rate list.
		///
		/// ## Dispatch Origin
		///
		/// May be called by any signed account.
		///
		/// The vault's debt after pending updates must meet the branch minimum. A successful call
		/// pays no transaction fee.
		#[pallet::call_index(16)]
		#[pallet::weight(T::WeightInfo::activate_dormant())]
		pub fn activate_dormant(
			origin: OriginFor<T>,
			collateral_id: CollateralIdOf<T>,
			stable_id: StableIdOf<T>,
			owner: T::AccountId,
			hint: Position<T::AccountId>,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;
			Self::do_activate_dormant(owner, collateral_id, stable_id, hint)?;
			Ok(Pays::No.into())
		}

		/// Sets the global debt limit for a stable asset.
		///
		/// ## Dispatch Origin
		///
		/// Requires [`Config::CreateOrigin`] for `stable_id`: the authority that can open
		/// branches for a stablecoin also bounds its total issuance. The depositor the origin
		/// resolves has no meaning here and is discarded.
		///
		/// The limit is measured in the stable asset's units and spans all of its collateral
		/// branches. A limit of zero blocks new debt. A non-zero limit needs an existing stable
		/// asset, which need not have a branch yet. A zero limit is accepted for any key, so the
		/// record of a destroyed asset can still be cleared.
		#[pallet::call_index(17)]
		#[pallet::weight(T::WeightInfo::set_global_debt_ceiling())]
		pub fn set_global_debt_ceiling(
			origin: OriginFor<T>,
			stable_id: StableIdOf<T>,
			ceiling: BalanceOf<T>,
		) -> DispatchResult {
			T::CreateOrigin::ensure_origin(origin, &stable_id)?;
			Self::do_set_global_debt_ceiling(stable_id, ceiling)
		}
	}

	/// Provides current vault rates to the sorted list.
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
					Pallet::<T>::vault_of(collateral_id, stable_id, item)
						.ok()
						.map(|vault| vault.annual_rate)
				},
				// FIFO order does not change after insertion. Thus, the stored insertion priority
				// remains authoritative.
				VaultListId::FinalRecovery(..) => T::VaultLists::priority(list_id, item),
			}
		}
	}

	impl<T: Config> Pallet<T> {
		/// Returns the account that holds collateral waiting for redistribution.
		pub fn redistribution_account(
			collateral_id: &CollateralIdOf<T>,
			stable_id: &StableIdOf<T>,
		) -> T::AccountId {
			stablecoin_primitives::market_sub_account(T::PalletId::get(), collateral_id, stable_id)
		}
	}
}
