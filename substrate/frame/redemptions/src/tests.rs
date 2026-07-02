use crate as pallet_redemptions;
use crate::{mock::*, types::RecoveryRegime, Error, Event};
use frame::deps::{
	frame_support::{assert_noop, assert_ok},
	sp_runtime::{
		traits::{One, Zero},
		FixedPointNumber, FixedU128, Permill,
	},
};
use pusd_primitives::{recovery_pricing, RedemptionTargetKind};

const HOUR_MS: Moment = 3_600 * 1_000;
const ONE_YEAR_MS: Moment = 31_557_600_000;

/// The fully accrued debt of `who`, read through the pallet's own preview so the
/// value is exactly what a live redemption would poke and cancel against.
fn preview_full_debt(who: AccountId) -> Balance {
	let preview = pallet_redemptions::Pallet::<Test>::preview_redeem(DOT, PUSD, 1_000_000_000)
		.expect("preview");
	assert_eq!(preview.steps_detail[0].target, who);
	preview.steps_detail[0].debt_cancellable
}

fn last_recovery_regime() -> Option<RecoveryRegime> {
	System::events().into_iter().rev().find_map(|r| match r.event {
		RuntimeEvent::Redemptions(Event::RecoveryRedemptionExecuted { regime, .. }) => Some(regime),
		_ => None,
	})
}

fn ordinary_event_emitted() -> bool {
	System::events().iter().any(|r| {
		matches!(r.event, RuntimeEvent::Redemptions(Event::OrdinaryRedemptionExecuted { .. }))
	})
}

#[test]
fn branch_registration_seeds_redemption_config() {
	build_and_execute(|| {
		assert!(crate::RedemptionConfigs::<Test>::get(DOT, PUSD).is_none());
		register_default_branch();
		let cfg = crate::RedemptionConfigs::<Test>::get(DOT, PUSD).expect("seeded on registration");
		assert_eq!(cfg.minimum_redemption_amount, 100);
	});
}

#[test]
fn branch_registration_rejects_invalid_default_redemption_config() {
	build_and_execute(|| {
		let mut bad = DefaultRedemptionConfig::get();
		bad.minimum_redemption_amount = 0;
		DefaultRedemptionConfig::set(bad);

		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		assert_noop!(
			pallet_vaults::Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				admin_caller(ADMIN),
				admin_caller(EMERGENCY_ADMIN),
				default_branch_config(),
			),
			Error::<Test>::InvalidRedemptionConfig
		);
		assert!(crate::RedemptionConfigs::<Test>::get(DOT, PUSD).is_none());
		assert!(pallet_vaults::Pallet::<Test>::branch_tcr(DOT, PUSD).is_none());
	});
}

#[test]
fn redeem_below_minimum_amount_reverts() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(5, 100)));
		mint_pusd(3, 1_000);
		assert_noop!(redeem(3, 99, 0, 4), Error::<Test>::BelowMinimumRedemptionAmount);
	});
}

#[test]
fn redeem_unregistered_branch_reverts() {
	build_and_execute(|| {
		mint_pusd(3, 1_000);
		assert_noop!(redeem(3, 200, 0, 4), Error::<Test>::InvalidBranch);
	});
}

#[test]
fn redeem_frozen_branch_reverts() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(5, 100)));
		mint_pusd(3, 1_000);
		assert_ok!(pallet_vaults::Pallet::<Test>::enable_frozen_mode(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD
		));
		// Frozen-mode enforcement lives vault-side: the first `redeem_step`
		// rejects the frozen branch and the whole redemption rolls back.
		assert_noop!(redeem(3, 200, 0, 4), pallet_vaults::Error::<Test>::BranchFrozen);
	});
}

#[test]
fn redeem_no_vault_reverts() {
	build_and_execute(|| {
		register_default_branch();
		mint_pusd(3, 1_000);
		assert_noop!(redeem(3, 200, 0, 4), Error::<Test>::NoRedeemableVault);
	});
}

#[test]
fn ordinary_redemption_burns_debt_pays_collateral_and_routes_fee() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(5, 100)));
		let debt_before = vault_debt(1);
		mint_pusd(3, 1_000_000);

		let redeemer_before = pusd_balance(3);
		let recipient_before = collateral_balance(4);
		let fee_before = pusd_balance(FEE_ACCOUNT);
		let held_before = held(1);
		let issuance_before = pusd_issuance();

		assert_ok!(redeem(3, 201, 0, 4));

		assert_eq!(vault_debt(1), debt_before - 200);
		assert_eq!(pusd_balance(3), redeemer_before - 201);
		assert_eq!(collateral_balance(4) - recipient_before, 160);
		assert_eq!(held_before - held(1), 160);
		assert_eq!(pusd_balance(FEE_ACCOUNT) - fee_before, 1);
		// Fees are transferred, so issuance must only fall by cancelled debt.
		assert_eq!(issuance_before - pusd_issuance(), 200);
		assert!(ordinary_event_emitted());
	});
}

