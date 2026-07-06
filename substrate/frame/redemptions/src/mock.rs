use crate as pallet_redemptions;
use crate::types::RedemptionConfig;
use frame::{
	deps::{
		frame_support::{
			derive_impl, parameter_types,
			traits::{
				fungible::{self, HoldConsideration, ItemOf, NativeFromLeft, NativeOrWithId},
				fungibles::{
					roles::Inspect as FungiblesRolesInspect, Inspect as FungiblesInspect,
					InspectHold, Mutate as FungiblesMutate,
				},
				tokens::imbalance::ResolveAssetTo,
				AsEnsureOriginWithArg, ConstU128, ConstU32, ConstU64, EitherOf,
				EnsureOriginWithArg, LinearStoragePrice,
			},
			PalletId,
		},
		sp_runtime::{
			traits::{Convert, IdentityLookup},
			BuildStorage, DispatchError, DispatchResult, FixedU128, Permill,
		},
	},
	testing_prelude::*,
};
pub use pallet_linked_list::Position;
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

#[frame::deps::frame_support::runtime]
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
	type CreateOrigin = AsEnsureOriginWithArg<frame_system::EnsureSigned<u64>>;
	type ForceOrigin = frame_system::EnsureRoot<u64>;
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
	pub const PusdAssetId: AssetIdForAssets = 1_000;
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

/// Multi-asset stable issuance surface: the whole `pallet-assets` instance.
pub type VaultStableAssets = Assets;

/// Single-asset view of the default stablecoin, for tests that read or move pUSD
/// directly without going through the market API.
pub type Pusd = ItemOf<Assets, PusdAssetId, AccountId>;

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

