//! Branch-registration hook.

use frame::deps::frame_support::pallet_prelude::DispatchResult;

pub trait OnBranchRegistered<AssetId> {
	fn on_branch_registered(collateral_id: &AssetId) -> DispatchResult;
}

/// No-op: useful for runtimes with no sibling pallets to seed yet.
impl<AssetId> OnBranchRegistered<AssetId> for () {
	fn on_branch_registered(_collateral_id: &AssetId) -> DispatchResult {
		Ok(())
	}
}

/// Run each handler in order, short-circuiting on the first error so the caller
/// can roll the registration back.
macro_rules! impl_on_branch_registered_tuple {
	($($handler:ident),+) => {
		impl<AssetId, $($handler: OnBranchRegistered<AssetId>),+> OnBranchRegistered<AssetId>
			for ($($handler,)+)
		{
			fn on_branch_registered(collateral_id: &AssetId) -> DispatchResult {
				$($handler::on_branch_registered(collateral_id)?;)+
				Ok(())
			}
		}
	};
}

impl_on_branch_registered_tuple!(A);
impl_on_branch_registered_tuple!(A, B);
impl_on_branch_registered_tuple!(A, B, C);
impl_on_branch_registered_tuple!(A, B, C, D);
impl_on_branch_registered_tuple!(A, B, C, D, E);
