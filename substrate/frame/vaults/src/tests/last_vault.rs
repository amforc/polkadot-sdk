use crate::{mock::*, tests::rate_pct, types::LiquidationSettlement};
use frame::traits::fungibles::Balanced;

#[test]
fn liquidate_only_vault_returns_last_vault_error() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// Drop the price so the vault is severely undercollateralized — but
		// `execute_liquidation` still rejects on the last-vault rule before any
		// allocation is built.
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_noop!(liquidate(DOT, PUSD, 1), crate::Error::<Test>::LastVaultCannotBeLiquidated);
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
		assert_ok!(liquidate(DOT, PUSD, 1));
	});
}

#[test]
fn execute_liquidation_rejects_healthy_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// Price 10 → CR = 1000 * 10 / 500 = 20 ≫ MCR 1.1.
		assert_noop!(liquidate(DOT, PUSD, 1), crate::Error::<Test>::VaultNotLiquidatable);
	});
}

#[test]
fn execute_liquidation_rejects_final_recovery_vault() {
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
		assert_noop!(liquidate(DOT, PUSD, 1), crate::Error::<Test>::VaultInFinalRecovery);
	});
}

#[test]
fn execute_liquidation_rejects_frozen_branch() {
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
		assert_noop!(liquidate(DOT, PUSD, 1), crate::Error::<Test>::BranchFrozen);
	});
}

#[test]
fn execute_liquidation_rejects_offset_debt_above_post_touch_debt() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));

		assert_noop!(
			liquidate_with(DOT, PUSD, 1, |post_touch| LiquidationAllocation {
				offset: OffsetAllocation {
					collateral_recipient: 10,
					debt: post_touch + 1,
					collateral: 0
				},
				redistribution_collateral: 0,
				keeper: KeeperCompensation { recipient: 10, collateral: 0 },
			}),
			crate::Error::<Test>::InvalidLiquidationSettlement
		);
		assert!(crate::pallet::Vaults::<Test>::contains_key((DOT, PUSD, 1)));

		assert_ok!(liquidate_with(DOT, PUSD, 1, |post_touch| LiquidationAllocation {
			offset: OffsetAllocation { collateral_recipient: 10, debt: post_touch, collateral: 0 },
			redistribution_collateral: 0,
			keeper: KeeperCompensation { recipient: 10, collateral: 0 },
		}));
	});
}

// A settlement paying out more collateral than is held is rejected and rolls
// back atomically.
#[test]
fn execute_liquidation_rejects_collateral_payout_above_held() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_noop!(
			Pallet::<Test>::execute_liquidation(&DOT, &PUSD, &1, |_snapshot, owner_surplus| {
				let redistribution_collateral =
					<VaultCollateralAssets as Balanced<AccountId>>::issue(DOT, 1);
				Ok(LiquidationSettlement {
					debt_offset: 0,
					redistribution_collateral,
					owner_surplus,
				})
			},),
			crate::Error::<Test>::InvalidLiquidationSettlement
		);
		assert!(crate::pallet::Vaults::<Test>::contains_key((DOT, PUSD, 1)));
	});
}

// The pallet's own share of the liquidation math: `keeper.collateral` and
// `offset.collateral` land on their recipients, and the redistributed debt is
// derived as `post_touch - offset.debt` (the orchestrator never supplies it).
#[test]
fn execute_liquidation_pays_keeper_and_derives_redistributed_debt() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));

		let keeper_pre = collateral_balance(DOT, 998);
		let offset_pre = collateral_balance(DOT, 999);
		let held_1 = held(DOT, 1);
		let mut post_touch_debt: Balance = 0;
		assert_ok!(liquidate_with(DOT, PUSD, 1, |post_touch| {
			post_touch_debt = post_touch;
			LiquidationAllocation {
				offset: OffsetAllocation { collateral_recipient: 999, debt: 200, collateral: 300 },
				redistribution_collateral: held_1 - 300 - 50,
				keeper: KeeperCompensation { recipient: 998, collateral: 50 },
			}
		}));

		assert_eq!(collateral_balance(DOT, 998), keeper_pre + 50, "keeper compensation paid");
		assert_eq!(collateral_balance(DOT, 999), offset_pre + 300, "offset collateral paid");

		// Vault 2 is the sole recipient with stake 1_000, so the share it absorbs
		// on touch is exactly the derived redistributed debt (0.301 × 1_000 — no
		// flooring loss).
		let v_pre = vault(DOT, PUSD, 2);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 2));
		let v_post = vault(DOT, PUSD, 2);
		assert_eq!(v_post.debt.principal - v_pre.debt.principal, post_touch_debt - 200);
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
		assert_ok!(liquidate(DOT, PUSD, 1));

		assert!(!vault_exists(DOT, PUSD, 1));
		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.dormant_redemption_target, None, "pointer cleared with the row");
	});
}
