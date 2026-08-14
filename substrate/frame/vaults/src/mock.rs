//! Test runtime for `pallet-vaults`.
//!
//! Native collateral uses `Balances`. Issued collateral uses `AssetsHolder`. Stable assets use
//! `pallet-assets`, with [`PUSD`] as the default and [`EUSD`] as the second test asset.

use alloc::collections::BTreeMap;

use crate as pallet_vaults;
use crate::{
	context::VaultOp,
	pallet::Branches,
	types::{BranchConfigBounds, Vault},
	BranchState, LiquidationConfig, VaultListId,
};
pub use crate::{
	pallet::{BalanceOf, CollateralCreditOf, StableCreditOf},
	types::BranchAdmins,
	BoundViolation, BranchConfig, BranchConfigDefect, BranchMode, Error, Event, HoldReason, Pallet,
};
pub use frame::{
	arithmetic::{FixedPointNumber, FixedU128, Permill, Saturating},
	prelude::{DispatchError, DispatchResult},
	testing_prelude::{assert_err, assert_noop, assert_ok},
};
use frame::{
	deps::{
		frame_support::transactional,
		sp_runtime::traits::{Convert as ConvertTrait, ConvertInto},
	},
	testing_prelude::*,
	traits::{
		fungible::{HoldConsideration, ItemOf, NativeFromLeft, NativeOrWithId},
		fungibles::{
			roles::Inspect as FungiblesRolesInspect, AssetFootprintPrice, AtLeastMinimumBalance,
			Balanced as FungiblesBalanced, BalancedHold as FungiblesBalancedHold, Credit,
			HoldConsideration as FungiblesHoldConsideration, Inspect as FungiblesInspect,
			InspectHold, SufficientAssets,
		},
		tokens::{fungible, ConversionToAssetBalance, FallbackOnUnavailable},
		AsEnsureOriginWithArg, EnsureOriginWithArg, IdentityLookup, LinearStoragePrice,
	},
};
pub use pallet_linked_list::Position;
use pusd_primitives::{
	OffsetLegs, OraclePriceConversion, RedemptionSettlement, StabilityPoolInspect,
	StabilityPoolOffset, VaultInterface,
};

pub type AccountId = u64;
pub type Balance = u128;
pub type AssetIdForAssets = u32;
/// Native or issued collateral asset ID.
pub type AssetId = NativeOrWithId<AssetIdForAssets>;
/// Stable asset ID from `pallet-assets`.
pub type StableId = AssetIdForAssets;
pub type Block = MockBlock<Test>;
pub type Moment = u64;
pub type VaultList = VaultListId<AssetId, StableId>;

/// Default full market administrator.
pub const ADMIN: AccountId = 100;
/// Default emergency market administrator.
pub const EMERGENCY_ADMIN: AccountId = 101;
/// Account that receives the protocol fee remainder.
pub const FEE_DEST: AccountId = 200;
/// Owner allowed to create a PUSD market with a deposit.
pub const PUSD_OWNER: AccountId = 1;
/// Account that receives collateral consumed by the mocked Stability Pool.
pub const SP_ACCOUNT: AccountId = 777;

/// Native collateral used by most tests.
pub const DOT: AssetId = AssetId::Native;

/// First issued collateral asset.
pub const TOKEN_X_ID: AssetIdForAssets = 1;
pub const TOKEN_X: AssetId = AssetId::WithId(TOKEN_X_ID);

/// Second issued collateral asset.
pub const ETH_ID: AssetIdForAssets = 2;
pub const ETH: AssetId = AssetId::WithId(ETH_ID);

/// Extra issued collateral assets for registry tests.
pub const COLL_C_ID: AssetIdForAssets = 3;
pub const COLL_C: AssetId = AssetId::WithId(COLL_C_ID);
pub const COLL_D_ID: AssetIdForAssets = 4;
pub const COLL_D: AssetId = AssetId::WithId(COLL_D_ID);

/// Default stable asset.
pub const PUSD: StableId = 1_000;
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
	type ExistentialDeposit = ConstU128<1>;
	type RuntimeHoldReason = RuntimeHoldReason;
}

