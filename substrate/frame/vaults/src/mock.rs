//! Test runtime for `pallet-vaults`.
//!
//! Native collateral uses `Balances`. Issued collateral uses `AssetsHolder`. Stable assets use
//! `pallet-assets`, with [`STABLE`] as the default and [`EUSD`] as the second test asset.

use alloc::collections::BTreeMap;

use crate as pallet_vaults;
use crate::{
	pallet::{Branches, CollateralCreditOf, StableCreditOf},
	types::{BranchConfigBounds, Vault},
	BranchState, HoldReason, LiquidationSettlement, VaultListId,
};
pub use crate::{
	types::BranchAdmins, BoundViolation, BranchConfig, BranchConfigDefect, BranchMode, Error,
	Pallet,
};
pub use frame::{
	arithmetic::{FixedPointNumber, FixedU128, Permill},
	prelude::{DispatchError, DispatchResult},
	testing_prelude::{assert_err, assert_noop, assert_ok, assert_storage_noop, hypothetically},
};
use frame::{
	deps::{
		frame_support::transactional,
		sp_runtime::traits::{ConvertInto, ConvertToValue},
	},
	prelude::DispatchResultWithPostInfo,
	testing_prelude::*,
	traits::{
		fungible::{HoldConsideration, ItemOf, NativeFromLeft, NativeOrWithId},
		fungibles::{
			AssetFootprintPrice, AtLeastMinimumBalance, Balanced as FungiblesBalanced, Credit,
			HoldConsideration as FungiblesHoldConsideration, Inspect as FungiblesInspect,
			InspectHold, SufficientAssets,
		},
		tokens::fungible,
		AsEnsureOriginWithArg, IdentityLookup, LinearStoragePrice,
	},
};
pub use pallet_linked_list::Position;
use stablecoin_primitives::{
	BranchSnapshot, OraclePriceConversion, RedemptionSettlement, VaultInterface,
};

pub type AccountId = u128;
pub type Balance = u128;
pub type AssetIdForAssets = u32;
/// Native or issued collateral asset ID.
pub type AssetId = NativeOrWithId<AssetIdForAssets>;
/// Stable asset ID from `pallet-assets`.
pub type StableId = AssetIdForAssets;
pub type Block = MockBlock<Test>;
pub type Moment = u64;
pub type VaultList = VaultListId<AssetId, StableId>;

/// Default full branch administrator.
pub const ADMIN: AccountId = 100;

/// Oracle price every branch is created at unless a test says otherwise.
pub const DEFAULT_PRICE: FixedU128 = FixedU128::from_u32(10);
/// Default emergency branch administrator.
pub const EMERGENCY_ADMIN: AccountId = 101;
/// Account that receives the protocol fee remainder.
pub const FEE_DEST: AccountId = 200;
/// Owner allowed to create a STABLE branch with a deposit.
pub const STABLECOIN_OWNER: AccountId = 1;

/// Native collateral used by most tests.
pub const DOT: AssetId = AssetId::Native;

/// First issued collateral asset.
pub const TOKEN_X_ID: AssetIdForAssets = 1;
pub const TOKEN_X: AssetId = AssetId::WithId(TOKEN_X_ID);

/// Second issued collateral asset.
pub const ETH_ID: AssetIdForAssets = 2;
pub const ETH: AssetId = AssetId::WithId(ETH_ID);

/// Default stable asset.
pub const STABLE: StableId = 1_000;
/// Second stable asset.
pub const EUSD: StableId = 1_001;

/// Stable asset with six decimals and a 0.01 minimum balance.
pub const USDX: StableId = 1_002;
/// One USDX in minor units.
pub const USD: Balance = 1_000_000;
/// Minimum USDX balance.
pub const USDX_ED: Balance = USD / 100;

/// Issued collateral that is not sufficient: its accounts exist only through a
/// consumer reference, which the system account needs a provider for.
pub const INSUFFICIENT_ID: AssetIdForAssets = 6;
pub const INSUFFICIENT: AssetId = AssetId::WithId(INSUFFICIENT_ID);

/// Issued collateral with ten decimals and a 0.01 minimum balance.
pub const XBT_ID: AssetIdForAssets = 5;
pub const XBT: AssetId = AssetId::WithId(XBT_ID);
/// One XBT in minor units.
pub const XBT_UNIT: Balance = 10_000_000_000;
/// Minimum XBT balance.
pub const XBT_ED: Balance = XBT_UNIT / 100;

/// Default global debt limit used by tests.
///
/// It is high enough to stay out of most tests.
pub const GLOBAL_CEILING: Balance = 1_000_000_000_000_000;

