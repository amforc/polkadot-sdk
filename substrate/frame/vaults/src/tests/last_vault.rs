use crate::{mock::*, tests::rate_pct};
use pusd_primitives::{KeeperCompensation, LiquidationAllocation, OffsetAllocation};

#[test]
fn liquidate_only_vault_returns_last_vault_error() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(5, 100)));
		// Drop the price so the vault is severely undercollateralized — but
		// `execute_liquidation` still rejects on the last-vault rule before any
		// allocation is built.
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_noop!(liquidate(DOT, 1), crate::Error::<Test>::LastVaultCannotBeLiquidated);
	});
}

#[test]
fn liquidate_succeeds_when_a_second_vault_exists() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		// Now the last-vault guard doesn't trip — vault 2 remains as a
		// redistribution recipient.
		assert_ok!(liquidate(DOT, 1));
	});
}

#[test]
fn execute_liquidation_rejects_healthy_vault() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(5, 100)));
		// Price 10 → CR = 1000 * 10 / 500 = 20 ≫ MCR 1.1.
		assert_noop!(liquidate(DOT, 1), crate::Error::<Test>::VaultNotLiquidatable);
	});
}

#[test]
fn execute_liquidation_rejects_final_recovery_vault() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(crate::Pallet::<Test>::enter_final_recovery(
			RuntimeOrigin::signed(99),
			DOT,
			PUSD,
			1
		));
		assert_noop!(liquidate(DOT, 1), crate::Error::<Test>::VaultInFinalRecovery);
	});
}

#[test]
fn execute_liquidation_rejects_frozen_branch() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(crate::Pallet::<Test>::enable_frozen_mode(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD
		));
		assert_noop!(liquidate(DOT, 1), crate::Error::<Test>::BranchFrozen);
	});
}

// An allocation that offsets more debt than the vault owes is rejected, and the
// single-transaction model rolls the rejection back atomically — the vault row
// and branch aggregates are untouched — so a follow-up valid allocation still
// succeeds.
//
// In production this can't be reached from a user: the `LiquidationAllocation`
// is computed *inside* the pallet from the liquidation math, never externally
// supplied. These two tests defensively guard the `VaultInterface` liquidation
// boundary (rejecting an inconsistent allocation and rolling back cleanly); the
// allocation *calculation* itself is exercised where that math lives.
#[test]
fn execute_liquidation_rejects_offset_debt_above_post_touch_debt() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));

		assert_noop!(
			liquidate_with(DOT, 1, |post_touch| LiquidationAllocation {
				offset: OffsetAllocation { recipient: 10, debt: post_touch + 1, collateral: 0 },
				redistribution_collateral: 0,
				keeper: KeeperCompensation { recipient: 10, collateral: 0 },
			}),
			crate::Error::<Test>::InvalidLiquidationAllocation
		);
		assert!(crate::pallet::Vaults::<Test>::contains_key((DOT, PUSD, 1)));

		assert_ok!(liquidate_with(DOT, 1, |post_touch| LiquidationAllocation {
			offset: OffsetAllocation { recipient: 10, debt: post_touch, collateral: 0 },
			redistribution_collateral: 0,
			keeper: KeeperCompensation { recipient: 10, collateral: 0 },
		}));
	});
}

// An allocation paying out more collateral than is held is rejected and rolls
// back atomically.
#[test]
fn execute_liquidation_rejects_collateral_payout_above_held() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let held = held(DOT, 1);

		assert_noop!(
			liquidate_with(DOT, 1, |_post_touch| LiquidationAllocation {
				offset: OffsetAllocation { recipient: 10, debt: 0, collateral: held + 1 },
				redistribution_collateral: 0,
				keeper: KeeperCompensation { recipient: 10, collateral: 0 },
			}),
			crate::Error::<Test>::InvalidLiquidationAllocation
		);
		assert!(crate::pallet::Vaults::<Test>::contains_key((DOT, PUSD, 1)));
	});
}

// Liquidating the vault parked as `dormant_redemption_target` must clear the
// pointer along with the row, or it dangles at a missing vault.
#[test]
fn liquidating_parked_dormant_owner_clears_pointer() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(2, 100)));
		// Partial redemption drains vault 1 below MinimumDebt → Dormant, and
		// parks it as the next redemption target.
		assert_ok!(redeem(DOT, 3, 350));
		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.dormant_redemption_target, Some(1));

		// Crash the price so the dormant husk is liquidatable, then liquidate.
		set_price(DOT, FixedU128::from_rational(1u128, 10u128));
		assert_ok!(liquidate(DOT, 1));

		assert!(crate::pallet::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.dormant_redemption_target, None, "pointer cleared with the row");
	});
}
