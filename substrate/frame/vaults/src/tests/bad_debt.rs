//! Bad-debt healing through `VaultInterface::heal`.

use crate::{mock::*, tests::rate_pct};
use pusd_primitives::{
	KeeperCompensation, LiquidationAllocation, OffsetAllocation, VaultInterface,
};

/// Seed recorded bad debt directly: recording only ever happens inside the
/// vault pallet (recovery settlement / orphan-debt sweeps).
fn record(amount: Balance) {
	mutate_branch_state(DOT, PUSD, |state| {
		state.debt.bad_debt = state.debt.bad_debt.saturating_add(amount);
	});
}

/// Issue a fresh credit of `amount`, heal with it, and return the surplus
/// handed back (the unconsumed part of the credit).
fn heal(amount: Balance) -> Result<Balance, DispatchError> {
	let credit = <Assets as frame::traits::fungibles::Balanced<AccountId>>::issue(PUSD, amount);
	<crate::Pallet<Test> as VaultInterface>::heal(&DOT, &PUSD, credit).map(|surplus| surplus.peek())
}

fn bad_debt() -> Balance {
	branch_state(DOT, PUSD).expect("branch state").debt.bad_debt
}

// Production heals `min(IF_balance, bad_debt)`: a partial heal is the IF not
// yet covering the whole recorded debt, cleared by a later top-up.
#[test]
fn heal_partial_then_exact_clears_bad_debt() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
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
		register_market(DOT, PUSD);
		record(500);

		// Defensive: production heals `min(IF_balance, bad_debt)`, so over-supply
		// can't happen — the cap returns the surplus rather than trusting the caller.
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

// The realistic dust path, end to end: a redistribution's per-stake flooring
// residue (501 of debt over stakes 1_000 + 999 strands exactly 1 unit) lands in
// `ownerless_debt`, is swept into `bad_debt` when the last husk closes, and is
// healed exactly.
#[test]
fn heal_clears_swept_flooring_dust() {
	use frame::traits::fungible::Mutate;
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 999, 500, rate_pct(5, 100)));
		assert_ok!(open(3, DOT, PUSD, 5_000, 500, rate_pct(5, 100))); // liquidatee

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(liquidate_with(DOT, PUSD, 3, |_| LiquidationAllocation {
			offset: OffsetAllocation { recipient: 0, debt: 0, collateral: 0 },
			redistribution_collateral: 0,
			keeper: KeeperCompensation { recipient: 3, collateral: 0 },
		}));
		assert_eq!(branch_state(DOT, PUSD).unwrap().ownerless_debt, 1);

		// Empty the branch: overpay-repay both recipients, close both husks.
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&1, 1_000));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&2, 1_000));
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			10_000
		));
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(2),
			DOT,
			PUSD,
			2,
			10_000
		));
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), DOT, PUSD, None));
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(2), DOT, PUSD, None));

		assert_eq!(bad_debt(), 1, "sweep promoted the flooring residue to bad debt");
		assert_eq!(heal(1), Ok(0));
		assert_eq!(bad_debt(), 0);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::BadDebtHealed {
			collateral_id: DOT,
			stable_id: PUSD,
			amount: 1,
		}));
	});
}

#[test]
fn heal_unknown_branch_errors() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		let credit = <Assets as frame::traits::fungibles::Balanced<AccountId>>::issue(PUSD, 10);
		assert_err!(
			<crate::Pallet<Test> as VaultInterface>::heal(&TOKEN_X, &PUSD, credit)
				.map(|surplus| surplus.peek()),
			crate::Error::<Test>::UnknownCollateral
		);
	});
}
