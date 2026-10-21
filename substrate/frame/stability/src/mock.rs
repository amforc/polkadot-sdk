//! The test runtime for `pallet-stability`.
//!
//! The mock runs the real vault stack, so a market registered through `pallet-vaults` seeds the
//! pool rows through the lifecycle hook, exactly as a production runtime does. Tests therefore
//! exercise the wiring as well as the pallet.
//!
//! Two conventions run through the tests. Collateral [`DOT`] is native and routes to `Balances`,
//! while `AssetId::WithId` routes to `AssetsHolder`. Stablecoin [`STABLE`] holds to a unit, and
//! [`USDX`] has six decimals, which the scale tests need.

use crate as pallet_stability;
use crate::types::PoolPrecision;
pub use crate::types::{
	Accumulators, CohortId, Deposit, DepositSnapshot, Leg, PendingDeposit, PoolSums,
	StabilityPoolConfig,
};
pub use frame::{
	arithmetic::{FixedPointNumber, FixedU128, One, Permill, Saturating, Zero},
	prelude::{DispatchError, DispatchResult},
	testing_prelude::{
		assert_err, assert_noop, assert_ok, assert_storage_noop, hypothetically, BadOrigin,
	},
};
use frame::{
	deps::sp_runtime::traits::ConvertInto,
	testing_prelude::*,
	traits::{
		fungible::{HoldConsideration, NativeFromLeft, NativeOrWithId},
		fungibles::Balanced as FungiblesBalanced,
		tokens::{fungible, imbalance::ResolveAssetTo},
		AsEnsureOriginWithArg, Convert, IdentityLookup, LinearStoragePrice,
	},
};
use stablecoin_primitives::{
	BranchSnapshot, OffsetLegs, ProvidePrice, StabilityPoolInspect, StabilityPoolOffset,
};

pub type AccountId = u128;
pub type Balance = u128;
pub type AssetIdForAssets = u32;
/// Collateral asset id: native DOT or an issued asset.
pub type AssetId = NativeOrWithId<AssetIdForAssets>;
/// Stablecoin asset id: a plain `pallet-assets` id.
pub type StableId = AssetIdForAssets;
pub type Block = MockBlock<Test>;
pub type VaultList = pallet_vaults::VaultListId<AssetId, StableId>;
pub type Moment = u64;

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

	#[runtime::pallet_index(7)]
	pub type Stability = pallet_stability;

	#[runtime::pallet_index(8)]
	pub type Redemptions = pallet_redemptions;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountId = AccountId;
	type AccountData = pallet_balances::AccountData<Balance>;
	type Lookup = IdentityLookup<Self::AccountId>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig as pallet_balances::DefaultConfig)]
impl pallet_balances::Config for Test {
	type AccountStore = System;
	type Balance = Balance;
	type ExistentialDeposit = ConstU128<1>;
	type RuntimeHoldReason = RuntimeHoldReason;
}

impl pallet_timestamp::Config for Test {
	type Moment = Moment;
	type OnTimestampSet = ();
	type MinimumPeriod = ConstU64<1>;
	type WeightInfo = ();
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
	pub const VaultsPalletId: PalletId = PalletId(*b"py/vault");
	pub const StabilityPalletId: PalletId = PalletId(*b"py/stabl");
}

impl pallet_linked_list::Config for Test {
	type WeightInfo = ();
	type ListId = VaultList;
	type ItemId = AccountId;
	type Priority = FixedU128;
	type MaxHintRepairSteps = MaxHintRepairSteps;
	type PriorityProvider = pallet_vaults::Pallet<Test>;
}

/// The collateral surface the vault pallet uses: `Balances` for the native asset, `AssetsHolder`
/// for the rest.
pub type VaultCollateralAssets =
	fungible::UnionOf<Balances, AssetsHolder, NativeFromLeft, AssetId, AccountId>;

/// The collateral surface the pool uses: the same ledger, reached through `Assets`.
///
/// The pool places nothing on hold. It receives offset collateral and pays claims, so it needs
/// `Mutate`, which `AssetsHolder` does not offer.
pub type PoolCollateralAssets =
	fungible::UnionOf<Balances, Assets, NativeFromLeft, AssetId, AccountId>;

/// A price source the tests drive with [`set_price`].
///
/// Prices are keyed by collateral alone. Stablecoins are taken to hold to one unit of the price's
/// quote.
pub struct MockOracle;
parameter_types! {
	pub static MockPrices: alloc::collections::BTreeMap<AssetId, FixedU128> =
		alloc::collections::BTreeMap::new();
	pub static MockOracleAvailable: bool = true;
}
impl ProvidePrice for MockOracle {
	type AssetId = AssetId;

	fn provide_price(collateral_id: &AssetId) -> Result<FixedU128, DispatchError> {
		if !MockOracleAvailable::get() {
			return Err(DispatchError::Other("oracle unavailable"));
		}
		MockPrices::get()
			.get(collateral_id)
			.copied()
			.ok_or(DispatchError::Other("no price"))
	}
}

pub fn set_price(collateral: AssetId, price: FixedU128) {
	MockPrices::mutate(|m| {
		m.insert(collateral, price);
	});
}

parameter_types! {
	pub const MarketDepositReason: RuntimeHoldReason =
		RuntimeHoldReason::Vaults(pallet_vaults::HoldReason::BranchCreationDeposit);
	pub const MarketDepositBase: Balance = 1_000;
}

/// Full admin of every market a test helper registers.
pub const ADMIN: AccountId = 100;
/// Emergency (tighten-only) admin of every market a test helper registers.
pub const EMERGENCY_ADMIN: AccountId = 101;