#[test]
fn redemption_targets_lowest_rate_vault_first() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, 1_000, 500, rate_pct(2, 100)));
		mint_pusd(3, 1_000_000);
		let v1_before = vault_debt(1);
		let v1_held_before = held(1);
		let v2_before = vault_debt(2);
		let v2_held_before = held(2);
		let recipient_before = collateral_balance(4);
		let fee_before = pusd_balance(FEE_ACCOUNT);

		// 201 pUSD at the 0.5% floor fee cancels exactly floor(201/1.005) = 200 debt,
		// paying floor(200/1.25) = 160 collateral and ceil(200 * 0.005) = 1 fee.
		assert_ok!(redeem(3, 201, 0, 4));
		assert_eq!(v1_before - vault_debt(1), 200);
		assert_eq!(v1_held_before - held(1), 160);
		assert_eq!(collateral_balance(4) - recipient_before, 160);
		assert_eq!(pusd_balance(FEE_ACCOUNT) - fee_before, 1);
		// The higher-rate vault is untouched in both debt and held collateral.
		assert_eq!(vault_debt(2), v2_before);
		assert_eq!(held(2), v2_held_before);
	});
}

#[test]
fn redemption_partially_fills_to_budget() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 10_000, 5_000, rate_pct(5, 100)));
		let debt_before = vault_debt(1);
		mint_pusd(3, 1_000_000);
		let redeemer_before = pusd_balance(3);
		let recipient_before = collateral_balance(4);
		let held_before = held(1);
		let fee_before = pusd_balance(FEE_ACCOUNT);
		let issuance_before = pusd_issuance();

		// 1_005 pUSD at the 0.5% floor cancels floor(1_005/1.005) = 1_000 debt,
		// leaving the vault partially filled with debt to spare.
		assert_ok!(redeem(3, 1_005, 0, 4));
		assert_eq!(vault_debt(1), debt_before - 1_000);
		assert_eq!(redeemer_before - pusd_balance(3), 1_005);
		assert_eq!(collateral_balance(4) - recipient_before, 800);
		assert_eq!(held_before - held(1), 800);
		assert_eq!(pusd_balance(FEE_ACCOUNT) - fee_before, 5);
		// Fees are routed, so issuance falls only by the cancelled debt.
		assert_eq!(issuance_before - pusd_issuance(), 1_000);
	});
}

#[test]
fn caller_max_steps_caps_the_loop() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, 1_000, 500, rate_pct(2, 100)));
		mint_pusd(3, 1_000_000);
		let v2_before = vault_debt(2);

		assert_ok!(redeem_capped(3, 100_000, 0, 4, 1));
		assert_eq!(vault_debt(1), 0);
		assert_eq!(vault_debt(2), v2_before);
	});
}

#[test]
fn underwater_ordinary_vault_is_skipped() {
	build_and_execute(|| {
		register_default_branch();
		// Open the well-collateralized vault first so the thin vault 1 doesn't
		// briefly push the fresh branch into Safety mode at the genesis price.
		assert_ok!(open(2, 3_000, 240, rate_pct(2, 100)));
		assert_ok!(open(1, 250, 240, rate_pct(1, 100)));
		// Vault 1 stays active but underwater, so redemption must skip it.
		set_price(DOT, FixedU128::from_rational(9, 10));
		mint_pusd(3, 1_000_000);
		let v1_before = vault_debt(1);
		let v1_held_before = held(1);
		let v2_before = vault_debt(2);
		let v2_held_before = held(2);
		let recipient_before = collateral_balance(4);
		let fee_before = pusd_balance(FEE_ACCOUNT);

		// 100 pUSD at the 0.5% floor fee cancels exactly floor(100/1.005) = 99 debt
		// against vault 2, paying floor(99/0.9) = 110 collateral and ceil(99*0.005) = 1 fee.
		assert_ok!(redeem(3, 100, 0, 4));
		// The skipped underwater vault keeps its debt and its held collateral.
		assert_eq!(vault_debt(1), v1_before);
		assert_eq!(held(1), v1_held_before);
		// The healthy vault behind it is redeemed across every dimension.
		assert_eq!(v2_before - vault_debt(2), 99);
		assert_eq!(v2_held_before - held(2), 110);
		assert_eq!(collateral_balance(4) - recipient_before, 110);
		assert_eq!(pusd_balance(FEE_ACCOUNT) - fee_before, 1);
	});
}

#[test]
fn underwater_prefix_skipped_once_while_healthy_vaults_redeem() {
	build_and_execute(|| {
		register_default_branch();
		// The cursor must not re-walk the underwater prefix after each removal.
		// Open the well-collateralized vaults first so the thin low-rate vaults
		// don't briefly push the fresh branch into Safety mode at the genesis price.
		assert_ok!(open(3, 3_000, 240, rate_pct(3, 100)));
		assert_ok!(open(4, 3_000, 240, rate_pct(4, 100)));
		assert_ok!(open(1, 250, 240, rate_pct(1, 100)));
		assert_ok!(open(2, 260, 240, rate_pct(2, 100)));
		set_price(DOT, FixedU128::from_rational(9, 10));
		mint_pusd(5, 1_000_000);
		let v1_before = vault_debt(1);
		let v2_before = vault_debt(2);
		let v3_before = vault_debt(3);
		let v4_before = vault_debt(4);
		let recipient_before = collateral_balance(6);
		let issuance_before = pusd_issuance();

		assert_ok!(redeem(5, 2_000, 0, 6));
		// The underwater low-rate prefix (vaults 1-2) is skipped, not re-walked.
		assert_eq!(vault_debt(1), v1_before);
		assert_eq!(vault_debt(2), v2_before);
		// The healthy vaults behind it drain fully.
		assert_eq!(vault_debt(3), 0);
		assert_eq!(vault_debt(4), 0);
		// Collateral is paid at face value, floored per step at price 0.9 (= debt * 10 / 9).
		let expected_collateral = v3_before * 10 / 9 + v4_before * 10 / 9;
		assert_eq!(collateral_balance(6) - recipient_before, expected_collateral);
		// Issuance falls by exactly the debt burned; fees are routed, not burned.
		assert_eq!(issuance_before - pusd_issuance(), v3_before + v4_before);
	});
}

