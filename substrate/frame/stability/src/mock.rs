//! The test runtime for `pallet-stability`.
//!
//! The mock runs the real vault stack, so a branch registered through `pallet-vaults` seeds the
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

/// Account identifier of the test runtime.
pub type AccountId = u128;
/// Balance type shared by every asset in the test runtime.
pub type Balance = u128;
/// Asset identifier inside `pallet-assets`.
pub type AssetIdForAssets = u32;
/// Collateral asset id: native DOT or an issued asset.
pub type AssetId = NativeOrWithId<AssetIdForAssets>;
/// Stablecoin asset id: a plain `pallet-assets` id.
pub type StableId = AssetIdForAssets;
/// Block type of the test runtime.
pub type Block = MockBlock<Test>;
/// Identifier of one branch's sorted vault list.
pub type VaultList = pallet_vaults::VaultListId<AssetId, StableId>;
/// Timestamp unit of the test clock, in milliseconds.
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
	/// The price [`MockOracle`] reports per collateral; a collateral with no entry has no price.
	pub static MockPrices: alloc::collections::BTreeMap<AssetId, FixedU128> =
		alloc::collections::BTreeMap::new();
	/// When `false`, [`MockOracle`] fails every query, modelling an oracle outage.
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

/// Sets the price [`MockOracle`] reports for `collateral`.
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

/// Full admin of every branch a test helper registers.
pub const ADMIN: AccountId = 100;
/// Emergency (tighten-only) admin of every branch a test helper registers.
pub const EMERGENCY_ADMIN: AccountId = 101;

/// The admin pair a branch registers with: `full` administers, `emergency` may only tighten.
pub fn branch_admins(
	full: AccountId,
	emergency: AccountId,
) -> pallet_vaults::types::BranchAdmins<AccountId> {
	pallet_vaults::types::BranchAdmins { full_admin: full, emergency_admin: emergency }
}

/// Who may register a branch: Root pays no deposit, the owner of the stablecoin pays one, and
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
	// Registration seeds the per-branch rows of both siblings. Redemptions comes first, so
	// the config that prices recovery offsets always exists once the pool rows do.
	type OnBranchLifecycle = (Redemptions, Stability);
	type StabilityPool = Stability;
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

/// Stages the vault benchmarks against this mock: native collateral, [`MockOracle`] prices and
/// the mock clock.
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
		use frame::traits::fungible::Mutate as FungibleMutate;
		<Balances as FungibleMutate<AccountId>>::mint_into(who, 1_000_000_000_000)
			.expect("minting native to a benchmark account cannot overflow the issuance; qed");
	}
}

/// Root as the governance override, or the stored full admin of the branch. A production runtime
/// composes the same pair.
///
/// The admin comes first so that its benchmark origin, which reads the branch, is the one the
/// benchmarks measure.
pub type StabilityUpdateOrigin = EitherOf<
	pallet_vaults::EnsureBranchFullAdmin<Test>,
	AsEnsureOriginWithArg<frame_system::EnsureRoot<AccountId>>,
>;

/// Where redemption fees go. Recovery offsets charge no fee, so only ordinary redemptions pay
/// into it.
pub const FEE_DEST: AccountId = 888;

/// Where branch teardown sends what is left in a pool account. It stands in for a treasury, and
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

/// The redemption parameters the first branch of a stablecoin registers with.
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
/// [`TOKEN_X_ID`] as a collateral identifier.
pub const TOKEN_X: AssetId = AssetId::WithId(TOKEN_X_ID);

/// An issued collateral whose minimum balance of 1_000 sits far above every other asset's, for
/// the custody-float tests. [`create_high_min_collateral`] creates it.
pub const HIGH_MIN_ID: AssetIdForAssets = 77;
/// [`HIGH_MIN_ID`] as a collateral identifier.
pub const HIGH_MIN: AssetId = AssetId::WithId(HIGH_MIN_ID);

/// The price every branch is registered at.
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

