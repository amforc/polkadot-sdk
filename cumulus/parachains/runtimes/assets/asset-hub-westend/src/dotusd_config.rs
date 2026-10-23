// Copyright (C) Amforc AG.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Stablecoin-framework configuration: price oracle, vault branches,
//! redemptions and the stability pool.

use super::*;
use frame_support::traits::{
	fungibles::{
		AssetFootprintPrice, AtLeastMinimumBalance,
		HoldConsideration as FungiblesHoldConsideration, SufficientAssets,
	},
	tokens::imbalance::ResolveAssetTo,
	EitherOf, LinearStoragePrice,
};
use pallet_vaults::stablecoin_primitives::OraclePriceConversion;
use sp_runtime::{
	traits::{AccountIdConversion, Convert, MaybeEquivalence},
	FixedU128,
};

/// Collateral is named in the runtime-wide asset namespace, so vaults can take
/// the native token, a trust-backed asset and a bridged foreign asset alike.
pub type VaultsCollateralId = xcm::v5::Location;
pub type VaultsStableId = AssetIdForTrustBackedAssets;

/// Vault collateral custody needs holds, so the assets side goes through the
/// holder pallets.
pub type VaultsCollateral = fungible::UnionOf<
	Balances,
	LocalAndForeignAssetsHolder,
	TargetFromLeft<WestendLocation, xcm::v5::Location>,
	VaultsCollateralId,
	AccountId,
>;

/// Stability-pool collateral is only transferred, never held.
pub type StabilityCollateral = NativeAndNonPoolAssets;

/// Location of a trust-backed asset, which is how the collateral namespace
/// names it.
pub type TrustBackedAssetLocation =
	AssetIdForTrustBackedAssetsConvert<TrustBackedAssetsPalletLocation, xcm::v5::Location>;

/// Root-settable price feed standing in for a real oracle integration.
///
/// Deliberately minimal: no operator set, no aggregation, no staleness window.
/// Swap for a production oracle before any mainnet deployment.
#[frame_support::pallet]
pub mod pallet_mock_oracle {
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use sp_runtime::FixedU128;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Collateral identifier the feeds are keyed by.
		type AssetId: Parameter + Member + MaxEncodedLen;
	}

	/// Price of one collateral unit in stablecoin units, keyed by collateral.
	#[pallet::storage]
	pub type Prices<T: Config> =
		StorageMap<_, Blake2_128Concat, <T as Config>::AssetId, FixedU128, OptionQuery>;

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::call_index(0)]
		#[pallet::weight(T::DbWeight::get().writes(1))]
		pub fn set_price(
			origin: OriginFor<T>,
			key: <T as Config>::AssetId,
			price: FixedU128,
		) -> DispatchResult {
			ensure_root(origin)?;
			Prices::<T>::insert(key, price);
			Ok(())
		}

		#[pallet::call_index(1)]
		#[pallet::weight(T::DbWeight::get().writes(1))]
		pub fn clear_price(origin: OriginFor<T>, key: <T as Config>::AssetId) -> DispatchResult {
			ensure_root(origin)?;
			Prices::<T>::remove(key);
			Ok(())
		}
	}
}

impl pallet_mock_oracle::Config for Runtime {
	type AssetId = VaultsCollateralId;
}

parameter_types! {
	pub const LinkedListMaxHintRepairSteps: u32 = 16;
}

impl pallet_linked_list::Config for Runtime {
	type WeightInfo = weights::pallet_linked_list::WeightInfo<Runtime>;
	type ListId = pallet_vaults::VaultListId<VaultsCollateralId, VaultsStableId>;
	type ItemId = AccountId;
	type Priority = FixedU128;
	type MaxHintRepairSteps = LinkedListMaxHintRepairSteps;
	#[cfg(feature = "runtime-benchmarks")]
	type PriorityProvider = pallet_linked_list::BenchPriorityProviderWithPrimary<Runtime, Vaults>;
	#[cfg(not(feature = "runtime-benchmarks"))]
	type PriorityProvider = Vaults;
}

