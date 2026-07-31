use crate::{mock::*, tests::rate_pct};

// Ordinary liquidation must not remove the last eligible vault because no vault could absorb
// redistributed debt.
#[test]
fn liquidate_only_vault_returns_last_vault_error() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_noop!(
			liquidate(9, DOT, PUSD, 1, 0, 0),
			crate::Error::<Test>::LastVaultCannotBeLiquidated
		);
	});
}

// The last-vault guard must allow liquidation when another vault can absorb redistributed debt.
#[test]
fn liquidate_succeeds_when_a_second_vault_exists() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(liquidate(9, DOT, PUSD, 1, 0, 0));
	});
}

// FinalRecovery owns last-vault settlement, so ordinary liquidation must not process that vault.
#[test]
fn liquidation_rejects_final_recovery_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(crate::Pallet::<Test>::enter_final_recovery(
			RuntimeOrigin::signed(99),
			DOT,
			PUSD,
			1
		));
		assert_noop!(liquidate(9, DOT, PUSD, 1, 0, 0), crate::Error::<Test>::VaultInFinalRecovery);
	});
}

// A governance freeze must block liquidation because the protocol must not change branch balances
// until the freeze ends.
#[test]
fn liquidation_rejects_frozen_branch() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			true
		));
		assert_noop!(liquidate(9, DOT, PUSD, 1, 0, 0), crate::Error::<Test>::BranchFrozen);
	});
}

// Liquidation must clear the dormant target with its vault row. Thus, redemption cannot select a
// vault that does not exist.
#[test]
fn liquidating_parked_dormant_owner_clears_pointer() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(redeem(DOT, PUSD, 3, 350));
		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.dormant_redemption_target, Some(1));

		set_price(DOT, FixedU128::from_rational(1u128, 10u128));
		assert_ok!(liquidate(9, DOT, PUSD, 1, 0, 0));

		assert!(crate::pallet::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.dormant_redemption_target, None, "pointer cleared with the row");
	});
}
