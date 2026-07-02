//! Test runtime for `pallet-vaults`.
//!
//! Conventions used in the tests:
//! - Collateral `AssetId::Native` routes to `Balances`; `AssetId::WithId(asset)` routes to
//!   `AssetsHolder`.
//! - Stablecoins are plain `pallet-assets` ids: [`PUSD`] (dotUSD) is the default coin every helper
//!   mints; [`EUSD`] (ethUSD) is the second coin used by the multi-stablecoin tests.
//! - `AccountId = u64`. Per-market redistribution sub-account derivation is exercised in production
//!   (`AccountId32`); a `u64` account is too narrow to keep those sub-accounts distinct, so the
//!   mock does not assert their independence.

use crate as pallet_vaults;
pub use crate::{
	pallet::{BalanceOf, StableCreditOf},
	BranchConfig, BranchMode, Error, Event, HoldReason, Pallet,
};
pub use frame::{
	arithmetic::{FixedPointNumber, FixedU128, Saturating},
	prelude::{DispatchError, DispatchResult},
	testing_prelude::{assert_err, assert_noop, assert_ok},
};
use frame::{
	testing_prelude::*,
	traits::{
		fungible::{HoldConsideration, ItemOf, NativeFromLeft, NativeOrWithId},
		fungibles::{
			roles::Inspect as FungiblesRolesInspect, Credit, Inspect as FungiblesInspect,
			InspectHold,
		},
		tokens::{fungible, imbalance::ResolveAssetTo},
		AsEnsureOriginWithArg, EnsureOriginWithArg, IdentityLookup, LinearStoragePrice,
		OnUnbalanced,
	},
};
pub use pallet_linked_list::Position;
use pusd_primitives::{
	KeeperCompensation, LiquidationAllocation, OffsetAllocation, RedemptionAllocation,
	VaultLiquidationInterface, VaultRedemptionInterface,
};

pub type AccountId = u64;
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
	pub const MaxBranches: u32 = 8;
	pub const MaxOnIdleVaultRefresh: u32 = 4;
	pub const VaultsPalletId: PalletId = PalletId(*b"pusd/vlt");
}

impl pallet_linked_list::Config for Test {
	type WeightInfo = ();
	type ListId = VaultList;
	type ItemId = AccountId;
	type Priority = FixedU128;
	type MaxHintRepairSteps = MaxHintRepairSteps;
	type PriorityProvider = pallet_vaults::Pallet<Test>;
}

/// Unified collateral surface: `Balances` (single-asset, native) on the
/// left; `AssetsHolder` (multi-asset, hold-aware) on the right.
pub type VaultCollateralAssets =
	fungible::UnionOf<Balances, AssetsHolder, NativeFromLeft, AssetId, AccountId>;

/// Multi-asset stable issuance surface: the whole `pallet-assets` instance.
pub type VaultStableAssets = Assets;

parameter_types! {
	pub const PusdAssetId: AssetIdForAssets = 1_000;
}
/// Single-asset view of the default stablecoin, for tests that read or move
/// pUSD directly without going through the market API.
pub type Pusd = ItemOf<Assets, PusdAssetId, AccountId>;

/// Naive oracle: tests poke `set_price(collateral, price)`. Prices are keyed by
/// collateral alone — issued coins are treated as $1-pegged at par.
pub struct MockOracle;
parameter_types! {
	pub static MockPrices: alloc::collections::BTreeMap<AssetId, FixedU128> =
		alloc::collections::BTreeMap::new();
	pub static MockOracleAvailable: bool = true;
}
impl pusd_primitives::ProvidePrice for MockOracle {
	type AssetId = AssetId;

	fn provide_price(collateral: &AssetId) -> Result<FixedU128, DispatchError> {
		if !MockOracleAvailable::get() {
			return Err(crate::pallet::Error::<Test>::OraclePriceNotAvailable.into());
		}
		MockPrices::get()
			.get(collateral)
			.copied()
			.ok_or_else(|| crate::pallet::Error::<Test>::OraclePriceNotAvailable.into())
	}
}

pub fn set_price(collateral: AssetId, price: FixedU128) {
	MockPrices::mutate(|m| {
		m.insert(collateral, price);
	});
}

/// Derived branch mode (`None` when the market is unknown or the mode cannot
/// be computed), for tests observing Normal/Safety/Frozen transitions.
pub fn branch_mode(collateral: &AssetId, stable: &StableId) -> Option<BranchMode> {
	crate::Pallet::<Test>::current_mode(collateral, stable).ok()
}

