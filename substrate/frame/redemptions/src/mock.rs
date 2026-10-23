//! Test runtime for `pallet-redemptions`.
//! Native collateral ([`DOT`]) lives in `Balances`, issued collateral ([`TOKEN_X`]) in
//! `pallet-assets` behind `AssetsHolder`. Stablecoins are plain `pallet-assets` ids: [`STABLE`] is
//! the unit-scale coin most tests use, [`USDX`] a 6-decimals coin for scale tests.
//!
//! The `test-utils` feature exposes this runtime to other crates.

use alloc::collections::BTreeMap;

use crate as pallet_redemptions;
use crate::types::RedemptionConfig;
pub use frame::{
	arithmetic::{FixedPointNumber, FixedU128, One, Permill, Saturating, Zero},
	prelude::{DispatchError, DispatchResult},
	testing_prelude::{assert_noop, assert_ok, assert_storage_noop, hypothetically, BadOrigin},
};
use frame::{
	deps::sp_runtime::traits::{ConvertInto, ConvertToValue},
	testing_prelude::*,
	traits::{
		fungible::{NativeFromLeft, NativeOrWithId},
		fungibles::{Inspect as FungiblesInspect, InspectHold, Mutate as FungiblesMutate},
		tokens::{fungible, imbalance::ResolveAssetTo},
		AsEnsureOriginWithArg, IdentityLookup,
	},
};
use pallet_linked_list::Position;
use pallet_vaults::{CollateralCreditOf, StableCreditOf};
use stablecoin_primitives::{
	BranchSnapshot, OffsetLegs, ProvidePrice, StabilityPoolInspect, StabilityPoolOffset,
	VaultInterface,
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
	pub type Redemptions = pallet_redemptions;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountId = AccountId;
	type AccountData = pallet_balances::AccountData<Balance>;
	// The preset's lookup is pinned to its own `u64` account id.
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
	pub const VaultsPalletId: PalletId = PalletId(*b"py/vault");
}

impl pallet_linked_list::Config for Test {
	type WeightInfo = ();
	type ListId = VaultList;
	type ItemId = AccountId;
	type Priority = FixedU128;
	type MaxHintRepairSteps = ConstU32<16>;
	type PriorityProvider = pallet_vaults::Pallet<Test>;
}

/// Unified collateral surface: `Balances` (native) on the left, `AssetsHolder`
/// (multi-asset, hold-aware) on the right.
pub type VaultCollateralAssets =
	fungible::UnionOf<Balances, AssetsHolder, NativeFromLeft, AssetId, AccountId>;

/// Oracle driven by [`set_price`] and [`clear_price`]. It prices collateral only: every
/// stablecoin is at par.
pub struct MockOracle;
parameter_types! {
	/// In storage so that every externality, and so every benchmark, starts unpriced.
	pub storage MockPrices: BTreeMap<AssetId, FixedU128> = BTreeMap::new();
}
impl ProvidePrice for MockOracle {
	type AssetId = AssetId;

	fn provide_price(collateral_id: &AssetId) -> Result<FixedU128, DispatchError> {
		// Production adapters report a missing price as `Unavailable`. The pallet propagates the
		// oracle's error, so a pallet error here would let tests assert what no runtime returns.
		MockPrices::get().get(collateral_id).copied().ok_or(DispatchError::Unavailable)
	}
}

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

/// Genesis owner of every test stablecoin, so [`EnsureAssetOwner`] admits it. It administers no
/// branch.
pub const STABLECOIN_OWNER: AccountId = 1;

/// Full admin of every branch a test helper registers.
pub const ADMIN: AccountId = 100;
/// Emergency (tighten-only) admin of every branch a test helper registers.
pub const EMERGENCY_ADMIN: AccountId = 101;

/// The `create_branch` admin bundle: `full` administers, `emergency` tightens.
pub fn branch_admins(
	full: AccountId,
	emergency: AccountId,
) -> pallet_vaults::types::BranchAdmins<AccountId> {
	pallet_vaults::types::BranchAdmins { full_admin: full, emergency_admin: emergency }
}