/// The admin pair a market registers with: `full` administers, `emergency` may only tighten.
pub fn branch_admins(
	full: AccountId,
	emergency: AccountId,
) -> pallet_vaults::types::BranchAdmins<AccountId> {
	pallet_vaults::types::BranchAdmins { full_admin: full, emergency_admin: emergency }
}

/// Who may register a market: Root pays no deposit, the owner of the stablecoin pays one, and
/// anyone else is refused.
pub type EnsureAssetOwner = stablecoin_primitives::EnsureStableOwnerOrRoot<Assets, AccountId>;

/// A flat, refundable 1_000-unit registration deposit, held in native balance.
pub type VaultsConsideration = HoldConsideration<
	AccountId,
	Balances,
	MarketDepositReason,
	LinearStoragePrice<MarketDepositBase, ConstU128<0>, Balance>,
>;

parameter_types! {
	/// Bounds wide enough that the default branch config never touches them.
	pub TestBranchConfigBounds: pallet_vaults::types::BranchConfigBounds =
		pallet_vaults::types::BranchConfigBounds {
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
	type FeeAccount = FeeAccounts;
	// The pool takes its `yield_share` of every minted credit, and the fee destination
	// receives the rest.
	type YieldHook = Stability;
	// Registration seeds the per-market rows of both siblings. Redemptions comes first, so
	// the config that prices recovery offsets always exists once the pool rows do.
	type OnBranchLifecycle = (Redemptions, Stability);
	type TimeProvider = Timestamp;
	type CreateOrigin = EnsureAssetOwner;
	type BranchConsideration = VaultsConsideration;
	// These suites assert raw balances; vault deposits are the vaults suite's concern.
	type VaultConsideration = ();
	type BranchConfigBounds = TestBranchConfigBounds;
	type ForceOrigin = frame_system::EnsureRoot<AccountId>;
	type GlobalDebtCeiling = pallet_vaults::StoredCeiling<Test>;
	type PalletId = VaultsPalletId;
	type VaultLists = LinkedList;
	type WeightInfo = ();
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = VaultsBenchHelper;
}

#[cfg(feature = "runtime-benchmarks")]
pub struct VaultsBenchHelper;
#[cfg(feature = "runtime-benchmarks")]
impl pallet_vaults::BenchmarkHelper<AssetId, StableId> for VaultsBenchHelper {
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
		MockPrices::mutate(|m| {
			m.remove(&collateral_id);
		});
	}

	fn advance_time(ms: u64) {
		advance_time(ms);
	}

	// The genesis config already creates every stable asset.
	fn ensure_stable_asset(_: StableId) {}
}

/// Root as the governance override, or the stored full admin of the market. A production runtime
/// composes the same pair.
pub type StabilityUpdateOrigin = EitherOf<
	AsEnsureOriginWithArg<frame_system::EnsureRoot<AccountId>>,
	pallet_vaults::EnsureBranchFullAdmin<Test>,
>;

/// Where redemption fees go. Recovery offsets charge no fee, so only ordinary redemptions pay
/// into it.
pub const FEE_DEST: AccountId = 888;

/// Where market teardown sends what is left in a pool account. It stands in for a treasury, and
/// it is separate from [`FEE_DEST`] so that tests can tell the two streams apart.
pub const DUST_DEST: AccountId = 889;

parameter_types! {
	pub const DustDestAccount: AccountId = DUST_DEST;
}

/// One insurance account per stablecoin. All are empty here, so every below-par head prices as
/// `BelowPar`.
pub struct InsuranceFundAccounts;
impl Convert<StableId, AccountId> for InsuranceFundAccounts {
	fn convert(stable: StableId) -> AccountId {
		700_000 + AccountId::from(stable)
	}
}

/// Sends the vault fees of every test stablecoin to [`FEE_DEST`].
pub struct FeeAccounts;
impl Convert<StableId, AccountId> for FeeAccounts {
	fn convert(_stable: StableId) -> AccountId {
		FEE_DEST
	}
}

parameter_types! {
	pub const FeeDestAccount: AccountId = FEE_DEST;
}

/// The redemption parameters the first market of a stablecoin registers with.
pub fn default_redemption_config() -> pallet_redemptions::types::RedemptionConfig<Balance> {
	pallet_redemptions::types::RedemptionConfig {
		minimum_redemption_amount: 100,
		dynamic_fee_decay_period: 6 * 3_600 * 1_000,
		dynamic_fee_floor: FixedU128::zero(),
		dynamic_fee_ceiling: FixedU128::one(),
		base_fee: Permill::from_rational(5u32, 1_000u32),
		fee_ceiling: Permill::one(),
		dynamic_fee_increase_divisor: FixedU128::from_rational(2u128, 1u128),
		final_recovery_bonus_buffer: Permill::from_percent(1),
	}
}

impl pallet_redemptions::Config for Test {
	type StableAssets = Assets;
	type Oracle = MockOracle;
	type Vaults = Vaults;
	type InsuranceFundAccount = InsuranceFundAccounts;
	type FeeHandler = ResolveAssetTo<FeeDestAccount, Assets>;
	type TimeProvider = Timestamp;
	type UpdateOrigin = EnsureAssetOwner;
	type MaxRedemptionSteps = ConstU32<20>;
	type WeightInfo = ();
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = RedemptionsBenchHelper;
}