/// One step of the initial state an [`ExtBuilder`] lays down, in the order it was added.
///
/// Order is part of the state: each deposit joins the cohort its timestamp selects, and a
/// matured deposit moves the clock for every step after it.
enum SetupStep {
	/// Creates the [`HIGH_MIN`] collateral asset.
	HighMinCollateral,
	/// Registers a branch through the vault engine.
	Branch(BranchSetup),
	/// Mints stablecoin [`STABLE`] to an account.
	StableBalance { who: AccountId, amount: Balance },
	/// Opens a vault in the default branch.
	Vault { owner: AccountId, collateral: Balance, debt: Balance },
	/// Deposits into the default pool behind the entry delay.
	Deposit { who: AccountId, amount: Balance },
	/// Deposits into the default pool and moves the clock to the deposit's deadline.
	MaturedDeposit { who: AccountId, amount: Balance },
}

/// A branch an [`ExtBuilder`] registers.
struct BranchSetup {
	collateral_id: AssetId,
	stable_id: StableId,
	/// `None` registers with [`branch_config_for`] the pair, which reads the asset minimums and
	/// so can only be resolved inside the externalities.
	branch_config: Option<pallet_vaults::BranchConfig<Balance>>,
	pool_config: StabilityPoolConfig<Balance>,
}

/// Builds the test externalities: genesis first, then the initial state each builder method
/// adds, applied in order through the real dispatchables.
///
/// The events the setup emits are cleared, so a test observes only its own.
#[derive(Default)]
pub struct ExtBuilder {
	steps: Vec<SetupStep>,
}

impl ExtBuilder {
	/// The single-depositor state: account 1 has 400 ready to activate in the default branch,
	/// and 600 in its wallet.
	pub fn single_depositor() -> Self {
		Self::default().default_branch().stable_balance(1, 600).matured_deposit(1, 400)
	}

	/// [`Self::single_depositor`] with real branch debt behind it: one vault at a TCR of 250%,
	/// which leaves the branch in Normal Mode and lets a test drive it into Safety Mode.
	pub fn branch_with_debt() -> Self {
		Self::default()
			.default_branch()
			.vault(5, 1_000, 500)
			.stable_balance(1, 600)
			.matured_deposit(1, 400)
	}

	/// Creates the [`HIGH_MIN`] collateral asset, which a branch on it needs first.
	pub fn high_min_collateral(mut self) -> Self {
		self.steps.push(SetupStep::HighMinCollateral);
		self
	}

	/// Registers the `collateral`/`stable` branch with [`branch_config_for`] the pair and
	/// [`default_pool_config`].
	pub fn branch(mut self, collateral_id: AssetId, stable_id: StableId) -> Self {
		self.steps.push(SetupStep::Branch(BranchSetup {
			collateral_id,
			stable_id,
			branch_config: None,
			pool_config: default_pool_config(),
		}));
		self
	}

	/// Registers the default [`DOT`]/[`STABLE`] branch.
	pub fn default_branch(self) -> Self {
		self.branch(DOT, STABLE)
	}

	/// Registers the most recently added branch with `config` instead.
	pub fn branch_config(mut self, config: pallet_vaults::BranchConfig<Balance>) -> Self {
		self.last_branch().branch_config = Some(config);
		self
	}

	/// Registers the pool of the most recently added branch with `config` instead.
	pub fn pool_config(mut self, config: StabilityPoolConfig<Balance>) -> Self {
		self.last_branch().pool_config = config;
		self
	}

	/// Registers the pool of the most recently added branch with the post-offset floor `min`.
	pub fn min_active_pool(self, min: Balance) -> Self {
		self.pool_config(StabilityPoolConfig {
			minimum_active_pool_balance: min,
			..default_pool_config()
		})
	}

	/// Mints `amount` of [`STABLE`] to `who`, on top of whatever its deposits mint.
	pub fn stable_balance(mut self, who: AccountId, amount: Balance) -> Self {
		self.steps.push(SetupStep::StableBalance { who, amount });
		self
	}

	/// Opens a vault of `owner` in the default branch, minting the collateral it locks.
	pub fn vault(mut self, owner: AccountId, collateral: Balance, debt: Balance) -> Self {
		self.steps.push(SetupStep::Vault { owner, collateral, debt });
		self
	}

	/// Mints `amount` for `who` and deposits it into the default pool, where it starts pending.
	pub fn deposit(mut self, who: AccountId, amount: Balance) -> Self {
		self.steps.push(SetupStep::Deposit { who, amount });
		self
	}