#[test]
fn slippage_bound_reverts_without_state_change() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(5, 100)));
		let debt_before = vault_debt(1);
		mint_pusd(3, 1_000_000);
		let redeemer_before = pusd_balance(3);
		let recipient_before = collateral_balance(4);
		let held_before = held(1);
		let issuance_before = pusd_issuance();

		// 201 pUSD would cancel 200 debt for only floor(200/1.25) = 160 collateral,
		// below the 161 floor, so the whole redemption reverts with no side effects.
		assert_noop!(redeem(3, 201, 161, 4), Error::<Test>::SlippageExceeded);
		assert_eq!(vault_debt(1), debt_before);
		assert_eq!(pusd_balance(3), redeemer_before);
		assert_eq!(collateral_balance(4), recipient_before);
		assert_eq!(held(1), held_before);
		assert_eq!(pusd_issuance(), issuance_before);
	});
}

#[test]
fn insufficient_pusd_balance_reverts() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(5, 100)));
		mint_pusd(3, 50);
		assert_noop!(redeem(3, 201, 0, 4), Error::<Test>::InsufficientPusdBalance);
	});
}

#[test]
fn base_rate_rises_after_ordinary_redemption() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 100_000, 50_000, rate_pct(5, 100)));
		mint_pusd(3, 1_000_000);
		assert_eq!(redemption_state().base_rate, FixedU128::zero());
		let debt_before = vault_debt(1);
		let branch_debt_before = branch_debt();
		let fee_before = pusd_balance(FEE_ACCOUNT);
		let recipient_before = collateral_balance(4);

		assert_ok!(redeem(3, 10_000, 0, 4));

		// 10_000 pUSD at the 0.5% floor cancels floor(10_000/1.005) = 9_950 debt,
		// charging ceil(9_950 * 0.005) = 50 fee and paying 9_950/1.25 = 7_960 collateral.
		assert_eq!(debt_before - vault_debt(1), 9_950);
		assert_eq!(pusd_balance(FEE_ACCOUNT) - fee_before, 50);
		assert_eq!(collateral_balance(4) - recipient_before, 7_960);
		// The branch debt aggregate falls by exactly the cancelled debt.
		assert_eq!(branch_debt_before - branch_debt(), 9_950);

		// The new base rate is decayed(0) + redeemed_fraction / increase_divisor,
		// computed against the branch debt captured before the redemption.
		let fraction = FixedU128::checked_from_rational(9_950u128, branch_debt_before)
			.expect("nonzero branch debt");
		let expected = crate::fees::increased_base_rate(
			FixedU128::zero(),
			fraction,
			FixedU128::from_rational(2, 1),
			FixedU128::zero(),
			FixedU128::one(),
		);
		assert!(expected > FixedU128::zero());
		assert_eq!(redemption_state().base_rate, expected);
		assert_eq!(redemption_state().last_fee_operation, 1_000);
	});
}

#[test]
fn base_rate_decays_between_redemptions() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000_000, 500_000, rate_pct(5, 100)));
		mint_pusd(3, 2_000_000);

		assert_ok!(redeem(3, 100_000, 0, 4));
		let raised = redemption_state().base_rate;
		assert!(raised > FixedU128::zero());

		advance_time(24 * HOUR_MS);
		assert_ok!(redeem(3, 1_000, 0, 4));
		assert!(redemption_state().base_rate < raised);
	});
}

#[test]
fn dormant_target_is_redeemed_before_rate_index() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, 1_000, 500, rate_pct(2, 100)));
		mint_pusd(3, 1_000_000);

		assert_ok!(redeem(3, 360, 0, 4));
		assert!(vault_status(1).expect("vault 1").is_dormant());
		assert_eq!(
			pallet_vaults::BranchStates::<Test>::get(DOT, PUSD)
				.unwrap()
				.dormant_redemption_target,
			Some(1)
		);

		let v2_before = vault_debt(2);
		let v1_residual = vault_debt(1);
		let recipient_before = collateral_balance(4);
		// Vault 1 is now Dormant (out of the rate index); the only way the second
		// redemption can reach it is via the Dormant slot, which is served before
		// the rate index. It redeems the Dormant vault and never touches ordinary
		// vault 2. (The amount cancelled is shaped by the base rate the first
		// redemption raised, so the exact figure lives in the fee-state layer.)
		assert_ok!(redeem(3, v1_residual + 10, 0, 4));
		let cancelled = v1_residual - vault_debt(1);
		assert!(cancelled > 0, "the Dormant slot was the redemption target");
		// Collateral paid is exactly the face-value amount for the debt cancelled.
		assert_eq!(collateral_balance(4) - recipient_before, cancelled * 4 / 5);
		// Priority: the ordinary vault behind the Dormant slot is untouched.
		assert_eq!(vault_debt(2), v2_before);
	});
}