#[cfg(feature = "runtime-benchmarks")]
pub struct RedemptionsBenchHelper;
#[cfg(feature = "runtime-benchmarks")]
impl pallet_redemptions::BenchmarkHelper<AssetId, StableId, AccountId, Balance>
	for RedemptionsBenchHelper
{
	fn setup_redeemable_branch(vaults: u32) -> (AssetId, StableId, AccountId, Balance) {
		register_branch(DOT, STABLE, default_branch_config());
		let debt: Balance = 300;
		for i in 0..vaults {
			let who = 1_000 + AccountId::from(i);
			mint_collateral(DOT, who, 10_000_000_000);
			open_vault(who, DOT, STABLE, 1_000_000, debt).expect("open benchmark vault");
		}
		let redeemer: AccountId = 1;
		let budget = debt.saturating_mul(Balance::from(vaults).saturating_add(2)).saturating_mul(2);
		mint_stable(STABLE, redeemer, budget.saturating_mul(2));
		(DOT, STABLE, redeemer, budget)
	}
}

impl pallet_stability::Config for Test {
	type StableAssets = Assets;
	type CollateralAssets = PoolCollateralAssets;
	type BranchInterface = Vaults;
	type RecoveryOffsets = Redemptions;
	type StableDustHandler = ResolveAssetTo<DustDestAccount, Assets>;
	type CollateralDustHandler = ResolveAssetTo<DustDestAccount, PoolCollateralAssets>;
	type TimeProvider = Timestamp;
	type UpdateOrigin = StabilityUpdateOrigin;
	type PalletId = StabilityPalletId;
	type WeightInfo = ();
}

/// The native collateral the tests use throughout.
pub const DOT: AssetId = AssetId::Native;

/// A second collateral that lives in `pallet-assets` rather than in `Balances`.
pub const TOKEN_X_ID: AssetIdForAssets = 1;
pub const TOKEN_X: AssetId = AssetId::WithId(TOKEN_X_ID);

/// An issued collateral whose minimum balance of 1_000 sits far above every other asset's, for
/// the custody-float tests. [`create_high_min_collateral`] creates it.
pub const HIGH_MIN_ID: AssetIdForAssets = 77;
pub const HIGH_MIN: AssetId = AssetId::WithId(HIGH_MIN_ID);

/// The price every market is registered at.
pub const REGISTRATION_PRICE: FixedU128 = FixedU128::from_rational(5, 4);

/// The stablecoin the helpers mint by default. One coin is one raw unit.
pub const STABLE: StableId = 1_000;

/// A stablecoin with six decimals, so that the tests can check the pallet against a realistic
/// denomination rather than against a unit.
pub const USDX: StableId = 6_000;
/// The raw units in one [`USDX`] coin.
pub const USDX_UNIT: Balance = 1_000_000;
/// The minimum balance of [`USDX`]: one hundredth of a coin.
pub const USDX_MIN_BALANCE: Balance = USDX_UNIT / 100;

pub fn new_test_ext() -> TestState {
	let t = RuntimeGenesisConfig {
		assets: pallet_assets::GenesisConfig {
			assets: vec![
				(TOKEN_X_ID, 1, true, 1),
				(STABLE, 1, true, 1),
				(USDX, 1, true, USDX_MIN_BALANCE),
			],
			metadata: vec![(USDX, b"USDX".to_vec(), b"USDX".to_vec(), 6)],
			accounts: vec![],
			next_asset_id: None,
			reserves: vec![],
		},
		system: Default::default(),
		balances: pallet_balances::GenesisConfig {
			// The fee account needs native funds for its asset-account deposit, and the
			// full admin pays the custody seed of every market Root registers.
			balances: (1u128..=10u128)
				.chain([FEE_DEST, ADMIN])
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
		MockPrices::set(alloc::collections::BTreeMap::new());
		MockOracleAvailable::set(true);
	});
	ext
}

/// Runs `test`, then checks every invariant of the pallet against the state it left behind.
pub fn build_and_execute(test: impl FnOnce()) {
	new_test_ext().execute_with(|| {
		test();
		// Every test here drives the real Vaults, Redemptions and LinkedList pallets, whose
		// invariants their own suites cannot see; they are reachable from outside their crates
		// only through the try-runtime hooks, which also run this pallet's checks.
		#[cfg(feature = "try-runtime")]
		{
			use frame::deps::frame_support::traits::{TryState, TryStateSelect};
			AllPalletsWithSystem::try_state(System::block_number(), TryStateSelect::All)
				.expect("post-test invariants hold");
		}
		#[cfg(not(feature = "try-runtime"))]
		crate::try_state::do_try_state::<Test>().expect("post-test invariants hold");
	});
}

/// [`build_and_execute`] with the default market already registered.
pub fn build_with_default_market(test: impl FnOnce()) {
	build_and_execute(|| {
		register_branch(DOT, STABLE, default_branch_config());
		test();
	});
}

/// The pool parameters every registered market starts with: a 5 second entry delay and a
/// 10 minute Safety-Mode withdrawal delay.
pub fn default_pool_config() -> StabilityPoolConfig<Balance> {
	StabilityPoolConfig {
		minimum_deposit: 100,
		minimum_active_pool_balance: 100,
		entry_delay: 5_000,
		safety_withdrawal_delay: 600_000,
		precision: PoolPrecision {
			p_min: FixedU128::from_inner(1_000_000_000),
			scale_factor: 1_000_000_000,
		},
		yield_share: Permill::from_percent(75),
	}
}

/// The market parameters most tests use: liquidation at 110%, opening at 120%, Safety Mode
/// below 130%.
pub fn default_branch_config() -> pallet_vaults::BranchConfig<Balance> {
	pallet_vaults::BranchConfig {
		minimum_collateralization_ratio: FixedU128::from_rational(110u128, 100u128),
		initial_collateralization_ratio: FixedU128::from_rational(120u128, 100u128),
		safety_collateralization_ratio: FixedU128::from_rational(130u128, 100u128),
		// High enough never to bind by accident, including at the raw-unit scales the
		// fixtures use. A test about the ceiling sets its own.
		debt_ceiling: 1_000_000_000_000,
		minimum_debt: 200,
		minimum_collateral: 1,
		minimum_borrow_rate: FixedU128::from_rational(1u128, 1_000u128),
		maximum_borrow_rate: FixedU128::from_rational(100u128, 100u128),
		upfront_fee_period: 7 * 24 * 3_600 * 1_000,
		rate_adjustment_cooldown: 24 * 3_600 * 1_000,
		redistribution_penalty: Permill::from_percent(5),
	}
}