parameter_types! {
	pub const VaultsPalletId: PalletId = PalletId(*b"py/vault");
	/// The native token as the collateral namespace names it.
	pub VaultsNativeCollateralId: VaultsCollateralId = WestendLocation::get();
	pub const VaultsMarketCreationHoldReason: RuntimeHoldReason =
		RuntimeHoldReason::Vaults(pallet_vaults::HoldReason::BranchCreationDeposit);
	pub const VaultsBranchCreationDeposit: Balance = 100 * UNITS;
	pub const VaultsVaultCreationHoldReason: RuntimeHoldReason =
		RuntimeHoldReason::Vaults(pallet_vaults::HoldReason::VaultCreationDeposit);
	/// Per-vault storage deposit at JAM parity: the whole supply (2.1 billion) against the
	/// state it can hold (21 million kilobytes) values one kilobyte at 100 units, so a byte
	/// costs 100 / 1024. Keys and rows are priced alike, with no per-item component.
	pub const VaultsVaultDepositBase: Balance = 0;
	pub const VaultsVaultDepositPerByte: Balance = 100 * UNITS / 1024;
	/// Governance envelope every permissionlessly-created branch config must sit
	/// inside: floors on the collateralization ratios and a cap on the borrow
	/// rate. Amounts are denominated in the branch's own assets, so the creator
	/// picks those.
	pub VaultsBranchConfigBounds: pallet_vaults::types::BranchConfigBounds =
		pallet_vaults::types::BranchConfigBounds {
			min_minimum_collateralization_ratio: FixedU128::from_rational(105, 100),
			min_initial_collateralization_ratio: FixedU128::from_rational(110, 100),
			min_safety_collateralization_ratio: FixedU128::from_rational(120, 100),
			max_borrow_rate: FixedU128::from_rational(400, 100),
		};
}

/// Bridges the mock oracle to the vault pallet's `ProvidePrice` surface.
pub struct VaultsOracleAdapter;
impl stablecoin_primitives::ProvidePrice for VaultsOracleAdapter {
	type AssetId = VaultsCollateralId;

	fn provide_price(
		collateral_id: &VaultsCollateralId,
	) -> Result<FixedU128, sp_runtime::DispatchError> {
		pallet_mock_oracle::Prices::<Runtime>::get(collateral_id)
			.ok_or(sp_runtime::DispatchError::Unavailable)
	}
}

/// Names each stablecoin in the collateral namespace, so the vault pallet can
/// reject a branch that would make an asset its own collateral.
pub struct VaultsStableToCollateralId;
impl Convert<VaultsStableId, VaultsCollateralId> for VaultsStableToCollateralId {
	fn convert(stable_id: VaultsStableId) -> VaultsCollateralId {
		// Prefixing a trust-backed id with the assets pallet location only fails on
		// the junction limit, which two junctions cannot hit. `Here` is the safe
		// fallback: no branch names it, so a failure blocks rather than allows.
		TrustBackedAssetLocation::convert_back(&stable_id)
			.unwrap_or_else(|| xcm::v5::Location::here())
	}
}

/// `CreateOrigin` for permissionless branch creation. Root creates deposit-free
/// (`None`); the stablecoin asset's owner creates with a refundable deposit
/// (`Some(who)`); every other origin is rejected.
pub type VaultsCreateOrigin = stablecoin_primitives::EnsureStableOwnerOrRoot<Assets, AccountId>;

/// Routes each stablecoin's vault fee remainder to the treasury.
pub struct VaultsFeeAccount;
impl Convert<VaultsStableId, AccountId> for VaultsFeeAccount {
	fn convert(_stable: VaultsStableId) -> AccountId {
		governance::TreasuryAccount::get()
	}
}

/// Vault borrowing yields to live PSM debt: the stored ceiling is the
/// stablecoin's total supply cap, and vaults mint only into what its PSM
/// instance has not already issued. PSM headroom flows back to vaults as PSM
/// debt is redeemed.
///
/// A stablecoin without a PSM instance keeps its stored ceiling unmodified.
/// PSM instances are keyed by the internal asset's location, the same bridge
/// [`StableInsuranceAccount`] uses; the converters must stay aligned or a
/// coin's PSM debt would silently read as zero. The debt walk is bounded by
/// the PSM's `MaxExternals` approved assets.
pub struct VaultsGlobalDebtCeiling;
impl Convert<VaultsStableId, Balance> for VaultsGlobalDebtCeiling {
	fn convert(stable_id: VaultsStableId) -> Balance {
		let psm_debt = TrustBackedAssetLocation::convert_back(&stable_id)
			.map(|internal_asset| {
				pallet_psm::PsmDebt::<Runtime>::iter_prefix_values(internal_asset)
					.fold(0, |acc: Balance, debt| acc.saturating_add(debt))
			})
			.unwrap_or(0);
		pallet_vaults::StoredCeiling::<Runtime>::convert(stable_id).saturating_sub(psm_debt)
	}
}

