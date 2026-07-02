//! `VaultBadDebtInterface` healing.

use crate::{mock::*, pallet::BranchStates};
use pusd_primitives::VaultBadDebtInterface;

/// Seed recorded bad debt directly: recording only ever happens inside the
/// vault pallet (recovery settlement / orphan-debt sweeps).
fn record(amount: Balance) {
	BranchStates::<Test>::mutate(DOT, PUSD, |maybe| {
		let state = maybe.as_mut().expect("branch registered");
		state.debt.bad_debt = state.debt.bad_debt.saturating_add(amount);
	});
}

/// Issue a fresh credit of `amount`, heal with it, and return the surplus
/// handed back (the unconsumed part of the credit).
fn heal(amount: Balance) -> Result<Balance, DispatchError> {
	let credit = <Assets as frame::traits::fungibles::Balanced<AccountId>>::issue(PUSD, amount);
	<crate::Pallet<Test> as VaultBadDebtInterface<AssetId, StableId, _>>::heal(&DOT, &PUSD, credit)
		.map(|surplus| surplus.peek())
}

fn bad_debt() -> Balance {
	BranchStates::<Test>::get(DOT, PUSD).expect("branch state").debt.bad_debt
}

// A partial heal followed by an exact one. In production the insurance flow
// heals `min(IF_balance, bad_debt)`, so a partial heal happens precisely when the
// insurance fund cannot yet cover the whole recorded bad debt; the residual is
// cleared by a later top-up. This pins that the residual tracks correctly across
// two heals.
#[test]
fn heal_partial_then_exact_clears_bad_debt() {
	build_and_execute(|| {
		register_default_branch();
		record(1_000);

		assert_eq!(heal(400), Ok(0), "fully consumed, no surplus");
		assert_eq!(bad_debt(), 600);
		assert_eq!(heal(600), Ok(0));
		assert_eq!(bad_debt(), 0);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::BadDebtHealed {
			collateral_id: DOT,
			stable_id: PUSD,
			amount: 600,
		}));
	});
}

#[test]
fn heal_caps_at_recorded_and_returns_surplus() {
	build_and_execute(|| {
		register_default_branch();
		record(500);

		// Over-supplying heals what is recorded and hands the rest back; the
		// caller decides whether a surplus is an error.
		assert_eq!(heal(501), Ok(1));
		assert_eq!(bad_debt(), 0);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::BadDebtHealed {
			collateral_id: DOT,
			stable_id: PUSD,
			amount: 500,
		}));

		// Nothing recorded any more: the whole credit comes back, and no
		// further `BadDebtHealed` lands (the test helper's issue/drop still
		// emits asset events, so count only ours).
		let healed_events = || {
			System::events()
				.into_iter()
				.filter(|e| {
					matches!(e.event, RuntimeEvent::Vaults(crate::Event::BadDebtHealed { .. }))
				})
				.count()
		};
		let before = healed_events();
		assert_eq!(heal(50), Ok(50));
		assert_eq!(healed_events(), before, "no-op heal emits no BadDebtHealed");
	});
}

#[test]
fn heal_unknown_branch_errors() {
	build_and_execute(|| {
		register_default_branch();
		let credit = <Assets as frame::traits::fungibles::Balanced<AccountId>>::issue(PUSD, 10);
		assert_err!(
			<crate::Pallet<Test> as VaultBadDebtInterface<AssetId, StableId, _>>::heal(
				&TOKEN_X, &PUSD, credit
			)
			.map(|surplus| surplus.peek()),
			crate::Error::<Test>::UnknownCollateral
		);
	});
}