/// [`default_branch_config`], with the floors raised to clear the minimum balances of a given
/// pair.
///
/// The 1-unit defaults only clear them for the 1-unit assets. Everything else is left alone, so a
/// test still reads as the default.
pub fn branch_config_for(
	collateral: AssetId,
	stable: StableId,
) -> pallet_vaults::BranchConfig<Balance> {
	use frame::traits::fungibles::Inspect as FungiblesInspect;
	let stable_minimum = <Assets as FungiblesInspect<AccountId>>::minimum_balance(stable);
	let collateral_minimum =
		<PoolCollateralAssets as FungiblesInspect<AccountId>>::minimum_balance(collateral);
	let config = default_branch_config();
	pallet_vaults::BranchConfig {
		minimum_debt: config.minimum_debt.max(stable_minimum),
		minimum_collateral: config.minimum_collateral.max(collateral_minimum),
		..config
	}
}

/// The registration payload of a market on `stable`.
///
/// Redemption policy is set per stablecoin, so only its first market carries one. A pool is per
/// market, so every market carries one.
pub fn registration_config(
	stable: StableId,
) -> (Option<pallet_redemptions::types::RedemptionConfig<Balance>>, StabilityPoolConfig<Balance>) {
	let redemption_config = (!pallet_redemptions::RedemptionConfigs::<Test>::contains_key(stable))
		.then(default_redemption_config);
	(redemption_config, default_pool_config())
}

/// Market configuration for recovery settlement tests, with no entry compensation.
/// These tests price the collateral available to settle the head.
pub fn recovery_branch_config() -> pallet_vaults::BranchConfig<Balance> {
	default_branch_config()
}

/// Creates the [`HIGH_MIN`] collateral asset.
pub fn create_high_min_collateral() {
	assert_ok!(Assets::force_create(RuntimeOrigin::root(), HIGH_MIN_ID, 1, true, 1_000));
}

/// Creates the `collateral`/[`STABLE`] market as `origin`, with the default admins, branch
/// parameters and registration.
pub fn create_branch_as(origin: RuntimeOrigin, collateral: AssetId) -> DispatchResult {
	Vaults::create_branch(
		origin,
		collateral.clone(),
		STABLE,
		branch_admins(ADMIN, EMERGENCY_ADMIN),
		branch_config_for(collateral, STABLE),
		registration_config(STABLE),
	)
}

/// Registers a market at [`REGISTRATION_PRICE`] and a debt ceiling high enough never to bind.
///
/// Registration also seeds the pool rows, through the lifecycle hook.
pub fn register_branch(
	collateral: AssetId,
	stable: StableId,
	config: pallet_vaults::BranchConfig<Balance>,
) {
	// A market cannot be registered without a live price.
	set_price(collateral.clone(), REGISTRATION_PRICE);
	// Account 1 owns every test stablecoin. As the depositor it pays the redistribution custody
	// seed of Vaults and the pool custody float of Stability, one collateral minimum each,
	// withdrawn under `Preserve`. The stablecoin float is one stablecoin minimum, also preserved.
	mint_collateral(
		collateral.clone(),
		1,
		3 * <VaultCollateralAssets as frame::traits::fungibles::Inspect<AccountId>>::minimum_balance(
			collateral.clone(),
		),
	);
	mint_stable(
		stable,
		1,
		2 * <Assets as frame::traits::fungibles::Inspect<AccountId>>::minimum_balance(stable),
	);
	Vaults::create_branch(
		RuntimeOrigin::signed(1),
		collateral.clone(),
		stable,
		branch_admins(ADMIN, EMERGENCY_ADMIN),
		config,
		registration_config(stable),
	)
	.expect("create_branch ok");
	Vaults::set_global_debt_ceiling(RuntimeOrigin::root(), stable, 1_000_000_000_000_000)
		.expect("set global debt ceiling");
	// The preserved remainder of the stablecoin float would skew every balance a test reads on
	// account 1, so registration leaves its stablecoin exactly where it found it.
	{
		use frame::traits::{
			fungibles::{Inspect as FungiblesInspect, Mutate as FungiblesMutate},
			tokens::{Fortitude, Precision, Preservation},
		};
		let remainder = <Assets as FungiblesInspect<AccountId>>::minimum_balance(stable);
		<Assets as FungiblesMutate<AccountId>>::burn_from(
			stable,
			&1,
			remainder,
			Preservation::Expendable,
			Precision::Exact,
			Fortitude::Force,
		)
		.expect("burn the preserved float remainder");
	}
}

/// Opens a vault, so that a test can create real market debt and move the TCR.
pub fn open_vault(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	collateral_amount: Balance,
	debt: Balance,
) -> DispatchResult {
	use frame::traits::fungible::Mutate as FungibleMutate;
	// A fresh account needs its existential deposit before anything else.
	assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(&who, 1));
	Vaults::open_vault(
		RuntimeOrigin::signed(who),
		collateral,
		stable,
		collateral_amount,
		debt,
		FixedU128::from_rational(5u128, 100u128),
		pallet_linked_list::Position::endpoints_only(),
	)
}

