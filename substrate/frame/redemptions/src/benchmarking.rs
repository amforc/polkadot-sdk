//! Redemptions cannot create vaults, so every benchmark stages real vault state through the vault
//! engine's [`BenchmarkVaults`] seeding API, in the order it documents.

#![cfg(feature = "runtime-benchmarks")]

use crate::{
	pallet::{
		BalanceOf, Call, CollateralIdOf, Config, Event, Millis, Pallet, RedemptionConfigOf,
		StableIdOf, Stablecoins,
	},
	types::{RecoveryRegime, RedemptionConfig, RedemptionTerms},
};
use alloc::vec::Vec;
use frame::{
	arithmetic::{FixedU128, Permill},
	benchmarking::prelude::*,
	deps::{
		frame_support::traits::EnsureOriginWithArg,
		sp_runtime::traits::{Convert, One, Saturating, Zero},
	},
	traits::fungibles::{Inspect as FungiblesInspect, Mutate as FungiblesMutate},
};
use frame_system::RawOrigin;
use stablecoin_primitives::{recovery_pricing, BenchmarkVaults, ProvidePrice, VaultInterface};

/// Interest the staged vaults accrue before the measured call, so its first settlement mints
/// interest through the yield route.
const ELAPSED_MS: Millis = 60 * 60 * 1_000;

/// Price, in stablecoin per collateral unit, that puts a staged recovery head below par: its
/// collateral is worth about two thirds of its debt, so the Insurance Fund covers the rest.
const BELOW_PAR_UNIT_PRICE: u128 = 2;

/// The least restrictive valid policy: a one-unit minimum and a full-range fee
/// band, so nothing in it becomes the binding constraint of a benchmark.
pub(crate) fn registration_config<T: Config>() -> RedemptionConfigOf<T> {
	RedemptionConfig {
		minimum_redemption_amount: BalanceOf::<T>::one(),
		dynamic_fee_decay_period: 6 * 3_600 * 1_000,
		dynamic_fee_floor: FixedU128::zero(),
		dynamic_fee_ceiling: FixedU128::one(),
		base_fee: Permill::from_rational(5u32, 1_000u32),
		fee_ceiling: Permill::one(),
		dynamic_fee_increase_divisor: FixedU128::from_rational(2u128, 1u128),
		final_recovery_bonus_buffer: Permill::from_percent(1),
	}
}

/// A fresh recipient exercises account creation for both native and sufficient collateral.
fn fresh_recipient<T: Config>() -> T::AccountId {
	let recipient: T::AccountId = account("recipient", 0, 0);
	assert!(!frame_system::Pallet::<T>::account_exists(&recipient));
	recipient
}

fn pallet_events<T: BenchmarkConfig>() -> Vec<Event<T>> {
	frame_system::Pallet::<T>::events()
		.into_iter()
		.filter_map(|record| record.event.try_into().ok())
		.collect()
}

/// Benchmark-only vault seeding, stablecoin minting and event inspection bounds.
pub(crate) trait BenchmarkConfig:
	Config<
	Vaults: BenchmarkVaults<
		CollateralIdOf<Self>,
		StableIdOf<Self>,
		Self::AccountId,
		BalanceOf<Self>,
	>,
	StableAssets: FungiblesMutate<Self::AccountId>,
	RuntimeEvent: TryInto<Event<Self>>,
>
{
}

impl<T> BenchmarkConfig for T where
	T: Config<
		Vaults: BenchmarkVaults<CollateralIdOf<T>, StableIdOf<T>, T::AccountId, BalanceOf<T>>,
		StableAssets: FungiblesMutate<T::AccountId>,
		RuntimeEvent: TryInto<Event<T>>,
	>
{
}

#[benchmarks(where T: BenchmarkConfig)]
mod benchmarks {
	use super::*;

	/// An ordinary walk over `s` active vaults, each fully redeemed.
	///
	/// Every target carries a pending redistribution share and accrued interest, the recipient
	/// is fresh, and the budget leaves change to refund.
	///
	/// Not staged: a step that closes its vault. Only a target at exactly 100% collateralization
	/// gives up all of its collateral, which [`BenchmarkVaults`] cannot stage per target. Such a
	/// step runs the Dormant settlement measured here and then the vault close: the row removal,
	/// the vault deposit release and `VaultClosed`. With native collateral the deposit hold sits in
	/// the owner's hold row this step already writes, so the close adds compute but no proof; with
	/// foreign collateral it also touches the owner's native hold row, one more key per closing
	/// step.
	#[benchmark]
	fn redeem(s: Linear<1, { T::MaxRedemptionSteps::get() }>) -> Result<(), BenchmarkError> {
		let (collateral_id, stable_id) = T::Vaults::register_market()?;
		let targets = T::Vaults::seed_redemption_targets(s, ELAPSED_MS)?;
		assert_eq!(targets.owners.len(), s as usize);
		// The fee rate is capped at 100%, so twice the debt redeems every target and leaves
		// change to refund.
		let budget = targets.total_debt.saturating_mul(2u32.into());
		let redeemer: T::AccountId = whitelisted_caller();
		let minimum_balance = T::StableAssets::minimum_balance(stable_id.clone());
		T::StableAssets::mint_into(
			stable_id.clone(),
			&redeemer,
			budget.saturating_add(minimum_balance),
		)?;
		let recipient = fresh_recipient::<T>();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(
			RawOrigin::Signed(redeemer.clone()),
			collateral_id.clone(),
			stable_id.clone(),
			RedemptionTerms {
				max_stable_to_spend: budget,
				min_collateral_out: BalanceOf::<T>::zero(),
			},
			recipient.clone(),
			s,
		);

		let executed = pallet_events::<T>().into_iter().find_map(|event| match event {
			Event::OrdinaryRedemptionExecuted { steps, stable_burned, collateral_out, .. } => {
				Some((steps, stable_burned, collateral_out))
			},
			_ => None,
		});
		let (steps, stable_burned, collateral_out) =
			executed.ok_or(BenchmarkError::Stop("no ordinary redemption executed"))?;
		assert_eq!(steps, s);
		assert_eq!(stable_burned, targets.total_debt);
		assert!(!collateral_out.is_zero());
		assert!(frame_system::Pallet::<T>::account_exists(&recipient));
		for owner in &targets.owners {
			let snapshot =
				T::Vaults::project_redemption_snapshot(&collateral_id, &stable_id, owner)?;
			assert!(snapshot.debt.is_zero());
		}
		Ok(())
	}

