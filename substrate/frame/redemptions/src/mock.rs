//! Test runtime for `pallet-redemptions`.
//! Native collateral ([`DOT`]) lives in `Balances`, issued collateral ([`TOKEN_X`]) in
//! `pallet-assets` behind `AssetsHolder`. Stablecoins are plain `pallet-assets` ids: [`PUSD`] is
//! the unit-scale coin most tests use, [`USDX`] a 6-decimals coin for scale tests.
//!
//! The `test-utils` feature exposes this runtime to other crates.

use crate as pallet_redemptions;
use crate::types::RedemptionConfig;
pub use frame::{
	arithmetic::{FixedPointNumber, FixedU128, One, Permill, Saturating, Zero},
	prelude::{DispatchError, DispatchResult},
	testing_prelude::{assert_noop, assert_ok, assert_storage_noop, hypothetically, BadOrigin},
};
use frame::{
	deps::sp_runtime::traits::ConvertInto,
	testing_prelude::*,
	traits::{
		fungible::{HoldConsideration, NativeFromLeft, NativeOrWithId},
		fungibles::{Inspect as FungiblesInspect, InspectHold, Mutate as FungiblesMutate},
		tokens::{fungible, imbalance::ResolveAssetTo},
		AsEnsureOriginWithArg, IdentityLookup, LinearStoragePrice,
	},
};
use pallet_linked_list::Position;
use pusd_primitives::{ProvidePrice, VaultInterface};

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

	#[runtime::pallet_index(7)]
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

/// Unified collateral surface: `Balances` (native) on the left, `AssetsHolder`
/// (multi-asset, hold-aware) on the right.
pub type VaultCollateralAssets =
	fungible::UnionOf<Balances, AssetsHolder, NativeFromLeft, AssetId, AccountId>;

/// Oracle driven by [`set_price`] and [`clear_price`]. It prices collateral only: every
/// stablecoin is at par.
pub struct MockOracle;
parameter_types! {
	pub static MockPrices: alloc::collections::BTreeMap<AssetId, FixedU128> =
		alloc::collections::BTreeMap::new();
}
impl ProvidePrice for MockOracle {
	type AssetId = AssetId;

	fn provide_price(collateral_id: &AssetId) -> Result<FixedU128, DispatchError> {
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

/// Takes the oracle down for `collateral` until the next [`set_price`].
pub fn clear_price(collateral: AssetId) {
	MockPrices::mutate(|m| {
		m.remove(&collateral);
	});
}

parameter_types! {
	pub const MarketDepositReason: RuntimeHoldReason =
		RuntimeHoldReason::Vaults(pallet_vaults::HoldReason::BranchCreationDeposit);
	pub const MarketDepositBase: Balance = 1_000;
}

/// Genesis owner of every test stablecoin, so [`EnsureAssetOwner`] admits it. It administers no
/// market.
pub const STABLECOIN_OWNER: AccountId = 1;

/// Full admin of every market a test helper registers.
pub const ADMIN: AccountId = 100;
/// Emergency (tighten-only) admin of every market a test helper registers.
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
pub type EnsureAssetOwner = pusd_primitives::EnsureStableOwnerOrRoot<Assets, AccountId>;

/// Flat 1_000-unit refundable creation deposit, held in native balance.
pub type VaultsConsideration = HoldConsideration<
	AccountId,
	Balances,
	MarketDepositReason,
	LinearStoragePrice<MarketDepositBase, ConstU128<0>, Balance>,
>;

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
	type FeeAccount = FeeAccounts;
	type YieldHook = ();
	// Registering a market seeds this pallet's redemption config via `on_registered`.
	type OnBranchLifecycle = Redemptions;
	type StabilityPool = ();
	type TimeProvider = Timestamp;
	type CreateOrigin = EnsureAssetOwner;
	type BranchConsideration = VaultsConsideration;
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
		PUSD
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

pub struct FeeAccounts;
impl Convert<StableId, AccountId> for FeeAccounts {
	fn convert(_stable: StableId) -> AccountId {
		VAULT_FEE_DEST
	}
}

parameter_types! {
	pub const FeeDestAccount: AccountId = FEE_DEST;
}

/// The redemption config spans every market on a coin, so the coin's owner or Root sets it, not
/// a market admin.
pub type RedemptionsUpdateOrigin = EnsureAssetOwner;

/// The redemption config that [`register_branch`] seeds on a stablecoin's first market.
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
	type UpdateOrigin = RedemptionsUpdateOrigin;
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
		use frame::traits::fungible::Mutate as FungibleMutate;
		register_branch(DOT, PUSD, default_branch_config());

		let debt: Balance = 300;
		for i in 0..vaults {
			let who = 1_000 + u64::from(i);
			let _ = <Balances as FungibleMutate<AccountId>>::mint_into(&who, 10_000_000_000);
			let rate = FixedU128::from_rational(u128::from(i) + 1, 1_000u128);
			open(who, DOT, PUSD, 1_000_000, debt, rate).expect("open benchmark vault");
		}

		let redeemer: AccountId = 1;
		let budget = debt.saturating_mul(u128::from(vaults).saturating_add(2)).saturating_mul(2);
		mint_stable(PUSD, redeemer, budget.saturating_mul(2));
		(DOT, PUSD, redeemer, budget)
	}
}