#[frame_construct_runtime]
mod runtime {
	#[runtime::runtime]
	#[runtime::derive(
		RuntimeCall,
		RuntimeEvent,
		RuntimeError,
		RuntimeOrigin,
		RuntimeTask,
		RuntimeHoldReason,
		RuntimeFreezeReason
	)]
	pub struct Test;

	#[runtime::pallet_index(0)]
	pub type System = frame_system;

	#[runtime::pallet_index(1)]
	pub type Timestamp = pallet_timestamp;

	#[runtime::pallet_index(2)]
	pub type Balances = pallet_balances;

	#[runtime::pallet_index(3)]
	pub type Assets = pallet_assets;

	#[runtime::pallet_index(4)]
	pub type AssetsHolder = pallet_assets_holder;

	#[runtime::pallet_index(5)]
	pub type LinkedList = pallet_linked_list;

	#[runtime::pallet_index(6)]
	pub type Vaults = pallet_vaults;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountId = AccountId;
	type AccountData = pallet_balances::AccountData<Balance>;
	type Lookup = IdentityLookup<Self::AccountId>;
}

#[derive_impl(pallet_timestamp::config_preludes::TestDefaultConfig)]
impl pallet_timestamp::Config for Test {}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig as pallet_balances::DefaultConfig)]
impl pallet_balances::Config for Test {
	type AccountStore = System;
	type Balance = Balance;
}

#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig as pallet_assets::DefaultConfig)]
impl pallet_assets::Config for Test {
	type CreateOrigin = AsEnsureOriginWithArg<frame_system::EnsureSigned<AccountId>>;
	type ForceOrigin = frame_system::EnsureRoot<AccountId>;
	type Currency = Balances;
	type Holder = AssetsHolder;
	type Balance = Balance;
}

impl pallet_assets_holder::Config for Test {
	type RuntimeHoldReason = RuntimeHoldReason;
	type RuntimeEvent = RuntimeEvent;
}

parameter_types! {
	pub const MaxHintRepairSteps: u32 = 16;
}

impl pallet_linked_list::Config for Test {
	type WeightInfo = ();
	type ListId = VaultList;
	type ItemId = AccountId;
	type Priority = FixedU128;
	type MaxHintRepairSteps = MaxHintRepairSteps;
	type PriorityProvider = Pallet<Test>;
}

/// Common interface for native and issued collateral.
pub type VaultCollateralAssets =
	fungible::UnionOf<Balances, AssetsHolder, NativeFromLeft, AssetId, AccountId>;

parameter_types! {
	pub const StableAssetId: AssetIdForAssets = STABLE;
}
/// Single-asset view of the default stable asset.
pub type Stable = ItemOf<Assets, StableAssetId, AccountId>;

/// Test oracle with one price per collateral asset.
pub struct MockOracle;
parameter_types! {
	pub storage MockPrices: BTreeMap<AssetId, FixedU128> = BTreeMap::new();
	pub storage MockOracleAvailable: bool = true;
}
impl stablecoin_primitives::ProvidePrice for MockOracle {
	type AssetId = AssetId;

	fn provide_price(collateral: &AssetId) -> Result<FixedU128, DispatchError> {
		// Production adapters report a missing price as `Unavailable`. The pallet propagates the
		// oracle's error, so a pallet error here would let tests assert what no runtime returns.
		if !MockOracleAvailable::get() {
			return Err(DispatchError::Unavailable);
		}
		MockPrices::get().get(collateral).copied().ok_or(DispatchError::Unavailable)
	}
}

parameter_types! {
	/// Share of each fee assigned to the Stability Pool.
	pub storage SpFeeShare: Permill = Permill::from_percent(75);
	/// Receives every test stablecoin's fee remainder.
	pub const FeeDestAccount: AccountId = FEE_DEST;
}

/// Burns the test Stability Pool fee share and returns the remainder.
pub struct MockYieldHook;
impl stablecoin_primitives::OnBranchYield<AssetId, Credit<AccountId, Assets>> for MockYieldHook {
	fn distribute_yield(
		_: &AssetId,
		branch: BranchSnapshot,
		credit: Credit<AccountId, Assets>,
	) -> Credit<AccountId, Assets> {
		assert_eq!(branch.now, Timestamp::get(), "the engine passes its own clock");
		let sp_share = SpFeeShare::get() * credit.peek();
		let (sp_credit, residual) = credit.split(sp_share);
		// The Stability Pool, not the branch, takes this share out of circulation.
		burn_external(sp_credit);
		residual
	}
}

/// The lifecycle hook a [`LifecycleLog`] entry records.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Encode, Decode)]
pub enum Lifecycle {
	Registered,
	Deregistered,
}

parameter_types! {
	/// Recorded branch lifecycle calls, with the stablecoin branch count the hook was given.
	pub storage LifecycleLog: Vec<(AssetId, StableId, Lifecycle, u32)> = Vec::new();
	/// Makes branch registration fail in the lifecycle hook.
	pub storage FailOnRegistered: bool = false;
	/// Makes branch removal fail in the lifecycle hook.
	pub storage FailOnDeregistered: bool = false;
}

fn record_lifecycle(entry: (AssetId, StableId, Lifecycle, u32)) {
	let mut log = LifecycleLog::get();
	log.push(entry);
	LifecycleLog::set(&log);
}