	/// A full below-par settlement of the `FinalRecovery` head.
	///
	/// The redeemer pays every planck it holds and the Insurance Fund pays its whole balance,
	/// so both stablecoin accounts are drained, all collateral goes to a fresh recipient and the
	/// vault closes. The head accrued interest.
	#[benchmark]
	fn redeem_recovery() -> Result<(), BenchmarkError> {
		let (collateral_id, stable_id) = T::Vaults::register_market()?;
		let head = T::Vaults::stage_recovery_head(ELAPSED_MS)?;
		T::Vaults::set_unit_price(BELOW_PAR_UNIT_PRICE);
		let price = T::Oracle::provide_price(&collateral_id)?;
		let collateral_value = recovery_pricing::collateral_value_ceil(head.collateral, price)
			.ok_or(BenchmarkError::Stop("collateral value overflowed"))?;
		assert!(!collateral_value.is_zero());
		assert!(collateral_value < head.debt);
		// The fund holds exactly the shortfall, so the settlement takes all of it.
		let shortfall = head.debt.saturating_sub(collateral_value);
		let insurance = T::InsuranceFundAccount::convert(stable_id.clone());
		assert!(T::StableAssets::balance(stable_id.clone(), &insurance).is_zero());
		T::StableAssets::mint_into(stable_id.clone(), &insurance, shortfall)?;
		// With the shortfall covered, the redeemer cancels debt worth the collateral.
		let redeemer: T::AccountId = whitelisted_caller();
		T::StableAssets::mint_into(stable_id.clone(), &redeemer, collateral_value)?;
		let recipient = fresh_recipient::<T>();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		redeem(
			RawOrigin::Signed(redeemer.clone()),
			collateral_id.clone(),
			stable_id.clone(),
			RedemptionTerms {
				max_stable_to_spend: collateral_value,
				min_collateral_out: BalanceOf::<T>::zero(),
			},
			recipient.clone(),
			1,
		);

		let executed = pallet_events::<T>().into_iter().find_map(|event| match event {
			Event::RecoveryRedemptionExecuted {
				stable_burned,
				insurance_cover,
				collateral_out,
				regime,
				..
			} => Some((stable_burned, insurance_cover, collateral_out, regime)),
			_ => None,
		});
		let (stable_burned, insurance_cover, collateral_out, regime) =
			executed.ok_or(BenchmarkError::Stop("no recovery redemption executed"))?;
		assert_eq!(regime, RecoveryRegime::InsuranceAdjusted);
		assert_eq!(stable_burned, collateral_value);
		assert_eq!(insurance_cover, shortfall);
		assert_eq!(collateral_out, head.collateral);
		assert!(T::StableAssets::balance(stable_id.clone(), &redeemer).is_zero());
		assert!(T::StableAssets::balance(stable_id.clone(), &insurance).is_zero());
		assert!(frame_system::Pallet::<T>::account_exists(&recipient));
		assert!(T::Vaults::project_redemption_snapshot(&collateral_id, &stable_id, &head.owner)
			.is_err());
		Ok(())
	}

	#[benchmark]
	fn set_redemption_config() -> Result<(), BenchmarkError> {
		let (_, stable_id) = T::Vaults::register_market()?;
		let config =
			RedemptionConfig { base_fee: Permill::from_percent(1), ..registration_config::<T>() };
		let old_config = Stablecoins::<T>::get(&stable_id)
			.map(|info| info.config)
			.ok_or(BenchmarkError::Stop("registration seeded no redemption config"))?;
		assert!(old_config != config);
		// A stablecoin owner's signed origin, where the runtime admits one, reads the owner too.
		let origin = T::UpdateOrigin::try_successful_origin(&stable_id)
			.map_err(|_| BenchmarkError::Weightless)?;
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, stable_id.clone(), config.clone());

		assert_eq!(Stablecoins::<T>::get(&stable_id).map(|info| info.config), Some(config.clone()));
		assert_eq!(
			pallet_events::<T>(),
			[Event::RedemptionConfigUpdated { stable_id, old_config, new_config: config }]
		);
		Ok(())
	}

	impl_benchmark_test_suite!(
		Pallet,
		crate::mock::ExtBuilder::default().build(),
		crate::mock::Test
	);
}