	/// [`Self::deposit`], then moves the clock to the deposit's cohort deadline.
	pub fn matured_deposit(mut self, who: AccountId, amount: Balance) -> Self {
		self.steps.push(SetupStep::MaturedDeposit { who, amount });
		self
	}

	/// Builds the genesis, then applies every setup step in order.
	pub fn build(self) -> TestState {
		let mut ext = genesis_ext();
		ext.execute_with(|| {
			for step in self.steps {
				apply_setup_step(step);
			}
			System::reset_events();
			ObservedStabilityEvents::set(0);
		});
		ext
	}

	/// Builds, runs `test`, then checks every invariant against the state it left behind.
	pub fn build_and_execute(self, test: impl FnOnce()) {
		self.build().execute_with(|| {
			test();
			// Every test here drives the real Vaults, Redemptions and LinkedList pallets, whose
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

	fn last_branch(&mut self) -> &mut BranchSetup {
		self.steps
			.iter_mut()
			.rev()
			.find_map(|step| match step {
				SetupStep::Branch(branch) => Some(branch),
				SetupStep::HighMinCollateral |
				SetupStep::StableBalance { .. } |
				SetupStep::Vault { .. } |
				SetupStep::Deposit { .. } |
				SetupStep::MaturedDeposit { .. } => None,
			})
			.expect("a branch is added before it is configured")
	}
}

fn apply_setup_step(step: SetupStep) {
	match step {
		SetupStep::HighMinCollateral => create_high_min_collateral(),
		SetupStep::Branch(branch) => {
			let config = branch.branch_config.unwrap_or_else(|| {
				branch_config_for(branch.collateral_id.clone(), branch.stable_id)
			});
			register_branch_with_pool(
				branch.collateral_id,
				branch.stable_id,
				config,
				branch.pool_config,
			);
		},
		SetupStep::StableBalance { who, amount } => mint_stable(STABLE, who, amount),
		SetupStep::Vault { owner, collateral, debt } => {
			mint_collateral(DOT, owner, collateral);
			assert_ok!(open_vault(owner, DOT, STABLE, collateral, debt));
		},
		SetupStep::Deposit { who, amount } => seed_deposit(who, amount),
		SetupStep::MaturedDeposit { who, amount } => seed_matured_deposit(who, amount),
	}
}

/// The genesis state, with the test assets created and the default accounts funded.
fn genesis_ext() -> TestState {
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
			// full admin pays the custody seed of every branch Root registers.
			balances: (1u128..=10u128)
				.chain([FEE_DEST, ADMIN])
				.map(|i| (i, 1_000_000_000_000))
				.collect(),
			..Default::default()
		},
	}
	.build_storage()
	.expect("the test genesis is well formed; qed");
	let mut ext: TestState = t.into();
	ext.execute_with(|| {
		System::set_block_number(1);
		Timestamp::set_timestamp(1_000);
		MockPrices::set(alloc::collections::BTreeMap::new());
		MockOracleAvailable::set(true);
	});
	ext
}

parameter_types! {
	/// How many pallet events [`stability_events_since_last_call`] has already returned.
	pub static ObservedStabilityEvents: usize = 0;
}

/// The events of this pallet recorded since the previous call, or since setup on the first.
pub fn stability_events_since_last_call() -> Vec<crate::Event<Test>> {
	let events: Vec<_> = System::events()
		.into_iter()
		.filter_map(|record| match record.event {
			RuntimeEvent::Stability(event) => Some(event),
			_ => None,
		})
		.collect();
	let already_seen = ObservedStabilityEvents::get();
	ObservedStabilityEvents::set(events.len());
	events.into_iter().skip(already_seen).collect()
}

/// The pool parameters every registered branch starts with: a 5 second entry delay and a
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

/// The branch parameters most tests use: liquidation at 110%, opening at 120%, Safety Mode
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
		final_recovery_reward_cooldown: 3_600 * 1_000,
		liquidation: pallet_vaults::LiquidationConfig {
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

/// The registration payload of a branch on `stable`.
///
/// Redemption policy is set per stablecoin, so only its first branch carries one. A pool is per
/// branch, so every branch carries one.
pub fn registration_config(
	stable: StableId,
) -> (Option<pallet_redemptions::types::RedemptionConfig<Balance>>, StabilityPoolConfig<Balance>) {
	let redemption_config = (!pallet_redemptions::Stablecoins::<Test>::contains_key(stable))
		.then(default_redemption_config);
	(redemption_config, default_pool_config())
}

/// Branch configuration for recovery settlement tests, with no entry compensation.
/// These tests price the collateral available to settle the head.
pub fn recovery_branch_config() -> pallet_vaults::BranchConfig<Balance> {
	let mut config = default_branch_config();
	config.liquidation.keeper_flat_compensation_value = 0;
	config.liquidation.keeper_percent_compensation = Permill::zero();
	config
}

/// Creates the [`HIGH_MIN`] collateral asset.
pub fn create_high_min_collateral() {
	assert_ok!(Assets::force_create(RuntimeOrigin::root(), HIGH_MIN_ID, 1, true, 1_000));
}

/// Keeper terms that make no just-in-time contribution.
pub const NO_JIT: pallet_vaults::JitTerms<Balance> =
	pallet_vaults::JitTerms { max_stable: 0, min_collateral_out: 0 };

/// Liquidates the vault of `owner` in the `collateral`/[`STABLE`] branch, signed by `keeper`, with
/// [`NO_JIT`] terms.
pub fn liquidate(keeper: AccountId, collateral: AssetId, owner: AccountId) -> DispatchResult {
	Vaults::liquidate(RuntimeOrigin::signed(keeper), collateral, STABLE, owner, NO_JIT)
}

/// Creates the `collateral`/[`STABLE`] branch as `origin`, with the default admins, branch
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

/// Registers a branch at [`REGISTRATION_PRICE`] and a debt ceiling high enough never to bind.
///
/// Registration also seeds the pool rows, through the lifecycle hook.
pub fn register_branch(
	collateral: AssetId,
	stable: StableId,
	config: pallet_vaults::BranchConfig<Balance>,
) {
	register_branch_with_pool(collateral, stable, config, default_pool_config());
}

/// [`register_branch`] with the pool registered under `pool_config`, for the parameters that
/// only registration can set.
pub fn register_branch_with_pool(
	collateral: AssetId,
	stable: StableId,
	config: pallet_vaults::BranchConfig<Balance>,
	pool_config: StabilityPoolConfig<Balance>,
) {
	// A branch cannot be registered without a live price.
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
		(registration_config(stable).0, pool_config),
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

/// Opens a vault, so that a test can create real branch debt and move the TCR.
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
	.map(|_| ())
	.map_err(|error| error.error)
}

/// The collateral `who` holds under the vault engine's collateral hold.
pub fn vault_collateral_on_hold(collateral: AssetId, who: AccountId) -> Balance {
	use frame::traits::fungibles::InspectHold;
	<VaultCollateralAssets as InspectHold<AccountId>>::balance_on_hold(
		collateral,
		&pallet_vaults::HoldReason::VaultCollateral.into(),
		&who,
	)
}

/// Mints `amount` of the stablecoin to `who`.
pub fn mint_stable(stable: StableId, who: AccountId, amount: Balance) {
	use frame::traits::fungibles::Mutate as FungiblesMutate;
	<Assets as FungiblesMutate<AccountId>>::mint_into(stable, &who, amount).expect("mint stable");
}

/// Mints `amount` of the collateral to `who`, from `Balances` or `pallet-assets` as it lives.
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

/// Claims the collateral of `who` in the default branch, paid to itself, and checks the payout.
pub fn assert_claim_collateral(who: AccountId, expected: Balance) {
	let before = collateral_balance(DOT, who);
	assert_ok!(claim_collateral(who, DOT, STABLE, who));
	assert_eq!(collateral_balance(DOT, who) - before, expected);
}

/// Claims the yield of `who` in the default branch, paid to itself, and checks the payout.
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

/// The active-leg sums row of the default branch at the given coordinates.
pub fn active_sums(epoch: u32, scale: u32) -> PoolSums {
	crate::PoolSumsStore::<Test>::get((DOT, STABLE, Leg::Active, epoch, scale))
		.expect("active sums row exists")
}

/// The pending-leg sums row of the default branch at the given coordinates.
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
		.map(|_| ())
		.map_err(|error| error.error)
}

/// Requests a withdrawal, signed by `who`.
pub fn request_withdraw(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	amount: Balance,
) -> DispatchResult {
	Stability::request_withdraw(RuntimeOrigin::signed(who), collateral, stable, amount)
		.map(|_| ())
		.map_err(|error| error.error)
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
		.map(|_| ())
		.map_err(|error| error.error)
}

/// Claims collateral to a named recipient, signed by `who`.
pub fn claim_collateral(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	recipient: AccountId,
) -> DispatchResult {
	Stability::claim_collateral(RuntimeOrigin::signed(who), collateral, stable, Some(recipient))
}

/// Claims yield to a named recipient, signed by `who`.
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

/// Compounds up to `amount` claimable yield, signed by `who`.
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
	mint_stable(STABLE, who, amount);
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
	mint_stable(STABLE, who, amount);
	assert_ok!(deposit_and_mature(who, DOT, STABLE, amount));
}

