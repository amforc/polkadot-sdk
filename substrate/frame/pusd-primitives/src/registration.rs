//! Market registration and deregistration hooks.

use codec::{DecodeWithMemTracking, MaxEncodedLen};
use frame::deps::frame_support::pallet_prelude::{DispatchResult, Parameter};

/// Lifecycle hook for `(collateral_id, stable_id)` markets.
///
/// `pallet-vaults` calls [`on_registered`] after registration and [`on_deregistered`] before
/// removal, passing the stablecoin's market count as of commit, so handlers need no counter of
/// their own. Both default to no-ops; an `Err` rolls back the extrinsic.
///
/// [`RegistrationConfig`] is a per-handler payload that Vaults forwards untouched. Tuples compose
/// payloads, e.g. `(Option<RedemptionConfig<_>>, StabilityPoolConfig<_>)`.
///
/// [`on_registered`]: OnBranchLifecycle::on_registered
/// [`on_deregistered`]: OnBranchLifecycle::on_deregistered
/// [`RegistrationConfig`]: OnBranchLifecycle::RegistrationConfig
pub trait OnBranchLifecycle<CollateralId, StableId, AccountId> {
	/// Handler-specific registration payload; a tuple of payloads for tuple implementations.
	type RegistrationConfig: Parameter + MaxEncodedLen + DecodeWithMemTracking;

	/// Called after a market is registered. `stablecoin_markets` includes it, so `1` marks the
	/// stablecoin's first market. Handlers with per-stablecoin state should key off this count,
	/// not their own storage, so callers can predict the outcome.
	///
	/// `funder` pays any refundable setup cost, such as an asset account deposit: the depositor of
	/// a signed creation, otherwise the market's full administrator.
	fn on_registered(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		stablecoin_markets: u32,
		config: Self::RegistrationConfig,
		funder: &AccountId,
	) -> DispatchResult {
		let _ = (collateral_id, stable_id, stablecoin_markets, config, funder);
		Ok(())
	}

	/// Called before an empty market is removed. `remaining_stablecoin_markets` excludes it.
	fn on_deregistered(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		remaining_stablecoin_markets: u32,
	) -> DispatchResult {
		let _ = (collateral_id, stable_id, remaining_stablecoin_markets);
		Ok(())
	}

	/// Builds a valid [`on_registered`] payload for the stablecoin's `stablecoin_markets`-th
	/// market, covering handlers whose payload differs between the first and later markets.
	///
	/// [`on_registered`]: OnBranchLifecycle::on_registered
	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_registration_config(stablecoin_markets: u32) -> Self::RegistrationConfig;
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
	) -> DispatchResult {
		for_tuples!( #(
			Tuple::on_deregistered(
				collateral_id,
				stable_id,
				remaining_stablecoin_markets,
			)?;
		)* );
		Ok(())
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_registration_config(stablecoin_markets: u32) -> Self::RegistrationConfig {
		for_tuples!( ( #( Tuple::benchmark_registration_config(stablecoin_markets) ),* ) )
	}
}