/// Records branch lifecycle calls and supports forced failures.
pub struct RecordingLifecycle;
impl stablecoin_primitives::OnBranchLifecycle<AssetId, StableId, AccountId> for RecordingLifecycle {
	type RegistrationConfig = ();

	fn on_registered(
		collateral_id: &AssetId,
		stable_id: &StableId,
		stablecoin_markets: u32,
		_config: Self::RegistrationConfig,
		_funder: &AccountId,
	) -> DispatchResult {
		record_lifecycle((
			collateral_id.clone(),
			*stable_id,
			Lifecycle::Registered,
			stablecoin_markets,
		));
		if FailOnRegistered::get() {
			return Err(DispatchError::Other("on_registered failure"));
		}
		Ok(())
	}
	fn on_deregistered(
		collateral_id: &AssetId,
		stable_id: &StableId,
		remaining_stablecoin_markets: u32,
		_funder: &AccountId,
	) -> DispatchResult {
		record_lifecycle((
			collateral_id.clone(),
			*stable_id,
			Lifecycle::Deregistered,
			remaining_stablecoin_markets,
		));
		if FailOnDeregistered::get() {
			return Err(DispatchError::Other("on_deregistered failure"));
		}
		Ok(())
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_registration_config(_stablecoin_markets: u32) -> Self::RegistrationConfig {}
}

/// Allows root or the stable asset owner to create a branch.
///
/// Root pays no deposit. The asset owner does.
pub type EnsureAssetOwner = stablecoin_primitives::EnsureStableOwnerOrRoot<Assets, AccountId>;

parameter_types! {
	pub const MarketDepositReason: RuntimeHoldReason =
		RuntimeHoldReason::Vaults(HoldReason::BranchCreationDeposit);
	pub const MarketDepositBase: Balance = 1_000;
}

/// Refundable 1,000-unit branch creation deposit.
pub type VaultsConsideration = HoldConsideration<
	AccountId,
	Balances,
	MarketDepositReason,
	LinearStoragePrice<MarketDepositBase, ConstU128<0>, Balance>,
>;

/// Refundable per-vault deposit in native units, distinct from [`MarketDepositBase`] so the two
/// holds cannot be confused in assertions.
pub const VAULT_DEPOSIT: Balance = 250;

parameter_types! {
	pub const VaultDepositReason: RuntimeHoldReason =
		RuntimeHoldReason::Vaults(HoldReason::VaultCreationDeposit);
	pub const VaultDepositBase: Balance = VAULT_DEPOSIT;
	pub const NativeCollateral: AssetId = DOT;
}

/// Settles in the collateral when it is native or sufficient, else in native, re-priced through
/// the oracle and never below the asset's minimum balance, as the runtimes compose it.
pub type VaultDepositPolicy = AssetFootprintPrice<
	SufficientAssets<VaultCollateralAssets, AccountId>,
	NativeCollateral,
	LinearStoragePrice<VaultDepositBase, ConstU128<0>, Balance>,
	AtLeastMinimumBalance<
		VaultCollateralAssets,
		OraclePriceConversion<MockOracle, NativeCollateral>,
		AccountId,
	>,
>;

pub type VaultDepositConsideration = FungiblesHoldConsideration<
	AccountId,
	VaultCollateralAssets,
	VaultDepositReason,
	VaultDepositPolicy,
>;

parameter_types! {
	pub const VaultsPalletId: PalletId = PalletId(*b"py/vault");
	pub TestBranchConfigBounds: BranchConfigBounds = BranchConfigBounds {
		min_minimum_collateralization_ratio: FixedU128::from_rational(105u128, 100u128),
		min_initial_collateralization_ratio: FixedU128::from_rational(110u128, 100u128),
		min_safety_collateralization_ratio: FixedU128::from_rational(120u128, 100u128),
		max_borrow_rate: FixedU128::from_rational(400u128, 100u128),
	};
}

impl pallet_vaults::Config for Test {
	type StableToCollateralId = ConvertInto;
	type CollateralAssets = VaultCollateralAssets;
	type StableAssets = Assets;
	type Oracle = MockOracle;
	type FeeAccount = ConvertToValue<FeeDestAccount>;
	type YieldHook = MockYieldHook;
	type OnBranchLifecycle = RecordingLifecycle;
	type TimeProvider = Timestamp;
	type CreateOrigin = EnsureAssetOwner;
	type BranchConsideration = VaultsConsideration;
	type VaultConsideration = VaultDepositConsideration;
	type BranchConfigBounds = TestBranchConfigBounds;
	type ForceOrigin = frame_system::EnsureRoot<AccountId>;
	type GlobalDebtCeiling = pallet_vaults::StoredCeiling<Test>;
	type PalletId = VaultsPalletId;
	type VaultLists = LinkedList;
	type WeightInfo = ();
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = MockBenchmarkHelper;
}

#[cfg(feature = "runtime-benchmarks")]
pub struct MockBenchmarkHelper;

#[cfg(feature = "runtime-benchmarks")]
impl pallet_vaults::BenchmarkHelper<AssetId, StableId> for MockBenchmarkHelper {
	fn collateral_asset_id() -> AssetId {
		DOT
	}

