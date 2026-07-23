//! Test runtime for `pallet-stability`.
//!
//! The mock composes the real vault stack — registering a market through
//! `pallet-vaults` seeds this pallet's pool rows via the `OnBranchLifecycle`
//! hook, exactly as a production runtime would.
//!
//! Conventions used in the tests:
//! - Collateral `AssetId::Native` ([`DOT`]) routes to `Balances`; `AssetId::WithId(asset)` routes
//!   to `AssetsHolder`. Stablecoins are plain `pallet-assets` ids: [`PUSD`] is the unit-scale
//!   default coin, [`USDX`] the 6-decimals coin the scale tests use.

use crate as pallet_stability;
use crate::types::{PoolPrecision, StabilityPoolConfig};
pub use frame::{
	arithmetic::{FixedPointNumber, FixedU128, One, Permill, Saturating, Zero},
	prelude::DispatchError,
	testing_prelude::{assert_noop, assert_ok, BadOrigin},
};
use frame::{
	deps::sp_runtime::traits::ConvertInto,
	testing_prelude::*,
	traits::{
		fungible::{HoldConsideration, NativeFromLeft, NativeOrWithId},
		fungibles::{roles::Inspect as FungiblesRolesInspect, Balanced as FungiblesBalanced},
		tokens::{fungible, imbalance::ResolveAssetTo},
		AsEnsureOriginWithArg, Convert, EnsureOriginWithArg, IdentityLookup, LinearStoragePrice,
	},
};
use pusd_primitives::ProvidePrice;

// 16 bytes so `into_sub_account_truncating` keeps the pallet id plus part of
// the market seed: a `u64` would truncate every pusd-pallet sub-account to
// the shared `"modl" + "pusd"` prefix, collapsing them into one account.
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
	pub const IdleMaxRefreshWeight: Option<Weight> = Some(Weight::MAX);
	pub const VaultsPalletId: PalletId = PalletId(*b"pusd/vlt");
	pub const StabilityPalletId: PalletId = PalletId(*b"pusd/stb");
}

impl pallet_linked_list::Config for Test {
	type WeightInfo = ();
	type ListId = VaultList;
	type ItemId = AccountId;
	type Priority = FixedU128;
	type MaxHintRepairSteps = MaxHintRepairSteps;
	type PriorityProvider = pallet_vaults::Pallet<Test>;
}

/// Unified collateral surface: `Balances` (native) on the left, `AssetsHolder`
/// (multi-asset, hold-aware) on the right.
pub type VaultCollateralAssets =
	fungible::UnionOf<Balances, AssetsHolder, NativeFromLeft, AssetId, AccountId>;

/// The pool's collateral surface: the same ledger, but through the plain
/// mutate-capable `Assets` side — the pool holds nothing, it only receives
/// offset collateral and pays claims, so it needs `Mutate`, not `MutateHold`
/// (which is all `AssetsHolder` offers).
pub type PoolCollateralAssets =
	fungible::UnionOf<Balances, Assets, NativeFromLeft, AssetId, AccountId>;

/// Naive oracle: tests poke [`set_price`]. Prices are keyed by collateral
/// alone — stablecoins are treated as $1-pegged at par.
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
		RuntimeHoldReason::Vaults(pallet_vaults::HoldReason::MarketCreationDeposit);
	pub const MarketDepositBase: Balance = 1_000;
}

/// Full admin of every market a test helper registers.
pub const ADMIN: AccountId = 100;
/// Emergency (tighten-only) admin of every market a test helper registers.
pub const EMERGENCY_ADMIN: AccountId = 101;

/// The `create_branch` admin bundle: `full` administers, `emergency` tightens.
/// Admins are stored as origin callers, here plain signed origins.
pub fn branch_admins(
	full: AccountId,
	emergency: AccountId,
) -> pallet_vaults::types::BranchAdmins<AccountId> {
	pallet_vaults::types::BranchAdmins { full_admin: full, emergency_admin: emergency }
}

/// `CreateOrigin`: Root creates deposit-free (`None`); the stable asset's owner
/// creates with a deposit (`Some(who)`); anyone else is rejected.
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

/// Flat 1_000-unit refundable creation deposit, held in native balance.
pub type VaultsConsideration = HoldConsideration<
	AccountId,
	Balances,
	MarketDepositReason,
	LinearStoragePrice<MarketDepositBase, ConstU128<0>, Balance>,
>;