fn setup_final_recovery(who: AccountId, coll: Balance, debt: Balance, recovery_price: FixedU128) {
	// Reset to a healthy price so the vault opens cleanly even when the branch
	// already holds FinalRecovery vaults parked at a depressed price.
	set_price(DOT, FixedU128::from_rational(5u128, 4u128));
	assert_ok!(open(who, coll, debt, rate_pct(5, 100)));
	set_price(DOT, recovery_price);
	assert_ok!(enter_final_recovery(who));
	assert!(vault_status(who).expect("fr vault").is_final_recovery());
}

#[test]
fn recovery_bonus_pays_more_than_face_value() {
	build_and_execute(|| {
		register_default_branch();
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		mint_pusd(3, 1_000_000);
		let recipient_before = collateral_balance(4);
		let fee_before = pusd_balance(FEE_ACCOUNT);
		let debt_before = vault_debt(1);

		assert_ok!(redeem(3, 200, 0, 4));

		let collateral_out = collateral_balance(4) - recipient_before;
		assert_eq!(collateral_out, 394, "recovery bonus payout");
		assert_eq!(vault_debt(1), debt_before - 200);
		assert_eq!(pusd_balance(FEE_ACCOUNT), fee_before);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
	});
}

#[test]
fn recovery_has_priority_over_ordinary_vaults() {
	build_and_execute(|| {
		register_default_branch();
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		// Reset to a healthy price: vault 1 is now a CR >= 100% recovery vault, so
		// the settlement uses the RecoveryBonus regime.
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		assert_ok!(open(2, 1_000, 500, rate_pct(5, 100)));
		let v1_before = vault_debt(1);
		let v2_before = vault_debt(2);
		let recipient_before = collateral_balance(4);
		mint_pusd(3, 1_000_000);

		assert_ok!(redeem(3, 200, 0, 4));
		// The FinalRecovery vault is served at its exact regime, before any ordinary vault.
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
		assert_eq!(v1_before - vault_debt(1), 200);
		// 5% redistribution-penalty bonus: floor(200 * 1.05 / 1.25) = 168 collateral.
		assert_eq!(collateral_balance(4) - recipient_before, 168);
		// The ordinary vault is untouched.
		assert_eq!(vault_debt(2), v2_before);
	});
}

#[test]
fn preview_reports_final_recovery_before_ordinary_targets() {
	build_and_execute(|| {
		register_default_branch();
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		// Reset to a healthy price so vault 2 opens as an ordinary rate-index target.
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		assert_ok!(open(2, 1_000, 500, rate_pct(5, 100)));
		let v1_before = vault_debt(1);
		let v2_before = vault_debt(2);

		let preview =
			pallet_redemptions::Pallet::<Test>::preview_redeem(DOT, PUSD, 200).expect("preview");

		assert_eq!(preview.steps, 1);
		assert_eq!(preview.steps_detail[0].target, 1);
		assert_eq!(preview.steps_detail[0].kind, RedemptionTargetKind::FinalRecovery);
		// Preview is the public path helper: it must expose the priority target
		// without applying the rolled-back vault touch.
		assert_eq!(vault_debt(1), v1_before);
		assert_eq!(vault_debt(2), v2_before);
	});
}

#[test]
fn insurance_adjusted_settlement_with_partial_fund() {
	build_and_execute(|| {
		register_default_branch();
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128));
		let debt = vault_debt(1);
		assert_eq!(debt, 501);
		mint_pusd(INSURANCE_FUND, 50);
		let market_cancel = debt - 50;
		mint_pusd(3, 1_000_000);

		let if_before = pusd_balance(INSURANCE_FUND);
		let recipient_before = collateral_balance(4);
		let issuance_before = pusd_issuance();

		assert_ok!(redeem(3, market_cancel, 0, 4));

		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
		// D=501, C=400, IF=50 -> market debt=451 and double flooring pays 997.
		assert_eq!(collateral_balance(4) - recipient_before, 997);
		assert_eq!(if_before - pusd_balance(INSURANCE_FUND), 50);
		assert_eq!(issuance_before - pusd_issuance(), debt);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
	});
}

#[test]
fn insurance_adjusted_settlement_with_empty_fund() {
	build_and_execute(|| {
		register_default_branch();
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128));
		let debt = vault_debt(1);
		assert_eq!(debt, 501);
		assert_eq!(pusd_balance(INSURANCE_FUND), 0);
		mint_pusd(3, 1_000_000);
		let recipient_before = collateral_balance(4);
		let issuance_before = pusd_issuance();

		assert_ok!(redeem(3, debt, 0, 4));

		assert_eq!(vault_debt(1), 0);
		// Empty fund: D=501, C=400, so C/D with double flooring pays 997.
		assert_eq!(collateral_balance(4) - recipient_before, 997);
		assert_eq!(issuance_before - pusd_issuance(), debt);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
	});
}