/// The native balance an account holds on hold, where every refundable market deposit ends up.
pub fn native_on_hold(who: AccountId) -> Balance {
	use frame::traits::fungible::InspectHold;
	<Balances as InspectHold<AccountId>>::total_balance_on_hold(&who)
}

pub fn mint_stable(stable: StableId, who: AccountId, amount: Balance) {
	use frame::traits::fungibles::Mutate as FungiblesMutate;
	<Assets as FungiblesMutate<AccountId>>::mint_into(stable, &who, amount).expect("mint stable");
}

pub fn mint_collateral(collateral: AssetId, who: AccountId, amount: Balance) {
	use frame::traits::{fungible::Mutate as FungibleMutate, fungibles::Mutate as FungiblesMutate};
	match collateral {
		AssetId::Native => {
			<Balances as FungibleMutate<AccountId>>::mint_into(&who, amount)
				.expect("mint native collateral");
		},
		AssetId::WithId(asset_id) => {
			<Assets as FungiblesMutate<AccountId>>::mint_into(asset_id, &who, amount)
				.expect("mint asset collateral");
		},
	}
}

/// Claims the collateral of `who` in the default market, paid to itself, and checks the payout.
pub fn assert_claim_collateral(who: AccountId, expected: Balance) {
	let before = collateral_balance(DOT, who);
	assert_ok!(claim_collateral(who, DOT, STABLE, who));
	assert_eq!(collateral_balance(DOT, who) - before, expected);
}

/// Claims the yield of `who` in the default market, paid to itself, and checks the payout.
pub fn assert_claim_yield(who: AccountId, expected: Balance) {
	let before = stable_balance(STABLE, who);
	assert_ok!(claim_yield(who, DOT, STABLE, who));
	assert_eq!(stable_balance(STABLE, who) - before, expected);
}

/// The default pool account holds nothing beyond its custody float and the aggregates are all
/// zero.
pub fn assert_pool_fully_drained() {
	assert_eq!(pool_stable_held(DOT, STABLE), 0);
	assert_eq!(pool_collateral_held(DOT, STABLE), 0);
	let state = pool_state(DOT, STABLE);
	assert_eq!(state.total_active_deposits, 0);
	assert_eq!(state.total_pending_deposits, 0);
	assert_eq!(state.total_collateral_gains_unclaimed, 0);
	assert_eq!(state.total_yield_unclaimed, 0);
}

/// The active-leg sums row of the default market at the given coordinates.
pub fn active_sums(epoch: u32, scale: u32) -> PoolSums {
	crate::PoolSumsStore::<Test>::get((DOT, STABLE, Leg::Active, epoch, scale))
		.expect("active sums row exists")
}

/// The pending-leg sums row of the default market at the given coordinates.
pub fn pending_sums(epoch: u32, scale: u32) -> PoolSums {
	crate::PoolSumsStore::<Test>::get((DOT, STABLE, Leg::Pending, epoch, scale))
		.expect("pending sums row exists")
}

/// The stablecoin balance of an account.
pub fn stable_balance(stable: StableId, who: AccountId) -> Balance {
	use frame::traits::fungibles::Inspect as FungiblesInspect;
	<Assets as FungiblesInspect<AccountId>>::balance(stable, &who)
}

/// The collateral balance of an account, on the surface the pool pays from.
pub fn collateral_balance(collateral: AssetId, who: AccountId) -> Balance {
	use frame::traits::fungibles::Inspect as FungiblesInspect;
	<PoolCollateralAssets as FungiblesInspect<AccountId>>::balance(collateral, &who)
}

/// Stablecoin the pool account holds for depositors: its balance less the custody float.
pub fn pool_stable_held(collateral: AssetId, stable: StableId) -> Balance {
	let pool = crate::Pools::<Test>::get(&collateral, stable).expect("pool registered");
	stable_balance(stable, Stability::pool_account(&collateral, &stable)) - pool.custody_stable
}

/// Collateral the pool account holds for depositors: its balance less the custody float.
pub fn pool_collateral_held(collateral: AssetId, stable: StableId) -> Balance {
	let pool = crate::Pools::<Test>::get(&collateral, stable).expect("pool registered");
	collateral_balance(collateral.clone(), Stability::pool_account(&collateral, &stable)) -
		pool.custody_collateral
}

/// Moves the clock forward.
///
/// Every delay in this pallet is measured in time, so the block number stays at one. It matters
/// only because events are not recorded at block zero.
pub fn advance_time(ms: Moment) {
	Timestamp::set_timestamp(Timestamp::get() + ms);
}

/// Deposits into a pool, signed by `who`.
pub fn deposit(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	amount: Balance,
) -> DispatchResult {
	Stability::deposit(RuntimeOrigin::signed(who), collateral, stable, amount)
}

pub fn request_withdraw(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	amount: Balance,
) -> DispatchResult {
	Stability::request_withdraw(RuntimeOrigin::signed(who), collateral, stable, amount)
}

/// Withdraws to a named recipient, signed by `who`.
pub fn withdraw(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	amount: Balance,
	recipient: AccountId,
) -> DispatchResult {
	Stability::withdraw(RuntimeOrigin::signed(who), collateral, stable, amount, Some(recipient))
}

pub fn claim_collateral(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	recipient: AccountId,
) -> DispatchResult {
	Stability::claim_collateral(RuntimeOrigin::signed(who), collateral, stable, Some(recipient))
}

pub fn claim_yield(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	recipient: AccountId,
) -> DispatchResult {
	Stability::claim_yield(RuntimeOrigin::signed(who), collateral, stable, Some(recipient))
}