parameter_types! {
	/// Governance envelope the test default config sits comfortably inside.
	pub TestBranchConfigGuard: pallet_vaults::types::BranchConfigGuard<Balance> =
		pallet_vaults::types::BranchConfigGuard {
			min_minimum_collateralization_ratio: FixedU128::from_rational(105u128, 100u128),
			min_initial_collateralization_ratio: FixedU128::from_rational(110u128, 100u128),
			min_safety_collateralization_ratio: FixedU128::from_rational(120u128, 100u128),
			min_minimum_debt: 100,
			min_minimum_collateral: 1,
			max_borrow_rate: FixedU128::from_rational(400u128, 100u128),
			max_branch_line: 1_000_000_000,
			max_ceiling_gap: 1_000_000_000,
			min_ceiling_ttl: 24 * 3_600 * 1_000,
		};
}

impl pallet_vaults::Config for Test {
	type RuntimeHoldReason = RuntimeHoldReason;
	type StableToCollateralId = ConvertInto;
	type CollateralAssets = VaultCollateralAssets;
	type StableAssets = Assets;
	type Oracle = MockOracle;
	type FeeHandler = ResolveAssetTo<FeeDestAccount, Assets>;
	// The pool takes its `yield_share` of every minted branch credit; the
	// fee destination receives the remainder.
	type YieldHook = Stability;
	// Registering a market seeds the siblings' per-market rows; redemptions
	// first, so its config (which recovery-offset pricing reads) always
	// exists whenever the pool rows do.
	type OnBranchLifecycle = (Redemptions, Stability);
	type TimeProvider = Timestamp;
	type CreateOrigin = EnsureAssetOwner;
	type Consideration = VaultsConsideration;
	type BranchConfigGuard = TestBranchConfigGuard;
	type ForceOrigin = frame_system::EnsureRoot<AccountId>;
	type PalletId = VaultsPalletId;
	type VaultLists = LinkedList;
	type IdleMaxRefreshWeight = IdleMaxRefreshWeight;
	type WeightInfo = ();
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = VaultsBenchHelper;
}

#[cfg(feature = "runtime-benchmarks")]
pub struct VaultsBenchHelper;
#[cfg(feature = "runtime-benchmarks")]
impl pallet_vaults::BenchmarkHelper<AssetId, StableId, AccountId, Balance> for VaultsBenchHelper {
	fn collateral_asset_id() -> AssetId {
		DOT
	}

	fn stable_asset_id() -> StableId {
		PUSD
	}

	fn mint_collateral(collateral_id: AssetId, who: &AccountId, amount: Balance) {
		use frame::traits::{
			fungible::Mutate as FungibleMutate, fungibles::Mutate as FungiblesMutate,
		};
		// Native ED first: fresh accounts need it before any other operation.
		let _ = <Balances as FungibleMutate<AccountId>>::mint_into(who, 1);
		match collateral_id {
			AssetId::Native => {
				<Balances as FungibleMutate<AccountId>>::mint_into(who, amount)
					.expect("mint native collateral for benchmark account");
			},
			AssetId::WithId(asset_id) => {
				<Assets as FungiblesMutate<AccountId>>::mint_into(asset_id, who, amount)
					.expect("mint asset collateral for benchmark account");
			},
		};
	}

	fn mint_stable(stable_id: StableId, who: &AccountId, amount: Balance) {
		use frame::traits::fungibles::Mutate as FungiblesMutate;
		<Assets as FungiblesMutate<AccountId>>::mint_into(stable_id, who, amount)
			.expect("mint stable for benchmark account");
	}

	fn set_oracle_price(collateral_id: AssetId, _stable_id: StableId, price: FixedU128) {
		set_price(collateral_id, price);
	}

	fn advance_time(ms: u64) {
		advance_time(ms);
	}

	fn synth_market(seed: u32) -> (AssetId, StableId) {
		(AssetId::WithId(10_000 + seed), 20_000 + seed)
	}
}

/// Root (the governance override) or the market's stored full admin, the same
/// composition a production runtime would use.
pub type StabilityUpdateOrigin = EitherOf<
	AsEnsureOriginWithArg<frame_system::EnsureRoot<AccountId>>,
	pallet_vaults::EnsureBranchFullAdmin<Test>,
>;

parameter_types! {
	pub static DefaultStabilityPoolConfig: StabilityPoolConfig<Balance> = default_pool_config();
}