#[test]
fn set_redemption_config_updates_and_validates() {
	build_and_execute(|| {
		register_default_branch();
		let mut cfg = crate::RedemptionConfigs::<Test>::get(DOT, PUSD).unwrap();
		cfg.minimum_redemption_amount = 250;
		assert_ok!(pallet_redemptions::Pallet::<Test>::set_redemption_config(
			RuntimeOrigin::root(),
			DOT,
			PUSD,
			cfg.clone()
		));
		assert_eq!(
			crate::RedemptionConfigs::<Test>::get(DOT, PUSD)
				.unwrap()
				.minimum_redemption_amount,
			250
		);

		let mut bad = cfg.clone();
		bad.base_rate_floor = FixedU128::from_rational(1u128, 1u128);
		bad.base_rate_ceiling = FixedU128::zero();
		assert_noop!(
			pallet_redemptions::Pallet::<Test>::set_redemption_config(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				bad
			),
			Error::<Test>::InvalidRedemptionConfig
		);

		let mut bad = cfg.clone();
		bad.minimum_redemption_amount = 0;
		assert_noop!(
			pallet_redemptions::Pallet::<Test>::set_redemption_config(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				bad
			),
			Error::<Test>::InvalidRedemptionConfig
		);

		let mut bad = cfg.clone();
		bad.base_rate_decay_period = 0;
		assert_noop!(
			pallet_redemptions::Pallet::<Test>::set_redemption_config(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				bad
			),
			Error::<Test>::InvalidRedemptionConfig
		);

		let mut bad = cfg.clone();
		bad.redemption_fee_floor = FixedU128::one();
		bad.redemption_fee_ceiling = FixedU128::zero();
		assert_noop!(
			pallet_redemptions::Pallet::<Test>::set_redemption_config(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				bad
			),
			Error::<Test>::InvalidRedemptionConfig
		);

		let mut bad = cfg.clone();
		bad.base_rate_increase_divisor = FixedU128::zero();
		assert_noop!(
			pallet_redemptions::Pallet::<Test>::set_redemption_config(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				bad
			),
			Error::<Test>::InvalidRedemptionConfig
		);

		assert_noop!(
			pallet_redemptions::Pallet::<Test>::set_redemption_config(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				cfg
			),
			frame::deps::sp_runtime::DispatchError::BadOrigin
		);
	});
}

#[test]
fn non_root_manager_can_update_redemption_config() {
	build_and_execute(|| {
		register_default_branch();
		let mut cfg = crate::RedemptionConfigs::<Test>::get(DOT, PUSD).unwrap();
		cfg.minimum_redemption_amount = 250;

		assert_ok!(pallet_redemptions::Pallet::<Test>::set_redemption_config(
			RuntimeOrigin::signed(999),
			DOT,
			PUSD,
			cfg
		));

		assert_eq!(
			crate::RedemptionConfigs::<Test>::get(DOT, PUSD)
				.unwrap()
				.minimum_redemption_amount,
			250
		);
	});
}

#[test]
fn set_redemption_config_unregistered_branch_reverts() {
	build_and_execute(|| {
		let cfg = DefaultRedemptionConfig::get();
		assert_noop!(
			pallet_redemptions::Pallet::<Test>::set_redemption_config(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				cfg
			),
			Error::<Test>::InvalidBranch
		);
	});
}

#[test]
fn preview_redeem_below_minimum_amount_returns_none() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(5, 100)));

		assert!(pallet_redemptions::Pallet::<Test>::preview_redeem(DOT, PUSD, 99).is_none());
	});
}

#[test]
fn preview_redeem_reports_path_without_side_effects() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(5, 100)));
		let debt_before = vault_debt(1);

		let preview =
			pallet_redemptions::Pallet::<Test>::preview_redeem(DOT, PUSD, 201).expect("preview");
		assert_eq!(preview.steps, 1);
		assert!(!preview.truncated);
		assert_eq!(preview.total_pusd_in, 201);
		assert_eq!(preview.total_collateral_out, 160);
		assert_eq!(preview.total_fee_pusd, 1);
		let step = &preview.steps_detail[0];
		assert_eq!(step.target, 1);
		assert_eq!(step.debt_cancellable, 200);
		assert_eq!(step.collateral_out, 160);
		assert_eq!(step.fee_pusd, 1);
		assert_eq!(step.pusd_in, 201);
		// Preview prepares snapshots inside a rollback-only transaction.
		assert_eq!(vault_debt(1), debt_before);
	});
}

#[test]
fn preview_redeem_walks_multiple_vaults() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, 1_000, 500, rate_pct(2, 100)));
		let v1_before = vault_debt(1);
		let v2_before = vault_debt(2);

		let preview = pallet_redemptions::Pallet::<Test>::preview_redeem(DOT, PUSD, 100_000)
			.expect("preview");
		assert_eq!(preview.steps, 2);
		assert!(!preview.truncated);
		assert_eq!(preview.steps_detail[0].target, 1);
		assert_eq!(preview.steps_detail[1].target, 2);
		// The rollback must cover every touched vault, not just the first one.
		assert_eq!(vault_debt(1), v1_before);
		assert_eq!(vault_debt(2), v2_before);
	});
}

#[test]
fn preview_redeem_none_when_no_target() {
	build_and_execute(|| {
		register_default_branch();
		assert!(pallet_redemptions::Pallet::<Test>::preview_redeem(DOT, PUSD, 200).is_none());
	});
}

#[test]
fn fee_and_base_rate_math() {
	let rate = crate::fees::fee_rate(rate_pct(15, 1_000), rate_pct(5, 1_000), FixedU128::one());
	assert_eq!(rate, rate_pct(2, 100));
	assert_eq!(crate::fees::fee_pusd::<u128>(1_000, rate), 20);
	assert_eq!(recovery_pricing::collateral_for_value::<u128>(1_000, rate_pct(2, 1)), 500);
	let new_base = crate::fees::increased_base_rate(
		rate_pct(15, 1_000),
		rate_pct(1_000, 100_000),
		rate_pct(2, 1),
		FixedU128::zero(),
		FixedU128::one(),
	);
	assert_eq!(new_base, rate_pct(2, 100));
}