/// Gives an existing default-pool row backed claimable balances without exercising their source.
pub fn seed_claimables(who: AccountId, collateral_gain: Balance, yield_gain: Balance) {
	seed_claimables_in(DOT, who, collateral_gain, yield_gain);
}

/// [`seed_claimables`] in the `collateral`/[`STABLE`] branch.
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

/// The pallet event `$variant` of the default branch, as a `RuntimeEvent`. The fields name
/// everything but the branch.
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

/// Drops the price until the TCR reaches 120%, which is under the Safety threshold and over the
/// liquidation ratio. Needs the vault of [`ExtBuilder::branch_with_debt`].
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

/// Offers `amount` of yield to the default branch, which must take all of it.
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

/// The branch as the vault engine would hand it to the pool right now.
///
/// Production callers pass the snapshot they already hold; tests that call the pool interfaces
/// directly read it here, from the same vault engine and clock. A branch the engine does not
/// know counts as frozen, as the pool's own dispatchables treat it.
pub fn branch_snapshot(collateral: &AssetId, stable: &StableId) -> BranchSnapshot {
	let mode = Stability::branch_mode_or_frozen(collateral, stable);
	BranchSnapshot { mode, now: Timestamp::get() }
}

/// Quotes a branch under its current branch, as the liquidation engine reads it.
///
/// Every caller probes a registered, unfrozen branch, which always quotes.
pub fn market_quote(collateral: &AssetId, stable: &StableId) -> crate::OffsetQuote<Test> {
	Stability::quote(collateral, stable, branch_snapshot(collateral, stable))
		.expect("an unfrozen branch quotes")
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
) -> Result<crate::pallet::StableCreditOf<Test>, DispatchError> {
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
	assert_eq!(burned.peek(), active + pending, "the pool gives up exactly the debt it cancels");
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

/// The live state of a pool. Panics if the branch is not registered.
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
		.map(|_post_info| ())
		.map_err(|error| error.error)
}