/// Account the redemption `FeeHandler` resolves pUSD fees into. Recovery
/// offsets are fee-free, so it only collects from ordinary redemptions.
pub const FEE_DEST: AccountId = 888;

/// Account market-teardown dust resolves into, standing in for the treasury.
/// Distinct from [`FEE_DEST`] so tests can tell revenue streams apart.
pub const DUST_DEST: AccountId = 889;

parameter_types! {
	pub const DustDestAccount: AccountId = DUST_DEST;
}

/// Each stablecoin's insurance cover lives at its own account; empty in
/// these tests, so every below-par head prices as `BelowPar` regardless.
pub struct InsuranceFundAccounts;
impl Convert<StableId, AccountId> for InsuranceFundAccounts {
	fn convert(stable: StableId) -> AccountId {
		700_000 + AccountId::from(stable)
	}
}

parameter_types! {
	pub const FeeDestAccount: AccountId = FEE_DEST;
	pub static DefaultRedemptionConfig: pallet_redemptions::types::RedemptionConfig<Balance> =
		pallet_redemptions::types::RedemptionConfig {
			minimum_redemption_amount: 100,
			dynamic_fee_decay_period: 6 * 3_600 * 1_000,
			dynamic_fee_floor: FixedU128::zero(),
			dynamic_fee_ceiling: FixedU128::one(),
			base_fee: Permill::from_rational(5u32, 1_000u32),
			fee_ceiling: Permill::one(),
			dynamic_fee_increase_divisor: FixedU128::from_rational(2u128, 1u128),
			final_recovery_bonus_buffer: Permill::from_percent(1),
		};
}

impl pallet_redemptions::Config for Test {
	type StableAssets = Assets;
	type Oracle = MockOracle;
	type Vaults = Vaults;
	type InsuranceFundAccount = InsuranceFundAccounts;
	type FeeHandler = ResolveAssetTo<FeeDestAccount, Assets>;
	type TimeProvider = Timestamp;
	type UpdateOrigin = StabilityUpdateOrigin;
	type DefaultRedemptionConfig = DefaultRedemptionConfig;
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
		register_branch(DOT, PUSD, default_branch_config());
		let debt: Balance = 300;
		for i in 0..vaults {
			let who = 1_000 + AccountId::from(i);
			mint_collateral(DOT, who, 10_000_000_000);
			open_vault(who, DOT, PUSD, 1_000_000, debt).expect("open benchmark vault");
		}
		let redeemer: AccountId = 1;
		let budget = debt.saturating_mul(Balance::from(vaults).saturating_add(2)).saturating_mul(2);
		mint_stable(PUSD, redeemer, budget.saturating_mul(2));
		(DOT, PUSD, redeemer, budget)
	}
}

impl pallet_stability::Config for Test {
	type CollateralAssetId = AssetId;
	type StableAssetId = StableId;
	type StableAssets = Assets;
	type CollateralAssets = PoolCollateralAssets;
	// The real vault pallet derives the mode (persisted freeze, oracle
	// health, live TCR), exactly as a production runtime would.
	type BranchModes = Vaults;
	// The real redemptions pallet prices recovery settlement, so offset
	// pricing and recovery-redemption pricing share one code path.
	type RecoveryOffsets = Redemptions;
	type StableDustHandler = ResolveAssetTo<DustDestAccount, Assets>;
	type CollateralDustHandler = ResolveAssetTo<DustDestAccount, PoolCollateralAssets>;
	// The runtime's single linked-list instance, shared with vaults; this
	// pallet only touches its `StabilityPending` lists.
	type PendingLists = LinkedList;
	type TimeProvider = Timestamp;
	type UpdateOrigin = StabilityUpdateOrigin;
	type DefaultStabilityPoolConfig = DefaultStabilityPoolConfig;
	type MaxPendingOffsetIterations = ConstU32<8>;
	type PalletId = StabilityPalletId;
	type WeightInfo = ();
}

/// DOT-equivalent native collateral asset id used across tests.
pub const DOT: AssetId = AssetId::Native;

/// A non-native test collateral that lives in `pallet-assets`.
pub const TOKEN_X_ID: AssetIdForAssets = 1;
pub const TOKEN_X: AssetId = AssetId::WithId(TOKEN_X_ID);

/// Default unit-scale stablecoin every helper mints against.
pub const PUSD: StableId = 1_000;