#[test]
fn ordinary_redemption_end_to_end() {
	build_and_execute(|| {
		register_default_branch();
		set_price(DOT, FixedU128::from_rational(2, 1));
		assert_ok!(open(1, 4_000, 5_000, rate_pct(5, 100)));
		set_base_rate(FixedU128::from_rational(15, 1_000));

		mint_pusd(3, 1_000_000);
		let recipient_before = collateral_balance(4);
		let fee_before = pusd_balance(FEE_ACCOUNT);
		let redeemer_before = pusd_balance(3);
		let debt_before = vault_debt(1);
		let held_before = held(1);
		let issuance_before = pusd_issuance();

		assert_ok!(redeem(3, 1_020, 0, 4));

		assert_eq!(collateral_balance(4) - recipient_before, 500);
		assert_eq!(pusd_balance(FEE_ACCOUNT) - fee_before, 20);
		assert_eq!(redeemer_before - pusd_balance(3), 1_020);
		assert_eq!(debt_before - vault_debt(1), 1_000);
		assert_eq!(held_before - held(1), 500);
		// Fees are transferred, so issuance must only fall by cancelled debt.
		assert_eq!(issuance_before - pusd_issuance(), 1_000);
	});
}

#[test]
fn recovery_bonus_math() {
	let bonus = recovery_pricing::recovery_bonus(
		rate_pct(120, 100),
		rate_pct(1, 100),
		Permill::from_percent(10),
	);
	assert_eq!(bonus, rate_pct(10, 100));
	assert_eq!(
		recovery_pricing::recovery_bonus_collateral_out::<u128>(2_000, bonus, rate_pct(2, 1)),
		1_100
	);
}

#[test]
fn insurance_adjusted_math() {
	let price = FixedU128::from_rational(2, 1);
	let split = recovery_pricing::insurance_adjusted::<u128>(10_000, 8_000, 1_000);
	assert_eq!(split.effective_cover, 1_000);
	assert_eq!(split.market_cancel_debt, 9_000);
	assert!(split.recovery_rate > FixedU128::from_rational(8_888, 10_000));
	assert!(split.recovery_rate < FixedU128::from_rational(8_889, 10_000));
	// Double flooring is intentional: value first, then collateral units.
	assert_eq!(
		recovery_pricing::recovery_rate_collateral_out::<u128>(3_000, split.recovery_rate, price),
		1_333
	);
	assert_eq!(
		recovery_pricing::recovery_rate_collateral_out::<u128>(9_000, split.recovery_rate, price),
		3_999
	);
}

#[test]
fn multiple_final_recovery_vaults_settle_fifo_head_with_per_head_insurance_fund() {
	build_and_execute(|| {
		register_default_branch();
		// Recovery price 0.10 → both vaults sit below 100% CR (insurance-adjusted).
		let rp = FixedU128::from_rational(1u128, 10u128);
		setup_final_recovery(1, 1_000, 500, rp);
		setup_final_recovery(2, 1_000, 500, rp);

		// Both vaults are in the FinalRecovery FIFO, oldest first; only the head
		// is exposed to redemption.
		assert_eq!(
			pallet_vaults::Pallet::<Test>::final_recovery_queue_head(DOT, PUSD, 10),
			alloc::vec![1u64, 2u64]
		);

		let debt1 = vault_debt(1);
		let debt2 = vault_debt(2);
		assert_eq!(debt1, 501);
		assert_eq!(debt2, 501);
		// Fund the Insurance Fund to exactly cover vault 1's shortfall, so the head
		// drains it and the next head must settle against a then-empty fund.
		let collateral_value_1 = rp.saturating_mul_int(held(1));
		let shortfall_1 = debt1 - collateral_value_1;
		mint_pusd(INSURANCE_FUND, shortfall_1);
		mint_pusd(3, 10_000);

		let issuance_before = pusd_issuance();
		let redeemer_before_1 = pusd_balance(3);
		let recipient_before_1 = collateral_balance(4);

		// First transaction settles only the FIFO head (vault 1).
		assert_ok!(redeem(3, 10_000, 0, 4));
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_none(), "head settled");
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		assert_eq!(pusd_balance(INSURANCE_FUND), 0, "head 1 drained the fund");
		// Head-only: vault 2 is untouched and becomes the new FIFO head.
		assert_eq!(vault_debt(2), debt2);
		assert!(vault_status(2).expect("vault 2").is_final_recovery());
		assert_eq!(
			pallet_vaults::Pallet::<Test>::final_recovery_queue_head(DOT, PUSD, 10),
			alloc::vec![2u64]
		);
		// The fund covered vault 1 exactly, so the redeemer paid only the
		// collateral-backed (recovery_rate == 1.0) portion.
		let pusd_in_1 = redeemer_before_1 - pusd_balance(3);
		assert_eq!(pusd_in_1, collateral_value_1);
		// recovery_rate == 1.0, so vault 1's entire 1_000 collateral is paid to the recipient.
		assert_eq!(collateral_balance(4) - recipient_before_1, 1_000);

		let redeemer_before_2 = pusd_balance(3);
		let recipient_before_2 = collateral_balance(4);

		// Second transaction settles vault 2 against the now-empty fund: the
		// recovery rate falls to C/D, so the redeemer covers the entire debt and
		// pUSD holders absorb the shortfall.
		assert_ok!(redeem(3, 10_000, 0, 4));
		// With the fund empty there is no insurance residual to burn, so the debt
		// is fully cancelled by the redeemer and the vault drops to zero debt (and
		// leaves the FIFO) rather than being settled-and-removed like vault 1.
		assert_eq!(vault_debt(2), 0, "second head fully settled");
		assert!(
			pallet_vaults::Pallet::<Test>::final_recovery_queue_head(DOT, PUSD, 10).is_empty(),
			"FIFO drained"
		);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		assert_eq!(pusd_balance(INSURANCE_FUND), 0, "no fund left to burn");
		let pusd_in_2 = redeemer_before_2 - pusd_balance(3);
		assert_eq!(pusd_in_2, debt2);
		assert!(pusd_in_2 > pusd_in_1, "drained fund pushes the loss onto the redeemer");
		// Empty fund: D=501, C=100, so C/D with double flooring pays 990.
		assert_eq!(collateral_balance(4) - recipient_before_2, 990);

		// Conservation: issuance falls by both vaults' full debt and no more
		// (redeemer burns + the atomic Insurance-Fund burn for vault 1).
		assert_eq!(issuance_before - pusd_issuance(), debt1 + debt2);
	});
}