pub fn set_oracle_available(v: bool) {
	MockOracleAvailable::set(v);
}

/// Account the protocol's fee residual resolves into, so tests can assert the
/// exact pUSD routed as fees.
pub const FEE_DEST: AccountId = 200;

parameter_types! {
	pub const FeeDestAccount: AccountId = FEE_DEST;
	/// Fraction of each minted fee credit routed to the Stability-Pool share by
	/// [`DealWithFees`]; the residual resolves to [`FEE_DEST`]. A settable
	/// static, so a test can exercise any split.
	pub static SpFeeShare: Permill = Permill::from_percent(75);
}

/// Runtime-side fee policy, built from the stock `OnUnbalanced` alone: split
/// each minted fee credit per [`SpFeeShare`] into the Stability-Pool share
/// (TODO: burned until the SP pallet lands) and the protocol residual resolved to
/// [`FEE_DEST`].
pub struct DealWithFees;
impl OnUnbalanced<Credit<AccountId, VaultStableAssets>> for DealWithFees {
	fn on_nonzero_unbalanced(credit: Credit<AccountId, VaultStableAssets>) {
		let sp_share = SpFeeShare::get() * credit.peek();
		let (sp_credit, residual) = credit.split(sp_share);
		drop(sp_credit);
		ResolveAssetTo::<FeeDestAccount, VaultStableAssets>::on_unbalanced(residual);
	}
}

parameter_types! {
	/// Log of `(collateral, stable, registered?)` lifecycle hook calls.
	pub static LifecycleLog: alloc::vec::Vec<(AssetId, StableId, bool)> = alloc::vec::Vec::new();
}

/// Records the market lifecycle hooks so tests can assert they fire.
pub struct RecordingLifecycle;
impl pusd_primitives::OnBranchLifecycle<AssetId, StableId> for RecordingLifecycle {
	fn on_registered(collateral_id: &AssetId, stable_id: &StableId) -> DispatchResult {
		LifecycleLog::mutate(|l| l.push((collateral_id.clone(), *stable_id, true)));
		Ok(())
	}
	fn on_deregistered(collateral_id: &AssetId, stable_id: &StableId) -> DispatchResult {
		LifecycleLog::mutate(|l| l.push((collateral_id.clone(), *stable_id, false)));
		Ok(())
	}
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

/// The origin caller under which `who` administers markets — admins are stored
/// as origin callers, not accounts.
pub fn admin_caller(who: AccountId) -> OriginCaller {
	frame_system::RawOrigin::Signed(who).into()
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
	type CollateralAssetId = AssetId;
	type StableAssetId = StableId;
	type SameAsset = pallet_vaults::SameAssetViaInto;
	type CollateralAssets = VaultCollateralAssets;
	type StableAssets = VaultStableAssets;
	type Oracle = MockOracle;
	type FeeHandler = DealWithFees;
	type OnBranchLifecycle = RecordingLifecycle;
	type TimeProvider = Timestamp;
	type CreateOrigin = EnsureAssetOwner;
	type Consideration = VaultsConsideration;
	type BranchConfigGuard = TestBranchConfigGuard;
	type GlobalManagerOrigin = frame_system::EnsureRoot<AccountId>;
	type PalletId = VaultsPalletId;
	type VaultLists = LinkedList;
	type MaxBranches = MaxBranches;
	type MaxOnIdleVaultRefresh = MaxOnIdleVaultRefresh;
	type WeightInfo = ();
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = MockBenchmarkHelper;
}

#[cfg(feature = "runtime-benchmarks")]
pub struct MockBenchmarkHelper;

#[cfg(feature = "runtime-benchmarks")]
impl pallet_vaults::BenchmarkHelper<AssetId, StableId, AccountId, Balance> for MockBenchmarkHelper {
	fn collateral_asset_id() -> AssetId {
		DOT
	}

	fn stable_asset_id() -> StableId {
		PUSD
	}