/// A 6-decimals stablecoin: 1 coin = [`USDX_UNIT`] raw units,
/// with a realistic 0.01-coin minimum balance.
pub const USDX: StableId = 6_000;
/// Raw units in one 6-decimals coin.
pub const USDX_UNIT: Balance = 1_000_000;
/// The [`USDX`] minimum balance: 0.01 coin.
pub const USDX_MIN_BALANCE: Balance = USDX_UNIT / 100;

pub fn new_test_ext() -> TestState {
	let t = RuntimeGenesisConfig {
		assets: pallet_assets::GenesisConfig {
			assets: vec![
				(TOKEN_X_ID, 1, true, 1),
				(PUSD, 1, true, 1),
				(USDX, 1, true, USDX_MIN_BALANCE),
			],
			metadata: vec![(USDX, b"USDX".to_vec(), b"USDX".to_vec(), 6)],
			accounts: vec![],
			next_asset_id: None,
			reserves: vec![],
		},
		system: Default::default(),
		balances: pallet_balances::GenesisConfig {
			balances: (1u128..=10u128).map(|i| (i, 1_000_000_000_000)).collect(),
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
		// Reset: a prior test on this thread may have replaced the default.
		DefaultStabilityPoolConfig::set(default_pool_config());
	});
	ext
}

/// Run `test` and check post-state invariants under `try-runtime`.
pub fn build_and_execute(test: impl FnOnce()) {
	new_test_ext().execute_with(|| {
		test();
		#[cfg(feature = "try-runtime")]
		crate::try_state::do_try_state::<Test>().expect("post-test invariants hold");
	});
}

/// The reference pool config seeded into every registered branch: 5_000 ms
/// entry delay, 600_000 ms safety withdrawal delay.
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

/// Default branch config: MCR=110%, ICR=120%, Safety=130%, ceiling 100M,
/// MinDebt=200, MinColl=1, rate bounds 0.1%-100%, 7d upfront fee,
/// 1d rate-cooldown, 5% redistribution penalty.
pub fn default_branch_config() -> pallet_vaults::BranchConfig<Balance> {
	pallet_vaults::BranchConfig {
		minimum_collateralization_ratio: FixedU128::from_rational(110u128, 100u128),
		initial_collateralization_ratio: FixedU128::from_rational(120u128, 100u128),
		safety_collateralization_ratio: FixedU128::from_rational(130u128, 100u128),
		debt_ceiling: 100_000_000,
		minimum_debt: 200,
		minimum_collateral: 1,
		minimum_borrow_rate: FixedU128::from_rational(1u128, 1_000u128),
		maximum_borrow_rate: FixedU128::from_rational(100u128, 100u128),
		upfront_fee_period: 7 * 24 * 3_600 * 1_000,
		rate_adjustment_cooldown: 24 * 3_600 * 1_000,
		redistribution_penalty: Permill::from_percent(5),
		ceiling_gap: 0,
		ceiling_ttl: 0,
	}
}

/// Registers the `(collateral, stable)` market at price 1.25$ with a high
/// global debt ceiling. Creation also seeds this pallet's pool rows through
/// the `OnBranchLifecycle` hook.
pub fn register_branch(
	collateral: AssetId,
	stable: StableId,
	config: pallet_vaults::BranchConfig<Balance>,
) {
	// `create_branch` requires a live price, so set it before creating.
	set_price(collateral.clone(), FixedU128::from_rational(5u128, 4u128));
	Vaults::create_branch(
		RuntimeOrigin::root(),
		collateral.clone(),
		stable,
		branch_admins(ADMIN, EMERGENCY_ADMIN),
		config,
	)
	.expect("create_branch ok");
	Vaults::set_global_debt_ceiling(RuntimeOrigin::root(), collateral, 1_000_000_000_000_000)
		.expect("set global debt ceiling");
	// No ED pre-fund for the pool sub-account: the registration hook's
	// provider reference keeps it alive, and pre-funding native balance
	// would show up as untracked collateral in the DOT-market identity.
}

