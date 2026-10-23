use crate::{mock::*, tests::percent};

// Ordinary liquidation must not remove the last eligible vault because no vault could absorb
// redistributed debt.
#[test]
fn liquidate_only_vault_returns_last_vault_error() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));

		assert_noop!(
			liquidate(9, DOT, STABLE, 1, 0, 0),
			crate::Error::<Test>::LastVaultCannotBeLiquidated
		);
	});
}

// FinalRecovery owns last-vault settlement, so ordinary liquidation must not process that vault.
#[test]
fn liquidation_rejects_final_recovery_vault() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(enter_final_recovery(99, DOT, STABLE, 1));

		assert_noop!(
			liquidate(9, DOT, STABLE, 1, 0, 0),
			crate::Error::<Test>::VaultInFinalRecovery
		);
	});
}

// A governance freeze must block liquidation because the protocol must not change branch balances
// until the freeze ends.
#[test]
fn liquidation_rejects_frozen_branch() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, true));

		assert_noop!(liquidate(9, DOT, STABLE, 1, 0, 0), crate::Error::<Test>::BranchFrozen);
	});
}
