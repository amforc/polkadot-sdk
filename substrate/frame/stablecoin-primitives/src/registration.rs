//! Branch registration and deregistration hooks.

use codec::{DecodeWithMemTracking, MaxEncodedLen};
use frame::deps::frame_support::pallet_prelude::{DispatchResult, Parameter};

/// Lifecycle hook for `(collateral_id, stable_id)` branches.
///
/// The vault engine calls [`on_registered`] after registration and [`on_deregistered`] before
/// removal, passing the stablecoin's branch count as of commit, so handlers need no counter of
/// their own. Both default to no-ops; an `Err` rolls back the extrinsic.
///
/// [`RegistrationConfig`] is a per-handler payload that the vault engine forwards untouched. Tuples
/// compose payloads, e.g. `(Option<RedemptionConfig<_>>, StabilityPoolConfig<_>)`.
///
/// [`on_registered`]: OnBranchLifecycle::on_registered
/// [`on_deregistered`]: OnBranchLifecycle::on_deregistered
/// [`RegistrationConfig`]: OnBranchLifecycle::RegistrationConfig
pub trait OnBranchLifecycle<CollateralId, StableId, AccountId> {
	/// Handler-specific registration payload; a tuple of payloads for tuple implementations.
	type RegistrationConfig: Parameter + MaxEncodedLen + DecodeWithMemTracking;

	/// Called after a branch is registered. `stablecoin_markets` includes it, so `1` marks the
	/// stablecoin's first branch. Handlers with per-stablecoin state should key off this count,
	/// not their own storage, so callers can predict the outcome.
	///
	/// `funder` pays any refundable setup cost, such as an asset account deposit: the depositor of
	/// a signed creation, otherwise the branch's full administrator.
	fn on_registered(
		_collateral_id: &CollateralId,
		_stable_id: &StableId,
		_stablecoin_markets: u32,
		_config: Self::RegistrationConfig,
		_funder: &AccountId,
	) -> DispatchResult {
		Ok(())
	}

	/// Called before an empty branch is removed. `remaining_stablecoin_markets` excludes it.
	///
	/// `funder` receives any refundable setup cost that [`on_registered`] charged, resolved the
	/// same way the registration resolved its payer.
	///
	/// [`on_registered`]: OnBranchLifecycle::on_registered
	fn on_deregistered(
		_collateral_id: &CollateralId,
		_stable_id: &StableId,
		_remaining_stablecoin_markets: u32,
		_funder: &AccountId,
	) -> DispatchResult {
		Ok(())
	}

	/// Builds a valid [`on_registered`] payload for the stablecoin's `stablecoin_markets`-th
	/// branch, covering handlers whose payload differs between the first and later branches.
	///
	/// [`on_registered`]: OnBranchLifecycle::on_registered
	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_registration_config(stablecoin_markets: u32) -> Self::RegistrationConfig;

	/// Stages worst-case removal of an empty branch, including custody dust and teardown rows.
	/// Defaults to no setup.
	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_stage_deregistration(
		_collateral_id: &CollateralId,
		_stable_id: &StableId,
	) -> DispatchResult {
		Ok(())
	}
}

/// Runs each handler in order, stopping at the first error.
#[impl_trait_for_tuples::impl_for_tuples(8)]
impl<CollateralId, StableId, AccountId> OnBranchLifecycle<CollateralId, StableId, AccountId>
	for Tuple
{
	// Composed tuple fields need `'static` payload types (E0310).
	for_tuples!( where #( Tuple::RegistrationConfig: 'static )* );

	for_tuples!( type RegistrationConfig = ( #( Tuple::RegistrationConfig ),* ); );

	fn on_registered(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		stablecoin_markets: u32,
		config: Self::RegistrationConfig,
		funder: &AccountId,
	) -> DispatchResult {
		for_tuples!( #(
			Tuple::on_registered(
				collateral_id,
				stable_id,
				stablecoin_markets,
				config.Tuple,
				funder,
			)?;
		)* );
		Ok(())
	}

	fn on_deregistered(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		remaining_stablecoin_markets: u32,
		funder: &AccountId,
	) -> DispatchResult {
		for_tuples!( #(
			Tuple::on_deregistered(
				collateral_id,
				stable_id,
				remaining_stablecoin_markets,
				funder,
			)?;
		)* );
		Ok(())
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_registration_config(stablecoin_markets: u32) -> Self::RegistrationConfig {
		for_tuples!( ( #( Tuple::benchmark_registration_config(stablecoin_markets) ),* ) )
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_stage_deregistration(
		collateral_id: &CollateralId,
		stable_id: &StableId,
	) -> DispatchResult {
		for_tuples!( #( Tuple::benchmark_stage_deregistration(collateral_id, stable_id)?; )* );
		Ok(())
	}
}