/// Settles the deposit of `owner`, signed by any `caller`.
pub fn settle(
	caller: AccountId,
	owner: AccountId,
	collateral: AssetId,
	stable: StableId,
) -> DispatchResult {
	Stability::settle_deposit(RuntimeOrigin::signed(caller), owner, collateral, stable)
}

pub fn compound(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	amount: Balance,
) -> DispatchResult {
	Stability::compound_yield(RuntimeOrigin::signed(who), collateral, stable, amount)
}

/// Mints for `who` and deposits into the default pool.
/// The deposit follows the configured entry delay and therefore starts pending.
pub fn seed_deposit(who: AccountId, amount: Balance) {
	seed_deposit_from_balance(who, amount, amount);
}

/// Mints `balance` for `who`, then deposits `amount` behind the configured entry delay.
pub fn seed_deposit_from_balance(who: AccountId, balance: Balance, amount: Balance) {
	mint_stable(STABLE, who, balance);
	assert_ok!(deposit(who, DOT, STABLE, amount));
}

/// Deposits through the configured entry delay, then moves the clock to that deposit's deadline.
///
/// This does not advance the cohort or realize the row. The next ordinary pool operation must do
/// that itself, exactly as it does in production.
pub fn deposit_and_mature(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	amount: Balance,
) -> DispatchResult {
	let result = deposit(who, collateral.clone(), stable, amount);
	if result.is_ok() {
		if let Some(deadline) = pending_deadline(collateral, stable, who) {
			let now = Timestamp::get();
			if deadline > now {
				advance_time(deadline - now);
			}
		}
	}
	result
}

/// Mints and deposits a position whose entry delay has elapsed in the default pool.
pub fn seed_matured_deposit(who: AccountId, amount: Balance) {
	seed_matured_deposit_from_balance(who, amount, amount);
}

/// Mints `balance` for `who`, deposits `amount`, then moves the clock to its cohort deadline.
pub fn seed_matured_deposit_from_balance(who: AccountId, balance: Balance, amount: Balance) {
	mint_stable(STABLE, who, balance);
	assert_ok!(deposit_and_mature(who, DOT, STABLE, amount));
}

/// Gives an existing default-pool row backed claimable balances without exercising their source.
pub fn seed_claimables(who: AccountId, collateral_gain: Balance, yield_gain: Balance) {
	seed_claimables_in(DOT, who, collateral_gain, yield_gain);
}

/// [`seed_claimables`] in the `collateral`/[`STABLE`] market.
pub fn seed_claimables_in(
	collateral: AssetId,
	who: AccountId,
	collateral_gain: Balance,
	yield_gain: Balance,
) {
	let pool = Stability::pool_account(&collateral, &STABLE);
	crate::Deposits::<Test>::mutate((collateral.clone(), STABLE, who), |row| {
		let row = row.as_mut().expect("deposit row exists");
		row.claimable_collateral += collateral_gain;
		row.claimable_yield += yield_gain;
	});
	mutate_pool_state(collateral.clone(), STABLE, |state| {
		state.total_collateral_gains_unclaimed += collateral_gain;
		state.total_yield_unclaimed += yield_gain;
	});
	if collateral_gain > 0 {
		mint_collateral(collateral, pool, collateral_gain);
	}
	if yield_gain > 0 {
		mint_stable(STABLE, pool, yield_gain);
	}
}

/// The pallet event `$variant` of the default market, as a `RuntimeEvent`. The fields name
/// everything but the market.
#[cfg(test)]
macro_rules! market_event {
	($variant:ident { $($fields:tt)* }) => {
		$crate::mock::RuntimeEvent::from($crate::Event::<$crate::mock::Test>::$variant {
			collateral_id: $crate::mock::DOT,
			stable_id: $crate::mock::STABLE,
			$($fields)*
		})
	};
}
#[cfg(test)]
pub(crate) use market_event;

/// Rewrites the stored state of a registered pool in place, bypassing every pallet check.
pub fn mutate_pool_state(
	collateral: AssetId,
	stable: StableId,
	mutate: impl FnOnce(&mut crate::types::PoolState<Balance>),
) {
	crate::Pools::<Test>::mutate(collateral, stable, |pool| {
		mutate(&mut pool.as_mut().expect("pool registered").state);
	});
}

/// The cohort deadline the pending deposit of `who` waits out, read from the open slots.
pub fn pending_deadline(collateral: AssetId, stable: StableId, who: AccountId) -> Option<Moment> {
	let pending = deposit_row(collateral.clone(), stable, who)?.pending_deposit?;
	let state = pool_state(collateral, stable);
	state.cohort(pending.cohort).map(|cohort| cohort.deadline)
}

/// The single-depositor fixture: account 1 has 400 ready to activate in the default market, and
/// 600 in its wallet.
pub fn seed_pool_with_matured_deposit() {
	register_branch(DOT, STABLE, default_branch_config());
	mint_stable(STABLE, 1, 1_000);
	assert_ok!(deposit_and_mature(1, DOT, STABLE, 400));
}

/// [`seed_pool_with_matured_deposit`] with real market debt behind it: one vault at a TCR of 250%,
/// which leaves the market in Normal Mode and lets a test drive it into Safety Mode.
pub fn seed_branch_with_debt() {
	register_branch(DOT, STABLE, default_branch_config());
	mint_collateral(DOT, 5, 2_000);
	assert_ok!(open_vault(5, DOT, STABLE, 1_000, 500));
	mint_stable(STABLE, 1, 1_000);
	assert_ok!(deposit_and_mature(1, DOT, STABLE, 400));
}

/// Drops the price until the TCR reaches 120%, which is under the Safety threshold and over the
/// liquidation ratio. Needs the vault of [`seed_branch_with_debt`].
pub fn enter_safety_mode() {
	set_price(DOT, FixedU128::from_rational(6u128, 10u128));
}

