//! Benchmarks for `pallet-vaults`.
//!
//! Every benchmark stages the costliest path of its call:
//!
//! - Parameterized rate-index calls vary the repair walk across its full budget. Others get a hint
//!   exactly `hint_repair_budget` steps stale, asserted before the call, so the full repair walk is
//!   measured.
//! - Vault calls first absorb a pending redistribution share, which moves collateral out of the
//!   redistribution account.
//! - Calls that issue interest find an hour of branch interest due.
//! - Payouts go to accounts that do not exist yet.
//!
//! Collateral is counted in collateral units and prices are quoted per unit (see
//! [`fixtures::collateral_unit`]), so the benchmarks run on runtimes with a production
//! existential deposit.

#![cfg(feature = "runtime-benchmarks")]

pub(crate) mod fixtures;
mod seeding;

use crate::{
	pallet::{
		BalanceOf, Branches, CollateralIdOf, Config, GlobalDebtCeilings, Pallet, StablecoinMarkets,
		Vaults,
	},
	types::{BranchAdmins, BranchConfigUpdate, VaultStatus},
	BenchmarkConfig,
};
use fixtures::*;
use frame::{
	arithmetic::FixedU128,
	benchmarking::prelude::*,
	deps::sp_runtime::DispatchError,
	traits::{
		fungibles::{Inspect as FungiblesInspect, Mutate as FungiblesMutate},
		Zero,
	},
};
use frame_system::RawOrigin;
use linked_list_interface::{Position, SortedListInterface};
use stablecoin_primitives::{BenchmarkVault, OnBranchLifecycle, VaultInterface, MILLIS_PER_YEAR};

/// Debt of the vault that keeps the `close_vault` branch alive.
const CLOSE_BACKGROUND_DEBT: u128 = 1_000_000;
fn vault_of<T: BenchmarkConfig>(
	owner: &T::AccountId,
) -> Result<crate::Vault<BalanceOf<T>>, DispatchError> {
	Pallet::<T>::vault_of(&collateral::<T>(), &stable::<T>(), owner)
}

fn collateral_balance<T: BenchmarkConfig>(who: &T::AccountId) -> BalanceOf<T> {
	<T::CollateralAssets as FungiblesInspect<T::AccountId>>::balance(collateral::<T>(), who)
}

fn assert_insert_walk<T: BenchmarkConfig>(priority: FixedU128, hint: &Position<T::AccountId>) {
	let steps = T::VaultLists::repair_steps_needed(&rate_list::<T>(), priority, hint.clone());
	assert_eq!(steps, T::VaultLists::repair_budget());
}

/// Registers the default branch and opens `owner`'s vault, leaves it a pending redistribution
/// share, and lets an hour of interest accrue. Returns the branch's collateral.
fn seed_touched_vault<T: BenchmarkConfig>(
	owner: &T::AccountId,
	collateral_units: u128,
) -> Result<CollateralIdOf<T>, DispatchError> {
	let asset = register_default_branch::<T>()?;
	open_funded_vault::<T>(owner, BenchmarkVault { collateral_units, ..default_vault::<T>() })?;
	seed_pending_redistribution::<T>()?;
	advance_time::<T>(ONE_HOUR_MS);
	Ok(asset)
}

/// Registers the default branch with one default vault and lets a year of interest accrue. Returns
/// the branch's collateral.
fn seed_accrued_market<T: BenchmarkConfig>() -> Result<CollateralIdOf<T>, DispatchError> {
	let asset = register_default_branch::<T>()?;
	open_funded_vault::<T>(&account("owner", 0, 0), default_vault::<T>())?;
	advance_time::<T>(MILLIS_PER_YEAR);
	Ok(asset)
}

#[benchmarks(where T: BenchmarkConfig)]
mod benchmarks {
	use super::*;
	use crate::pallet::Call;

	#[benchmark]
	fn open_vault(s: Linear<0, { T::VaultLists::repair_budget() }>) -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let WorstCaseChain { seeds, bounds, .. } = seed_worst_case_chain::<T>()?;
		let hint = seed_gap(&seeds, s);
		let caller = funded_caller::<T>()?;
		let collateral = collateral_units::<T>(SEED_COLL);
		let debt = balance::<T>(SEED_DEBT);
		let new_rate = bounds.interior();
		assert_eq!(
			T::VaultLists::repair_steps_needed(&rate_list::<T>(), new_rate, hint.clone()),
			s,
		);
		advance_time::<T>(ONE_HOUR_MS);
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(
			RawOrigin::Signed(caller.clone()),
			asset.clone(),
			stable::<T>(),
			collateral,
			debt,
			new_rate,
			hint,
		);