#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig as pallet_assets::DefaultConfig)]
impl pallet_assets::Config for Test {
	type AssetId = AssetIdForAssets;
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

/// Interface for all test stable assets.
pub type VaultStableAssets = Assets;

parameter_types! {
	pub const PusdAssetId: AssetIdForAssets = PUSD;
}
/// Single-asset view of the default stable asset.
pub type Pusd = ItemOf<Assets, PusdAssetId, AccountId>;

/// Test oracle with one price per collateral asset.
pub struct MockOracle;
parameter_types! {
	pub static MockPrices: BTreeMap<AssetId, FixedU128> = BTreeMap::new();
	pub static MockOracleAvailable: bool = true;
}
impl pusd_primitives::ProvidePrice for MockOracle {
	type AssetId = AssetId;

	fn provide_price(collateral: &AssetId) -> Result<FixedU128, DispatchError> {
		if !MockOracleAvailable::get() {
			return Err(Error::<Test>::OraclePriceNotAvailable.into());
		}
		MockPrices::get().get(collateral).copied().ok_or(DispatchError::Unavailable)
	}
}

parameter_types! {
	pub const FeeDestAccount: AccountId = FEE_DEST;
	/// Share of each fee assigned to the Stability Pool.
	pub static SpFeeShare: Permill = Permill::from_percent(75);
}

/// Routes all test stablecoin fee remainders to [`FEE_DEST`].
pub struct FeeAccounts;
impl ConvertTrait<StableId, AccountId> for FeeAccounts {
	fn convert(_stable: StableId) -> AccountId {
		FEE_DEST
	}
}

/// Burns the test Stability Pool fee share and returns the remainder.
pub struct MockYieldHook;
impl pusd_primitives::OnBranchYield<AssetId, Credit<AccountId, VaultStableAssets>>
	for MockYieldHook
{
	fn distribute_yield(
		_: &AssetId,
		credit: Credit<AccountId, VaultStableAssets>,
	) -> Credit<AccountId, VaultStableAssets> {
		let sp_share = SpFeeShare::get() * credit.peek();
		let (sp_credit, residual) = credit.split(sp_share);
		drop(sp_credit);
		residual
	}
}

parameter_types! {
	/// Recorded market lifecycle calls.
	pub static LifecycleLog: Vec<(AssetId, StableId, bool, u32)> = Vec::new();
	/// Makes market registration fail in the lifecycle hook.
	pub static FailOnRegistered: bool = false;
	/// Makes market removal fail in the lifecycle hook.
	pub static FailOnDeregistered: bool = false;
}

/// Records market lifecycle calls and supports forced failures.
pub struct RecordingLifecycle;
impl pusd_primitives::OnBranchLifecycle<AssetId, StableId, AccountId> for RecordingLifecycle {
	type RegistrationConfig = ();

	fn on_registered(
		collateral_id: &AssetId,
		stable_id: &StableId,
		stablecoin_markets: u32,
		_config: Self::RegistrationConfig,
		_funder: &AccountId,
	) -> DispatchResult {
		LifecycleLog::mutate(|l| {
			l.push((collateral_id.clone(), *stable_id, true, stablecoin_markets))
		});
		if FailOnRegistered::get() {
			return Err(DispatchError::Other("on_registered failure"));
		}
		Ok(())
	}
	fn on_deregistered(
		collateral_id: &AssetId,
		stable_id: &StableId,
		remaining_stablecoin_markets: u32,
	) -> DispatchResult {
		LifecycleLog::mutate(|l| {
			l.push((collateral_id.clone(), *stable_id, false, remaining_stablecoin_markets))
		});
		if FailOnDeregistered::get() {
			return Err(DispatchError::Other("on_deregistered failure"));
		}
		Ok(())
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_registration_config(_stablecoin_markets: u32) -> Self::RegistrationConfig {}
}

/// Allows root or the stable asset owner to create a market.
///
/// Root pays no deposit. The asset owner does.
pub struct EnsureAssetOwner;
impl EnsureOriginWithArg<RuntimeOrigin, StableId> for EnsureAssetOwner {
	type Success = Option<AccountId>;
	fn try_origin(o: RuntimeOrigin, stable: &StableId) -> Result<Self::Success, RuntimeOrigin> {
		match Into::<Result<frame_system::RawOrigin<AccountId>, RuntimeOrigin>>::into(o.clone()) {
			Ok(frame_system::RawOrigin::Root) => Ok(None),
			Ok(frame_system::RawOrigin::Signed(who)) => {
				if <Assets as FungiblesRolesInspect<AccountId>>::owner(*stable) == Some(who) {
					Ok(Some(who))
				} else {
					Err(o)
				}
			},
			_ => Err(o),
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn try_successful_origin(_: &StableId) -> Result<RuntimeOrigin, ()> {
		Ok(RuntimeOrigin::root())
	}
}

parameter_types! {
	pub const MarketDepositReason: RuntimeHoldReason =
		RuntimeHoldReason::Vaults(HoldReason::BranchCreationDeposit);
	pub const MarketDepositBase: Balance = 1_000;
}

/// Refundable 1,000-unit market creation deposit.
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
	/// Units of an asset that one native unit buys through the mock fallback quote.
	pub static MockFallbackRate: Option<FixedU128> = Some(FixedU128::one());
}

pub struct MockFallbackConversion;
impl ConversionToAssetBalance<Balance, AssetId, Balance> for MockFallbackConversion {
	type Error = DispatchError;