/// Settles a vault deposit in the collateral when it is native or sufficient, else in WND.
///
/// A pool quote is deliberately not used as a fallback: it is an instantaneous spot price that
/// anyone can skew within a block, so it must not be able to gut an anti-spam deposit. Should a
/// runtime want to price collaterals the oracle does not cover, wrap the oracle conversion in
/// [`FallbackOnUnavailable`](frame_support::traits::tokens::FallbackOnUnavailable) with
/// [`PoolQuoteConversion`](pallet_asset_conversion::PoolQuoteConversion) as the secondary:
///
/// ```ignore
/// FallbackOnUnavailable<
///     OraclePriceConversion<VaultsOracleAdapter, VaultsNativeCollateralId>,
///     PoolQuoteConversion<AssetConversion, WestendLocation>,
/// >
/// ```
///
/// and pair it with a floor that preserves the native storage value (e.g. a time-weighted quote
/// or a fixed minimum in WND terms) rather than relying on the minimum-balance floor alone.
pub type VaultsDepositPolicy = AssetFootprintPrice<
	SufficientAssets<VaultsCollateral, AccountId>,
	VaultsNativeCollateralId,
	LinearStoragePrice<VaultsVaultDepositBase, VaultsVaultDepositPerByte, Balance>,
	AtLeastMinimumBalance<
		VaultsCollateral,
		OraclePriceConversion<VaultsOracleAdapter, VaultsNativeCollateralId>,
		AccountId,
	>,
>;

pub type VaultsVaultConsideration = FungiblesHoldConsideration<
	AccountId,
	VaultsCollateral,
	VaultsVaultCreationHoldReason,
	VaultsDepositPolicy,
>;

impl pallet_vaults::Config for Runtime {
	type StableToCollateralId = VaultsStableToCollateralId;
	type CollateralAssets = VaultsCollateral;
	type StableAssets = Assets;
	type Oracle = VaultsOracleAdapter;
	type FeeAccount = VaultsFeeAccount;
	type YieldHook = Stability;
	type OnBranchLifecycle = (Redemptions, Stability);
	type StabilityPool = Stability;
	type TimeProvider = RelayTimestamp;
	type CreateOrigin = VaultsCreateOrigin;
	type BranchConsideration = HoldConsideration<
		AccountId,
		Balances,
		VaultsMarketCreationHoldReason,
		ConstantStoragePrice<VaultsBranchCreationDeposit, Balance>,
	>;
	type VaultConsideration = VaultsVaultConsideration;
	type BranchConfigBounds = VaultsBranchConfigBounds;
	type ForceOrigin = EnsureRoot<AccountId>;
	type GlobalDebtCeiling = VaultsGlobalDebtCeiling;
	type PalletId = VaultsPalletId;
	type VaultLists = LinkedList;
	type WeightInfo = weights::pallet_vaults::WeightInfo<Runtime>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = VaultsBenchmarkHelper;
}

parameter_types! {
	/// Seeds the per-stablecoin insurance-fund accounts of coins that no PSM
	/// instance mints against.
	pub const InsuranceFundPalletId: PalletId = PalletId(*b"py/insur");
}

/// Isolates each stablecoin's Insurance Fund cover in a separate account.
///
/// A stablecoin minted by a PSM instance already names the account that instance's
/// fee revenue accrues to, and that revenue is exactly what funds the coin's
/// insurance cover, so the instance's `fee_destination` is its insurance account.
/// A stablecoin without a PSM gets its own sub-account of the insurance pallet id.
pub struct StableInsuranceAccount;
impl Convert<VaultsStableId, AccountId> for StableInsuranceAccount {
	fn convert(stable_id: VaultsStableId) -> AccountId {
		// PSM instances are keyed by the internal asset's location, which is how the
		// collateral namespace names a trust-backed stablecoin.
		TrustBackedAssetLocation::convert_back(&stable_id)
			.and_then(pallet_psm::Psm::<Runtime>::get)
			.map(|info| info.fee_destination)
			.unwrap_or_else(|| InsuranceFundPalletId::get().into_sub_account_truncating(stable_id))
	}
}

parameter_types! {
	pub const RedemptionsMaxSteps: u32 = 16;
}

impl pallet_redemptions::Config for Runtime {
	type StableAssets = Assets;
	type Oracle = VaultsOracleAdapter;
	type Vaults = Vaults;
	type InsuranceFundAccount = StableInsuranceAccount;
	type FeeHandler = ResolveAssetTo<governance::TreasuryAccount, Assets>;
	type TimeProvider = RelayTimestamp;
	type UpdateOrigin = VaultsCreateOrigin;
	type MaxRedemptionSteps = RedemptionsMaxSteps;
	type WeightInfo = weights::pallet_redemptions::WeightInfo<Runtime>;
}

parameter_types! {
	pub const StabilityPalletId: PalletId = PalletId(*b"py/stabl");
}