/// Restores the registration price, which lifts the TCR back over the Safety threshold.
pub fn exit_safety_mode() {
	set_price(DOT, REGISTRATION_PRICE);
}

/// Replaces the parameters of the default pool, through governance.
pub fn set_pool_config(config: StabilityPoolConfig<Balance>) {
	assert_ok!(Stability::set_stability_pool_config(RuntimeOrigin::root(), DOT, STABLE, config));
}

/// Sets the post-offset floor of the default pool, through governance.
pub fn set_min_active_pool(min: Balance) {
	set_pool_config(StabilityPoolConfig {
		minimum_active_pool_balance: min,
		..default_pool_config()
	});
}

/// Mints stablecoin as the vault engine does and offers it to the pool, returning what the pool
/// could not take.
pub fn distribute_yield(
	collateral: AssetId,
	stable: StableId,
	amount: Balance,
) -> crate::pallet::StableCreditOf<Test> {
	let credit = issue_stable(stable, amount);
	offer_yield(collateral, stable, credit)
}

/// Offers `amount` of yield through [`distribute_yield`] and returns what the pool handed back.
pub fn distribute_yield_leftover(
	collateral: AssetId,
	stable: StableId,
	amount: Balance,
) -> Balance {
	distribute_yield(collateral, stable, amount).peek()
}

/// Offers `amount` of yield to the default market, which must take all of it.
pub fn distribute_all_yield(amount: Balance) {
	assert_eq!(distribute_yield_leftover(DOT, STABLE, amount), 0, "the pool takes the whole yield");
}

/// Issues stablecoin the way the vault engine mints interest.
pub fn issue_stable(stable: StableId, amount: Balance) -> crate::pallet::StableCreditOf<Test> {
	<Assets as FungiblesBalanced<AccountId>>::issue(stable, amount)
}

/// Offers an existing credit to the pool, returning what the pool could not take.
///
/// This calls the engine directly rather than through `OnBranchYield`, so the whole credit
/// reaches the pool and no `yield_share` cut is taken.
pub fn offer_yield(
	collateral: AssetId,
	stable: StableId,
	credit: crate::pallet::StableCreditOf<Test>,
) -> crate::pallet::StableCreditOf<Test> {
	let Some(pool) = crate::Pools::<Test>::get(&collateral, &stable) else {
		return credit;
	};
	let branch = branch_snapshot(&collateral, &stable);
	Stability::do_distribute_yield(&collateral, &stable, branch, pool, credit)
}

/// The market as the vault engine would hand it to the pool right now.
///
/// Production callers pass the snapshot they already hold; tests that call the pool interfaces
/// directly read it here, from the same vault engine and clock. A market the engine does not
/// know counts as frozen, as the pool's own dispatchables treat it.
pub fn branch_snapshot(collateral: &AssetId, stable: &StableId) -> BranchSnapshot {
	let mode = Stability::branch_mode_or_frozen(collateral, stable);
	BranchSnapshot { mode, now: Timestamp::get() }
}

/// Quotes a market under its current branch, as the liquidation engine reads it.
///
/// Every caller probes a registered, unfrozen market, which always quotes.
pub fn market_quote(collateral: &AssetId, stable: &StableId) -> crate::OffsetQuote<Test> {
	Stability::quote(collateral, stable, branch_snapshot(collateral, stable))
		.expect("an unfrozen market quotes")
}

/// Runs `operation` and checks that it wrote nothing, returning its value.
///
/// The value-returning form of `assert_storage_noop!`, for operations that hand a credit back.
pub fn storage_noop<R>(operation: impl FnOnce() -> R) -> R {
	use frame::deps::{sp_io::storage::root, sp_runtime::StateVersion};
	let root_before = root(StateVersion::V1);
	let result = operation();
	assert_eq!(root(StateVersion::V1), root_before, "storage has been mutated");
	result
}

/// Offers `amount` of yield and checks that the pool declines it: the credit comes back whole
/// and nothing is written.
///
/// Issuing and dropping the credit is not itself storage-neutral, since `pallet-assets` records
/// events for both, so only the offer sits inside the check.
pub fn assert_yield_declined(collateral: AssetId, stable: StableId, amount: Balance) {
	let credit = issue_stable(stable, amount);
	let returned = storage_noop(|| offer_yield(collateral, stable, credit));
	assert_eq!(returned.peek(), amount);
	drop(returned);
}

/// Issues collateral, standing in for what a liquidation seizes. Dropping it, or any part split
/// off it, only undoes the issuance made here.
pub fn issue_collateral(
	collateral: AssetId,
	amount: Balance,
) -> crate::pallet::CollateralCreditOf<Test> {
	<PoolCollateralAssets as FungiblesBalanced<AccountId>>::issue(collateral, amount)
}

/// Calls the pool's offset interface for `market` directly, issuing each leg's collateral
/// credit in the asset that leg names.
pub fn offset_with(
	market: (AssetId, StableId),
	branch: BranchSnapshot,
	debt: OffsetLegs<Balance>,
	collateral: OffsetLegs<(AssetId, Balance)>,
) -> Result<Option<crate::pallet::StableCreditOf<Test>>, DispatchError> {
	let (collateral_id, stable_id) = market;
	let credits = OffsetLegs {
		active: issue_collateral(collateral.active.0, collateral.active.1),
		pending: issue_collateral(collateral.pending.0, collateral.pending.1),
	};
	Stability::offset(&collateral_id, &stable_id, branch, debt, credits)
}