	fn to_asset_balance(balance: Balance, _asset: AssetId) -> Result<Balance, Self::Error> {
		MockFallbackRate::get()
			.map(|rate| rate.saturating_mul_int(balance))
			.ok_or(DispatchError::Unavailable)
	}
}

/// Settles in the collateral when it is native or sufficient, else in native; re-prices through
/// the oracle first and the mock quote only when the oracle has no feed, never below the asset's
/// minimum balance.
pub type VaultDepositPolicy = AssetFootprintPrice<
	SufficientAssets<VaultCollateralAssets, AccountId>,
	NativeCollateral,
	LinearStoragePrice<VaultDepositBase, ConstU128<0>, Balance>,
	AtLeastMinimumBalance<
		VaultCollateralAssets,
		FallbackOnUnavailable<
			OraclePriceConversion<MockOracle, NativeCollateral>,
			MockFallbackConversion,
		>,
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
	pub static ActiveSpCapacity: Balance = 0;
	pub static PendingSpCapacity: Balance = 0;
	pub static PendingSpInspectionCalls: u32 = 0;
	pub static SpOffsetCalls: u32 = 0;
	pub const IdleMaxRefreshWeight: Option<Weight> = Some(Weight::MAX);
	pub const VaultsPalletId: PalletId = PalletId(*b"pusd/vlt");
	pub TestBranchConfigBounds: BranchConfigBounds = BranchConfigBounds {
		min_minimum_collateralization_ratio: FixedU128::from_rational(105u128, 100u128),
		min_initial_collateralization_ratio: FixedU128::from_rational(110u128, 100u128),
		min_safety_collateralization_ratio: FixedU128::from_rational(120u128, 100u128),
		max_borrow_rate: FixedU128::from_rational(400u128, 100u128),
	};
}

pub struct MockStabilityPool;

impl MockStabilityPool {
	/// Settle one non-zero leg against its capacity knob, mirroring the real
	/// pallet's exact semantics: the debt must still fit the capacity a
	/// sizing read reported, or the whole offset fails.
	fn settle_leg(
		debt: Balance,
		collateral: CollateralCreditOf<Test>,
		capacity: &mut Balance,
	) -> DispatchResult {
		ensure!(!debt.is_zero() && debt <= *capacity, Error::<Test>::InvalidLiquidationPlan);
		let resolved = <VaultCollateralAssets as FungiblesBalanced<AccountId>>::resolve(
			&SP_ACCOUNT,
			collateral,
		);
		ensure!(resolved.is_ok(), Error::<Test>::CollateralPayoutFailed);
		*capacity -= debt;
		Ok(())
	}
}

impl StabilityPoolInspect<AssetId, StableId, Balance> for MockStabilityPool {
	fn reducible_active(_: &AssetId, _: &StableId, max_debt: Balance) -> Balance {
		max_debt.min(ActiveSpCapacity::get())
	}

