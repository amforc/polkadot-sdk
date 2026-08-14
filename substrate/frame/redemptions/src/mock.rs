//! Test runtime for `pallet-redemptions`.
//!
//! Conventions used in the tests:
//! - Collateral `AssetId::Native` ([`DOT`]) routes to `Balances`; `AssetId::WithId(asset)` routes
//!   to `AssetsHolder`. Stablecoins are plain `pallet-assets` ids: [`PUSD`] is the unit-scale
//!   default coin, [`USDX`] the 6-decimals coin the scale tests use.

use crate as pallet_redemptions;
use crate::types::{RedemptionConfig, RedemptionQuote};
pub use frame::{
	arithmetic::{FixedPointNumber, FixedU128, One, Permill, Saturating, Zero},
	prelude::{DispatchError, DispatchResult},
	testing_prelude::{assert_noop, assert_ok, BadOrigin},
};
use frame::{
	deps::sp_runtime::traits::ConvertInto,
	testing_prelude::*,
	traits::{
		fungible::{HoldConsideration, NativeFromLeft, NativeOrWithId},
		fungibles::{
			roles::Inspect as FungiblesRolesInspect, Inspect as FungiblesInspect, InspectHold,
			Mutate as FungiblesMutate,
		},
		tokens::{fungible, imbalance::ResolveAssetTo, Fortitude, Preservation},
		AsEnsureOriginWithArg, EnsureOriginWithArg, IdentityLookup, LinearStoragePrice,
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
	pub const IdleMaxRefreshWeight: Option<Weight> = Some(Weight::MAX);
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
		RuntimeHoldReason::Vaults(pallet_vaults::HoldReason::BranchCreationDeposit);
	pub const MarketDepositBase: Balance = 1_000;
}

/// Genesis owner of each test stablecoin.
///
/// [`EnsureAssetOwner`] gives this account authority for
/// [`pallet_redemptions::Call::set_redemption_config`]. This account is not a market admin.
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
	type IdleMaxRefreshWeight = IdleMaxRefreshWeight;
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
		MockPrices::mutate(|prices| {
			prices.remove(&collateral_id);
		});
	}

	fn advance_time(ms: u64) {
		advance_time(ms);
	}
}

/// Account the redemption `FeeHandler` resolves the pUSD fee into, so tests can
/// assert the exact fee routed.
pub const FEE_DEST: AccountId = 888;

/// Each stablecoin's cover lives at its own account, mirroring a runtime that
/// derives per-stable sub-accounts.
pub fn insurance_account(stable: StableId) -> AccountId {
	700_000 + AccountId::from(stable)
}

pub struct InsuranceFundAccounts;
impl Convert<StableId, AccountId> for InsuranceFundAccounts {
	fn convert(stable: StableId) -> AccountId {
		insurance_account(stable)
	}
}

/// Collects vault fees separately from redemption fees in [`FEE_DEST`].
pub const VAULT_FEE_DEST: AccountId = 887;

/// Routes all test stablecoin vault fees to [`VAULT_FEE_DEST`].
pub struct FeeAccounts;
impl Convert<StableId, AccountId> for FeeAccounts {
	fn convert(_stable: StableId) -> AccountId {
		VAULT_FEE_DEST
	}
}

parameter_types! {
	pub const FeeDestAccount: AccountId = FEE_DEST;
}

/// The redemption config is per-stablecoin, so its authority is the coin's, not
/// any one market's: the same [`EnsureAssetOwner`] vaults takes as
/// `CreateOrigin`, which already admits Root as the governance override.
pub type RedemptionsUpdateOrigin = EnsureAssetOwner;

/// Test fixture for a stablecoin's first-market redemption policy.
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

/// Default stablecoin-wide global debt ceiling for test markets — high enough
/// that the global cap never binds unless a test sets a lower one.
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
			// The fee account needs native funds to pay its asset-account deposit.
			// The full admin pays the custody seed of every Root-created market.
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
		MockOracleAvailable::set(true);
		// Every market's registration seed comes out of the full admin's balance.
		mint_collateral(TOKEN_X_ID, ADMIN, 1_000_000_000_000);
	});
	ext
}