/// The stored debt of a vault, principal and settled interest together. Zero if it is absent.
pub fn vault_debt(collateral: AssetId, stable: StableId, who: AccountId) -> Balance {
	pallet_vaults::Vaults::<Test>::get((collateral, stable, who))
		.map(|record| record.vault.debt.principal + record.vault.debt.interest)
		.unwrap_or_default()
}

/// Asserts that Vaults reported `debt` cancelled against `collateral` for the vault of `owner` in
/// the default branch, paid to its pool account at the rate [`open_vault`] sets.
///
/// The recovery events of this pallet carry no amounts, because the vault engine reports them.
pub fn assert_vault_redeemed(
	owner: AccountId,
	debt: pallet_vaults::DebtBreakdown<Balance>,
	collateral: Balance,
) {
	System::assert_has_event(RuntimeEvent::Vaults(pallet_vaults::Event::VaultRedeemed {
		collateral_id: DOT,
		stable_id: STABLE,
		owner,
		recipient: Stability::pool_account(&DOT, &STABLE),
		debt_cancelled: debt,
		collateral_to_recipient: collateral,
		vault_annual_rate: FixedU128::from_rational(5u128, 100u128),
	}));
}

/// Runs an active-pool recovery offset, signed by an arbitrary account.
pub fn offset_recovery(
	collateral: AssetId,
	stable: StableId,
	max_stable_in: Balance,
) -> DispatchResult {
	Stability::offset_recovery(RuntimeOrigin::signed(99), collateral, stable, max_stable_in)
}