	fn mint_collateral(collateral_id: AssetId, who: &AccountId, amount: Balance) {
		use frame::traits::fungible::Mutate as FungibleMutate;
		// Native ED first: without it withdraw / borrow / change_rate fail for
		// fresh accounts even when the subsequent asset mint exceeds ED.
		<Balances as FungibleMutate<AccountId>>::mint_into(who, 1).ok();
		match collateral_id {
			AssetId::Native => <Balances as FungibleMutate<AccountId>>::mint_into(who, amount)
				.expect("mint native collateral for benchmark account"),
			AssetId::WithId(asset_id) => {
				<Assets as frame::traits::fungibles::Mutate<AccountId>>::mint_into(
					asset_id, who, amount,
				)
				.expect("mint asset collateral for benchmark account")
			},
		};
	}

	fn mint_stable(stable_id: StableId, who: &AccountId, amount: Balance) {
		<Assets as frame::traits::fungibles::Mutate<AccountId>>::mint_into(stable_id, who, amount)
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

pub fn rate_list(collateral: AssetId) -> VaultList {
	pallet_vaults::VaultListId::Rate(collateral, PUSD)
}

pub fn final_recovery_list(collateral: AssetId) -> VaultList {
	pallet_vaults::VaultListId::FinalRecovery(collateral, PUSD)
}

/// Rate index list id for an explicit market.
pub fn rate_list_for(collateral: AssetId, stable: StableId) -> VaultList {
	pallet_vaults::VaultListId::Rate(collateral, stable)
}

/// DOT-equivalent native collateral asset id used across tests.
pub const DOT: AssetId = AssetId::Native;

/// A non-native test collateral that lives in `pallet-assets`. Used in tests
/// that exercise the multi-asset side of the union.
pub const TOKEN_X_ID: AssetIdForAssets = 1;
pub const TOKEN_X: AssetId = AssetId::WithId(TOKEN_X_ID);

/// A second issued collateral, used by the multi-market tests as "ETH".
pub const ETH_ID: AssetIdForAssets = 2;
pub const ETH: AssetId = AssetId::WithId(ETH_ID);

/// Third and fourth issued collaterals. Only the registry-capacity test needs
/// them: with two stablecoins, four collaterals make exactly `MaxBranches`
/// valid markets, leaving `COLL_D` spare to probe the full-registry rejection.
pub const COLL_C_ID: AssetIdForAssets = 3;
pub const COLL_C: AssetId = AssetId::WithId(COLL_C_ID);
pub const COLL_D_ID: AssetIdForAssets = 4;
pub const COLL_D: AssetId = AssetId::WithId(COLL_D_ID);

/// Default stablecoin ("dotUSD") every helper mints against.
pub const PUSD: StableId = 1_000;
/// Second stablecoin ("ethUSD") on a different peg.
pub const EUSD: StableId = 1_001;

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
			],
			metadata: vec![],
			accounts: vec![],
			next_asset_id: None,
			reserves: vec![],
		},
		system: Default::default(),
		balances: pallet_balances::GenesisConfig {
			balances: (1u64..=10u64).map(|i| (i, 1_000_000_000_000)).collect(),
			..Default::default()
		},
	}
	.build_storage()
	.unwrap();
	let mut ext: TestState = t.into();
	ext.execute_with(|| {
		System::set_block_number(1);
		Timestamp::set_timestamp(1_000);
		// Mint the issued collaterals to test accounts. Native DOT was already
		// minted via the balances genesis above.
		for who in 1u64..=10 {
			for asset in [TOKEN_X_ID, ETH_ID, COLL_C_ID, COLL_D_ID] {
				<Assets as frame::traits::fungibles::Mutate<AccountId>>::mint_into(
					asset,
					&who,
					1_000_000_000_000,
				)
				.expect("mint issued collateral in test setup");
			}
		}
		MockPrices::set(alloc::collections::BTreeMap::new());
		MockOracleAvailable::set(true);
		LifecycleLog::set(alloc::vec::Vec::new());
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

/// Default branch config: MCR=110%, ICR=120%, Safety=130%, ceiling 100M,
/// MinDebt=200, MinColl=1, rate bounds 0.1%-400%, 7d upfront fee,
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
		maximum_borrow_rate: FixedU128::from_rational(400u128, 100u128),
		upfront_fee_period: 7 * 24 * 3_600 * 1_000,
		rate_adjustment_cooldown: 24 * 3_600 * 1_000,
		redistribution_penalty: Permill::from_percent(5),
		ceiling_gap: 0,
		ceiling_ttl: 0,
	}
}

/// Register the default `(DOT, PUSD)` market and price it at 10.
pub fn register_default_branch() {
	register_market(DOT, PUSD);
}