	fn reducible_pending(_: &AssetId, _: &StableId, max_debt: Balance, _: Balance) -> Balance {
		PendingSpInspectionCalls::mutate(|calls| *calls += 1);
		max_debt.min(PendingSpCapacity::get())
	}
}

impl StabilityPoolOffset<AssetId, StableId, Balance, CollateralCreditOf<Test>>
	for MockStabilityPool
{
	fn offset(
		_: &AssetId,
		_: &StableId,
		debt: OffsetLegs<Balance>,
		collateral: OffsetLegs<CollateralCreditOf<Test>>,
	) -> DispatchResult {
		SpOffsetCalls::mutate(|calls| *calls += 1);
		if debt.active.is_zero() {
			ensure!(collateral.active.peek().is_zero(), Error::<Test>::InvalidLiquidationPlan);
			drop(collateral.active);
		} else {
			let mut capacity = ActiveSpCapacity::get();
			Self::settle_leg(debt.active, collateral.active, &mut capacity)?;
			ActiveSpCapacity::set(capacity);
		}
		if debt.pending.is_zero() {
			ensure!(collateral.pending.peek().is_zero(), Error::<Test>::InvalidLiquidationPlan);
			drop(collateral.pending);
		} else {
			let mut capacity = PendingSpCapacity::get();
			Self::settle_leg(debt.pending, collateral.pending, &mut capacity)?;
			PendingSpCapacity::set(capacity);
		}
		Ok(())
	}
}

impl pallet_vaults::Config for Test {
	type StableToCollateralId = ConvertInto;
	type CollateralAssets = VaultCollateralAssets;
	type StableAssets = VaultStableAssets;
	type Oracle = MockOracle;
	type FeeAccount = FeeAccounts;
	type YieldHook = MockYieldHook;
	type OnBranchLifecycle = RecordingLifecycle;
	type StabilityPool = MockStabilityPool;
	type TimeProvider = Timestamp;
	type CreateOrigin = EnsureAssetOwner;
	type BranchConsideration = VaultsConsideration;
	type VaultConsideration = VaultDepositConsideration;
	type BranchConfigBounds = TestBranchConfigBounds;
	type ForceOrigin = frame_system::EnsureRoot<AccountId>;
	type GlobalDebtCeiling = pallet_vaults::StoredCeiling<Test>;
	type PalletId = VaultsPalletId;
	type VaultLists = LinkedList;
	type IdleMaxRefreshWeight = IdleMaxRefreshWeight;
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
		PUSD
	}

	fn set_oracle_price(collateral_id: AssetId, price: FixedU128) {
		set_price(collateral_id, price);
	}

	fn clear_oracle_price(collateral_id: AssetId) {
		MockPrices::mutate(|prices| {
			prices.remove(&collateral_id);
		});
	}

