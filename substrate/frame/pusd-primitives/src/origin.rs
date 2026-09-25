//! Origin checks the runtime wires into the pUSD protocol pallets.

use core::marker::PhantomData;
use frame::{
	deps::frame_system::RawOrigin,
	traits::{fungibles::roles::Inspect as RolesInspect, EnsureOriginWithArg, OriginTrait},
};

/// Admits `Root` or a signed origin that owns the stablecoin asset passed as the argument.
///
/// `Root` succeeds with `None` (deposit-free
/// creation) and the stablecoin's owner succeeds with `Some(owner)` (creation against a
/// refundable deposit). Every other origin is rejected and handed back unchanged.
///
/// `Assets` reports the stablecoin owner through
/// [`fungibles::roles::Inspect`](frame::traits::fungibles::roles::Inspect).
///
/// Under `runtime-benchmarks`, [`EnsureOriginWithArg::try_successful_origin`] returns `Root`.
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