/// Register a `(collateral, PUSD)` market and price it at 10.
pub fn register_branch_for(collateral: AssetId) {
	register_market(collateral, PUSD);
}

/// Default per-collateral global debt ceiling for test markets — high enough
/// that the global cap never binds unless a test sets a lower one.
pub const GLOBAL_CEILING: Balance = 1_000_000_000_000_000;

/// Register an explicit `(collateral, stable)` market, price it at 10, and grant
/// the collateral a high global debt ceiling so borrowing is enabled.
/// Register a market with the autoline enabled (`gap`/`ttl`) and a high global
/// ceiling. `line_max` is the static `debt_ceiling` cap.
pub fn register_autoline_market(
	collateral: AssetId,
	stable: StableId,
	line_max: Balance,
	gap: Balance,
	ttl: Moment,
) {
	let config = BranchConfig {
		debt_ceiling: line_max,
		ceiling_gap: gap,
		ceiling_ttl: ttl,
		..default_branch_config()
	};
	set_price(collateral.clone(), FixedU128::from_rational(10u128, 1u128));
	pallet_vaults::Pallet::<Test>::create_branch(
		RuntimeOrigin::root(),
		collateral.clone(),
		stable,
		admin_caller(ADMIN),
		admin_caller(EMERGENCY_ADMIN),
		config,
	)
	.expect("create_branch ok");
	pallet_vaults::Pallet::<Test>::set_global_debt_ceiling(
		RuntimeOrigin::root(),
		collateral,
		GLOBAL_CEILING,
	)
	.expect("set global debt ceiling");
}

pub fn register_market(collateral: AssetId, stable: StableId) {
	// `create_branch` requires a price, so set it before creating.
	set_price(collateral.clone(), FixedU128::from_rational(10u128, 1u128));
	pallet_vaults::Pallet::<Test>::create_branch(
		RuntimeOrigin::root(),
		collateral.clone(),
		stable,
		admin_caller(ADMIN),
		admin_caller(EMERGENCY_ADMIN),
		default_branch_config(),
	)
	.expect("create_branch ok");
	pallet_vaults::Pallet::<Test>::set_global_debt_ceiling(
		RuntimeOrigin::root(),
		collateral,
		GLOBAL_CEILING,
	)
	.expect("set global debt ceiling");
}

/// Advance `pallet_timestamp` by `ms` milliseconds without touching block #.
/// Use this for interest-accrual tests where only wall-clock matters.
pub fn advance_time(ms: Moment) {
	let now = pallet_timestamp::Pallet::<Test>::get();
	Timestamp::set_timestamp(now + ms);
}

/// Advance both block number and timestamp by `n` blocks of `ms_per_block`.
/// Use this for `on_idle`-sensitive tests.
pub fn advance_blocks(n: u64, ms_per_block: Moment) {
	for _ in 0..n {
		let block = System::block_number();
		System::set_block_number(block + 1);
		advance_time(ms_per_block);
	}
}

/// Open a vault on the default `PUSD` market with `(None, None)` hints.
pub fn open(
	who: AccountId,
	collateral: AssetId,
	collateral_amount: Balance,
	debt: Balance,
	rate: FixedU128,
) -> DispatchResult {
	open_market(who, collateral, PUSD, collateral_amount, debt, rate)
}

/// Open a vault on an explicit `(collateral, stable)` market.
pub fn open_market(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	collateral_amount: Balance,
	debt: Balance,
	rate: FixedU128,
) -> DispatchResult {
	pallet_vaults::Pallet::<Test>::open_vault(
		RuntimeOrigin::signed(who),
		collateral,
		stable,
		collateral_amount,
		debt,
		rate,
		Position::endpoints_only(),
	)
}

/// Drive a liquidation on the default `PUSD` market through the trait surface.
pub fn liquidate(collateral: AssetId, owner: AccountId) -> DispatchResult {
	liquidate_with(collateral, owner, |post_touch| LiquidationAllocation {
		offset: OffsetAllocation { recipient: owner, debt: post_touch, collateral: 0 },
		redistribution_collateral: 0,
		keeper: KeeperCompensation { recipient: owner, collateral: 0 },
	})
}

/// Same as `liquidate` but the caller supplies the post-touch debt allocation.
pub fn liquidate_with(
	collateral: AssetId,
	owner: AccountId,
	build: impl FnOnce(Balance) -> LiquidationAllocation<AccountId, Balance>,
) -> DispatchResult {
	liquidate_market_with(collateral, PUSD, owner, build)
}