	fn stable_asset_id() -> StableId {
		STABLE
	}

	fn set_oracle_price(collateral_id: AssetId, price: FixedU128) {
		set_price(collateral_id, price);
	}

	fn clear_oracle_price(collateral_id: AssetId) {
		clear_price(collateral_id);
	}

	fn advance_time(ms: u64) {
		advance_time(ms);
	}

	// The genesis config already creates every stable asset.
	fn ensure_stable_asset(_: StableId) {}
}

/// Builds the test externalities: genesis balances, the oracle and pool knobs, and the branches a
/// test starts from.
#[derive(Default)]
pub struct ExtBuilder {
	prices: BTreeMap<AssetId, FixedU128>,
	branches: Vec<(AssetId, StableId, BranchConfig<Balance>)>,
}

impl ExtBuilder {
	/// Seeds the oracle price of `collateral`. A branch registered by [`Self::branch`] opens at
	/// this price, so a test can start a branch away from [`DEFAULT_PRICE`].
	pub fn price(mut self, collateral: AssetId, price: FixedU128) -> Self {
		self.prices.insert(collateral, price);
		self
	}

	/// Registers a branch with [`default_branch_config`] and the default global debt ceiling.
	pub fn branch(self, collateral: AssetId, stable: StableId) -> Self {
		self.branch_with(collateral, stable, default_branch_config())
	}

	/// Registers a branch with `config` and the default global debt ceiling.
	pub fn branch_with(
		mut self,
		collateral: AssetId,
		stable: StableId,
		config: BranchConfig<Balance>,
	) -> Self {
		self.branches.push((collateral, stable, config));
		self
	}

	/// Builds fresh storage with the configured branches registered at block 1.
	///
	/// The native price defaults to [`DEFAULT_PRICE`]: vault deposits on non-native collateral
	/// are re-priced from it, as in the runtimes.
	pub fn build(mut self) -> TestState {
		self.prices.entry(DOT).or_insert(DEFAULT_PRICE);
		let mut ext: TestState = genesis_storage().into();
		ext.execute_with(|| {
			System::set_block_number(1);
			Timestamp::set_timestamp(1_000);
			MockPrices::set(&self.prices);
			for (collateral, stable, config) in self.branches {
				let price = self.prices.get(&collateral).copied().unwrap_or(DEFAULT_PRICE);
				register_market_with(collateral, stable, price, config);
			}
		});
		ext
	}