impl pallet_stability::Config for Runtime {
	type StableAssets = Assets;
	type CollateralAssets = StabilityCollateral;
	type TimeProvider = RelayTimestamp;
	type BranchInterface = Vaults;
	type RecoveryOffsets = Redemptions;
	type StableDustHandler = ResolveAssetTo<governance::TreasuryAccount, Assets>;
	type CollateralDustHandler = ResolveAssetTo<governance::TreasuryAccount, StabilityCollateral>;
	type UpdateOrigin = EitherOf<
		pallet_vaults::EnsureBranchFullAdmin<Runtime>,
		AsEnsureOriginWithArg<EnsureRoot<AccountId>>,
	>;
	type PalletId = StabilityPalletId;
	type WeightInfo = weights::pallet_stability::WeightInfo<Runtime>;
}

#[cfg(feature = "runtime-benchmarks")]
use frame_support::traits::Get as _;

/// A power of ten keeps exact-par prices divisible; greater than one exercises asset scaling.
#[cfg(feature = "runtime-benchmarks")]
const BENCHMARK_COLLATERAL_MIN_BALANCE: Balance = 10_000_000;

/// Sufficient foreign collateral exercises Location keys, holder custody and oracle repricing.
/// Sufficiency allows payouts to fresh accounts; non-sufficient collateral uses a cheaper
/// native deposit hold.
#[cfg(feature = "runtime-benchmarks")]
fn benchmark_collateral_id() -> VaultsCollateralId {
	xcm::v5::Location::new(
		2,
		[
			xcm::v5::Junction::GlobalConsensus(EthereumNetwork::get()),
			xcm::v5::Junction::AccountKey20 { network: None, key: [0xee; 20] },
		],
	)
}

#[cfg(feature = "runtime-benchmarks")]
pub struct VaultsBenchmarkHelper;

#[cfg(feature = "runtime-benchmarks")]
impl pallet_vaults::BenchmarkHelper<VaultsCollateralId, VaultsStableId, AccountId>
	for VaultsBenchmarkHelper
{
	fn collateral_asset_id() -> VaultsCollateralId {
		benchmark_collateral_id()
	}

	fn set_oracle_price(asset_id: VaultsCollateralId, price: FixedU128) {
		pallet_mock_oracle::Prices::<Runtime>::insert(asset_id, price);
	}

	// Writes the moment directly: `set_timestamp` runs Aura's `on_timestamp_set`, which rejects a
	// clock that runs ahead of the current slot. `RelayTimestamp` reads this value when no relay
	// slot is recorded, as in benchmarks.
	fn advance_time(ms: u64) {
		pallet_timestamp::Now::<Runtime>::mutate(|now| *now = now.saturating_add(ms));
	}

	// Maximal PSM rows and sibling-parachain keys exercise the full debt-ceiling lookup.
	// Zero debt preserves the ceiling; `insert` retains rows that mutation to zero would delete.
	fn stage_debt_ceiling(stable_id: VaultsStableId) {
		let internal = TrustBackedAssetLocation::convert_back(&stable_id)
			.expect("a two-junction trust-backed location fits the junction limit; qed");
		let externals = <Runtime as pallet_psm::Config>::MaxExternals::get();
		for index in 0..externals {
			pallet_psm::PsmDebt::<Runtime>::insert(
				&internal,
				Location::new(1, [Parachain(2000), GeneralIndex(index.into())]),
				0,
			);
		}
		assert_eq!(
			pallet_psm::PsmDebt::<Runtime>::iter_prefix(&internal).count(),
			usize::try_from(externals).expect("u32 fits usize on every supported target; qed"),
		);
	}

	// Equal prices per minimum balance keep deposit costs equal in native and collateral units.
	fn ensure_collateral_asset(collateral_id: VaultsCollateralId) {
		use frame_support::traits::fungibles::{Create, Inspect as FungiblesInspect};
		let native = VaultsNativeCollateralId::get();
		if collateral_id == native {
			return;
		}
		if !<ForeignAssets as FungiblesInspect<AccountId>>::asset_exists(collateral_id.clone()) {
			let asset_owner: AccountId = frame_benchmarking::account("collateral_owner", 0, 0);
			<ForeignAssets as Create<AccountId>>::create(
				collateral_id.clone(),
				asset_owner,
				true,
				BENCHMARK_COLLATERAL_MIN_BALANCE,
			)
			.expect("the benchmark collateral location is unused; qed");
		}
		assert!(<ForeignAssets as FungiblesInspect<AccountId>>::asset_exists(
			collateral_id.clone()
		));
		assert!(<VaultsCollateral as FungiblesInspect<AccountId>>::is_sufficient(collateral_id));
		pallet_mock_oracle::Prices::<Runtime>::insert(
			native,
			FixedU128::from_rational(10, ExistentialDeposit::get()),
		);
	}

	fn fund_native(who: &AccountId) {
		use frame_support::traits::fungible::Mutate as FungibleMutate;
		<Balances as FungibleMutate<AccountId>>::mint_into(who, 1_000 * UNITS)
			.expect("minting native to a benchmark account cannot overflow the issuance; qed");
	}
}
