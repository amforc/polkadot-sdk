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
	EitherOf, EnsureOriginWithArg, LinearStoragePrice,
};
use pallet_vaults::pusd_primitives::OraclePriceConversion;
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
	type WeightInfo = ();
	type ListId = pallet_vaults::VaultListId<VaultsCollateralId, VaultsStableId>;
	type ItemId = AccountId;
	type Priority = FixedU128;
	type MaxHintRepairSteps = LinkedListMaxHintRepairSteps;
	#[cfg(feature = "runtime-benchmarks")]
	type PriorityProvider = pallet_linked_list::BenchPriorityProvider<Runtime>;
	#[cfg(not(feature = "runtime-benchmarks"))]
	type PriorityProvider = Vaults;
}

parameter_types! {
	pub const VaultsPalletId: PalletId = PalletId(*b"py/vault");
	pub const VaultsIdleMaxRefreshWeight: Option<Weight> = None;
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
	/// Governance envelope every permissionlessly-created market config must sit
	/// inside: floors on the collateralization ratios and a cap on the borrow
	/// rate. Amounts are denominated in the market's own assets, so the creator
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
impl pusd_primitives::ProvidePrice for VaultsOracleAdapter {
	type AssetId = VaultsCollateralId;

	fn provide_price(
		collateral_id: &VaultsCollateralId,
	) -> Result<FixedU128, sp_runtime::DispatchError> {
		pallet_mock_oracle::Prices::<Runtime>::get(collateral_id)
			.ok_or(sp_runtime::DispatchError::Unavailable)
	}
}

/// Names each stablecoin in the collateral namespace, so the vault pallet can
/// reject a market that would make an asset its own collateral.
pub struct VaultsStableToCollateralId;
impl Convert<VaultsStableId, VaultsCollateralId> for VaultsStableToCollateralId {
	fn convert(stable_id: VaultsStableId) -> VaultsCollateralId {
		// Prefixing a trust-backed id with the assets pallet location only fails on
		// the junction limit, which two junctions cannot hit. `Here` is the safe
		// fallback: no market names it, so a failure blocks rather than allows.
		TrustBackedAssetLocation::convert_back(&stable_id)
			.unwrap_or_else(|| xcm::v5::Location::here())
	}
}

/// `CreateOrigin` for permissionless market creation. Root creates deposit-free
/// (`None`); the stablecoin asset's owner creates with a refundable deposit
/// (`Some(who)`); every other origin is rejected.
pub struct VaultsCreateOrigin;
impl EnsureOriginWithArg<RuntimeOrigin, VaultsStableId> for VaultsCreateOrigin {
	type Success = Option<AccountId>;

	fn try_origin(
		o: RuntimeOrigin,
		stable_id: &VaultsStableId,
	) -> Result<Self::Success, RuntimeOrigin> {
		use frame_support::traits::fungibles::roles::Inspect as RolesInspect;
		use frame_system::RawOrigin;
		match o.clone().into() {
			Ok(RawOrigin::Root) => Ok(None),
			Ok(RawOrigin::Signed(who)) => {
				if <Assets as RolesInspect<AccountId>>::owner(*stable_id) == Some(who.clone()) {
					Ok(Some(who))
				} else {
					Err(o)
				}
			},
			_ => Err(o),
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn try_successful_origin(_stable_id: &VaultsStableId) -> Result<RuntimeOrigin, ()> {
		Ok(RuntimeOrigin::root())
	}
}

/// Routes each stablecoin's vault fee remainder to the treasury.
pub struct VaultsFeeAccount;
impl Convert<VaultsStableId, AccountId> for VaultsFeeAccount {
	fn convert(_stable: VaultsStableId) -> AccountId {
		governance::TreasuryAccount::get()
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
	type TimeProvider = Timestamp;
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
	type PalletId = VaultsPalletId;
	type IdleMaxRefreshWeight = VaultsIdleMaxRefreshWeight;
	type VaultLists = LinkedList;
	type WeightInfo = ();
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = VaultsBenchmarkHelper;
}

parameter_types! {
	/// Seeds the per-stablecoin insurance-fund accounts of coins that no PSM
	/// instance mints against.
	pub const InsuranceFundPalletId: PalletId = PalletId(*b"pusd/ins");
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
	type TimeProvider = Timestamp;
	type UpdateOrigin = VaultsCreateOrigin;
	type MaxRedemptionSteps = RedemptionsMaxSteps;
	type WeightInfo = pallet_redemptions::weights::SubstrateWeight<Runtime>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = RedemptionsBenchmarkHelper;
}

parameter_types! {
	pub const StabilityPalletId: PalletId = PalletId(*b"py/stabl");
}

impl pallet_stability::Config for Runtime {
	type StableAssets = Assets;
	type CollateralAssets = StabilityCollateral;
	type TimeProvider = Timestamp;
	type BranchModes = Vaults;
	type RecoveryOffsets = Redemptions;
	type StableDustHandler = ResolveAssetTo<governance::TreasuryAccount, Assets>;
	type CollateralDustHandler = ResolveAssetTo<governance::TreasuryAccount, StabilityCollateral>;
	type UpdateOrigin = EitherOf<
		AsEnsureOriginWithArg<EnsureRoot<AccountId>>,
		pallet_vaults::EnsureBranchFullAdmin<Runtime>,
	>;
	type PalletId = StabilityPalletId;
	type WeightInfo = ();
}

/// Trust-backed asset id the benchmarks mint their stablecoin under. Benchmarks
/// create the asset themselves, so any id outside the well-known range works.
#[cfg(feature = "runtime-benchmarks")]
const BENCHMARK_STABLE_ASSET_ID: VaultsStableId = 50_000_342;

/// One whole stablecoin unit, at the 6 decimals the benchmarks register it with.
/// The runtime-wide constant went away with the single-PSM configuration.
#[cfg(feature = "runtime-benchmarks")]
const PUSD: Balance = 1_000_000;

#[cfg(feature = "runtime-benchmarks")]
pub struct VaultsBenchmarkHelper;

#[cfg(feature = "runtime-benchmarks")]
impl pallet_vaults::BenchmarkHelper<VaultsCollateralId, VaultsStableId> for VaultsBenchmarkHelper {
	fn collateral_asset_id() -> VaultsCollateralId {
		VaultsNativeCollateralId::get()
	}

	fn stable_asset_id() -> VaultsStableId {
		BENCHMARK_STABLE_ASSET_ID
	}

	fn set_oracle_price(asset_id: VaultsCollateralId, price: FixedU128) {
		pallet_mock_oracle::Prices::<Runtime>::insert(asset_id, price);
	}

	fn clear_oracle_price(asset_id: VaultsCollateralId) {
		pallet_mock_oracle::Prices::<Runtime>::remove(asset_id);
	}

	fn advance_time(ms: u64) {
		let now = <pallet_timestamp::Pallet<Runtime>>::get();
		<pallet_timestamp::Pallet<Runtime>>::set_timestamp(now + ms);
	}
}

#[cfg(feature = "runtime-benchmarks")]
fn fund_vaults_benchmark_collateral(
	asset_id: VaultsCollateralId,
	who: &AccountId,
	amount: Balance,
) {
	use frame_support::traits::{fungibles::Balanced as FungiblesBalanced, tokens::Precision};

	if System::providers(who) == 0 {
		System::inc_providers(who);
	}
	let debt = <VaultsCollateral as FungiblesBalanced<AccountId>>::deposit(
		asset_id,
		who,
		amount,
		Precision::Exact,
	)
	.expect("fund collateral for benchmark account");
	drop(debt);
}

#[cfg(feature = "runtime-benchmarks")]
use sp_runtime::traits::StaticLookup;

#[cfg(feature = "runtime-benchmarks")]
pub struct RedemptionsBenchmarkHelper;

#[cfg(feature = "runtime-benchmarks")]
impl pallet_redemptions::BenchmarkHelper<VaultsCollateralId, VaultsStableId, AccountId, Balance>
	for RedemptionsBenchmarkHelper
{
	fn setup_redeemable_branch(
		vaults: u32,
	) -> (VaultsCollateralId, VaultsStableId, AccountId, Balance) {
		use frame_support::traits::fungibles::Mutate as FungiblesMutate;
		use frame_system::RawOrigin;
		use pallet_vaults::{pusd_primitives::OnBranchLifecycle as _, BenchmarkHelper as _};

		let collateral_id = VaultsNativeCollateralId::get();
		let stable_id: VaultsStableId = BENCHMARK_STABLE_ASSET_ID;

		// The benchmark genesis does not create the stablecoin asset, so opening
		// vaults (which mints stablecoin debt) and funding the redeemer would fail
		// without it.
		{
			use frame_support::traits::fungibles::{Create, Inspect as FungiblesInspect};
			if !<Assets as FungiblesInspect<AccountId>>::asset_exists(stable_id) {
				let asset_owner: AccountId = frame_benchmarking::account("pusd_owner", 0, 0);
				<Assets as Create<AccountId>>::create(stable_id, asset_owner, true, 1)
					.expect("create stablecoin asset for benchmark");
			}
		}

		// `create_branch` validates the oracle price, so set it first.
		VaultsBenchmarkHelper::set_oracle_price(
			collateral_id.clone(),
			FixedU128::from_rational(10u128, 1u128),
		);
		let branch_config = pallet_vaults::BranchConfig {
			minimum_collateralization_ratio: FixedU128::from_rational(110u128, 100u128),
			initial_collateralization_ratio: FixedU128::from_rational(120u128, 100u128),
			safety_collateralization_ratio: FixedU128::from_rational(130u128, 100u128),
			debt_ceiling: 1_000_000 * PUSD,
			minimum_debt: 10 * PUSD,
			minimum_collateral: {
				use frame_support::traits::fungibles::Inspect as FungiblesInspect;
				<VaultsCollateral as FungiblesInspect<AccountId>>::minimum_balance(
					collateral_id.clone(),
				)
			},
			minimum_borrow_rate: FixedU128::from_rational(1u128, 1_000u128),
			maximum_borrow_rate: FixedU128::from_rational(1u128, 1u128),
			upfront_fee_period: 7 * 24 * 60 * 60 * 1_000,
			rate_adjustment_cooldown: 24 * 60 * 60 * 1_000,
			liquidation: pallet_vaults::LiquidationConfig {
				offset_penalty: Permill::from_percent(5),
				keeper_flat_compensation_value: 100,
				keeper_percent_compensation: Permill::from_rational(1u32, 1_000u32),
				keeper_compensation_cap_value: 10_000,
				minimum_jit_contribution: 100,
				redistribution_penalty: Permill::from_percent(5),
			},
		};
		let full_admin: AccountId = frame_benchmarking::account("vaults_admin", 0, 0);
		let emergency_admin: AccountId = frame_benchmarking::account("vaults_emergency", 0, 0);
		let admins = pallet_vaults::types::BranchAdmins {
			full_admin: <Runtime as frame_system::Config>::Lookup::unlookup(full_admin.clone()),
			emergency_admin: <Runtime as frame_system::Config>::Lookup::unlookup(emergency_admin),
		};
		// A Root-created market charges its full admin one minimum balance of collateral, which
		// the redistribution account carries until removal refunds it.
		fund_vaults_benchmark_collateral(collateral_id.clone(), &full_admin, 100 * UNITS);
		// Each handler's own benchmark payload, not the runtime's production parameters: those
		// carry live minimums that would bind inside a benchmark.
		let lifecycle_config =
			<Runtime as pallet_vaults::Config>::OnBranchLifecycle::benchmark_registration_config(1);
		pallet_vaults::Pallet::<Runtime>::create_branch(
			RawOrigin::Root.into(),
			collateral_id.clone(),
			stable_id,
			admins,
			branch_config,
			lifecycle_config,
		)
		.expect("create branch for benchmark");
		pallet_vaults::Pallet::<Runtime>::set_global_debt_ceiling(
			RawOrigin::Root.into(),
			stable_id,
			1_000_000_000 * PUSD,
		)
		.expect("set global debt ceiling for benchmark");

		// Native collateral must clear the runtime existential deposit, with
		// headroom so the vault's collateral hold leaves free balance above it.
		let collateral: Balance = 1_000 * UNITS;
		let funding: Balance = collateral.saturating_mul(10);
		let debt: Balance = 20 * PUSD; // above the 10-PUSD minimum_debt
		for i in 0..vaults {
			let owner: AccountId = frame_benchmarking::account("redemption_vault", i, 0);
			fund_vaults_benchmark_collateral(collateral_id.clone(), &owner, funding);
			let rate = FixedU128::from_rational(u128::from(i) + 1, 1_000u128);
			pallet_vaults::Pallet::<Runtime>::open_vault(
				RawOrigin::Signed(owner).into(),
				collateral_id.clone(),
				stable_id,
				collateral,
				debt,
				rate,
				pallet_linked_list::Position::endpoints_only(),
			)
			.expect("open benchmark vault");
		}

		let redeemer: AccountId = frame_benchmarking::account("redeemer", 0, 0);
		// The redeemer receives collateral onto its free balance, so it must exist
		// above the existential deposit before the redemption pays out.
		fund_vaults_benchmark_collateral(collateral_id.clone(), &redeemer, funding);
		let budget = debt.saturating_mul(u128::from(vaults).saturating_add(2)).saturating_mul(2);
		<Assets as FungiblesMutate<AccountId>>::mint_into(
			stable_id,
			&redeemer,
			budget.saturating_mul(2),
		)
		.expect("mint stablecoin for redeemer");
		(collateral_id, stable_id, redeemer, budget)
	}
}