/// Runs `test` and then checks its final storage against the pallet's `try_state` invariants.
pub fn build_and_execute(test: impl FnOnce()) {
	new_test_ext().execute_with(|| {
		test();
		crate::try_state::do_try_state::<Test>().expect("post-test invariants hold");
	});
}

/// Default branch config: MCR=110%, ICR=120%, Safety=130%, ceiling 100M,
/// MinDebt=200, MinColl=1, rate bounds 0.1%-100%, 7d upfront fee,
/// 1d rate-cooldown, 5% redistribution penalty.
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

/// [`default_branch_config`] with its figures scaled by [`USDX_UNIT`], for the
/// `(DOT, USDX)` market. The seeded redemption config keeps the test default,
/// whose 100-raw-unit minimum admits arbitrarily small redemptions.
pub fn usdx_branch_config() -> pallet_vaults::BranchConfig<Balance> {
	pallet_vaults::BranchConfig {
		debt_ceiling: 1_000_000_000 * USDX_UNIT,
		minimum_debt: 200 * USDX_UNIT,
		..default_branch_config()
	}
}

/// Registers the `(collateral, stable)` market at price 1.25$ with a high
/// global debt ceiling.
pub fn register_branch(
	collateral: AssetId,
	stable: StableId,
	config: pallet_vaults::BranchConfig<Balance>,
) {
	// `create_branch` requires a live price, so set it before creating.
	set_price(collateral.clone(), FixedU128::from_rational(5u128, 4u128));
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

/// Open a vault for `who` on the `(collateral, stable)` market with
/// `(None, None)` rate-index hints.
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

/// Redeems on the `(collateral, stable)` market with `who` as the signed origin.
pub fn redeem(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	max_stable_to_spend: Balance,
	min_collateral_out: Balance,
	recipient: AccountId,
	max_steps: u32,
) -> DispatchResultWithPostInfo {
	let snapshot =
		RedeemSnapshot::take(who, &collateral, stable, recipient, max_stable_to_spend, max_steps);
	let result = Redemptions::redeem(
		RuntimeOrigin::signed(who),
		collateral,
		stable,
		crate::RedemptionTerms { max_stable_to_spend, min_collateral_out },
		recipient,
		max_steps,
	);
	if result.is_ok() {
		snapshot.assert_settled();
	}
	result
}

/// Records the state used to check a redemption after settlement.
struct RedeemSnapshot {
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	recipient: AccountId,
	/// Execution must reproduce this quote when the account can fund it.
	quote: Option<RedemptionQuote<Balance>>,
	/// Number of events before dispatch. Settlement events must follow them.
	events: usize,
	supply: Balance,
	redeemer_stable: Balance,
	fee_dest_stable: Balance,
	vault_fee_dest_stable: Balance,
	insurance_stable: Balance,
	recipient_collateral: Balance,
	fee_state: pallet_redemptions::RedemptionState,
}

/// Contains the values from an ordinary or recovery settlement event.
struct Settlement {
	collateral_id: AssetId,
	stable_id: StableId,
	redeemer: AccountId,
	recipient: AccountId,
	stable_burned: Balance,
	insurance_cover: Balance,
	fee: Balance,
	collateral_out: Balance,
	/// Number of steps in an ordinary settlement.
	///
	/// A recovery settlement uses `None` because it settles one FIFO head and does not change the
	/// dynamic fee.
	ordinary_steps: Option<u32>,
}

impl Settlement {
	fn from_event(event: RuntimeEvent) -> Option<Self> {
		match event {
			RuntimeEvent::Redemptions(pallet_redemptions::Event::OrdinaryRedemptionExecuted {
				collateral_id,
				stable_id,
				redeemer,
				recipient,
				stable_burned,
				collateral_out,
				fee,
				steps,
			}) => Some(Self {
				collateral_id,
				stable_id,
				redeemer,
				recipient,
				stable_burned,
				insurance_cover: 0,
				fee,
				collateral_out,
				ordinary_steps: Some(steps),
			}),
			RuntimeEvent::Redemptions(pallet_redemptions::Event::RecoveryRedemptionExecuted {
				collateral_id,
				stable_id,
				redeemer,
				recipient,
				stable_burned,
				insurance_cover,
				collateral_out,
				..
			}) => Some(Self {
				collateral_id,
				stable_id,
				redeemer,
				recipient,
				stable_burned,
				insurance_cover,
				fee: 0,
				collateral_out,
				ordinary_steps: None,
			}),
			_ => None,
		}
	}
}

/// Calculates recipient-side collateral as the total balance less all holds.
fn free_collateral(collateral: &AssetId, who: AccountId) -> Balance {
	collateral_balance(collateral.clone(), who) - held(collateral.clone(), who)
}

impl RedeemSnapshot {
	fn take(
		who: AccountId,
		collateral: &AssetId,
		stable: StableId,
		recipient: AccountId,
		max_stable_to_spend: Balance,
		max_steps: u32,
	) -> Self {
		let spendable = <Assets as FungiblesInspect<AccountId>>::reducible_balance(
			stable,
			&who,
			Preservation::Preserve,
			Fortitude::Polite,
		);
		let quote =
			Redemptions::preview_redeem(collateral.clone(), stable, max_stable_to_spend, max_steps)
				.ok()
				.filter(|quote| spendable >= quote.stable_in());
		Self {
			who,
			collateral: collateral.clone(),
			stable,
			recipient,
			quote,
			events: System::events().len(),
			supply: Assets::total_supply(stable),
			redeemer_stable: Assets::balance(stable, who),
			fee_dest_stable: Assets::balance(stable, FEE_DEST),
			vault_fee_dest_stable: Assets::balance(stable, VAULT_FEE_DEST),
			insurance_stable: Assets::balance(stable, insurance_account(stable)),
			recipient_collateral: free_collateral(collateral, recipient),
			fee_state: pallet_redemptions::RedemptionStates::<Test>::get(stable),
		}
	}

	fn assert_settled(&self) {
		let settlements: Vec<Settlement> = System::events()
			.into_iter()
			.skip(self.events)
			.filter_map(|record| Settlement::from_event(record.event))
			.collect();
		assert_eq!(settlements.len(), 1, "one redemption settles per call");
		let settled = &settlements[0];
		assert_eq!(settled.collateral_id, self.collateral);
		assert_eq!(settled.stable_id, self.stable);
		assert_eq!(settled.redeemer, self.who);
		assert_eq!(settled.recipient, self.recipient);
		self.assert_moves(settled);
		match settled.ordinary_steps {
			Some(_) => self.assert_ordinary_fee_state(),
			None => assert_eq!(
				pallet_redemptions::RedemptionStates::<Test>::get(self.stable),
				self.fee_state,
				"a recovery settlement leaves the ordinary accelerator alone"
			),
		}
		self.assert_quote_honoured(settled);
	}

	fn assert_moves(&self, settled: &Settlement) {
		let stable = self.stable;
		assert_eq!(
			self.redeemer_stable - Assets::balance(stable, self.who),
			settled.stable_burned + settled.fee,
			"the redeemer pays the burned debt plus the fee"
		);
		assert_eq!(
			Assets::balance(stable, FEE_DEST) - self.fee_dest_stable,
			settled.fee,
			"the fee lands in FEE_DEST"
		);
		assert_eq!(
			self.insurance_stable - Assets::balance(stable, insurance_account(stable)),
			settled.insurance_cover,
			"the Insurance Fund pays exactly the cover"
		);
		let yield_settled = Assets::balance(stable, VAULT_FEE_DEST) - self.vault_fee_dest_stable;
		assert_eq!(
			self.supply + yield_settled - Assets::total_supply(stable),
			settled.stable_burned + settled.insurance_cover,
			"issuance falls by the burn and the cover, and by nothing else"
		);
		assert_eq!(
			free_collateral(&self.collateral, self.recipient) - self.recipient_collateral,
			settled.collateral_out,
			"the recipient receives exactly the collateral out"
		);
	}

	fn assert_ordinary_fee_state(&self) {
		let config = pallet_redemptions::RedemptionConfigs::<Test>::get(self.stable)
			.expect("a redeemed coin has a policy");
		let state = pallet_redemptions::RedemptionStates::<Test>::get(self.stable);
		assert!(state.dynamic_fee >= config.dynamic_fee_floor, "dynamic fee below the floor");
		assert!(state.dynamic_fee <= config.dynamic_fee_ceiling, "dynamic fee above the ceiling");
		assert_eq!(state.last_fee_operation, Timestamp::get(), "fee state not stamped now");
		// An event reports each fee-state change and no event reports an unchanged state.
		let reported =
			System::events()
				.into_iter()
				.skip(self.events)
				.find_map(|record| match record.event {
					RuntimeEvent::Redemptions(
						pallet_redemptions::Event::RedemptionDynamicFeeUpdated {
							old_dynamic_fee,
							new_dynamic_fee,
							..
						},
					) => Some((old_dynamic_fee, new_dynamic_fee)),
					_ => None,
				});
		match reported {
			Some((old, new)) => {
				assert_eq!(old, self.fee_state.dynamic_fee, "reported old dynamic fee");
				assert_eq!(new, state.dynamic_fee, "reported new dynamic fee");
				assert_ne!(old, new, "a fee move reported where there was none");
			},
			None => assert_eq!(state.dynamic_fee, self.fee_state.dynamic_fee, "an unreported move"),
		}
	}

	fn assert_quote_honoured(&self, settled: &Settlement) {
		let Some(quote) = &self.quote else { return };
		assert_eq!(settled.stable_burned, quote.debt_cancelled, "execution vs quoted debt");
		assert_eq!(settled.fee, quote.fee, "execution vs quoted fee");
		assert_eq!(settled.collateral_out, quote.collateral_out, "execution vs quoted collateral");
		if let Some(steps) = settled.ordinary_steps {
			assert_eq!(steps, quote.steps, "execution vs quoted steps");
		}
	}
}

/// Park `owner`'s vault in `FinalRecovery`. The call is permissionless, so an
/// arbitrary keeper signs it; tests set the last-vault and price preconditions.
pub fn enter_final_recovery(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
) -> DispatchResult {
	Vaults::enter_final_recovery(RuntimeOrigin::signed(99), collateral, stable, owner)
}

pub fn mint_stable(stable: StableId, who: AccountId, amount: Balance) {
	<Assets as FungiblesMutate<AccountId>>::mint_into(stable, &who, amount).expect("mint stable");
}

/// Fund `who` with a `pallet-assets` collateral. Native `DOT` is already funded
/// in genesis; the others need minting before a vault can be opened.
pub fn mint_collateral(collateral: AssetIdForAssets, who: AccountId, amount: Balance) {
	<Assets as FungiblesMutate<AccountId>>::mint_into(collateral, &who, amount)
		.expect("mint collateral");
}

/// Advance `pallet_timestamp` by `ms` milliseconds without touching block #.
pub fn advance_time(ms: Moment) {
	Timestamp::set_timestamp(Timestamp::get() + ms);
}

/// Overwrite the stablecoin's fee state, anchored at current time so the next
/// redemption observes exactly `rate`.
pub fn set_dynamic_fee(stable: StableId, rate: FixedU128) {
	pallet_redemptions::RedemptionStates::<Test>::insert(
		stable,
		pallet_redemptions::RedemptionState {
			dynamic_fee: rate,
			last_fee_operation: Timestamp::get(),
		},
	);
}

/// Total balance of `(collateral, who)` on the collateral surface (includes any hold).
pub fn collateral_balance(collateral: AssetId, who: AccountId) -> Balance {
	<VaultCollateralAssets as FungiblesInspect<AccountId>>::balance(collateral, &who)
}

/// Held collateral on `(collateral, who)` for the `VaultCollateral` reason.
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

/// Gets stablecoin-wide debt, which is the denominator for the dynamic-fee increase.
pub fn stablecoin_debt(stable: StableId) -> Balance {
	pallet_vaults::Pallet::<Test>::stablecoin_debt(&stable)
}

/// One market's share of that denominator.
pub fn branch_outstanding(collateral: AssetId, stable: StableId) -> Balance {
	pallet_vaults::Branches::<Test>::get(collateral, stable)
		.map(|b| b.state.debt.outstanding())
		.unwrap_or_default()
}
