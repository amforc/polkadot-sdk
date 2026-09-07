//! Market registration / deregistration lifecycle hook.

use codec::{DecodeWithMemTracking, MaxEncodedLen};
use frame::deps::frame_support::pallet_prelude::{DispatchResult, Parameter};

/// Lifecycle hook for `(collateral_id, stable_id)` markets. A market is one
/// stablecoin against one collateral. `pallet-vaults` calls [`on_registered`]
/// after registration and [`on_deregistered`] before removal, both with the
/// stablecoin's market count once the call commits. This lets handlers maintain
/// either per-market or stablecoin-wide state without duplicating Vaults'
/// market counter.
///
/// [`RegistrationConfig`] is the handler's own registration payload. Vaults
/// forwards it without interpreting it, so denomination-sensitive amounts stay
/// with the pallet that stores them. Tuple composition combines handler
/// payloads, e.g. `(Option<RedemptionConfig<_>>, StabilityPoolConfig<_>)`.
///
/// Both lifecycle methods default to a no-op, so an implementer overrides only
/// the edge it cares about. Returning `Err` short-circuits the surrounding
/// extrinsic and rolls the registration (or removal) back.
///
/// [`on_registered`]: OnBranchLifecycle::on_registered
/// [`on_deregistered`]: OnBranchLifecycle::on_deregistered
/// [`RegistrationConfig`]: OnBranchLifecycle::RegistrationConfig
pub trait OnBranchLifecycle<CollateralId, StableId, AccountId> {
	/// Handler-specific configuration supplied at market registration.
	///
	/// Tuple implementers compose this as a tuple of the inner payloads.
	type RegistrationConfig: Parameter + MaxEncodedLen + DecodeWithMemTracking;

	/// Run when a new market is registered. `stablecoin_markets` includes it, so a count of one
	/// is the market that seeds whatever the handler keeps per stablecoin. Handlers that keep
	/// such state decide from that count rather than from their own storage, so the rule they
	/// enforce is the one a caller can predict.
	///
	/// `funder` is the account charged for any refundable setup cost the handler takes, such as
	/// an asset account deposit. Vaults resolves it the same way it funds its own collateral
	/// custody: the depositor a signed creation charged, and the market's full administrator
	/// otherwise. A handler can therefore always name a payer, whoever created the market.
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

	/// Run when an empty market is removed. `remaining_stablecoin_markets`
	/// excludes it.
	fn on_deregistered(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		remaining_stablecoin_markets: u32,
	) -> DispatchResult {
		let _ = (collateral_id, stable_id, remaining_stablecoin_markets);
		Ok(())
	}

	/// Builds a payload [`on_registered`] accepts for the `stablecoin_markets`-th
	/// market of a stablecoin.
	///
	/// The count is the same one [`on_registered`] receives, so a handler whose
	/// payload differs between the first market and later ones stays
	/// constructible for both.
	///
	/// [`on_registered`]: OnBranchLifecycle::on_registered
	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_registration_config(stablecoin_markets: u32) -> Self::RegistrationConfig;
}

/// Run each handler in order, short-circuiting on the first error so the caller
/// can roll the transaction back.
#[impl_trait_for_tuples::impl_for_tuples(8)]
impl<CollateralId, StableId, AccountId> OnBranchLifecycle<CollateralId, StableId, AccountId>
	for Tuple
{
	// Each payload becomes a field of the composed tuple, so without this the projected
	// associated types outlive nothing the compiler can name (E0310).
	for_tuples!( where #( Tuple::RegistrationConfig: 'static )* );

	for_tuples!( type RegistrationConfig = ( #( Tuple::RegistrationConfig ),* ); );

	fn on_registered(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		stablecoin_markets: u32,
		config: Self::RegistrationConfig,
		funder: &AccountId,
	) -> DispatchResult {
		// Each handler takes its own field by value, so the composed tuple is moved apart
		// field by field.
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
