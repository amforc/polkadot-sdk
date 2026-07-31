use crate::{mock::*, tests::rate_pct};

#[test]
fn liquidate_only_vault_returns_last_vault_error() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// Drop the price so the vault is severely undercollateralized — but
		// Liquidation rejects on the last-vault rule.
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_noop!(
			liquidate(9, DOT, PUSD, 1, 0, 0),
			crate::Error::<Test>::LastVaultCannotBeLiquidated
		);
	});
}

#[test]
fn liquidate_succeeds_when_a_second_vault_exists() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		// Now the last-vault guard doesn't trip — vault 2 remains as a
		// redistribution recipient.
		assert_ok!(liquidate(9, DOT, PUSD, 1, 0, 0));
	});
}

#[test]
fn liquidation_rejects_healthy_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// Price 10 → CR = 1000 * 10 / 500 = 20 ≫ MCR 1.1.
		assert_noop!(liquidate(9, DOT, PUSD, 1, 0, 0), crate::Error::<Test>::VaultNotLiquidatable);
	});
}

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

// Liquidating the vault parked as `dormant_redemption_target` must clear the
// pointer along with the row, or it dangles at a missing vault.
#[test]
fn liquidating_parked_dormant_owner_clears_pointer() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		// Partial redemption drains vault 1 below MinimumDebt → Dormant, and
		// parks it as the next redemption target.
		assert_ok!(redeem(DOT, PUSD, 3, 350));
		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.dormant_redemption_target, Some(1));

		// Crash the price so the dormant husk is liquidatable, then liquidate.
		set_price(DOT, FixedU128::from_rational(1u128, 10u128));
		assert_ok!(liquidate(9, DOT, PUSD, 1, 0, 0));

		assert!(!vault_exists(DOT, PUSD, 1));
		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.dormant_redemption_target, None, "pointer cleared with the row");
	});
}
