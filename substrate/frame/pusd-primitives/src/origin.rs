//! Origin checks for the pUSD pallets.

use core::marker::PhantomData;
use frame::{
	deps::frame_system::RawOrigin,
	traits::{fungibles::roles::Inspect as RolesInspect, EnsureOriginWithArg, OriginTrait},
};

/// Admits `Root` or the owner of the stablecoin passed as the argument.
///
/// `Root` yields `None` (no deposit); the owner, as reported by `Assets` through
/// [`fungibles::roles::Inspect`](frame::traits::fungibles::roles::Inspect), yields `Some(owner)`
/// (refundable deposit). Other origins are returned unchanged. The benchmark origin is `Root`.
///
/// ```ignore
/// type CreateOrigin = pusd_primitives::EnsureStableOwnerOrRoot<Assets, AccountId>;
/// ```
pub struct EnsureStableOwnerOrRoot<Assets, AccountId>(PhantomData<(Assets, AccountId)>);

impl<OuterOrigin, Assets, AccountId> EnsureOriginWithArg<OuterOrigin, Assets::AssetId>
	for EnsureStableOwnerOrRoot<Assets, AccountId>
where
	OuterOrigin: OriginTrait<AccountId = AccountId>,
	Assets: RolesInspect<AccountId>,
	AccountId: Clone + PartialEq,
{
	type Success = Option<AccountId>;

	fn try_origin(
		origin: OuterOrigin,
		stable_id: &Assets::AssetId,
	) -> Result<Self::Success, OuterOrigin> {
		let success = match origin.as_system_ref() {
			Some(RawOrigin::Root) => Some(None),
			Some(RawOrigin::Signed(who)) => {
				if Assets::owner(stable_id.clone()).as_ref() == Some(who) {
					Some(Some(who.clone()))
				} else {
					None
				}
			},
			_ => None,
		};
		success.ok_or(origin)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn try_successful_origin(_stable_id: &Assets::AssetId) -> Result<OuterOrigin, ()> {
		Ok(OuterOrigin::root())
	}
}