/// Liquidate on an explicit market with a caller-supplied allocation.
pub fn liquidate_market_with(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
	build: impl FnOnce(Balance) -> LiquidationAllocation<AccountId, Balance>,
) -> DispatchResult {
	<Pallet<Test> as VaultLiquidationInterface<AssetId, StableId, AccountId, Balance>>::execute_liquidation(
		&collateral,
		&stable,
		&owner,
		|snapshot| Ok(build(snapshot.debt)),
	)
}

/// Drive a redemption on the default `PUSD` market through the trait surface.
/// Returns the owner that was redeemed against.
pub fn redeem(
	collateral: AssetId,
	redeemer: AccountId,
	amount: Balance,
) -> Result<AccountId, DispatchError> {
	redeem_market(collateral, PUSD, redeemer, amount)
}

/// Redeem on an explicit `(collateral, stable)` market.
pub fn redeem_market(
	collateral: AssetId,
	stable: StableId,
	redeemer: AccountId,
	amount: Balance,
) -> Result<AccountId, DispatchError> {
	let target = <Pallet<Test> as VaultRedemptionInterface<
		AssetId,
		StableId,
		AccountId,
		Balance,
	>>::next_redemption_target(&collateral, &stable, None)
	.ok_or(DispatchError::Other("no redemption target"))?;
	let owner = target.owner;
	let price = MockPrices::get().get(&collateral).copied().expect("price set");
	redeem_step(&collateral, &stable, &owner, |snapshot| {
		let debt_to_cancel = core::cmp::min(amount, snapshot.debt);
		let collateral_to_redeemer =
			(FixedU128::saturating_from_integer(debt_to_cancel) / price).saturating_mul_int(1u128);
		Ok(Some(RedemptionAllocation {
			redeemer,
			debt_to_cancel,
			collateral_to_redeemer,
			fee_collateral_retained: 0,
		}))
	})?;
	Ok(owner)
}

/// One redemption step against an explicit vault, through the trait surface.
pub fn redeem_step(
	collateral: &AssetId,
	stable: &StableId,
	owner: &AccountId,
	build_allocation: impl FnOnce(
		pusd_primitives::RedemptionStepSnapshot<Balance>,
	) -> Result<
		Option<RedemptionAllocation<AccountId, Balance>>,
		DispatchError,
	>,
) -> Result<Option<RedemptionAllocation<AccountId, Balance>>, DispatchError> {
	<Pallet<Test> as VaultRedemptionInterface<AssetId, StableId, AccountId, Balance>>::redeem_step(
		collateral,
		stable,
		owner,
		build_allocation,
	)
}

/// Held collateral on `(collateral, who)` for the `VaultCollateral` reason.
/// Aggregated across every market the owner backs with this collateral.
pub fn held(collateral: AssetId, who: AccountId) -> Balance {
	<VaultCollateralAssets as InspectHold<AccountId>>::balance_on_hold(
		collateral,
		&HoldReason::VaultCollateral.into(),
		&who,
	)
}

/// Total balance of `(collateral, who)` on the collateral surface (includes any hold).
pub fn collateral_balance(collateral: AssetId, who: AccountId) -> Balance {
	<VaultCollateralAssets as FungiblesInspect<AccountId>>::balance(collateral, &who)
}

/// Default-stablecoin (`PUSD`) balance of `who`.
pub fn pusd_balance(who: AccountId) -> Balance {
	stable_balance(PUSD, who)
}

/// pUSD routed to [`FEE_DEST`] by the `FeeHandler` — the cumulative protocol
/// fee (upfront-fee and branch-interest residual) in the default coin.
pub fn fee_dest_balance() -> Balance {
	stable_balance(PUSD, FEE_DEST)
}

/// Balance of an explicit stablecoin for `who`.
pub fn stable_balance(stable: StableId, who: AccountId) -> Balance {
	<VaultStableAssets as FungiblesInspect<AccountId>>::balance(stable, &who)
}

/// Native creation deposit currently held against `who`.
pub fn creation_deposit_held(who: AccountId) -> Balance {
	use frame::traits::fungible::InspectHold;
	<Balances as InspectHold<AccountId>>::balance_on_hold(
		&RuntimeHoldReason::Vaults(pallet_vaults::HoldReason::MarketCreationDeposit),
		&who,
	)
}