	fn advance_time(ms: u64) {
		advance_time(ms);
	}
}

/// Builds fresh storage for a test.
pub fn new_test_ext() -> TestState {
	let t = RuntimeGenesisConfig {
		assets: pallet_assets::GenesisConfig {
			assets: vec![
				(TOKEN_X_ID, 1, true, 1),
				(ETH_ID, 1, true, 1),
				(COLL_C_ID, 1, true, 1),
				(COLL_D_ID, 1, true, 1),
				(PUSD, 1, true, 1),
				(EUSD, 1, true, 1),
				(USDX, 1, true, USDX_ED),
				(XBT_ID, 1, true, XBT_ED),
				(INSUFFICIENT_ID, 1, false, 1),
			],
			metadata: vec![],
			accounts: vec![],
			next_asset_id: None,
			reserves: vec![],
		},
		system: Default::default(),
		balances: pallet_balances::GenesisConfig {
			// The fee account needs native funds to pay its asset-account deposit.
			// The full admin funds the custody seed of every Root-created market.
			balances: (1u64..=10u64)
				.chain([SP_ACCOUNT, FEE_DEST, ADMIN])
				.map(|i| (i, 1_000_000_000_000))
				.collect(),
			..Default::default()
		},
	}
	.build_storage()
	.unwrap();
	let mut ext: TestState = t.into();
	ext.execute_with(|| {
		System::set_block_number(1);
		Timestamp::set_timestamp(1_000);
		// Native collateral is already funded. Mint the issued assets now.
		for who in (1u64..=10).chain([ADMIN]) {
			for asset in [TOKEN_X_ID, ETH_ID, COLL_C_ID, COLL_D_ID] {
				<Assets as frame::traits::fungibles::Mutate<AccountId>>::mint_into(
					asset,
					&who,
					1_000_000_000_000,
				)
				.expect("mint issued collateral in test setup");
			}
			<Assets as frame::traits::fungibles::Mutate<AccountId>>::mint_into(
				XBT_ID,
				&who,
				100_000_000 * XBT_UNIT,
			)
			.expect("mint realistic collateral in test setup");
		}
		// Only the market creator holds the insufficient asset: an account of one takes a consumer
		// reference, which changes what that account may spend down to.
		<Assets as frame::traits::fungibles::Mutate<AccountId>>::mint_into(
			INSUFFICIENT_ID,
			&ADMIN,
			1_000_000_000_000,
		)
		.expect("mint insufficient collateral in test setup");
		MockPrices::set(BTreeMap::new());
		MockOracleAvailable::set(true);
		ActiveSpCapacity::set(0);
		PendingSpCapacity::set(0);
		PendingSpInspectionCalls::set(0);
		SpOffsetCalls::set(0);
		LifecycleLog::set(Vec::new());
		FailOnRegistered::set(false);
		FailOnDeregistered::set(false);
		MockFallbackRate::set(Some(FixedU128::one()));
	});
	ext
}

/// Runs a test and checks invariants when `try-runtime` is enabled.
pub fn build_and_execute(test: impl FnOnce()) {
	new_test_ext().execute_with(|| {
		test();
		#[cfg(feature = "try-runtime")]
		crate::try_state::do_try_state::<Test>().expect("post-test invariants hold");
	});
}

/// Sets the oracle price for a collateral asset.
pub fn set_price(collateral: AssetId, price: FixedU128) {
	MockPrices::mutate(|m| {
		m.insert(collateral, price);
	});
}

/// Advances time without changing the block number.
pub fn advance_time(ms: Moment) {
	let now = Timestamp::get();
	Timestamp::set_timestamp(now + ms);
}

/// Returns the default market settings used by tests.
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
		liquidation: LiquidationConfig {
			offset_penalty: Permill::from_percent(5),
			// The keeper is paid out of the offset penalty, which on the smallest
			// vault here is 5% of a 200 debt.
			keeper_flat_compensation_value: 10,
			keeper_percent_compensation: Permill::from_rational(1u32, 1_000u32),
			keeper_compensation_cap_value: 10_000,
			minimum_jit_contribution: 100,
			redistribution_penalty: Permill::from_percent(5),
		},
	}
}

/// Builds a market administrator pair.
pub fn branch_admins(full: AccountId, emergency: AccountId) -> BranchAdmins<AccountId> {
	BranchAdmins { full_admin: full, emergency_admin: emergency }
}