/// Native collateral.
pub const DOT: AssetId = AssetId::Native;

/// Collateral issued by `pallet-assets`.
pub const TOKEN_X_ID: AssetIdForAssets = 1;
pub const TOKEN_X: AssetId = AssetId::WithId(TOKEN_X_ID);

/// Unit-scale stablecoin.
pub const PUSD: StableId = 1_000;

/// A 6-decimals stablecoin with a realistic 0.01-coin minimum balance.
pub const USDX: StableId = 6_000;
/// Raw units in one [`USDX`].
pub const USDX_UNIT: Balance = 1_000_000;
/// The [`USDX`] minimum balance: 0.01 coin.
pub const USDX_MIN_BALANCE: Balance = USDX_UNIT / 100;

/// Stablecoin-wide debt ceiling that [`register_branch`] sets, high enough never to bind.
pub const GLOBAL_CEILING: Balance = 1_000_000_000_000_000;

pub fn new_test_ext() -> TestState {
	let t = RuntimeGenesisConfig {
		assets: pallet_assets::GenesisConfig {
			assets: vec![
				(TOKEN_X_ID, 1, true, 1),
				(PUSD, STABLECOIN_OWNER, true, 1),
				(USDX, STABLECOIN_OWNER, true, USDX_MIN_BALANCE),
			],
			metadata: vec![(USDX, b"USDX".to_vec(), b"USDX".to_vec(), 6)],
			accounts: vec![],
			next_asset_id: None,
			reserves: vec![],
		},
		system: Default::default(),
		balances: pallet_balances::GenesisConfig {
			// The fee and insurance accounts pay native asset-account deposits, and a
			// Root-created market takes its custody seed from its full admin.
			balances: (1u64..=10u64)
				.chain([insurance_account(PUSD), FEE_DEST, VAULT_FEE_DEST, ADMIN])
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
		// The custody seed of a TOKEN_X market.
		mint_collateral(TOKEN_X_ID, ADMIN, 1_000_000_000_000);
	});
	ext
}

/// Runs `test` and then checks its final storage against the pallet's `try_state` invariants.
pub fn build_and_execute(test: impl FnOnce()) {
	new_test_ext().execute_with(|| {
		test();
		crate::try_state::do_try_state::<Test>().expect("post-test invariants hold");
		// The tests also drive Vaults, whose invariants are reachable from here only through its
		// try-runtime hook.
		#[cfg(feature = "try-runtime")]
		<Vaults as frame::deps::frame_support::traits::Hooks<_>>::try_state(System::block_number())
			.expect("post-test vaults invariants hold");
	});
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

/// The collateral price that [`register_branch`] opens every market at: 1 unit = 1.25 pUSD.
pub const DEFAULT_PRICE: FixedU128 = FixedU128::from_rational(5, 4);

/// Registers the `(collateral, stable)` market at [`DEFAULT_PRICE`] under [`GLOBAL_CEILING`],
/// seeding [`default_redemption_config`] when it is the coin's first market.
pub fn register_branch(
	collateral: AssetId,
	stable: StableId,
	config: pallet_vaults::BranchConfig<Balance>,
) {
	// `create_branch` requires a live price, so set it before creating.
	set_price(collateral.clone(), DEFAULT_PRICE);
	let redemption_config =
		(!crate::RedemptionConfigs::<Test>::contains_key(stable)).then(default_redemption_config);
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

/// Opens `who`'s vault on the `(collateral, stable)` market without rate-index hints.
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

/// The `max_steps` that defers to [`crate::Config::MaxRedemptionSteps`].
pub const MAX_STEPS_CONFIGURED: u32 = 0;

/// The parties, market, and terms of one [`redeem`] call.
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
	/// [`REDEEMER`] spends up to `max_stable_to_spend` on `(DOT, PUSD)` and [`RECIPIENT`]
	/// receives the collateral, with no slippage floor and [`MAX_STEPS_CONFIGURED`].
	pub fn spend(max_stable_to_spend: Balance) -> Self {
		Self {
			who: REDEEMER,
			collateral: DOT,
			stable: PUSD,
			max_stable_to_spend,
			min_collateral_out: 0,
			recipient: RECIPIENT,
			max_steps: MAX_STEPS_CONFIGURED,
		}
	}
}

/// Dispatches `redeem` signed by `options.who` and returns the settlement its events report.
pub fn redeem(options: RedeemOptions) -> Result<Settlement, DispatchError> {
	let RedeemOptions {
		who,
		collateral,
		stable,
		max_stable_to_spend,
		min_collateral_out,
		recipient,
		max_steps,
	} = options;
	let call = RedeemCall {
		who,
		collateral: collateral.clone(),
		stable,
		recipient,
		events_before: System::events().len(),
	};
	Redemptions::redeem(
		RuntimeOrigin::signed(who),
		collateral,
		stable,
		crate::RedemptionTerms { max_stable_to_spend, min_collateral_out },
		recipient,
		max_steps,
	)
	.map_err(|failed| failed.error)?;
	Ok(call.read_settlement())
}

/// The parties and market that one `redeem` call's events must report.
struct RedeemCall {
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	recipient: AccountId,
	/// Event count before dispatch; the call's events follow.
	events_before: usize,
}

/// One ordinary or recovery settlement, as its event reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settlement {
	pub stable_burned: Balance,
	pub insurance_cover: Balance,
	pub fee: Balance,
	pub collateral_out: Balance,
	/// Number of steps in an ordinary settlement, `None` for a recovery settlement.
	pub ordinary_steps: Option<u32>,
}