/// Root acts deposit-free (`None`), the stablecoin's owner with a deposit (`Some(who)`), and
/// anyone else is rejected.
pub type EnsureAssetOwner = stablecoin_primitives::EnsureStableOwnerOrRoot<Assets, AccountId>;

parameter_types! {
	/// Governance bounds that [`default_branch_config`] sits inside.
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
	type FeeAccount = ConvertToValue<VaultFeeDestAccount>;
	type YieldHook = ();
	// Registering a branch seeds this pallet's redemption config via `on_registered`.
	type OnBranchLifecycle = Redemptions;
	type StabilityPool = BenchmarkSeedablePool;
	type TimeProvider = Timestamp;
	type CreateOrigin = EnsureAssetOwner;
	// Branch and vault deposits are the vaults suite's concern; these suites assert raw balances.
	type BranchConsideration = ();
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
impl pallet_vaults::BenchmarkHelper<AssetId, StableId, AccountId> for VaultsBenchHelper {
	fn collateral_asset_id() -> AssetId {
		DOT
	}

	fn set_oracle_price(collateral_id: AssetId, price: FixedU128) {
		set_price(collateral_id, price);
	}

	fn advance_time(ms: u64) {
		advance_time(ms);
	}

	// The benchmark collateral is native, which exists without being created.
	fn ensure_collateral_asset(collateral_id: AssetId) {
		assert_eq!(collateral_id, DOT);
	}

	// `StoredCeiling` reads only the ceiling row the benchmarks write themselves.
	fn stage_debt_ceiling(_: StableId) {}

	fn fund_native(who: &AccountId) {
		<Balances as fungible::Mutate<AccountId>>::mint_into(who, 1_000_000_000_000)
			.expect("minting native to a benchmark account cannot overflow the issuance; qed");
	}
}

/// No Stability Pool: nothing absorbs a liquidation, so it redistributes, exactly as with `()`.
///
/// The benchmarks still queue pool cohorts, which a real pool needs for its yield route; this
/// pool accepts them as deadlines only, so the same staging runs here and on a real runtime.
pub struct BenchmarkSeedablePool;

impl StabilityPoolInspect<AssetId, StableId, Balance> for BenchmarkSeedablePool {
	type Quote = ();

	fn quote(_: &AssetId, _: &StableId, _: BranchSnapshot) -> Option<()> {
		None
	}

	fn quote_active(_: &(), _: Balance) -> Balance {
		0
	}

	fn quote_pending(_: &(), _: Balance, _: Balance) -> Balance {
		0
	}
}

impl StabilityPoolOffset<AssetId, StableId, Balance, CollateralCreditOf<Test>, StableCreditOf<Test>>
	for BenchmarkSeedablePool
{
	// Forwarded so that `()`'s defensive check on a nonzero leg still runs.
	fn offset(
		collateral_id: &AssetId,
		stable_id: &StableId,
		branch: BranchSnapshot,
		debt: OffsetLegs<Balance>,
		collateral: OffsetLegs<CollateralCreditOf<Test>>,
	) -> Result<StableCreditOf<Test>, DispatchError> {
		<() as StabilityPoolOffset<_, _, _, _, _>>::offset(
			collateral_id,
			stable_id,
			branch,
			debt,
			collateral,
		)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_queue_deposit(
		_: &AssetId,
		_: &StableId,
		_: u32,
		entry_delay: u64,
		_: Balance,
	) -> Result<u64, DispatchError> {
		assert!(entry_delay > 0);
		Ok(Timestamp::get() + entry_delay)
	}
}

/// Signs, and pays for, the redemptions a test runs.
pub const REDEEMER: AccountId = 3;
/// Receives the collateral of [`REDEEMER`]'s redemptions.
pub const RECIPIENT: AccountId = 4;
/// Signs a redemption that competes with, or differs from, [`REDEEMER`]'s.
pub const OTHER_REDEEMER: AccountId = 5;
/// Receives the collateral of [`OTHER_REDEEMER`]'s redemptions.
pub const OTHER_RECIPIENT: AccountId = 6;

/// Receives redemption fees, apart from [`VAULT_FEE_DEST`], so tests can read them exactly.
pub const FEE_DEST: AccountId = 888;

/// Insurance Fund account of `stable`: one per stablecoin, as a runtime would derive them.
pub fn insurance_account(stable: StableId) -> AccountId {
	700_000 + AccountId::from(stable)
}

pub struct InsuranceFundAccounts;
impl Convert<StableId, AccountId> for InsuranceFundAccounts {
	fn convert(stable: StableId) -> AccountId {
		insurance_account(stable)
	}
}

/// Receives the vault fees of every test stablecoin.
pub const VAULT_FEE_DEST: AccountId = 887;

parameter_types! {
	pub const FeeDestAccount: AccountId = FEE_DEST;
	pub const VaultFeeDestAccount: AccountId = VAULT_FEE_DEST;
}

/// The redemption config that [`register_branch`] seeds on a stablecoin's first branch.
pub fn default_redemption_config() -> RedemptionConfig<Balance> {
	RedemptionConfig {
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
	// The redemption config spans every branch on a coin, so the coin's owner or Root sets it,
	// not a branch admin.
	type UpdateOrigin = EnsureAssetOwner;
	type MaxRedemptionSteps = ConstU32<20>;
	type WeightInfo = ();
}

/// Native collateral.
pub const DOT: AssetId = AssetId::Native;

/// Collateral issued by `pallet-assets`.
pub const TOKEN_X_ID: AssetIdForAssets = 1;
pub const TOKEN_X: AssetId = AssetId::WithId(TOKEN_X_ID);

/// Unit-scale stablecoin.
pub const STABLE: StableId = 1_000;

/// A 6-decimals stablecoin with a realistic 0.01-coin minimum balance.
pub const USDX: StableId = 6_000;
/// Raw units in one [`USDX`].
pub const USDX_UNIT: Balance = 1_000_000;
/// The [`USDX`] minimum balance: 0.01 coin.
pub const USDX_MIN_BALANCE: Balance = USDX_UNIT / 100;

/// Stablecoin-wide debt ceiling that [`register_branch`] sets, high enough never to bind.
pub const GLOBAL_CEILING: Balance = 1_000_000_000_000_000;

/// The terms of a vault an [`ExtBuilder`] opens in the branch added before it.
struct VaultSetup {
	owner: AccountId,
	collateral_amount: Balance,
	debt: Balance,
	rate: FixedU128,
}

/// One step of the initial state an [`ExtBuilder`] lays down, in the order it was added.
///
/// Order is part of the state: a vault opens in the branch added before it, and a price holds
/// until the next step that moves it.
enum SetupStep {
	/// Registers a branch through [`register_branch`].
	Branch { collateral: AssetId, stable: StableId, config: pallet_vaults::BranchConfig<Balance> },
	/// Sets the oracle price of a collateral.
	Price { collateral: AssetId, price: FixedU128 },
	/// Opens an ordinary vault.
	Vault(VaultSetup),
	/// Opens a vault at [`DEFAULT_PRICE`], then parks it in `FinalRecovery` at `price`.
	FinalRecoveryVault { vault: VaultSetup, price: FixedU128 },
}

/// Builds the test externalities: genesis first, then the initial state each builder method
/// adds, applied in order through the real dispatchables.
///
/// The events the setup emits are cleared, so a test observes only its own.
pub struct ExtBuilder {
	/// `pallet-assets` balances that genesis mints: `(asset, who, amount)`.
	asset_balances: Vec<(AssetIdForAssets, AccountId, Balance)>,
	steps: Vec<SetupStep>,
}

impl Default for ExtBuilder {
	fn default() -> Self {
		Self {
			// A Root-created branch takes its custody seed from its full admin, so a TOKEN_X
			// branch needs [`ADMIN`] to hold some.
			asset_balances: vec![(TOKEN_X_ID, ADMIN, 1_000_000_000_000)],
			steps: Vec::new(),
		}
	}
}

impl ExtBuilder {
	/// Registers the `(DOT, STABLE)` branch with [`default_branch_config`].
	pub fn default_branch(self) -> Self {
		self.branch(DOT, STABLE, default_branch_config())
	}

	/// Registers the `(collateral, stable)` branch with `config`, as [`register_branch`] does.
	pub fn branch(
		mut self,
		collateral: AssetId,
		stable: StableId,
		config: pallet_vaults::BranchConfig<Balance>,
	) -> Self {
		self.steps.push(SetupStep::Branch { collateral, stable, config });
		self
	}

	/// Sets the oracle price of `collateral` for every step after this one.
	pub fn price(mut self, collateral: AssetId, price: FixedU128) -> Self {
		self.steps.push(SetupStep::Price { collateral, price });
		self
	}

	/// Opens `owner`'s vault in the most recently added branch.
	pub fn vault(
		mut self,
		owner: AccountId,
		collateral_amount: Balance,
		debt: Balance,
		rate: FixedU128,
	) -> Self {
		let vault = VaultSetup { owner, collateral_amount, debt, rate };
		self.steps.push(SetupStep::Vault(vault));
		self
	}

	/// Opens `owner`'s vault in the most recently added branch at [`DEFAULT_PRICE`] and a 5%
	/// rate, then parks it in `FinalRecovery` at `price`, which holds after the step.
	///
	/// The vault must be its branch's last eligible vault and below the MCR at `price`.
	pub fn final_recovery_vault(
		mut self,
		owner: AccountId,
		collateral_amount: Balance,
		debt: Balance,
		price: FixedU128,
	) -> Self {
		let rate = FixedU128::from_rational(5, 100);
		let vault = VaultSetup { owner, collateral_amount, debt, rate };
		self.steps.push(SetupStep::FinalRecoveryVault { vault, price });
		self
	}

	/// Mints `amount` of `stable` to `who` at genesis.
	pub fn stable_balance(mut self, stable: StableId, who: AccountId, amount: Balance) -> Self {
		self.asset_balances.push((stable, who, amount));
		self
	}

	/// Mints `amount` of the [`TOKEN_X`] collateral to `who` at genesis.
	pub fn token_x_balance(mut self, who: AccountId, amount: Balance) -> Self {
		self.asset_balances.push((TOKEN_X_ID, who, amount));
		self
	}

	/// Builds the genesis, then applies every setup step in order.
	pub fn build(self) -> TestState {
		let mut ext = genesis_ext(self.asset_balances);
		ext.execute_with(|| {
			System::set_block_number(1);
			Timestamp::set_timestamp(1_000);
			let mut branch = None;
			for step in self.steps {
				apply_setup_step(step, &mut branch);
			}
			System::reset_events();
		});
		ext
	}

	/// Builds, runs `test`, then checks every invariant against the state it left behind.
	pub fn build_and_execute(self, test: impl FnOnce()) {
		self.build().execute_with(|| {
			test();
			// Every test here drives the real Vaults, LinkedList and Assets pallets, whose
			// invariants their own suites cannot see; they are reachable from outside their
			// crates only through the try-runtime hooks, which also run this pallet's checks.
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
}

fn genesis_ext(asset_balances: Vec<(AssetIdForAssets, AccountId, Balance)>) -> TestState {
	RuntimeGenesisConfig {
		assets: pallet_assets::GenesisConfig {
			assets: vec![
				(TOKEN_X_ID, 1, true, 1),
				(STABLE, STABLECOIN_OWNER, true, 1),
				(USDX, STABLECOIN_OWNER, true, USDX_MIN_BALANCE),
			],
			metadata: vec![(USDX, b"USDX".to_vec(), b"USDX".to_vec(), 6)],
			accounts: asset_balances,
			next_asset_id: None,
			reserves: vec![],
		},
		system: Default::default(),
		balances: pallet_balances::GenesisConfig {
			// The fee and insurance accounts pay native asset-account deposits, and a
			// Root-created branch takes its custody seed from its full admin.
			balances: (1..=10)
				.chain([insurance_account(STABLE), FEE_DEST, VAULT_FEE_DEST, ADMIN])
				.map(|i| (i, 1_000_000_000_000))
				.collect(),
			..Default::default()
		},
	}
	.build_storage()
	.expect("the test genesis config is valid; qed")
	.into()
}

/// Applies one setup step; `branch` is the most recently registered branch, which the vault
/// steps open in.
fn apply_setup_step(step: SetupStep, branch: &mut Option<(AssetId, StableId)>) {
	match step {
		SetupStep::Branch { collateral, stable, config } => {
			register_branch(collateral.clone(), stable, config);
			*branch = Some((collateral, stable));
		},
		SetupStep::Price { collateral, price } => set_price(collateral, price),
		SetupStep::Vault(vault) => {
			let (collateral, stable) =
				branch.clone().expect("a branch is added before a vault opens in it");
			open_setup_vault(&vault, collateral, stable);
		},
		SetupStep::FinalRecoveryVault { vault, price } => {
			let (collateral, stable) =
				branch.clone().expect("a branch is added before a vault opens in it");
			// An earlier `FinalRecovery` vault may have left the price depressed.
			set_price(collateral.clone(), DEFAULT_PRICE);
			open_setup_vault(&vault, collateral.clone(), stable);
			set_price(collateral.clone(), price);
			enter_final_recovery(collateral.clone(), stable, vault.owner)
				.expect("the vault enters recovery");
			let (_, status) = Vaults::vault_after_touch(collateral, stable, vault.owner)
				.expect("the parked vault exists");
			assert!(status.is_final_recovery(), "the vault is parked in FinalRecovery");
		},
	}
}

fn open_setup_vault(vault: &VaultSetup, collateral: AssetId, stable: StableId) {
	open(vault.owner, collateral, stable, vault.collateral_amount, vault.debt, vault.rate)
		.expect("the vault opens");
}

/// The branch config that most tests register.
pub fn default_branch_config() -> pallet_vaults::BranchConfig<Balance> {
	pallet_vaults::BranchConfig {
		minimum_collateralization_ratio: FixedU128::from_rational(110u128, 100u128),
		initial_collateralization_ratio: FixedU128::from_rational(120u128, 100u128),
		safety_collateralization_ratio: FixedU128::from_rational(130u128, 100u128),
		debt_ceiling: 1_000_000_000_000,
		minimum_debt: 200,
		minimum_collateral: 1,
		minimum_borrow_rate: FixedU128::from_rational(1u128, 1_000u128),
		maximum_borrow_rate: FixedU128::from_rational(100u128, 100u128),
		upfront_fee_period: 7 * 24 * 3_600 * 1_000,
		rate_adjustment_cooldown: 24 * 3_600 * 1_000,
		final_recovery_reward_cooldown: 3_600 * 1_000,
		liquidation: pallet_vaults::LiquidationConfig {
			offset_penalty: Permill::from_percent(5),
			// Keeper compensation is off. These tests price settlements of a collateral they
			// state up front, and final-recovery entry would otherwise pay part of it to the
			// entering keeper. The vaults suite covers that schedule.
			keeper_flat_compensation_value: 0,
			keeper_percent_compensation: Permill::zero(),
			keeper_compensation_cap_value: 10_000,
			minimum_jit_contribution: 100,
			redistribution_penalty: Permill::from_percent(5),
		},
	}
}

/// [`default_branch_config`] with its debt amounts in whole [`USDX`]. The seeded redemption
/// config is not scaled: its minimum stays at 100 raw units.
pub fn usdx_branch_config() -> pallet_vaults::BranchConfig<Balance> {
	pallet_vaults::BranchConfig {
		debt_ceiling: 1_000_000_000 * USDX_UNIT,
		minimum_debt: 200 * USDX_UNIT,
		..default_branch_config()
	}
}

/// The collateral price that [`register_branch`] opens every branch at: 1 unit = 1.25 STABLE.
pub const DEFAULT_PRICE: FixedU128 = FixedU128::from_rational(5, 4);

/// Registers the `(collateral, stable)` branch at [`DEFAULT_PRICE`] under [`GLOBAL_CEILING`],
/// seeding [`default_redemption_config`] when it is the coin's first branch.
pub fn register_branch(
	collateral: AssetId,
	stable: StableId,
	config: pallet_vaults::BranchConfig<Balance>,
) {
	// `create_branch` requires a live price, so set it before creating.
	set_price(collateral.clone(), DEFAULT_PRICE);
	let redemption_config =
		(!crate::Stablecoins::<Test>::contains_key(stable)).then(default_redemption_config);
	Vaults::create_branch(
		RuntimeOrigin::root(),
		collateral.clone(),
		stable,
		branch_admins(ADMIN, EMERGENCY_ADMIN),
		config,
		redemption_config,
	)
	.expect("create_branch ok");
	Vaults::set_global_debt_ceiling(RuntimeOrigin::root(), stable, GLOBAL_CEILING)
		.expect("set global debt ceiling");
}

/// Opens `who`'s vault on the `(collateral, stable)` branch without rate-index hints.
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
	.map(|_| ())
	.map_err(|error| error.error)
}

/// The `max_steps` that defers to [`crate::Config::MaxRedemptionSteps`].
pub const MAX_STEPS_CONFIGURED: u32 = 0;

/// The parties, branch, and terms of one [`redeem`] call.
pub struct RedeemOptions {
	pub who: AccountId,
	pub collateral: AssetId,
	pub stable: StableId,
	pub max_stable_to_spend: Balance,
	pub min_collateral_out: Balance,
	pub recipient: AccountId,
	pub max_steps: u32,
}

impl RedeemOptions {
	/// [`REDEEMER`] spends up to `max_stable_to_spend` on `(DOT, STABLE)` and [`RECIPIENT`]
	/// receives the collateral, with no slippage floor and [`MAX_STEPS_CONFIGURED`].
	pub fn spend(max_stable_to_spend: Balance) -> Self {
		Self {
			who: REDEEMER,
			collateral: DOT,
			stable: STABLE,
			max_stable_to_spend,
			min_collateral_out: 0,
			recipient: RECIPIENT,
			max_steps: MAX_STEPS_CONFIGURED,
		}
	}
}

/// Dispatches `redeem` signed by `options.who` and returns the settlement its events report.
pub fn redeem(options: RedeemOptions) -> Result<Settlement, DispatchError> {
	let events_before = System::events().len();
	Redemptions::redeem(
		RuntimeOrigin::signed(options.who),
		options.collateral.clone(),
		options.stable,
		crate::RedemptionTerms {
			max_stable_to_spend: options.max_stable_to_spend,
			min_collateral_out: options.min_collateral_out,
		},
		options.recipient,
		options.max_steps,
	)
	.map_err(|failed| failed.error)?;
	Ok(redeem_settlement(&options, events_before))
}

/// One ordinary or recovery settlement, as its event reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settlement {
	pub stable_burned: Balance,
	pub insurance_cover: Balance,
	pub fee: Balance,
	pub collateral_out: Balance,
	pub kind: SettlementKind,
}

/// What the settlement event of a redemption reports beyond its amounts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementKind {
	/// An ordinary walk that visited `steps` vaults.
	Ordinary { steps: u32 },
	/// A `FinalRecovery` head settled under `regime`.
	Recovery { regime: crate::types::RecoveryRegime },
}

impl Settlement {
	/// The `(stable_burned, fee, collateral_out)` figures, for compact pinning.
	pub fn figures(&self) -> (Balance, Balance, Balance) {
		(self.stable_burned, self.fee, self.collateral_out)
	}
}

/// Reads the single settlement event of the [`redeem`] call that `options` describes, and checks
/// it against the per-step `VaultRedeemed` events: both pallets must agree on what was settled,
/// where, and for whom.
fn redeem_settlement(options: &RedeemOptions, events_before: usize) -> Settlement {
	let mut vault_debt_cancelled = 0;
	let mut vault_collateral_out = 0;
	let mut settlement = None;
	for record in System::events().into_iter().skip(events_before) {
		let (collateral_id, stable_id, redeemer, settled) = match record.event {
			RuntimeEvent::Vaults(pallet_vaults::Event::VaultRedeemed {
				collateral_id,
				stable_id,
				recipient,
				debt_cancelled,
				collateral_to_recipient,
				..
			}) => {
				assert_eq!(collateral_id, options.collateral);
				assert_eq!(stable_id, options.stable);
				assert_eq!(recipient, options.recipient, "the vaults pay the named recipient");
				vault_debt_cancelled += debt_cancelled.total();
				vault_collateral_out += collateral_to_recipient;
				continue;
			},
			RuntimeEvent::Redemptions(pallet_redemptions::Event::OrdinaryRedemptionExecuted {
				collateral_id,
				stable_id,
				redeemer,
				stable_burned,
				collateral_out,
				fee,
				steps,
			}) => (
				collateral_id,
				stable_id,
				redeemer,
				Settlement {
					stable_burned,
					insurance_cover: 0,
					fee,
					collateral_out,
					kind: SettlementKind::Ordinary { steps },
				},
			),
			RuntimeEvent::Redemptions(pallet_redemptions::Event::RecoveryRedemptionExecuted {
				collateral_id,
				stable_id,
				redeemer,
				stable_burned,
				insurance_cover,
				collateral_out,
				regime,
			}) => (
				collateral_id,
				stable_id,
				redeemer,
				Settlement {
					stable_burned,
					insurance_cover,
					fee: 0,
					collateral_out,
					kind: SettlementKind::Recovery { regime },
				},
			),
			_ => continue,
		};
		assert_eq!(collateral_id, options.collateral);
		assert_eq!(stable_id, options.stable);
		assert_eq!(redeemer, options.who);
		assert!(settlement.replace(settled).is_none(), "one redemption settles per call");
	}
	let settled = settlement.expect("a successful redemption settles");
	assert_eq!(
		vault_debt_cancelled,
		settled.stable_burned + settled.insurance_cover,
		"the redeemer and the insurance cover cancel the vaults' debt between them"
	);
	assert_eq!(
		vault_collateral_out, settled.collateral_out,
		"the event totals the collateral that the vaults paid"
	);
	settled
}

/// Parks `owner`'s vault in `FinalRecovery`. Any account may sign the call; the caller sets up
/// its preconditions: the branch's last eligible vault, below the MCR.
pub fn enter_final_recovery(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
) -> DispatchResult {
	Vaults::enter_final_recovery(RuntimeOrigin::signed(99), collateral, stable, owner)
		.map(|_post_info| ())
		.map_err(|error| error.error)
}

pub fn mint_stable(stable: StableId, who: AccountId, amount: Balance) {
	<Assets as FungiblesMutate<AccountId>>::mint_into(stable, &who, amount).expect("mint stable");
}

parameter_types! {
	/// How many of this pallet's events [`redemption_events_since_last_call`] has returned. It
	/// lives in storage so that `hypothetically!` rolls it back with the events it counts.
	storage ObservedRedemptionEvents: u32 = 0;
}

/// This pallet's events deposited since the previous call, in order.
pub fn redemption_events_since_last_call() -> Vec<crate::Event<Test>> {
	let events = System::read_events_for_pallet::<crate::Event<Test>>();
	let observed = usize::try_from(ObservedRedemptionEvents::get())
		.expect("a u32 count fits usize on every supported target; qed");
	assert!(observed <= events.len(), "events are only cleared by the setup");
	ObservedRedemptionEvents::set(
		&u32::try_from(events.len()).expect("a test deposits fewer than u32::MAX events; qed"),
	);
	events.into_iter().skip(observed).collect()
}

/// Advances the timestamp by `ms` without advancing the block number.
pub fn advance_time(ms: Moment) {
	Timestamp::set_timestamp(Timestamp::get() + ms);
}

/// Total `collateral` balance of `who`, held or free.
pub fn collateral_balance(collateral: AssetId, who: AccountId) -> Balance {
	<VaultCollateralAssets as FungiblesInspect<AccountId>>::balance(collateral, &who)
}

/// The `collateral` that `who`'s vaults hold on its account.
pub fn held(collateral: AssetId, who: AccountId) -> Balance {
	<VaultCollateralAssets as InspectHold<AccountId>>::balance_on_hold(
		collateral,
		&pallet_vaults::HoldReason::VaultCollateral.into(),
		&who,
	)
}

/// The vault's stored debt (principal + settled interest); zero when absent.
pub fn vault_debt(collateral: AssetId, stable: StableId, who: AccountId) -> Balance {
	pallet_vaults::Vaults::<Test>::get((collateral, stable, who))
		.map(|record| record.vault.debt.principal + record.vault.debt.interest)
		.unwrap_or_default()
}

/// Debt across every branch on `stable`: the denominator of the dynamic-fee increase.
pub fn stablecoin_debt(stable: StableId) -> Balance {
	pallet_vaults::Pallet::<Test>::stablecoin_debt(&stable)
}