#[test]
fn insurance_adjusted_recovery_burns_fund_only_when_market_debt_exhausted() {
	build_and_execute(|| {
		register_default_branch();
		// Recovery price 0.40 → CR < 100% with a partial Insurance Fund cover.
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128));
		mint_pusd(INSURANCE_FUND, 50);
		mint_pusd(3, 10_000);

		let debt_before = vault_debt(1);
		let if_before = pusd_balance(INSURANCE_FUND);
		let recipient_before = collateral_balance(4);
		let issuance_before = pusd_issuance();

		// First transaction cancels only part of the market-cancellable debt: the
		// fund must stay untouched until the market portion is fully exhausted.
		assert_ok!(redeem(3, 200, 0, 4));
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_some(), "still settling");
		assert!(vault_status(1).expect("vault 1").is_final_recovery());
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		assert_eq!(pusd_balance(INSURANCE_FUND), if_before, "fund untouched mid-settlement");
		// Recovery is fee-free and budget-bound: this step cancels exactly 200 debt
		// (200 < market_cancel_debt) and pays collateral at recovery_rate = C/(D-50).
		assert_eq!(debt_before - vault_debt(1), 200);
		assert_eq!(collateral_balance(4) - recipient_before, 442);

		// Second transaction finishes the market portion; only now does the atomic
		// Insurance-Fund burn fire, covering the residual and removing the vault.
		assert_ok!(redeem(3, 10_000, 0, 4));
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_none(), "vault settled");
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		assert_eq!(pusd_balance(INSURANCE_FUND), 0, "residual burned on completion");
		// Conservation: the full vault debt leaves issuance across the two txs.
		assert_eq!(issuance_before - pusd_issuance(), debt_before);
	});
}

#[test]
fn recovery_redemption_leaves_ordinary_base_rate_untouched() {
	build_and_execute(|| {
		register_default_branch();
		// CR >= 100% → a clean recovery-bonus settlement.
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		let seeded = FixedU128::from_rational(3u128, 100u128);
		set_base_rate(seeded);
		let state_before = redemption_state();
		mint_pusd(3, 1_000_000);

		assert_ok!(redeem(3, 200, 0, 4));

		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
		assert!(!ordinary_event_emitted(), "recovery must not emit an ordinary redemption");
		// Recovery activity must not feed the ordinary-redemption fee accelerator.
		assert_eq!(redemption_state().base_rate, state_before.base_rate);
		assert_eq!(redemption_state().last_fee_operation, state_before.last_fee_operation);
	});
}

#[test]
fn ordinary_redemption_succeeds_in_safety_mode() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(5, 100)));
		// Drop the price so branch TCR falls below the 130% safety threshold while
		// the vault stays above 100% and remains redeemable — i.e. the branch is
		// in Safety mode (mode is derived from live TCR).
		set_price(DOT, FixedU128::from_rational(6u128, 10u128));
		assert!(
			branch_tcr() < FixedU128::from_rational(130u128, 100u128),
			"fixture must put the branch below the safety threshold"
		);

		let debt_before = vault_debt(1);
		mint_pusd(3, 1_000_000);
		let redeemer_before = pusd_balance(3);
		let recipient_before = collateral_balance(4);
		let fee_before = pusd_balance(FEE_ACCOUNT);
		let tcr_before = branch_tcr();

		// 201 pUSD cancels floor(201/1.005) = 200 debt at price 0.6, paying
		// floor(200/0.6) = 333 collateral and ceil(200*0.005) = 1 fee.
		assert_ok!(redeem(3, 201, 0, 4));
		assert_eq!(debt_before - vault_debt(1), 200);
		assert_eq!(redeemer_before - pusd_balance(3), 201);
		assert_eq!(collateral_balance(4) - recipient_before, 333);
		assert_eq!(pusd_balance(FEE_ACCOUNT) - fee_before, 1);
		// Ordinary redemptions are permitted in Safety mode precisely because they
		// raise branch TCR — the invariant that legitimizes them here.
		assert!(branch_tcr() > tcr_before, "redemption must raise TCR in safety mode");
	});
}

