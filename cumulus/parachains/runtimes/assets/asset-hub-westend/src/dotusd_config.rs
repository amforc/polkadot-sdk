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
	EitherOf, LinearStoragePrice, PriceProvider,
};
use pallet_vaults::stablecoin_primitives::OraclePriceConversion;
use sp_price_oracle::PairId;
use sp_runtime::{
	traits::{AccountIdConversion, BlockNumberProvider, Convert, MaybeEquivalence},
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

/// Test Tether USD registered as a foreign asset on Westend.
pub fn sepolia_usdt_location() -> VaultsCollateralId {
	xcm::v5::Location::new(
		2,
		[
			xcm::v5::Junction::GlobalConsensus(EthereumNetwork::get()),
			xcm::v5::Junction::AccountKey20 {
				network: None,
				key: hex_literal::hex!("7169d38820dfd117c3fa1f22a697dba58d90ba06"),
			},
		],
	)
}

/// Maximum age of a published dotUSD collateral price in relay-chain blocks (about 54 seconds).
const MAX_ORACLE_PRICE_AGE: BlockNumber = 9;

/// Converts whole-token USD quotes to six-decimal dotUSD base units per collateral base unit.
pub struct VaultsOracleAdapter;

impl VaultsOracleAdapter {
	/// The USD pair to configure on the oracle and the divisor for its whole-token price.
	pub fn collateral_feed(collateral: &VaultsCollateralId) -> Option<(PairId, u128)> {
		// WND uses DOT/USD on Westend. WND has 12 decimals.
		if collateral == &VaultsNativeCollateralId::get() {
			return Some((PairId(2), 1_000_000));
		}
		// Both Westend test USDT assets use USDT/USD and have the same decimals as dotUSD.
		if TrustBackedAssetLocation::convert_back(&1984).as_ref() == Some(collateral) ||
			collateral == &sepolia_usdt_location()
		{
			return Some((PairId(3), 1));
		}
		None
	}
}

impl stablecoin_primitives::ProvidePrice for VaultsOracleAdapter {
	type AssetId = VaultsCollateralId;

	fn provide_price(
		collateral_id: &VaultsCollateralId,
	) -> Result<FixedU128, sp_runtime::DispatchError> {
		use sp_runtime::DispatchError;

		let (pair, divisor) =
			Self::collateral_feed(collateral_id).ok_or(DispatchError::Unavailable)?;
		let point =
			PriceOracle::price(pair).ok_or(DispatchError::Other("oracle price unavailable"))?;
		let now =
			<Runtime as pallet_price_oracle::Config>::BlockNumberProvider::current_block_number();
		if now.checked_sub(point.updated_at).is_none_or(|age| age > MAX_ORACLE_PRICE_AGE) {
			return Err(DispatchError::Other("stale oracle price"));
		}
		let price = point.price.into_inner() / divisor;
		if price == 0 {
			return Err(DispatchError::Other("zero oracle price"));
		}
		Ok(FixedU128::from_inner(price))
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

#[cfg(feature = "runtime-benchmarks")]
pub struct VaultsBenchmarkHelper;

#[cfg(feature = "runtime-benchmarks")]
impl pallet_vaults::BenchmarkHelper<VaultsCollateralId, VaultsStableId, AccountId>
	for VaultsBenchmarkHelper
{
	fn collateral_asset_id() -> VaultsCollateralId {
		sepolia_usdt_location()
	}

	fn set_oracle_price(asset_id: VaultsCollateralId, price: FixedU128) {
		let (pair, scale) = VaultsOracleAdapter::collateral_feed(&asset_id)
			.expect("both benchmark assets have a feed; qed");
		let whole_unit_price = price
			.into_inner()
			.checked_mul(scale)
			.expect("benchmark prices fit after conversion to whole asset units; qed");
		let updated_at =
			<Runtime as pallet_price_oracle::Config>::BlockNumberProvider::current_block_number();
		pallet_price_oracle::Prices::<Runtime>::insert(
			pair,
			frame_support::traits::PricePoint {
				price: FixedU128::from_inner(whole_unit_price),
				updated_at,
				signers: 1,
			},
		);
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
		Self::set_oracle_price(native, FixedU128::from_rational(10, ExistentialDeposit::get()));
	}

	fn fund_native(who: &AccountId) {
		use frame_support::traits::fungible::Mutate as FungibleMutate;
		<Balances as FungibleMutate<AccountId>>::mint_into(who, 1_000 * UNITS)
			.expect("minting native to a benchmark account cannot overflow the issuance; qed");
	}
}
