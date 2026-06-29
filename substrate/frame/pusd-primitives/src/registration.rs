//! Branch-registration hook.

use frame::deps::frame_support::pallet_prelude::DispatchResult;

pub trait OnBranchRegistered<AssetId> {
	fn on_branch_registered(collateral_id: &AssetId) -> DispatchResult;
}

/// Run each handler in order, short-circuiting on the first error so the caller
/// can roll the registration back.
#[impl_trait_for_tuples::impl_for_tuples(8)]
impl<AssetId> OnBranchRegistered<AssetId> for Tuple {
	fn on_branch_registered(collateral_id: &AssetId) -> DispatchResult {
		for_tuples!( #( Tuple::on_branch_registered(collateral_id)?; )* );
		Ok(())
	}
}