#[test]
fn redeem_with_oracle_down_reverts() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(5, 100)));
		mint_pusd(3, 1_000);
		// The preamble reads the oracle itself, so a failing feed is refused
		// before any vault is touched.
		MockOracleAvailable::set(false);
		assert_noop!(redeem(3, 201, 0, 4), Error::<Test>::OracleUnavailable);
	});
}

#[test]
fn redeem_zero_price_reverts() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 1_000, 500, rate_pct(5, 100)));
		mint_pusd(3, 1_000);
		set_price(DOT, FixedU128::zero());
		assert_noop!(redeem(3, 201, 0, 4), Error::<Test>::OracleUnavailable);
	});
}

#[test]
fn ordinary_redemption_accrues_target_interest_before_cancelling() {
	build_and_execute(|| {
		register_default_branch();
		// A full year at 100% APR makes accrued interest a visible, large integer.
		assert_ok!(open(1, 1_000_000, 500_000, rate_pct(100, 100)));
		let debt_at_open = vault_debt(1);
		let stamped_at_open = vault_interest_time(1);
		mint_pusd(3, 2_000_000);

		advance_time(ONE_YEAR_MS);

		// The preview pokes in a rolled-back transaction, so it reports the fully
		// accrued debt the live redemption will operate on.
		let accrued = preview_full_debt(1);
		assert!(accrued > debt_at_open, "a year of interest must grow the debt");

		let redeemer_before = pusd_balance(3);
		let recipient_before = collateral_balance(4);
		let fee_before = pusd_balance(FEE_ACCOUNT);
		assert_ok!(redeem(3, 1_005, 0, 4));

		// Redeeming poked the target: its interest clock advanced to now, and the
		// 1_000 debt cancellation landed on the accrued balance, not the stale
		// opening principal.
		assert_eq!(vault_interest_time(1), branch_interest_time(now_ms()));
		assert!(vault_interest_time(1) > stamped_at_open);
		assert_eq!(vault_debt(1), accrued - 1_000);
		// Every dimension of the fill: 1_000 debt + ceil(1_000*0.005) = 5 fee in,
		// floor(1_000/1.25) = 800 collateral out.
		assert_eq!(redeemer_before - pusd_balance(3), 1_005);
		assert_eq!(collateral_balance(4) - recipient_before, 800);
		assert_eq!(pusd_balance(FEE_ACCOUNT) - fee_before, 5);
	});
}

#[test]
fn ordinary_redemption_strictly_improves_branch_tcr() {
	build_and_execute(|| {
		register_default_branch();
		// A face-value redemption removes pUSD debt and at most an equal
		// pUSD-value of collateral, so a TCR above 1 can only rise. This is why
		// the SPEC permits ordinary redemptions in Safety mode.
		assert_ok!(open(1, 10_000, 5_000, rate_pct(5, 100)));
		mint_pusd(3, 1_000_000);
		let tcr_before = branch_tcr();
		assert!(tcr_before > FixedU128::one());

		assert_ok!(redeem(3, 201, 0, 4));

		assert!(branch_tcr() > tcr_before, "redemption must improve branch TCR");
	});
}

#[test]
fn preview_matches_execution_for_partial_fill() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, 10_000, 5_000, rate_pct(5, 100)));
		mint_pusd(3, 1_000_000);

		let budget = 1_005;
		let preview =
			pallet_redemptions::Pallet::<Test>::preview_redeem(DOT, PUSD, budget).expect("preview");
		assert_eq!(preview.steps, 1);

		let redeemer_before = pusd_balance(3);
		let recipient_before = collateral_balance(4);
		let fee_before = pusd_balance(FEE_ACCOUNT);
		let v1_before = vault_debt(1);

		assert_ok!(redeem(3, budget, 0, 4));

		// Execution reproduces the quote exactly across every dimension.
		assert_eq!(redeemer_before - pusd_balance(3), preview.total_pusd_in);
		assert_eq!(collateral_balance(4) - recipient_before, preview.total_collateral_out);
		assert_eq!(pusd_balance(FEE_ACCOUNT) - fee_before, preview.total_fee_pusd);
		assert_eq!(v1_before - vault_debt(1), preview.steps_detail[0].debt_cancellable);
	});
}

#[test]
fn split_redemptions_equal_a_single_redemption_without_fees() {
	// With fees neutralized, redeeming a total in equal chunks must net the same
	// collateral out, debt cancelled, and issuance drop as redeeming it in one
	// call: pure path-independence of the redemption mechanic.
	let run = |chunks: &[Balance]| -> (Balance, Balance, Balance) {
		let mut result = (0, 0, 0);
		build_and_execute(|| {
			register_default_branch();
			set_fee_free_config();
			assert_ok!(open(1, 100_000, 10_000, rate_pct(5, 100)));
			mint_pusd(3, 1_000_000);
			let debt_before = vault_debt(1);
			let coll_before = collateral_balance(4);
			let issuance_before = pusd_issuance();
			for &amount in chunks {
				assert_ok!(redeem(3, amount, 0, 4));
			}
			result = (
				collateral_balance(4) - coll_before,
				debt_before - vault_debt(1),
				issuance_before - pusd_issuance(),
			);
		});
		result
	};

	assert_eq!(run(&[100, 100, 100]), run(&[300]));
}