/// The proportional share of `amount`: `floor(amount * numerator / denominator)`.
///
/// The offset simulation uses it to slice collateral pro rata to the debt each leg burns.
pub fn pro_rata_floor(amount: Balance, numerator: Balance, denominator: Balance) -> Balance {
	assert!(numerator <= denominator);
	assert!(denominator > 0);
	stablecoin_primitives::mul_div_floor(amount, numerator, denominator)
		.expect("share of amount fits")
}

/// The legs one [`simulate_offset`] call settled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SimulatedOffset {
	/// Debt the active pool cancelled.
	pub active: Balance,
	/// Debt the pending deposits cancelled.
	pub pending: Balance,
	/// Collateral not handed to the pool, returned to the caller.
	pub leftover: Balance,
}

impl SimulatedOffset {
	/// Debt the pool cancelled across both legs.
	pub fn debt(&self) -> Balance {
		self.active + self.pending
	}
}

/// Offers up to `max_debt` to the pool through its public offset interface.
///
/// One `quote` under the current branch sizes the active leg, then the pending leg behind it,
/// and a single `offset` settles both, as the trait documents. `collateral_total` backs all of
/// `max_debt`: each leg takes the pro-rata floor of it for its debt, and the rest stays with the
/// caller. That split is the test's choice; the pool settles whatever collateral it is handed.
/// Legs that size to nothing make no call.
///
/// A settlement of quoted legs must not be refused, and the pool must give up exactly the debt
/// it cancels, so either failure panics.
pub fn simulate_offset(
	collateral: AssetId,
	stable: StableId,
	max_debt: Balance,
	collateral_total: Balance,
) -> SimulatedOffset {
	assert!(max_debt > 0);
	let branch = branch_snapshot(&collateral, &stable);
	let (active, pending) = match Stability::quote(&collateral, &stable, branch) {
		Some(quote) => {
			let active = Stability::quote_active(&quote, max_debt);
			assert!(active <= max_debt);
			let remaining = max_debt - active;
			let pending = if remaining.is_zero() {
				0
			} else {
				Stability::quote_pending(&quote, remaining, active)
			};
			assert!(pending <= remaining);
			(active, pending)
		},
		None => (0, 0),
	};
	if active.is_zero() && pending.is_zero() {
		return SimulatedOffset { active, pending, leftover: collateral_total };
	}
	let mut credit = issue_collateral(collateral.clone(), collateral_total);
	let active_collateral = credit.extract(pro_rata_floor(collateral_total, active, max_debt));
	let pending_collateral = credit.extract(pro_rata_floor(collateral_total, pending, max_debt));
	let burned = Stability::offset(
		&collateral,
		&stable,
		branch,
		OffsetLegs { active, pending },
		OffsetLegs { active: active_collateral, pending: pending_collateral },
	)
	.expect("quoted legs settle");
	let given_up = burned.as_ref().map_or(0, |credit| credit.peek());
	assert_eq!(given_up, active + pending, "the pool gives up exactly the debt it cancels");
	SimulatedOffset { active, pending, leftover: credit.peek() }
}

/// The deposit row of an account, or `None` if it was never created or has been removed.
pub fn deposit_row(
	collateral: AssetId,
	stable: StableId,
	who: AccountId,
) -> Option<crate::pallet::DepositOf<Test>> {
	crate::Deposits::<Test>::get((collateral, stable, who))
}

/// The pending amount the next settlement of `who` would store, read through the public view.
///
/// Zero when the settlement would leave nothing pending. The view must not write, which the
/// read checks.
pub fn pending_after_settlement(collateral: AssetId, stable: StableId, who: AccountId) -> Balance {
	storage_noop(|| Stability::deposit_after_settlement(collateral, stable, who))
		.expect("pool and row exist")
		.pending_deposit
		.map_or(0, |pending| pending.amount)
}

/// The live state of a pool. Panics if the market is not registered.
pub fn pool_state(collateral: AssetId, stable: StableId) -> crate::types::PoolState<Balance> {
	crate::Pools::<Test>::get(collateral, stable).expect("pool registered").state
}

/// Moves the vault of `owner` into `FinalRecovery`. The call is permissionless, so any account
/// may sign it; the test sets up the price first.
pub fn enter_final_recovery(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
) -> DispatchResult {
	Vaults::enter_final_recovery(RuntimeOrigin::signed(99), collateral, stable, owner)
}

/// The stored debt of a vault, principal and settled interest together. Zero if it is absent.
pub fn vault_debt(collateral: AssetId, stable: StableId, who: AccountId) -> Balance {
	pallet_vaults::Vaults::<Test>::get((collateral, stable, who))
		.map(|record| record.vault.debt.principal + record.vault.debt.interest)
		.unwrap_or_default()
}

/// Asserts that Vaults reported `debt` cancelled against `collateral` for the vault of `owner`.
///
/// The recovery events of this pallet carry no amounts, because the vault engine reports them.
pub fn assert_vault_redeemed(owner: AccountId, debt: Balance, collateral: Balance) {
	let reported = System::events().iter().any(|record| {
		matches!(
			record.event,
			RuntimeEvent::Vaults(pallet_vaults::Event::VaultRedeemed {
				owner: redeemed,
				debt_cancelled,
				collateral_to_recipient,
				..
			}) if redeemed == owner && debt_cancelled == debt && collateral_to_recipient == collateral
		)
	});
	assert!(reported, "no `VaultRedeemed` for vault {owner}: debt {debt}, collateral {collateral}");
}

/// Runs an active-pool recovery offset, signed by an arbitrary account.
pub fn offset_recovery(
	collateral: AssetId,
	stable: StableId,
	max_stable_in: Balance,
) -> DispatchResult {
	Stability::offset_recovery(RuntimeOrigin::signed(99), collateral, stable, max_stable_in)
}