/// Creates a market without setting its global debt limit.
pub fn create_market(
	collateral: AssetId,
	stable: StableId,
	price: FixedU128,
	config: BranchConfig<Balance>,
) {
	// Market creation requires a collateral price.
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

/// Creates a market and enables borrowing with the default global debt limit.
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

/// Creates a market with default settings and a price of 10.
pub fn register_market(collateral: AssetId, stable: StableId) {
	register_market_with(
		collateral,
		stable,
		FixedU128::from_rational(10u128, 1u128),
		default_branch_config(),
	);
}

/// Creates all ten mock markets without global debt limits.
pub fn register_ten_markets() {
	for collateral in [DOT, TOKEN_X, ETH, COLL_C, COLL_D] {
		for stable in [PUSD, EUSD] {
			create_market(
				collateral.clone(),
				stable,
				FixedU128::from_rational(10u128, 1u128),
				default_branch_config(),
			);
		}
	}
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

/// Calls the production liquidation dispatchable.
pub fn liquidate(
	keeper: AccountId,
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
	max_jit_stable: Balance,
	min_jit_collateral_out: Balance,
) -> DispatchResult {
	Vaults::liquidate(
		RuntimeOrigin::signed(keeper),
		collateral,
		stable,
		owner,
		crate::JitTerms { max_stable: max_jit_stable, min_collateral_out: min_jit_collateral_out },
	)
}

/// Removes a vault and records its whole debt as redistribution.
///
/// This bypasses liquidation pricing only for tests concerned with the
/// redistribution ledger. Economic and custody tests use [`liquidate`].
#[transactional]
pub fn redistribute_for_test(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
	redistribution_collateral: Balance,
) -> Result<Balance, DispatchError> {
	let mut op = VaultOp::<Test>::load_priced(collateral.clone(), stable, &owner)?;
	let snapshot = op.prepare_liquidation()?;
	let held = op.vault().collateral;
	let (mut collateral_credit, shortfall) =
		VaultCollateralAssets::slash(collateral, &HoldReason::VaultCollateral.into(), &owner, held);
	ensure!(shortfall.is_zero(), Error::<Test>::InvalidLiquidationPlan);
	ensure!(
		redistribution_collateral <= collateral_credit.peek(),
		Error::<Test>::InvalidLiquidationPlan
	);
	let redistribution_credit = collateral_credit.extract(redistribution_collateral);
	Vaults::settle_liquidation_custody(
		op,
		pusd_primitives::DebtCollateral {
			debt: snapshot.debt,
			collateral: redistribution_collateral,
		},
		redistribution_credit,
		collateral_credit,
	)?;
	Ok(snapshot.debt)
}

/// Redeems from the next market target at the oracle price.
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
pub fn redeem_step(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
	recipient: AccountId,
	debt_to_cancel: Balance,
	collateral_to_recipient: Balance,
) -> DispatchResult {
	use frame::deps::frame_support::storage::{with_transaction, TransactionOutcome};
	with_transaction(|| {
		let result = <Vaults as VaultInterface>::redeem_step(
			&collateral,
			&stable,
			&owner,
			&recipient,
			settlement(stable, debt_to_cancel, collateral_to_recipient),
		);
		match result {
			Ok(()) => TransactionOutcome::Commit(Ok(())),
			Err(error) => TransactionOutcome::Rollback(Err(error)),
		}
	})
}

/// Builds a redemption settlement with newly issued stable assets.
pub fn settlement(
	stable: StableId,
	debt_to_cancel: Balance,
	collateral_to_recipient: Balance,
) -> RedemptionSettlement<StableCreditOf<Test>, Balance> {
	let debt_payment =
		<VaultStableAssets as FungiblesBalanced<AccountId>>::issue(stable, debt_to_cancel);
	RedemptionSettlement { debt_payment, collateral_to_recipient }
}

/// Returns a market's accounting state, or `None` if it is not registered.
pub fn branch_state(
	collateral: AssetId,
	stable: StableId,
) -> Option<BranchState<AccountId, Balance>> {
	Branches::<Test>::get(collateral, stable).map(|branch| branch.state)
}

/// Returns a market's settings, or `None` if it is not registered.
pub fn branch_config(collateral: AssetId, stable: StableId) -> Option<BranchConfig<Balance>> {
	Branches::<Test>::get(collateral, stable).map(|branch| branch.config)
}

/// Updates market state through the pallet's commit path.
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

/// Returns the current market mode, if it can be calculated.
pub fn branch_mode(collateral: AssetId, stable: StableId) -> Option<BranchMode> {
	Vaults::current_mode(&collateral, &stable).ok()
}

/// Returns the rate-list ID for a market.
pub fn rate_list(collateral: AssetId, stable: StableId) -> VaultList {
	VaultListId::Rate(collateral, stable)
}

/// Returns collateral held for vaults across all of the owner's markets.
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

/// Returns a collateral asset's minimum balance, which is the size of the custody seed.
pub fn min_collateral_balance(collateral: AssetId) -> Balance {
	<VaultCollateralAssets as FungiblesInspect<AccountId>>::minimum_balance(collateral)
}

/// Returns an account's balance of one stable asset.
pub fn stable_balance(stable: StableId, who: AccountId) -> Balance {
	<VaultStableAssets as FungiblesInspect<AccountId>>::balance(stable, &who)
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
	<VaultStableAssets as frame::traits::fungibles::Mutate<AccountId>>::mint_into(
		stable, &who, amount,
	)
	.expect("mint stable");
}

/// Returns the total issuance of one stable asset.
pub fn total_stable(stable: StableId) -> Balance {
	<VaultStableAssets as FungiblesInspect<AccountId>>::total_issuance(stable)
}

/// Returns the market creation deposit held from an account.
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

/// Returns a vault's accounting row, or `None` when the vault is missing.
pub fn try_vault(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
) -> Option<Vault<Balance>> {
	Vaults::vault_of(&collateral, &stable, &owner).ok()
}

/// Whether a vault row exists.
pub fn vault_exists(collateral: AssetId, stable: StableId, owner: AccountId) -> bool {
	crate::pallet::Vaults::<Test>::contains_key((collateral, stable, owner))
}