pub fn set_price(asset: AssetId, price: FixedU128) {
	MockPrices::mutate(|m| {
		m.insert(asset, price);
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

/// The origin caller under which `who` administers markets.
pub fn admin_caller(who: AccountId) -> OriginCaller {
	frame_system::RawOrigin::Signed(who).into()
}

/// The `create_branch` admin bundle: `full` administers, `emergency` tightens.
pub fn branch_admins(
	full: AccountId,
	emergency: AccountId,
) -> pallet_vaults::types::BranchAdmins<OriginCaller> {
	pallet_vaults::types::BranchAdmins {
		full_admin: admin_caller(full),
		emergency_admin: admin_caller(emergency),
	}
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
	type FeeHandler = ();
	// Registering a market seeds this pallet's redemption config via `on_registered`.
	type OnBranchLifecycle = Redemptions;
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

	fn mint_collateral(asset_id: AssetId, who: &AccountId, amount: Balance) {
		use frame::deps::frame_support::traits::fungible::Mutate as FungibleMutate;
		let _ = <Balances as FungibleMutate<AccountId>>::mint_into(who, 1);
		match asset_id {
			AssetId::Native => {
				<Balances as FungibleMutate<AccountId>>::mint_into(who, amount).unwrap();
			},
			AssetId::WithId(id) => {
				<Assets as FungiblesMutate<AccountId>>::mint_into(id, who, amount).unwrap();
			},
		};
	}

	fn mint_stable(stable_id: StableId, who: &AccountId, amount: Balance) {
		<Assets as FungiblesMutate<AccountId>>::mint_into(stable_id, who, amount)
			.expect("mint stable for benchmark account");
	}

	fn set_oracle_price(asset_id: AssetId, _stable_id: StableId, price: FixedU128) {
		set_price(asset_id, price);
	}

	fn advance_time(ms: u64) {
		advance_time(ms);
	}

	fn synth_market(seed: u32) -> (AssetId, StableId) {
		(AssetId::WithId(10_000 + seed), 20_000 + seed)
	}
}

/// Base offset for per-stable insurance accounts: `insurance_account(stable)`
/// is `INSURANCE_FUND_BASE + stable`.
pub const INSURANCE_FUND_BASE: AccountId = 700_000;
/// `insurance_account(PUSD)`, the default stablecoin's fund; `new_test_ext`
/// pair-asserts the two stay in sync.
pub const INSURANCE_FUND: AccountId = 701_000;
pub const FEE_ACCOUNT: AccountId = 888;

/// Each stablecoin's cover lives at its own account, mirroring a runtime that
/// derives per-stable sub-accounts.
pub fn insurance_account(stable_id: StableId) -> AccountId {
	INSURANCE_FUND_BASE + AccountId::from(stable_id)
}

pub struct InsuranceFundAccounts;
impl Convert<StableId, AccountId> for InsuranceFundAccounts {
	fn convert(stable_id: StableId) -> AccountId {
		insurance_account(stable_id)
	}
}

parameter_types! {
	pub const FeeDestAccount: AccountId = FEE_ACCOUNT;
}

/// Root (the governance override) or the market's stored full admin, the same
/// composition a production runtime would use.
pub type RedemptionsUpdateOrigin = EitherOf<
	AsEnsureOriginWithArg<frame_system::EnsureRoot<AccountId>>,
	pallet_vaults::EnsureBranchFullAdmin<Test>,
>;

parameter_types! {
	pub static DefaultRedemptionConfig: RedemptionConfig<Balance, Moment> = RedemptionConfig {
		minimum_redemption_amount: 100,
		dynamic_fee_decay_period: 6 * 3_600 * 1_000,
		dynamic_fee_floor: FixedU128::zero(),
		dynamic_fee_ceiling: FixedU128::one(),
		base_fee: FixedU128::from_rational(5u128, 1_000u128),
		fee_ceiling: FixedU128::one(),
		dynamic_fee_increase_divisor: FixedU128::from_rational(2u128, 1u128),
		final_recovery_bonus_buffer: FixedU128::from_rational(1u128, 100u128),
	};
}

impl pallet_redemptions::Config for Test {
	type CollateralAssetId = AssetId;
	type StableAssetId = StableId;
	type StableAssets = VaultStableAssets;
	type Oracle = MockOracle;
	type Vaults = Vaults;
	type InsuranceFundAccount = InsuranceFundAccounts;
	type FeeHandler = ResolveAssetTo<FeeDestAccount, VaultStableAssets>;
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
		use frame::deps::frame_support::traits::fungible::Mutate as FungibleMutate;
		register_default_branch();

		let debt: Balance = 300;
		for i in 0..vaults {
			let who = 1_000 + u64::from(i);
			let _ = <Balances as FungibleMutate<AccountId>>::mint_into(&who, 10_000_000_000);
			let rate = FixedU128::from_rational(u128::from(i) + 1, 1_000u128);
			open(who, 1_000_000, debt, rate).expect("open benchmark vault");
		}

		let redeemer: AccountId = 1;
		let budget = debt.saturating_mul(u128::from(vaults).saturating_add(2)).saturating_mul(2);
		mint_pusd(redeemer, budget.saturating_mul(2));
		(DOT, PUSD, redeemer, budget)
	}
}

/// DOT-equivalent native collateral asset id used across tests.
pub const DOT: AssetId = AssetId::Native;
pub const TOKEN_X_ID: AssetIdForAssets = 1;
/// Default stablecoin every helper mints and redeems against.
pub const PUSD: StableId = 1_000;

pub fn new_test_ext() -> TestState {
	let t = RuntimeGenesisConfig {
		assets: pallet_assets::GenesisConfig {
			assets: vec![(TOKEN_X_ID, 1, true, 1), (PusdAssetId::get(), 1, true, 1)],
			metadata: vec![],
			accounts: vec![],
			next_asset_id: None,
			reserves: vec![],
		},
		system: Default::default(),
		balances: pallet_balances::GenesisConfig {
			balances: (1u64..=10u64)
				.chain([INSURANCE_FUND, FEE_ACCOUNT])
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
		assert_eq!(insurance_account(PUSD), INSURANCE_FUND);
	});
	ext
}

pub fn build_and_execute(test: impl FnOnce()) {
	new_test_ext().execute_with(|| {
		test();
		#[cfg(feature = "try-runtime")]
		crate::Pallet::<Test>::do_try_state().expect("post-test invariants hold");
	});
}

/// Default per-collateral global debt ceiling for test markets.
pub const GLOBAL_CEILING: Balance = 1_000_000_000_000_000;

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

/// Registers the default `(DOT, PUSD)` market. Creation also seeds the
/// redemptions config through the `OnBranchLifecycle` hook.
pub fn register_default_branch() {
	set_price(DOT, FixedU128::from_rational(5u128, 4u128)); // 1.25$
	pallet_vaults::Pallet::<Test>::create_branch(
		RuntimeOrigin::root(),
		DOT,
		PUSD,
		branch_admins(ADMIN, EMERGENCY_ADMIN),
		default_branch_config(),
	)
	.expect("create_branch ok");
	pallet_vaults::Pallet::<Test>::set_global_debt_ceiling(
		RuntimeOrigin::root(),
		DOT,
		GLOBAL_CEILING,
	)
	.expect("set global debt ceiling");
	fund_redistribution_account();
}

fn fund_redistribution_account() {
	use frame::deps::frame_support::traits::fungible::Mutate as FungibleMutate;
	let redistribution: AccountId =
		pallet_vaults::Pallet::<Test>::redistribution_account(&DOT, &PUSD);
	let _ = <Balances as FungibleMutate<AccountId>>::mint_into(&redistribution, 1);
}

pub fn open(who: AccountId, coll: Balance, debt: Balance, rate: FixedU128) -> DispatchResult {
	pallet_vaults::Pallet::<Test>::open_vault(
		RuntimeOrigin::signed(who),
		DOT,
		PUSD,
		coll,
		debt,
		rate,
		Position::endpoints_only(),
	)
}

/// Tests set the last-vault and price preconditions before calling this.
pub fn enter_final_recovery(who: AccountId) -> DispatchResult {
	pallet_vaults::Pallet::<Test>::enter_final_recovery(RuntimeOrigin::signed(99), DOT, PUSD, who)
}

pub fn mint_pusd(who: AccountId, amount: Balance) {
	<Assets as FungiblesMutate<AccountId>>::mint_into(PUSD, &who, amount).expect("mint pusd");
}

pub fn rate_pct(num: u128, denom: u128) -> FixedU128 {
	FixedU128::from_rational(num, denom)
}

pub fn redeem(
	redeemer: AccountId,
	max_pusd_in: Balance,
	min_collateral_out: Balance,
	recipient: AccountId,
) -> DispatchResultWithPostInfo {
	redeem_capped(redeemer, max_pusd_in, min_collateral_out, recipient, 0)
}

pub fn redeem_capped(
	redeemer: AccountId,
	max_pusd_in: Balance,
	min_collateral_out: Balance,
	recipient: AccountId,
	max_steps: u32,
) -> DispatchResultWithPostInfo {
	pallet_redemptions::Pallet::<Test>::redeem(
		RuntimeOrigin::signed(redeemer),
		DOT,
		PUSD,
		max_pusd_in,
		min_collateral_out,
		recipient,
		max_steps,
	)
}

pub fn advance_time(ms: Moment) {
	let now = pallet_timestamp::Pallet::<Test>::get();
	Timestamp::set_timestamp(now + ms);
}

pub fn pusd_balance(who: AccountId) -> Balance {
	<Pusd as fungible::Inspect<AccountId>>::balance(&who)
}

pub fn pusd_issuance() -> Balance {
	<Pusd as fungible::Inspect<AccountId>>::total_issuance()
}

pub fn held(who: AccountId) -> Balance {
	<VaultCollateralAssets as InspectHold<AccountId>>::balance_on_hold(
		DOT,
		&pallet_vaults::HoldReason::VaultCollateral.into(),
		&who,
	)
}

pub fn collateral_balance(who: AccountId) -> Balance {
	<VaultCollateralAssets as FungiblesInspect<AccountId>>::balance(DOT, &who)
}

pub fn vault_debt(who: AccountId) -> Balance {
	pallet_vaults::Vaults::<Test>::get((DOT, PUSD, who))
		.map(|v| v.debt.principal + v.debt.interest)
		.unwrap_or_default()
}

pub fn vault_status(who: AccountId) -> Option<pallet_vaults::VaultStatus> {
	pallet_vaults::Pallet::<Test>::vault_status(DOT, PUSD, who)
}

pub fn redemption_state() -> pallet_redemptions::RedemptionState<Moment> {
	pallet_redemptions::RedemptionStates::<Test>::get(DOT, PUSD)
}

/// Anchored at current time so the next redemption observes exactly `rate`.
pub fn set_dynamic_fee(rate: FixedU128) {
	let now = pallet_timestamp::Pallet::<Test>::get();
	pallet_redemptions::RedemptionStates::<Test>::insert(
		DOT,
		PUSD,
		pallet_redemptions::RedemptionState { dynamic_fee: rate, last_fee_operation: now },
	);
}

pub fn now_ms() -> Moment {
	pallet_timestamp::Pallet::<Test>::get()
}

/// Branch TCR as the vault pallet reports it, including pending interest.
pub fn branch_tcr() -> FixedU128 {
	pallet_vaults::Pallet::<Test>::branch_tcr(DOT, PUSD).expect("branch registered")
}

/// Fully accrued branch debt: the denominator the dynamic-fee accelerator uses.
pub fn branch_debt() -> Balance {
	<pallet_vaults::Pallet<Test> as VaultInterface>::branch_debt(&DOT, &PUSD)
}

/// The interest-clock value stamped on a vault the last time it was poked.
pub fn vault_interest_time(who: AccountId) -> Moment {
	pallet_vaults::Vaults::<Test>::get((DOT, PUSD, who))
		.expect("vault")
		.last_interest_time
}

/// The interest-clock value a poke at `now` writes onto a touched vault.
pub fn branch_interest_time(now: Moment) -> Moment {
	pallet_vaults::Branches::<Test>::get((DOT, PUSD))
		.expect("branch")
		.state
		.interest_time(now)
}

/// Overwrites the branch config so redemptions carry no fee and the dynamic fee
/// stays pinned at zero, isolating the redemption mechanic from fee dynamics.
pub fn set_fee_free_config() {
	let mut cfg = DefaultRedemptionConfig::get();
	cfg.dynamic_fee_ceiling = FixedU128::zero();
	cfg.base_fee = FixedU128::zero();
	cfg.fee_ceiling = FixedU128::zero();
	pallet_redemptions::Pallet::<Test>::set_redemption_config(
		RuntimeOrigin::root(),
		DOT,
		PUSD,
		cfg,
	)
	.expect("fee-free config is valid");
}