		let vault = vault_of::<T>(&caller)?;
		assert_eq!(vault.collateral, collateral);
		assert!(vault.debt.total() >= debt);
		let neighbours = T::VaultLists::neighbors(&rate_list::<T>(), &caller);
		assert_eq!(neighbours, Some(seed_gap(&seeds, 0)));
		assert!(interest_issued::<T>());
		Ok(())
	}

	#[benchmark]
	fn deposit_collateral_for() -> Result<(), BenchmarkError> {
		let owner: T::AccountId = account("owner", 0, 0);
		let asset = seed_touched_vault::<T>(&owner, SEED_COLL)?;
		let caller = funded_caller::<T>()?;
		let deposit = collateral_units::<T>(SEED_COLL);
		let before = vault_of::<T>(&owner)?;
		let held = redistribution_held::<T>();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone(), deposit);

		let after = vault_of::<T>(&owner)?;
		assert!(after.collateral > before.collateral.saturating_add(deposit));
		assert_worst_case_touch::<T>(held);
		Ok(())
	}

	/// A partial withdrawal. A withdrawal that empties a debt-free vault closes it instead, which
	/// the call is charged through `close_vault`.
	#[benchmark]
	fn withdraw_collateral() -> Result<(), BenchmarkError> {
		let caller: T::AccountId = account("caller", 0, 0);
		let asset = seed_touched_vault::<T>(&caller, SEED_COLL * 10)?;
		let withdraw = collateral_units::<T>(SEED_COLL);
		let recipient: T::AccountId = account("recipient", 0, 0);
		assert!(collateral_balance::<T>(&recipient).is_zero());
		let before = vault_of::<T>(&caller)?;
		let held = redistribution_held::<T>();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(
			RawOrigin::Signed(caller.clone()),
			asset.clone(),
			stable::<T>(),
			withdraw,
			Some(recipient.clone()),
		);

		assert_eq!(collateral_balance::<T>(&recipient), withdraw);
		let after = vault_of::<T>(&caller)?;
		assert!(after.collateral > before.collateral.saturating_sub(withdraw));
		assert_worst_case_touch::<T>(held);
		Ok(())
	}

	#[benchmark]
	fn borrow() -> Result<(), BenchmarkError> {
		let MidChainCaller { asset, caller, .. } = seed_mid_chain_caller::<T>()?;
		let before = vault_of::<T>(&caller)?;
		let neighbours = T::VaultLists::neighbors(&rate_list::<T>(), &caller);
		let held = redistribution_held::<T>();
		let recipient: T::AccountId = account("borrow_recipient", 0, 0);
		let amount = balance::<T>(SEED_DEBT);
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(
			RawOrigin::Signed(caller.clone()),
			asset,
			stable::<T>(),
			amount,
			Some(before.annual_rate),
			Some(recipient.clone()),
			Position::default(),
		);

		assert_eq!(T::StableAssets::balance(stable::<T>(), &recipient), amount);
		let after = vault_of::<T>(&caller)?;
		assert!(after.debt.total() > before.debt.total().saturating_add(amount));
		assert_eq!(after.annual_rate, before.annual_rate);
		assert_eq!(T::VaultLists::neighbors(&rate_list::<T>(), &caller), neighbours);
		assert_worst_case_touch::<T>(held);
		Ok(())
	}

	#[benchmark]
	fn borrow_activate(
		s: Linear<0, { T::VaultLists::repair_budget() }>,
	) -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let WorstCaseChain { seeds, bounds, .. } = seed_worst_case_chain::<T>()?;
		let caller: T::AccountId = account("caller", 0, 0);
		open_funded_vault::<T>(
			&caller,
			BenchmarkVault { collateral_units: SEED_COLL * 10, ..default_vault::<T>() },
		)?;
		redeem_debt_only::<T>(&caller)?;
		assert_eq!(status_of::<T>(&caller), VaultStatus::Dormant);
		let rate = bounds.interior();
		let hint = seed_gap(&seeds, s);
		assert_eq!(T::VaultLists::repair_steps_needed(&rate_list::<T>(), rate, hint.clone()), s);
		seed_pending_redistribution::<T>()?;
		advance_time::<T>(ONE_HOUR_MS);
		let held = redistribution_held::<T>();
		let recipient: T::AccountId = account("borrow_recipient", 0, 0);
		let amount = balance::<T>(SEED_DEBT);
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		borrow(
			RawOrigin::Signed(caller.clone()),
			asset,
			stable::<T>(),
			amount,
			Some(rate),
			Some(recipient.clone()),
			hint,
		);

		assert_eq!(status_of::<T>(&caller), VaultStatus::Active);
		assert_eq!(T::StableAssets::balance(stable::<T>(), &recipient), amount);
		assert_eq!(vault_of::<T>(&caller)?.annual_rate, rate);
		assert_eq!(T::VaultLists::neighbors(&rate_list::<T>(), &caller), Some(seed_gap(&seeds, 0)));
		assert_worst_case_touch::<T>(held);
		Ok(())
	}

	#[benchmark]
	fn borrow_relocate(
		s: Linear<0, { T::VaultLists::repair_budget() }>,
	) -> Result<(), BenchmarkError> {
		let MidChainCaller { asset, caller, seeds, interior_rate, .. } =
			seed_mid_chain_caller::<T>()?;
		let extra_debt = balance::<T>(SEED_DEBT);
		let recipient: T::AccountId = account("borrow_recipient", 0, 0);
		assert!(T::StableAssets::balance(stable::<T>(), &recipient).is_zero());
		let interior_hint = seed_gap(&seeds, s);
		assert_eq!(
			T::VaultLists::re_insert_steps_needed(
				&rate_list::<T>(),
				&caller,
				interior_rate,
				interior_hint.clone()
			),
			s
		);
		let before = vault_of::<T>(&caller)?;
		let held = redistribution_held::<T>();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		borrow(
			RawOrigin::Signed(caller.clone()),
			asset.clone(),
			stable::<T>(),
			extra_debt,
			Some(interior_rate),
			Some(recipient.clone()),
			interior_hint,
		);

		assert_eq!(T::StableAssets::balance(stable::<T>(), &recipient), extra_debt);
		let after = vault_of::<T>(&caller)?;
		assert!(after.debt.total() > before.debt.total().saturating_add(extra_debt));
		assert_eq!(after.annual_rate, interior_rate);
		let neighbours = T::VaultLists::neighbors(&rate_list::<T>(), &caller);
		assert_eq!(neighbours, Some(seed_gap(&seeds, 0)));
		assert_worst_case_touch::<T>(held);
		Ok(())
	}

	/// A full payoff from an interior node, which takes the vault out of the rate index.
	#[benchmark]
	fn repay_for() -> Result<(), BenchmarkError> {
		let MidChainCaller { asset, caller: owner, .. } = seed_mid_chain_caller::<T>()?;
		let payoff = projected_debt::<T>(&owner)?;
		let payer: T::AccountId = account("payer", 0, 0);
		T::StableAssets::mint_into(stable::<T>(), &payer, payoff)?;
		let held = redistribution_held::<T>();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(RawOrigin::Signed(payer.clone()), asset.clone(), stable::<T>(), owner.clone(), None);

		assert!(T::StableAssets::balance(stable::<T>(), &payer).is_zero());
		assert!(vault_of::<T>(&owner)?.debt.total().is_zero());
		assert_eq!(status_of::<T>(&owner), VaultStatus::Dormant);
		assert!(!T::VaultLists::contains(&rate_list::<T>(), &owner));
		assert_worst_case_touch::<T>(held);
		Ok(())
	}

	#[benchmark]
	fn change_rate() -> Result<(), BenchmarkError> {
		let MidChainCaller { asset, caller, .. } = seed_mid_chain_caller::<T>()?;
		let rate = vault_of::<T>(&caller)?.annual_rate;
		let neighbours = T::VaultLists::neighbors(&rate_list::<T>(), &caller);
		let held = redistribution_held::<T>();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), asset, stable::<T>(), rate, Position::default());

		assert_eq!(vault_of::<T>(&caller)?.annual_rate, rate);
		assert_eq!(T::VaultLists::neighbors(&rate_list::<T>(), &caller), neighbours);
		assert_worst_case_touch::<T>(held);
		Ok(())
	}

	#[benchmark]
	fn change_rate_relocate(
		s: Linear<0, { T::VaultLists::repair_budget() }>,
	) -> Result<(), BenchmarkError> {
		let MidChainCaller { asset, caller, seeds, interior_rate, .. } =
			seed_mid_chain_caller::<T>()?;
		let interior_hint = seed_gap(&seeds, s);
		assert_eq!(
			T::VaultLists::re_insert_steps_needed(
				&rate_list::<T>(),
				&caller,
				interior_rate,
				interior_hint.clone()
			),
			s
		);
		let held = redistribution_held::<T>();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		change_rate(
			RawOrigin::Signed(caller.clone()),
			asset.clone(),
			stable::<T>(),
			interior_rate,
			interior_hint,
		);

		assert_eq!(vault_of::<T>(&caller)?.annual_rate, interior_rate);
		let neighbours = T::VaultLists::neighbors(&rate_list::<T>(), &caller);
		assert_eq!(neighbours, Some(seed_gap(&seeds, 0)));
		assert_worst_case_touch::<T>(held);
		Ok(())
	}

	/// A debt-free vault cannot carry a pending redistribution share: absorbing one would give it
	/// debt, which `close_vault` rejects. Only the branch accrual and the payout are staged.
	#[benchmark]
	fn close_vault() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		// A second vault prevents residual interest drift from triggering Safety mode on close.
		// Its debt must also yield whole units of interest over the hour.
		let background: T::AccountId = account("background", 0, 0);
		let background_vault =
			BenchmarkVault { debt: balance::<T>(CLOSE_BACKGROUND_DEBT), ..default_vault::<T>() };
		open_funded_vault::<T>(&background, background_vault)?;
		let caller: T::AccountId = account("caller", 0, 0);
		open_funded_vault::<T>(&caller, default_vault::<T>())?;
		// `close_vault` needs a debt-free vault with collateral still held.
		redeem_debt_only::<T>(&caller)?;
		advance_time::<T>(ONE_HOUR_MS);
		let collateral = vault_of::<T>(&caller)?.collateral;
		let recipient: T::AccountId = account("recipient", 0, 0);
		assert!(collateral_balance::<T>(&recipient).is_zero());
		let fees = fee_balance::<T>();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), asset.clone(), stable::<T>(), Some(recipient.clone()));

		assert!(!Vaults::<T>::contains_key((&asset, &stable::<T>(), &caller)));
		assert_eq!(collateral_balance::<T>(&recipient), collateral);
		assert!(fee_balance::<T>() > fees);
		assert!(interest_issued::<T>());
		Ok(())
	}

	#[benchmark]
	fn poke() -> Result<(), BenchmarkError> {
		let owner: T::AccountId = account("owner", 0, 0);
		let asset = seed_touched_vault::<T>(&owner, SEED_COLL)?;
		let caller: T::AccountId = whitelisted_caller();
		let before = vault_of::<T>(&owner)?;
		let held = redistribution_held::<T>();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone());

		let after = vault_of::<T>(&owner)?;
		assert!(after.collateral > before.collateral);
		assert!(after.debt.total() > before.debt.total());
		assert_worst_case_touch::<T>(held);
		Ok(())
	}

	/// Entry behind a queued vault. No pending redistribution share: the first head needs a branch
	/// with no other stake bearer.
	#[benchmark]
	fn enter_final_recovery() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let prior = recovery_cycle::<T>(0, FixedU128::from_rational(5, 100))?;
		let owner: T::AccountId = account("target", 0, 0);
		let vault =
			BenchmarkVault { collateral_units: RECOVERY_VAULT_COLL, ..default_vault::<T>() };
		open_funded_vault::<T>(&owner, vault)?;
		advance_time::<T>(ONE_HOUR_MS);
		set_unit_price::<T>(RECOVERY_TRIGGER_PRICE);
		let caller: T::AccountId = whitelisted_caller();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), asset.clone(), stable::<T>(), owner.clone());

		assert_eq!(status_of::<T>(&owner), VaultStatus::FinalRecovery);
		assert_eq!(T::VaultLists::count(&recovery_list::<T>()), 2);
		assert_eq!(T::VaultLists::tail(&recovery_list::<T>()), Some(prior));
		assert!(interest_issued::<T>());
		Ok(())
	}

	/// The exiting vault sits mid-FIFO and re-enters the rate index between two seeds through the
	/// full repair walk.
	#[benchmark]
	fn exit_final_recovery() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let interior_rate = rate_bounds::<T>()?.interior();
		let first = recovery_cycle::<T>(0, FixedU128::from_rational(5, 100))?;
		let owner = recovery_cycle::<T>(1, interior_rate)?;
		let _last = recovery_cycle::<T>(2, FixedU128::from_rational(5, 100))?;
		let WorstCaseChain { seeds, interior_hint: hint, .. } = seed_worst_case_chain::<T>()?;
		assert_insert_walk::<T>(interior_rate, &hint);
		advance_time::<T>(ONE_HOUR_MS);
		let caller: T::AccountId = whitelisted_caller();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone(), hint);

		assert_eq!(status_of::<T>(&owner), VaultStatus::Active);
		let neighbours = T::VaultLists::neighbors(&rate_list::<T>(), &owner);
		assert_eq!(neighbours, Some(seed_gap(&seeds, 0)));
		assert_eq!(T::VaultLists::count(&recovery_list::<T>()), 2);
		let next =
			<Pallet<T> as VaultInterface>::next_redemption_target(&asset, &stable::<T>(), None);
		assert_eq!(next, Some((first, VaultStatus::FinalRecovery)));
		assert!(interest_issued::<T>());
		Ok(())
	}

	/// The Dormant target absorbs a pending redistribution share that lifts it back above the
	/// minimum debt, then re-enters the rate index between two seeds through the full repair walk.
	#[benchmark]
	fn activate_dormant() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let interior_rate = rate_bounds::<T>()?.interior();
		let owner: T::AccountId = account("target", 0, 0);
		let target_debt = balance::<T>(SEED_DEBT);
		let vault = BenchmarkVault { annual_rate: interior_rate, ..default_vault::<T>() };
		open_funded_vault::<T>(&owner, vault)?;
		repay_in_full::<T>(&owner, target_debt)?;
		assert_eq!(status_of::<T>(&owner), VaultStatus::Dormant);
		// The target is the only other stake bearer, so it takes the victim's whole debt, which
		// clears the minimum debt.
		seed_pending_redistribution::<T>()?;
		let WorstCaseChain { seeds, interior_hint: hint, .. } = seed_worst_case_chain::<T>()?;
		assert_insert_walk::<T>(interior_rate, &hint);
		advance_time::<T>(ONE_HOUR_MS);
		let before = vault_of::<T>(&owner)?;
		assert!(before.debt.total().is_zero());
		let held = redistribution_held::<T>();
		let caller: T::AccountId = whitelisted_caller();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>(), owner.clone(), hint);

		assert_eq!(status_of::<T>(&owner), VaultStatus::Active);
		let neighbours = T::VaultLists::neighbors(&rate_list::<T>(), &owner);
		assert_eq!(neighbours, Some(seed_gap(&seeds, 0)));
		let after = vault_of::<T>(&owner)?;
		assert!(after.collateral > before.collateral);
		assert!(
			after.debt.total() >=
				Pallet::<T>::branch_of(&asset, &stable::<T>())?.config.minimum_debt
		);
		assert_worst_case_touch::<T>(held);
		Ok(())
	}

	/// Signed by the stablecoin owner, so the creation deposit is taken and the custody seeds
	/// and stablecoin float are charged to it.
	#[benchmark]
	fn create_branch() -> Result<(), BenchmarkError> {
		let BranchRegistration { asset, origin, funder, signed } =
			prepare_branch_registration::<T>()?;
		let config = default_branch_config::<T>();
		let admins = branch_admins::<T>();
		let funder_collateral = collateral_balance::<T>(&funder);

		#[extrinsic_call]
		_(
			origin as T::RuntimeOrigin,
			asset.clone(),
			stable::<T>(),
			admins,
			config,
			first_market_lifecycle_config::<T>(),
		);

		let branch = Pallet::<T>::branch_of(&asset, &stable::<T>())?;
		assert_eq!(branch.deposit.is_some(), signed);
		let stable_key = T::StableToCollateralId::convert(stable::<T>());
		assert_eq!(StablecoinMarkets::<T>::get(&stable_key), Some(1));
		let custody = Pallet::<T>::redistribution_account(&asset, &stable::<T>());
		let seed = T::CollateralAssets::minimum_balance(asset.clone());
		assert_eq!(collateral_balance::<T>(&custody), seed);
		assert!(collateral_balance::<T>(&funder) <= funder_collateral.saturating_sub(seed));
		Ok(())
	}

	/// Removes a signed branch, so its deposit is released, after every lifecycle handler staged
	/// its costliest teardown.
	#[benchmark]
	fn remove_branch() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		T::OnBranchLifecycle::benchmark_stage_deregistration(&asset, &stable::<T>())?;
		let signed = create_origin::<T>()?.as_signer().is_some();
		assert_eq!(Pallet::<T>::branch_of(&asset, &stable::<T>())?.deposit.is_some(), signed);
		let origin = full_admin_origin::<T>();

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, asset.clone(), stable::<T>());

		assert!(!Branches::<T>::contains_key(&asset, &stable::<T>()));
		let stable_key = T::StableToCollateralId::convert(stable::<T>());
		assert!(!StablecoinMarkets::<T>::contains_key(&stable_key));
		let custody = Pallet::<T>::redistribution_account(&asset, &stable::<T>());
		assert!(collateral_balance::<T>(&custody).is_zero());
		Ok(())
	}

	#[benchmark]
	fn set_branch_admins() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let origin = full_admin_origin::<T>();
		let new_full: T::AccountId = account("new_full_admin", 0, 0);
		let new_emergency: T::AccountId = account("new_emergency_admin", 0, 0);
		let admins = BranchAdmins {
			full_admin: T::Lookup::unlookup(new_full.clone()),
			emergency_admin: T::Lookup::unlookup(new_emergency.clone()),
		};

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, asset.clone(), stable::<T>(), admins);

		let branch = Pallet::<T>::branch_of(&asset, &stable::<T>())?;
		assert_eq!(
			branch.admins,
			BranchAdmins { full_admin: new_full, emergency_admin: new_emergency }
		);
		Ok(())
	}

	/// Signed by the stablecoin owner, which reads the asset's owner.
	#[benchmark]
	fn set_global_debt_ceiling() -> Result<(), BenchmarkError> {
		let stable_id = stable::<T>();
		ensure_stable_asset::<T>()?;
		let origin = create_origin::<T>()?;
		let ceiling = balance::<T>(GLOBAL_CEILING);

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, stable_id.clone(), ceiling);

		assert_eq!(GlobalDebtCeilings::<T>::get(&stable_id), ceiling);
		Ok(())
	}

	/// A live branch issuing a year of interest. A dead oracle would freeze the branch instead,
	/// which issues nothing.
	#[benchmark]
	fn refresh_branch() -> Result<(), BenchmarkError> {
		let asset = seed_accrued_market::<T>()?;
		let fees = fee_balance::<T>();
		let caller: T::AccountId = whitelisted_caller();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), asset.clone(), stable::<T>());

		let branch = Pallet::<T>::branch_of(&asset, &stable::<T>())?;
		assert!(branch.state.frozen.is_none());
		assert!(fee_balance::<T>() > fees);
		assert!(interest_issued::<T>());
		Ok(())
	}

	#[benchmark]
	fn set_param() -> Result<(), BenchmarkError> {
		let asset = register_default_branch::<T>()?;
		let origin = full_admin_origin::<T>();
		// Keep below the 120% borrow ratio, or new vaults would open liquidatable.
		let new_value = FixedU128::from_rational(115, 100);

		#[extrinsic_call]
		_(
			origin as T::RuntimeOrigin,
			asset.clone(),
			stable::<T>(),
			BranchConfigUpdate::MinimumCollateralizationRatio(new_value),
		);

		let config = Pallet::<T>::branch_of(&asset, &stable::<T>())?.config;
		assert_eq!(config.minimum_collateralization_ratio, new_value);
		Ok(())
	}

	/// Freezing is the heavier direction: it first issues the branch's pending interest.
	#[benchmark]
	fn set_governance_frozen() -> Result<(), BenchmarkError> {
		let asset = seed_accrued_market::<T>()?;
		let origin = full_admin_origin::<T>();
		let fees = fee_balance::<T>();
		frame_system::Pallet::<T>::reset_events();

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, asset.clone(), stable::<T>(), true);

		let branch = Pallet::<T>::branch_of(&asset, &stable::<T>())?;
		assert!(branch.state.frozen.is_some());
		assert!(fee_balance::<T>() > fees);
		assert!(interest_issued::<T>());
		Ok(())
	}

	impl_benchmark_test_suite!(
		Pallet,
		crate::mock::ExtBuilder::default().build(),
		crate::mock::Test
	);
}
