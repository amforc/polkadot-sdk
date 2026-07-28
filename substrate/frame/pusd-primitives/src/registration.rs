//! Market registration / deregistration lifecycle hook.

use frame::deps::{frame_support::pallet_prelude::DispatchResult, sp_runtime::Permill};

/// Lifecycle hook for `(collateral_id, stable_id)` markets. A market is one
/// stablecoin against one collateral. `pallet-vaults` calls [`on_registered`]
/// after registration with the stablecoin's new market count, and
/// [`on_deregistered`] before removal with the count that will remain. This
/// lets handlers maintain either per-market or stablecoin-wide state without
/// duplicating Vaults' market counter.
///
/// Both methods default to a no-op, so an implementer overrides only the edge
/// it cares about. Returning `Err` short-circuits the surrounding extrinsic and
/// rolls the registration (or removal) back.
///
/// [`on_registered`]: OnBranchLifecycle::on_registered
/// [`on_deregistered`]: OnBranchLifecycle::on_deregistered
pub trait OnBranchLifecycle<CollateralId, StableId> {
	/// Run when a new market is registered. `stablecoin_markets` includes it.
	fn on_registered(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		stablecoin_markets: u32,
	) -> DispatchResult {
		let _ = (collateral_id, stable_id, stablecoin_markets);
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

	/// Validate a proposed redistribution penalty before Vaults stores it.
	///
	/// Pallets whose own market parameters constrain this value can reject the
	/// change. The default accepts it.
	fn validate_redistribution_penalty(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		redistribution_penalty: Permill,
	) -> DispatchResult {
		let _ = (collateral_id, stable_id, redistribution_penalty);
		Ok(())
	}
}

/// Run each handler in order, short-circuiting on the first error so the caller
/// can roll the transaction back.
#[impl_trait_for_tuples::impl_for_tuples(8)]
impl<CollateralId, StableId> OnBranchLifecycle<CollateralId, StableId> for Tuple {
	fn on_registered(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		stablecoin_markets: u32,
	) -> DispatchResult {
		for_tuples!( #(
			Tuple::on_registered(collateral_id, stable_id, stablecoin_markets)?;
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

	fn validate_redistribution_penalty(
		collateral_id: &CollateralId,
		stable_id: &StableId,
		redistribution_penalty: Permill,
	) -> DispatchResult {
		for_tuples!( #(
			Tuple::validate_redistribution_penalty(
				collateral_id,
				stable_id,
				redistribution_penalty,
			)?;
		)* );
		Ok(())
	}
}
