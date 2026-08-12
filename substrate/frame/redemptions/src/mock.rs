//! Test runtime for `pallet-redemptions`.
//!
//! Conventions used in the tests:
//! - Collateral `AssetId::Native` ([`DOT`]) routes to `Balances`; `AssetId::WithId(asset)` routes
//!   to `AssetsHolder`. Stablecoins are plain `pallet-assets` ids: [`PUSD`] is the unit-scale
//!   default coin, [`USDX`] the 6-decimals coin the scale tests use.

use crate as pallet_redemptions;
use crate::types::RedemptionConfig;
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
		tokens::{fungible, imbalance::ResolveAssetTo},
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
	pub TestBranchConfigBounds: pallet_vaults::types::BranchConfigBounds<Balance> =
		pallet_vaults::types::BranchConfigBounds {
			min_minimum_collateralization_ratio: FixedU128::from_rational(105u128, 100u128),
			min_initial_collateralization_ratio: FixedU128::from_rational(110u128, 100u128),
			min_safety_collateralization_ratio: FixedU128::from_rational(120u128, 100u128),
			min_minimum_debt: 100,
			min_minimum_collateral: 1,
			max_borrow_rate: FixedU128::from_rational(400u128, 100u128),
			max_debt_ceiling: 1_000_000_000_000_000,
			max_ceiling_gap: 1_000_000_000,
			min_ceiling_ttl: 24 * 3_600 * 1_000,
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
	type TimeProvider = Timestamp;
	type CreateOrigin = EnsureAssetOwner;
	type Consideration = VaultsConsideration;
	type BranchConfigBounds = TestBranchConfigBounds;
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

parameter_types! {
	pub static DefaultRedemptionConfig: RedemptionConfig<Balance> = RedemptionConfig {
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
	type UpdateOrigin = RedemptionsUpdateOrigin;
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

/// Default per-collateral global debt ceiling for test markets — high enough
/// that the global cap never binds unless a test sets a lower one.
pub const GLOBAL_CEILING: Balance = 1_000_000_000_000_000;

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
			// The fee account needs native funds to pay its asset-account deposit.
			balances: (1u64..=10u64)
				.chain([insurance_account(PUSD), FEE_DEST, VAULT_FEE_DEST])
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

/// Run `test` and check post-state invariants under `try-runtime`.
pub fn build_and_execute(test: impl FnOnce()) {
	new_test_ext().execute_with(|| {
		test();
		#[cfg(feature = "try-runtime")]
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
		redistribution_penalty: Permill::from_percent(5),
		ceiling_gap: 0,
		ceiling_ttl: 0,
	}
}

/// [`default_branch_config`] with its figures scaled by [`USDX_UNIT`], for the
/// `(DOT, USDX)` market. The seeded redemption config keeps the test default,
/// whose 100-raw-unit minimum admits arbitrarily small redemptions.
pub fn usdx_branch_config() -> pallet_vaults::BranchConfig<Balance> {
	pallet_vaults::BranchConfig {
		// The largest ceiling the guard envelope admits: 1_000 coins.
		debt_ceiling: TestBranchConfigBounds::get().max_debt_ceiling,
		minimum_debt: 200 * USDX_UNIT,
		..default_branch_config()
	}
}

/// Registers the `(collateral, stable)` market at price 1.25$ with a high
/// global debt ceiling. Creation also seeds the redemptions config through the
/// `OnBranchLifecycle` hook.
pub fn register_branch(
	collateral: AssetId,
	stable: StableId,
	config: pallet_vaults::BranchConfig<Balance>,
) {
	use frame::traits::fungible::Mutate as FungibleMutate;
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
	Vaults::set_global_debt_ceiling(RuntimeOrigin::root(), collateral.clone(), GLOBAL_CEILING)
		.expect("set global debt ceiling");
	// Native ED so the redistribution sub-account can receive funds later.
	let redistribution: AccountId = Vaults::redistribution_account(&collateral, &stable);
	let _ = <Balances as FungibleMutate<AccountId>>::mint_into(&redistribution, 1);
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

/// Redeem on the `(collateral, stable)` market, mirroring the extrinsic's
/// argument order with `who` as the signed origin.
pub fn redeem(
	who: AccountId,
	collateral: AssetId,
	stable: StableId,
	max_pusd_in: Balance,
	min_collateral_out: Balance,
	recipient: AccountId,
	max_steps: u32,
) -> DispatchResultWithPostInfo {
	Redemptions::redeem(
		RuntimeOrigin::signed(who),
		collateral,
		stable,
		crate::RedemptionTerms { max_stable_in: max_pusd_in, min_collateral_out },
		recipient,
		max_steps,
	)
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
		.map(|v| v.debt.principal + v.debt.interest)
		.unwrap_or_default()
}

/// Stablecoin-wide debt: the denominator the dynamic-fee accelerator uses.
pub fn stablecoin_debt(stable: StableId) -> Balance {
	pallet_vaults::Pallet::<Test>::stablecoin_debt(&stable)
}

/// One market's share of that denominator.
pub fn branch_outstanding(collateral: AssetId, stable: StableId) -> Balance {
	pallet_vaults::Branches::<Test>::get(collateral, stable)
		.map(|b| b.state.debt.outstanding())
		.unwrap_or_default()
}