impl Settlement {
	/// The `(stable_burned, fee, collateral_out)` figures, for compact pinning.
	pub fn figures(&self) -> (Balance, Balance, Balance) {
		(self.stable_burned, self.fee, self.collateral_out)
	}
}

impl RedeemCall {
	/// Reads the call's single settlement event and checks it against the per-step
	/// `VaultRedeemed` events: both pallets must agree on what was settled, where, and for whom.
	fn read_settlement(&self) -> Settlement {
		let mut vault_debt_cancelled = 0;
		let mut vault_collateral_out = 0;
		let mut settlement = None;
		for record in System::events().into_iter().skip(self.events_before) {
			let (collateral_id, stable_id, redeemer, settled) = match record.event {
				RuntimeEvent::Vaults(pallet_vaults::Event::VaultRedeemed {
					collateral_id,
					stable_id,
					recipient,
					debt_cancelled,
					collateral_to_recipient,
					..
				}) => {
					assert_eq!(collateral_id, self.collateral);
					assert_eq!(stable_id, self.stable);
					assert_eq!(recipient, self.recipient, "the vaults pay the named recipient");
					vault_debt_cancelled += debt_cancelled;
					vault_collateral_out += collateral_to_recipient;
					continue;
				},
				RuntimeEvent::Redemptions(
					pallet_redemptions::Event::OrdinaryRedemptionExecuted {
						collateral_id,
						stable_id,
						redeemer,
						stable_burned,
						collateral_out,
						fee,
						steps,
					},
				) => (
					collateral_id,
					stable_id,
					redeemer,
					Settlement {
						stable_burned,
						insurance_cover: 0,
						fee,
						collateral_out,
						ordinary_steps: Some(steps),
					},
				),
				RuntimeEvent::Redemptions(
					pallet_redemptions::Event::RecoveryRedemptionExecuted {
						collateral_id,
						stable_id,
						redeemer,
						stable_burned,
						insurance_cover,
						collateral_out,
						..
					},
				) => (
					collateral_id,
					stable_id,
					redeemer,
					Settlement {
						stable_burned,
						insurance_cover,
						fee: 0,
						collateral_out,
						ordinary_steps: None,
					},
				),
				_ => continue,
			};
			assert_eq!(collateral_id, self.collateral);
			assert_eq!(stable_id, self.stable);
			assert_eq!(redeemer, self.who);
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
}

/// Parks `owner`'s vault in `FinalRecovery`. Any account may sign the call; the caller sets up
/// its preconditions: the market's last eligible vault, below the MCR.
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

/// Funds `who` with a `pallet-assets` collateral. Genesis funds only [`DOT`].
pub fn mint_collateral(collateral: AssetIdForAssets, who: AccountId, amount: Balance) {
	<Assets as FungiblesMutate<AccountId>>::mint_into(collateral, &who, amount)
		.expect("mint collateral");
}

/// Advances the timestamp by `ms` without advancing the block number.
pub fn advance_time(ms: Moment) {
	Timestamp::set_timestamp(Timestamp::get() + ms);
}

/// Overwrites the stablecoin's fee state, stamped now so the next redemption starts from exactly
/// `rate`.
pub fn set_dynamic_fee(stable: StableId, rate: FixedU128) {
	pallet_redemptions::RedemptionStates::<Test>::insert(
		stable,
		pallet_redemptions::RedemptionState {
			dynamic_fee: rate,
			last_fee_operation: Timestamp::get(),
		},
	);
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

/// Debt across every market on `stable`: the denominator of the dynamic-fee increase.
pub fn stablecoin_debt(stable: StableId) -> Balance {
	pallet_vaults::Pallet::<Test>::stablecoin_debt(&stable)
}