/// Open a vault for `who` on the `(collateral, stable)` market with
/// `(None, None)` rate-index hints, so mode tests can create real branch
/// debt and drive the TCR.
pub fn open_vault(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	collateral_amount: Balance,
	debt: Balance,
) -> DispatchResult {
	use frame::traits::fungible::Mutate as FungibleMutate;
	// Native ED first: fresh accounts need it before any other operation.
	let _ = <Balances as FungibleMutate<AccountId>>::mint_into(&who, 1);
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

/// Stablecoin balance of `(stable, who)` in `pallet-assets`.
pub fn stable_balance(stable: StableId, who: AccountId) -> Balance {
	use frame::traits::fungibles::Inspect as FungiblesInspect;
	<Assets as FungiblesInspect<AccountId>>::balance(stable, &who)
}

/// Balance of `(collateral, who)` on the pool's collateral surface.
pub fn collateral_balance(collateral: AssetId, who: AccountId) -> Balance {
	use frame::traits::fungibles::Inspect as FungiblesInspect;
	<PoolCollateralAssets as FungiblesInspect<AccountId>>::balance(collateral, &who)
}

/// Advance mock time by `ms` milliseconds. The pallet is purely time-based;
/// the block number stays at its genesis value of 1 (needed only so events
/// are recorded).
pub fn advance_time(ms: Moment) {
	Timestamp::set_timestamp(Timestamp::get() + ms);
}

/// Deposit into the `(collateral, stable)` pool, mirroring the extrinsic's
/// argument order with `who` as the signed origin.
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

/// Withdraw to an explicit `recipient` (the extrinsic defaults a `None`
/// recipient to the caller), with `who` as the signed origin.
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

/// Poke `owner`'s deposit, signed by `caller` (permissionless).
pub fn poke(
	caller: AccountId,
	owner: AccountId,
	collateral: AssetId,
	stable: StableId,
) -> DispatchResult {
	Stability::poke_deposit(RuntimeOrigin::signed(caller), owner, collateral, stable)
}

pub fn compound(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	amount: Balance,
) -> DispatchResult {
	Stability::compound_yield(RuntimeOrigin::signed(who), collateral, stable, amount)
}

/// Mint `amount` for `who` and deposit it into the default (DOT, PUSD) pool;
/// the deposit stays pending until [`activate_all`] (or any other touch past
/// the entry delay) folds it in.
pub fn seed_deposit(who: AccountId, amount: Balance) {
	mint_stable(PUSD, who, amount);
	assert_ok!(deposit(who, DOT, PUSD, amount));
}

/// Advance past the default entry delay and fold every listed depositor's
/// matured pending deposit into the (DOT, PUSD) active pool. Activation is
/// automatic on any touch; the permissionless poke stands in for one.
pub fn activate_all(depositors: &[AccountId]) {
	advance_time(5_000);
	for who in depositors {
		assert_ok!(poke(*who, *who, DOT, PUSD));
	}
}

/// The canonical single-depositor fixture: register the default (DOT, PUSD)
/// market, deposit 400 for user 1 at t = 1_000 (minting 1_000, so 600 stays
/// in the wallet), active from t = 6_000.
pub fn seed_active_deposit() {
	register_branch(DOT, PUSD, default_branch_config());
	mint_stable(PUSD, 1, 1_000);
	assert_ok!(deposit(1, DOT, PUSD, 400));
	activate_all(&[1]);
}

/// Register the default (DOT, PUSD) market, give it real branch debt (a
/// 1000-collateral / 500-debt vault: TCR 250% at the 1.25 registration
/// price), and activate a 400 deposit for user 1 — all still in Normal Mode.
pub fn seed_branch_with_debt() {
	register_branch(DOT, PUSD, default_branch_config());
	mint_collateral(DOT, 5, 2_000);
	assert_ok!(open_vault(5, DOT, PUSD, 1_000, 500));
	mint_stable(PUSD, 1, 1_000);
	assert_ok!(deposit(1, DOT, PUSD, 400));
	activate_all(&[1]);
}

/// TCR = 1000 * 0.6 / 500 = 120%: below the 130% Safety threshold, above
/// the 110% MCR. Needs [`seed_branch_with_debt`]'s vault to bite.
pub fn enter_safety_mode() {
	set_price(DOT, FixedU128::from_rational(6u128, 10u128));
}

/// Restore the 1.25 registration price: TCR back above the Safety threshold.
pub fn exit_safety_mode() {
	set_price(DOT, FixedU128::from_rational(5u128, 4u128));
}

/// Replace the (DOT, PUSD) pool's `minimum_active_pool_balance` (the §6.5
/// post-offset floor) via governance.
pub fn set_min_active_pool(min: Balance) {
	let mut config = default_pool_config();
	config.minimum_active_pool_balance = min;
	assert_ok!(Stability::set_stability_pool_config(RuntimeOrigin::root(), DOT, PUSD, config));
}

/// Mint a fresh stablecoin credit (as the vault engine's yield minting does)
/// and hand it to the pool for distribution, returning what the pool could
/// not take.
pub fn distribute_yield(
	collateral: AssetId,
	stable: StableId,
	amount: Balance,
) -> crate::pallet::StableCreditOf<Test> {
	let credit = <Assets as FungiblesBalanced<AccountId>>::issue(stable, amount);
	// The engine fn, not the `OnBranchYield` impl: the full credit enters
	// the pool, with no `yield_share` cut taken.
	let Some(pool) = crate::Pools::<Test>::get(&collateral, &stable) else {
		return credit;
	};
	Stability::do_distribute_yield(&collateral, &stable, pool, credit)
}

/// Issue a fresh collateral credit, standing in for the future liquidations
/// pallet's seized collateral. Dropping it (or a remainder split off it)
/// only rescinds the issuance created here.
pub fn issue_collateral(
	collateral: AssetId,
	amount: Balance,
) -> crate::pallet::CollateralCreditOf<Test> {
	<PoolCollateralAssets as FungiblesBalanced<AccountId>>::issue(collateral, amount)
}

/// Run an active-pool offset against a freshly issued collateral credit.
/// Returns the cancelled debt and the unconsumed remainder's amount.
pub fn simulate_offset(
	collateral: AssetId,
	stable: StableId,
	max_debt: Balance,
	collateral_for_pool: Balance,
) -> (Balance, Balance) {
	let credit = issue_collateral(collateral.clone(), collateral_for_pool);
	let (result, remainder) =
		Stability::do_offset_liquidation(&collateral, &stable, max_debt, credit);
	(result, remainder.peek())
}

/// Run a pending-deposit backstop offset with the same credit plumbing.
pub fn simulate_pending_offset(
	collateral: AssetId,
	stable: StableId,
	max_debt_to_offset: Balance,
	remaining_collateral: Balance,
) -> (crate::types::PendingOffsetResult<Balance>, Balance) {
	let credit = issue_collateral(collateral.clone(), remaining_collateral);
	let (result, remainder) =
		Stability::do_offset_pending_liquidation(&collateral, &stable, max_debt_to_offset, credit);
	(result, remainder.peek())
}

/// The caller's deposit row; `None` when pruned or never created.
pub fn deposit_row(
	collateral: AssetId,
	stable: StableId,
	who: AccountId,
) -> Option<crate::pallet::DepositOf<Test>> {
	crate::Deposits::<Test>::get((collateral, stable, who))
}

fn pending_list(collateral: AssetId, stable: StableId) -> VaultList {
	pusd_primitives::StableListId::StabilityPending(collateral, stable)
}

/// Whether `who` sits in the branch's pending-deposit FIFO.
pub fn pending_contains(collateral: AssetId, stable: StableId, who: AccountId) -> bool {
	LinkedList::contains(pending_list(collateral, stable), who)
}

/// The oldest member of the branch's pending-deposit FIFO (the list tail).
pub fn pending_oldest(collateral: AssetId, stable: StableId) -> Option<AccountId> {
	LinkedList::tail(pending_list(collateral, stable))
}

pub fn pending_count(collateral: AssetId, stable: StableId) -> u32 {
	LinkedList::count(pending_list(collateral, stable))
}

/// The branch's live pool state; panics when the branch is not registered.
pub fn pool_state(collateral: AssetId, stable: StableId) -> crate::types::PoolState<Balance> {
	crate::Pools::<Test>::get(collateral, stable).expect("pool registered").state
}

/// Park `owner`'s vault in `FinalRecovery`. The call is permissionless, so
/// an arbitrary keeper signs it; tests set the price preconditions.
pub fn enter_final_recovery(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
) -> DispatchResult {
	Vaults::enter_final_recovery(RuntimeOrigin::signed(99), collateral, stable, owner)
}

/// The vault's stored debt (principal + settled interest); zero when absent.
pub fn vault_debt(collateral: AssetId, stable: StableId, who: AccountId) -> Balance {
	pallet_vaults::Vaults::<Test>::get((collateral, stable, who))
		.map(|v| v.debt.principal + v.debt.interest)
		.unwrap_or_default()
}

/// Trigger an active-pool recovery offset, signed by an arbitrary keeper.
pub fn offset_recovery(
	collateral: AssetId,
	stable: StableId,
	max_stable_in: Balance,
) -> DispatchResult {
	Stability::offset_recovery(RuntimeOrigin::signed(99), collateral, stable, max_stable_in)
}