	/// Runs a test, then checks the pallet's invariants against the state it left behind.
	pub fn build_and_execute(self, test: impl FnOnce()) {
		self.build().execute_with(|| {
			test();
			check_invariants();
		});
	}
}

/// Genesis storage shared by every test: the issued assets and the funded accounts.
fn genesis_storage() -> frame::deps::sp_runtime::Storage {
	RuntimeGenesisConfig {
		assets: pallet_assets::GenesisConfig {
			assets: vec![
				(TOKEN_X_ID, 1, true, 1),
				(ETH_ID, 1, true, 1),
				(STABLE, 1, true, 1),
				(EUSD, 1, true, 1),
				(USDX, 1, true, USDX_ED),
				(XBT_ID, 1, true, XBT_ED),
				(INSUFFICIENT_ID, 1, false, 1),
			],
			metadata: vec![],
			// Native collateral is funded below; these are the issued collaterals. Only the branch
			// creator holds the insufficient asset: an account of one takes a consumer reference,
			// which changes what that account may spend down to.
			accounts: (1..=10)
				.chain([ADMIN])
				.flat_map(|who| {
					[TOKEN_X_ID, ETH_ID]
						.map(|asset| (asset, who, 1_000_000_000_000))
						.into_iter()
						.chain([(XBT_ID, who, 100_000_000 * XBT_UNIT)])
				})
				.chain([(INSUFFICIENT_ID, ADMIN, 1_000_000_000_000)])
				.collect(),
			next_asset_id: None,
			reserves: vec![],
		},
		system: Default::default(),
		balances: pallet_balances::GenesisConfig {
			// The fee account needs native funds to pay its asset-account deposit.
			// The full admin funds the custody seed of every Root-created branch.
			balances: (1..=10).chain([FEE_DEST, ADMIN]).map(|i| (i, 1_000_000_000_000)).collect(),
			..Default::default()
		},
	}
	.build_storage()
	.expect("the test genesis config is valid; qed")
}

parameter_types! {
	/// Net stablecoin issued outside branch debt, per stable asset.
	///
	/// Kept in storage so a rolled-back transaction rolls the ledger back with the supply.
	pub storage ExternalStableIssuance: BTreeMap<StableId, i128> = BTreeMap::new();
}

/// Records stablecoin issued (`delta > 0`) or burned (`delta < 0`) outside branch debt.
///
/// Every supply change a test or test hook makes behind the pallet's back must pass here, or
/// [`check_stable_supply`] reports it.
fn record_external_issuance(stable: StableId, delta: i128) {
	let mut ledger = ExternalStableIssuance::get();
	let entry = ledger.entry(stable).or_default();
	*entry = entry.checked_add(delta).expect("external issuance ledger overflow");
	ExternalStableIssuance::set(&ledger);
}

/// Issues stablecoin outside branch debt, recorded so [`check_stable_supply`] still balances.
fn issue_external(stable: StableId, amount: Balance) -> StableCreditOf<Test> {
	record_external_issuance(stable, signed(amount));
	<Assets as FungiblesBalanced<AccountId>>::issue(stable, amount)
}

/// Burns stablecoin outside branch debt, recorded so [`check_stable_supply`] still balances.
fn burn_external(credit: StableCreditOf<Test>) {
	record_external_issuance(credit.asset(), -signed(credit.peek()));
	drop(credit);
}

/// Converts a balance into the ledger's signed domain.
fn signed(amount: Balance) -> i128 {
	i128::try_from(amount).expect("test balances fit in i128")
}

/// Checks `try_state` and the stablecoin supply against the branch debt ledger.
#[track_caller]
pub fn check_invariants() {
	crate::try_state::do_try_state::<Test>().expect("accounting invariants hold");
	check_stable_supply();
}

/// Each stable asset's issuance equals the debt its branches carry plus net external issuance.
///
/// `try_state` recomputes aggregates from vault rows, so a debt change that moves rows and
/// aggregates together passes it even when no stablecoin moved. Supply is the one quantity the
/// pallet does not write, so this identity catches debt cancelled without its burn and debt
/// created without its mint.
#[track_caller]
pub fn check_stable_supply() {
	let external = ExternalStableIssuance::get();
	for stable in [STABLE, EUSD, USDX] {
		let debt = Branches::<Test>::iter()
			.filter(|(_, branch_stable, _)| *branch_stable == stable)
			.map(|(_, _, branch)| signed(branch.state.debt.outstanding()))
			.try_fold(0i128, i128::checked_add)
			.expect("debt sum overflow");
		let expected = debt
			.checked_add(external.get(&stable).copied().unwrap_or_default())
			.expect("expected supply overflow");
		assert_eq!(
			signed(total_stable(stable)),
			expected,
			"stable {stable} supply != Σ branch debt + external issuance",
		);
	}
}

/// Sets the oracle price for a collateral asset.
pub fn set_price(collateral: AssetId, price: FixedU128) {
	let mut prices = MockPrices::get();
	prices.insert(collateral, price);
	MockPrices::set(&prices);
}

/// Takes the oracle down for `collateral` until the next [`set_price`].
pub fn clear_price(collateral: AssetId) {
	let mut prices = MockPrices::get();
	prices.remove(&collateral);
	MockPrices::set(&prices);
}

/// Advances time without changing the block number.
pub fn advance_time(ms: Moment) {
	let now = Timestamp::get();
	Timestamp::set_timestamp(now + ms);
}

/// Returns the default branch settings used by tests.
pub fn default_branch_config() -> BranchConfig<Balance> {
	BranchConfig {
		minimum_collateralization_ratio: FixedU128::from_rational(110u128, 100u128),
		initial_collateralization_ratio: FixedU128::from_rational(120u128, 100u128),
		safety_collateralization_ratio: FixedU128::from_rational(130u128, 100u128),
		debt_ceiling: 1_000_000_000_000,
		minimum_debt: 200,
		minimum_collateral: 1,
		minimum_borrow_rate: FixedU128::from_rational(1u128, 1_000u128),
		maximum_borrow_rate: FixedU128::from_rational(400u128, 100u128),
		upfront_fee_period: 7 * 24 * 3_600 * 1_000,
		rate_adjustment_cooldown: 24 * 3_600 * 1_000,
		redistribution_penalty: Permill::from_percent(5),
	}
}

/// Builds a branch administrator pair.
pub fn branch_admins(full: AccountId, emergency: AccountId) -> BranchAdmins<AccountId> {
	BranchAdmins { full_admin: full, emergency_admin: emergency }
}

/// Creates a branch without setting its global debt limit.
pub fn create_market(
	collateral: AssetId,
	stable: StableId,
	price: FixedU128,
	config: BranchConfig<Balance>,
) {
	// Branch creation requires a collateral price.
	set_price(collateral.clone(), price);
	Vaults::create_branch(
		RuntimeOrigin::root(),
		collateral,
		stable,
		branch_admins(ADMIN, EMERGENCY_ADMIN),
		config,
		(),
	)
	.expect("create_branch ok");
}

/// Creates a branch and enables borrowing with the default global debt limit.
pub fn register_market_with(
	collateral: AssetId,
	stable: StableId,
	price: FixedU128,
	config: BranchConfig<Balance>,
) {
	create_market(collateral.clone(), stable, price, config);
	Vaults::set_global_debt_ceiling(RuntimeOrigin::root(), stable, GLOBAL_CEILING)
		.expect("set global debt ceiling");
}

/// Creates a branch with default settings and [`DEFAULT_PRICE`].
pub fn register_market(collateral: AssetId, stable: StableId) {
	register_market_with(collateral, stable, DEFAULT_PRICE, default_branch_config());
}

/// Opens a vault using endpoint-only list hints.
pub fn open(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	collateral_amount: Balance,
	debt: Balance,
	rate: FixedU128,
) -> DispatchResult {
	Vaults::open_vault(
		RuntimeOrigin::signed(who),
		collateral,
		stable,
		collateral_amount,
		debt,
		rate,
		Position::endpoints_only(),
	)
}

/// Liquidation amounts used by the test helpers.
///
/// The real interface uses collateral credits instead.
pub struct OffsetAllocation<AccountId, Balance> {
	pub collateral_recipient: AccountId,
	pub debt: Balance,
	pub collateral: Balance,
}

pub struct KeeperCompensation<AccountId, Balance> {
	pub recipient: AccountId,
	pub collateral: Balance,
}

pub struct LiquidationAllocation<AccountId, Balance> {
	pub offset: OffsetAllocation<AccountId, Balance>,
	pub redistribution_collateral: Balance,
	pub keeper: KeeperCompensation<AccountId, Balance>,
}

/// Liquidates a vault by fully offsetting its current debt.
pub fn liquidate(collateral: AssetId, stable: StableId, owner: AccountId) -> DispatchResult {
	liquidate_with(collateral, stable, owner, |post_touch| LiquidationAllocation {
		offset: OffsetAllocation { collateral_recipient: owner, debt: post_touch, collateral: 0 },
		redistribution_collateral: 0,
		keeper: KeeperCompensation { recipient: owner, collateral: 0 },
	})
}

/// Liquidates a vault with a caller-built allocation.
pub fn liquidate_with(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
	build: impl FnOnce(Balance) -> LiquidationAllocation<AccountId, Balance>,
) -> DispatchResult {
	Vaults::execute_liquidation(&collateral, &stable, &owner, |snapshot, mut collateral_credit| {
		let allocation = build(snapshot.debt);
		let total = allocation
			.offset
			.collateral
			.saturating_add(allocation.redistribution_collateral)
			.saturating_add(allocation.keeper.collateral);
		ensure!(total <= collateral_credit.peek(), Error::<Test>::InvalidLiquidationSettlement);

		resolve_test_collateral(
			&mut collateral_credit,
			allocation.offset.collateral,
			&allocation.offset.collateral_recipient,
		)?;
		resolve_test_collateral(
			&mut collateral_credit,
			allocation.keeper.collateral,
			&allocation.keeper.recipient,
		)?;
		let redistribution_collateral =
			collateral_credit.extract(allocation.redistribution_collateral);
		// No Stability Pool burns the offset here, so its stablecoin stays in circulation without
		// market debt behind it. The transactional liquidation rolls this back with the step.
		record_external_issuance(stable, signed(allocation.offset.debt));
		Ok(LiquidationSettlement {
			debt_offset: allocation.offset.debt,
			redistribution_collateral,
			owner_surplus: collateral_credit,
		})
	})
}

/// Calls the `poke` dispatchable as `who` against `owner`'s vault.
pub fn poke(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
) -> DispatchResult {
	Vaults::poke(RuntimeOrigin::signed(who), collateral, stable, owner)
}

/// Calls the `refresh_branch` dispatchable as `who`.
pub fn refresh_branch(who: AccountId, collateral: AssetId, stable: StableId) -> DispatchResult {
	Vaults::refresh_branch(RuntimeOrigin::signed(who), collateral, stable)
}

/// Calls the `repay_for` dispatchable; `None` repays the whole debt.
pub fn repay(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
	amount: Option<Balance>,
) -> DispatchResult {
	Vaults::repay_for(RuntimeOrigin::signed(who), collateral, stable, owner, amount)
}

/// Calls the `borrow` dispatchable to the caller with endpoint hints.
pub fn borrow(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	amount: Balance,
	new_rate: Option<FixedU128>,
) -> DispatchResult {
	Vaults::borrow(
		RuntimeOrigin::signed(who),
		collateral,
		stable,
		amount,
		new_rate,
		None,
		Position::endpoints_only(),
	)
}

/// Calls the `change_rate` dispatchable with endpoint hints.
pub fn change_rate(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	new_rate: FixedU128,
) -> DispatchResult {
	Vaults::change_rate(
		RuntimeOrigin::signed(who),
		collateral,
		stable,
		new_rate,
		Position::endpoints_only(),
	)
}

/// Calls the `close_vault` dispatchable; `None` returns collateral to the owner.
pub fn close_vault(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	recipient: Option<AccountId>,
) -> DispatchResult {
	Vaults::close_vault(RuntimeOrigin::signed(who), collateral, stable, recipient)
}

/// Calls the `deposit_collateral_for` dispatchable.
pub fn deposit_collateral(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
	amount: Balance,
) -> DispatchResult {
	Vaults::deposit_collateral_for(RuntimeOrigin::signed(who), collateral, stable, owner, amount)
}

/// Calls the `withdraw_collateral` dispatchable; `None` pays the owner.
pub fn withdraw_collateral(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	amount: Balance,
	recipient: Option<AccountId>,
) -> DispatchResult {
	Vaults::withdraw_collateral(RuntimeOrigin::signed(who), collateral, stable, amount, recipient)
}

/// Calls the `set_governance_frozen` dispatchable as a signed branch admin.
pub fn set_governance_frozen(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	frozen: bool,
) -> DispatchResult {
	Vaults::set_governance_frozen(RuntimeOrigin::signed(who), collateral, stable, frozen)
}

/// Calls the `enter_final_recovery` dispatchable as `keeper`.
pub fn enter_final_recovery(
	keeper: AccountId,
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
) -> DispatchResult {
	Vaults::enter_final_recovery(RuntimeOrigin::signed(keeper), collateral, stable, owner)
}

/// Calls the `exit_final_recovery` dispatchable with endpoint hints.
pub fn exit_final_recovery(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
) -> DispatchResult {
	Vaults::exit_final_recovery(
		RuntimeOrigin::signed(who),
		collateral,
		stable,
		owner,
		Position::endpoints_only(),
	)
}

/// Calls the `activate_dormant` dispatchable with endpoint hints.
pub fn activate_dormant(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
) -> DispatchResultWithPostInfo {
	Vaults::activate_dormant(
		RuntimeOrigin::signed(who),
		collateral,
		stable,
		owner,
		Position::endpoints_only(),
	)
}

/// Removes a vault and records its whole debt as redistribution.
///
/// This bypasses liquidation pricing only for tests concerned with the
/// redistribution ledger. Economic and custody tests use [`liquidate_with`].
pub fn redistribute_for_test(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
	redistribution_collateral: Balance,
) -> Result<Balance, DispatchError> {
	let mut redistributed: Balance = 0;
	liquidate_with(collateral, stable, owner, |post_touch| {
		redistributed = post_touch;
		LiquidationAllocation {
			offset: OffsetAllocation { collateral_recipient: 0, debt: 0, collateral: 0 },
			redistribution_collateral,
			keeper: KeeperCompensation { recipient: 0, collateral: 0 },
		}
	})?;
	Ok(redistributed)
}

fn resolve_test_collateral(
	collateral: &mut CollateralCreditOf<Test>,
	amount: Balance,
	recipient: &AccountId,
) -> DispatchResult {
	if amount.is_zero() {
		return Ok(());
	}
	let credit = collateral.extract(amount);
	debug_assert_eq!(credit.peek(), amount);
	<VaultCollateralAssets as FungiblesBalanced<AccountId>>::resolve(recipient, credit).map_err(
		|credit| {
			drop(credit);
			TokenError::CannotCreate.into()
		},
	)
}

/// Redeems from the next branch target at the oracle price.
///
/// Returns the target owner.
pub fn redeem(
	collateral: AssetId,
	stable: StableId,
	recipient: AccountId,
	amount: Balance,
) -> Result<AccountId, DispatchError> {
	let (owner, _status) =
		<Vaults as VaultInterface>::next_redemption_target(&collateral, &stable, None)
			.ok_or(DispatchError::Other("no redemption target"))?;
	redeem_from(collateral, stable, owner, recipient, amount)?;
	Ok(owner)
}

/// Redeems from a chosen vault at the oracle price.
///
/// The helper issues the stable assets used for payment.
pub fn redeem_from(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
	recipient: AccountId,
	amount: Balance,
) -> DispatchResult {
	let price = MockPrices::get().get(&collateral).copied().expect("price set");
	let snapshot =
		<Vaults as VaultInterface>::project_redemption_snapshot(&collateral, &stable, &owner)?;
	let debt_to_cancel = core::cmp::min(amount, snapshot.debt);
	let collateral_to_recipient =
		(FixedU128::saturating_from_integer(debt_to_cancel) / price).saturating_mul_int(1u128);
	redeem_step(collateral, stable, owner, recipient, debt_to_cancel, collateral_to_recipient)
}

/// Runs one redemption step against a chosen vault, paying with newly issued
/// stable assets.
///
/// The credit is issued and consumed inside one transaction layer, so a
/// rejected settlement rolls the issuance back with the step.
#[transactional]
pub fn redeem_step(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
	recipient: AccountId,
	debt_to_cancel: Balance,
	collateral_to_recipient: Balance,
) -> DispatchResult {
	<Vaults as VaultInterface>::redeem_step(
		&collateral,
		&stable,
		&owner,
		&recipient,
		settlement(stable, debt_to_cancel, collateral_to_recipient),
	)
}

/// Builds a redemption settlement with newly issued stable assets.
pub fn settlement(
	stable: StableId,
	debt_to_cancel: Balance,
	collateral_to_recipient: Balance,
) -> RedemptionSettlement<StableCreditOf<Test>, Balance> {
	// Stands in for the redeemer's stablecoin, which the branch burns against the debt.
	let debt_payment = issue_external(stable, debt_to_cancel);
	RedemptionSettlement { debt_payment, collateral_to_recipient }
}

/// Returns a registered branch's accounting state, failing the calling test if it is absent.
#[track_caller]
pub fn market_state(collateral: AssetId, stable: StableId) -> BranchState<AccountId, Balance> {
	Branches::<Test>::get(collateral, stable).expect("branch is registered").state
}

/// Returns a registered branch's settings, failing the calling test if it is absent.
#[track_caller]
pub fn market_config(collateral: AssetId, stable: StableId) -> BranchConfig<Balance> {
	Branches::<Test>::get(collateral, stable).expect("branch is registered").config
}

/// Updates branch state through the pallet's commit path.
pub fn mutate_branch_state(
	collateral: AssetId,
	stable: StableId,
	mutate: impl FnOnce(&mut BranchState<AccountId, Balance>),
) {
	Vaults::try_mutate_branch_state(&collateral, &stable, |_, state, _| {
		mutate(state);
		Ok(())
	})
	.expect("branch committed");
}

/// Returns the current branch mode, if it can be calculated.
pub fn branch_mode(collateral: AssetId, stable: StableId) -> Option<BranchMode> {
	Vaults::current_mode(&collateral, &stable).ok()
}

/// Returns collateral held for vaults across all of the owner's branches.
pub fn held(collateral: AssetId, who: AccountId) -> Balance {
	<VaultCollateralAssets as InspectHold<AccountId>>::balance_on_hold(
		collateral,
		&HoldReason::VaultCollateral.into(),
		&who,
	)
}

/// Returns an account's total collateral balance, including holds.
pub fn collateral_balance(collateral: AssetId, who: AccountId) -> Balance {
	<VaultCollateralAssets as FungiblesInspect<AccountId>>::balance(collateral, &who)
}

/// Returns an account's balance of one stable asset.
pub fn stable_balance(stable: StableId, who: AccountId) -> Balance {
	<Assets as FungiblesInspect<AccountId>>::balance(stable, &who)
}

/// Mints collateral through the fungible traits: the holder union has no minting side, so the
/// native and issued halves are addressed directly.
pub fn mint_collateral(collateral: AssetId, who: AccountId, amount: Balance) {
	match collateral {
		NativeOrWithId::Native => {
			<Balances as fungible::Mutate<AccountId>>::mint_into(&who, amount)
				.expect("mint native collateral")
		},
		NativeOrWithId::WithId(asset) => {
			<Assets as frame::traits::fungibles::Mutate<AccountId>>::mint_into(asset, &who, amount)
				.expect("mint issued collateral")
		},
	};
}

/// Mints stable assets for liquidation and repayment tests.
pub fn mint_stable(stable: StableId, who: AccountId, amount: Balance) {
	record_external_issuance(stable, signed(amount));
	<Assets as frame::traits::fungibles::Mutate<AccountId>>::mint_into(stable, &who, amount)
		.expect("mint stable");
}

/// Returns the total issuance of one stable asset.
pub fn total_stable(stable: StableId) -> Balance {
	<Assets as FungiblesInspect<AccountId>>::total_issuance(stable)
}

/// Returns the branch creation deposit held from an account.
pub fn creation_deposit_held(who: AccountId) -> Balance {
	use frame::traits::fungible::InspectHold;
	<Balances as InspectHold<AccountId>>::balance_on_hold(
		&RuntimeHoldReason::Vaults(HoldReason::BranchCreationDeposit),
		&who,
	)
}

/// Returns the vault deposit an account has on hold in one asset.
pub fn vault_deposit_held(asset: AssetId, who: AccountId) -> Balance {
	<VaultCollateralAssets as InspectHold<AccountId>>::balance_on_hold(
		asset,
		&HoldReason::VaultCreationDeposit.into(),
		&who,
	)
}

/// Returns a vault's accounting row, panicking when the vault is missing.
pub fn vault(collateral: AssetId, stable: StableId, owner: AccountId) -> Vault<Balance> {
	Vaults::vault_of(&collateral, &stable, &owner).expect("vault stored")
}

/// Whether a vault row exists.
pub fn vault_exists(collateral: AssetId, stable: StableId, owner: AccountId) -> bool {
	crate::pallet::Vaults::<Test>::contains_key((collateral, stable, owner))
}
