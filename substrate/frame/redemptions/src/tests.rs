use crate::{
	mock::*,
	types::{RecoveryOffsetQuote, RecoveryRegime, RedemptionConfig, RedemptionQuote},
	weights::WeightInfo,
	Error, Event,
};
use pusd_primitives::{
	collateralization_ratio, recovery_pricing, reducible_debit, LiquidationSettlement, Position,
	RecoveryOffsetInterface, RecoveryOffsetResult, VaultInterface,
};

const HOUR_MS: Moment = 3_600 * 1_000;
const ONE_YEAR_MS: Moment = 31_557_600_000;

fn rate_pct(num: u128, denom: u128) -> FixedU128 {
	FixedU128::from_rational(num, denom)
}

/// Branch TCR as the vault pallet reports it, including pending interest.
fn branch_tcr(collateral: AssetId, stable: StableId) -> FixedU128 {
	Vaults::branch_tcr(collateral, stable).expect("branch registered")
}

/// The fully accrued debt of `who`, read through Vaults' projected snapshot.
fn projected_full_debt(who: AccountId) -> Balance {
	Vaults::project_redemption_snapshot(&DOT, &PUSD, &who).expect("snapshot").debt
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
		assert!(crate::RedemptionConfigs::<Test>::get(PUSD).is_none());
		register_branch(DOT, PUSD, default_branch_config());
		let cfg = crate::RedemptionConfigs::<Test>::get(PUSD).expect("seeded on registration");
		assert_eq!(cfg.minimum_redemption_amount, 100);
	});
}

// Config and fee state are refcounted per stablecoin: a second market on the
// coin reuses the live row rather than reseeding it, and only the last market
// to leave clears it. Otherwise one market deregistering would wipe fee state
// its siblings are still pricing against.
#[test]
fn redemption_config_outlives_all_but_the_last_market_on_the_coin() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		set_dynamic_fee(PUSD, FixedU128::from_rational(3u128, 100u128));

		set_price(TOKEN_X, FixedU128::one());
		assert_ok!(Vaults::create_branch(
			RuntimeOrigin::root(),
			TOKEN_X,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
		));

		// The second market joins the coin's existing fee state untouched.
		assert_eq!(
			crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee,
			FixedU128::from_rational(3u128, 100u128)
		);

		assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), TOKEN_X, PUSD));
		assert!(crate::RedemptionConfigs::<Test>::get(PUSD).is_some());
		assert_eq!(
			crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee,
			FixedU128::from_rational(3u128, 100u128)
		);

		// The last market out clears both rows.
		assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), DOT, PUSD));
		assert!(crate::RedemptionConfigs::<Test>::get(PUSD).is_none());
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, FixedU128::zero());
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
			Vaults::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
			),
			Error::<Test>::InvalidRedemptionConfig
		);
		assert!(crate::RedemptionConfigs::<Test>::get(PUSD).is_none());
		assert!(Vaults::branch_tcr(DOT, PUSD).is_none());
	});
}

#[test]
fn redeem_below_minimum_amount_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000);
		assert_noop!(
			redeem(3, DOT, PUSD, 99, 0, 4, 0),
			Error::<Test>::BelowMinimumRedemptionAmount
		);
	});
}

#[test]
fn redeem_unregistered_branch_reverts() {
	build_and_execute(|| {
		mint_stable(PUSD, 3, 1_000);
		assert_noop!(redeem(3, DOT, PUSD, 200, 0, 4, 0), Error::<Test>::InvalidBranch);
	});
}

#[test]
fn redeem_frozen_branch_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000);
		assert_ok!(Vaults::set_governance_frozen(RuntimeOrigin::signed(ADMIN), DOT, PUSD, true));
		// Frozen-mode enforcement lives vault-side: the first `redeem_step`
		// rejects the frozen branch and the whole redemption rolls back.
		assert_noop!(
			redeem(3, DOT, PUSD, 200, 0, 4, 0),
			pallet_vaults::Error::<Test>::BranchFrozen
		);
	});
}

#[test]
fn redeem_no_vault_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 3, 1_000);
		assert_noop!(redeem(3, DOT, PUSD, 200, 0, 4, 0), Error::<Test>::NoRedeemableVault);
	});
}

#[test]
fn ordinary_redemption_hits_lowest_rate_vault_and_settles_every_dimension() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		mint_stable(PUSD, 3, 1_000_000);
		let v1_before = vault_debt(DOT, PUSD, 1);
		let v1_held_before = held(DOT, 1);
		let v2_before = vault_debt(DOT, PUSD, 2);
		let v2_held_before = held(DOT, 2);
		let redeemer_before = Assets::balance(PUSD, 3);
		let recipient_before = collateral_balance(DOT, 4);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let issuance_before = Assets::total_supply(PUSD);

		// 201 is debt to cancel, with the fee on top. Both vaults owe 501 (500
		// plus a 1-unit 7-day upfront fee), so the coin carries 1_002 and this
		// redemption is 201/1_002 = 20.06% of it: the accelerator rises to half
		// that, 10.03%, and the redemption pays the rate it caused —
		// 10.03% + 0.5% = 10.53%, so ceil(201 * 0.105299) = 22 fee. Collateral
		// is floor(201/1.25) = 160.
		assert_ok!(redeem(3, DOT, PUSD, 201, 0, 4, 0));

		// The lowest-rate vault absorbs the whole fill, in debt and in held
		// collateral; the higher-rate vault is untouched in both.
		assert_eq!(v1_before - vault_debt(DOT, PUSD, 1), 201);
		assert_eq!(v1_held_before - held(DOT, 1), 160);
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
		assert_eq!(held(DOT, 2), v2_held_before);
		// Money movement across every dimension: the redeemer pays the debt it
		// cancelled plus the fee, which is charged on top rather than out of it.
		assert_eq!(redeemer_before - Assets::balance(PUSD, 3), 223);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 160);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, 22);
		// Fees are transferred, so issuance must only fall by cancelled debt.
		assert_eq!(issuance_before - Assets::total_supply(PUSD), 201);
		// The event reports exactly the figures it settled.
		System::assert_has_event(RuntimeEvent::Redemptions(Event::OrdinaryRedemptionExecuted {
			collateral_id: DOT,
			stable_id: PUSD,
			redeemer: 3,
			recipient: 4,
			pusd_burned: 201,
			collateral_out: 160,
			fee_pusd: 22,
			steps: 1,
		}));
	});
}

#[test]
fn redemption_partially_fills_to_budget() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 10_000, 5_000, rate_pct(5, 100)));
		let debt_before = vault_debt(DOT, PUSD, 1);
		mint_stable(PUSD, 3, 1_000_000);
		let redeemer_before = Assets::balance(PUSD, 3);
		let recipient_before = collateral_balance(DOT, 4);
		let held_before = held(DOT, 1);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let issuance_before = Assets::total_supply(PUSD);

		// The sole vault owes 5_005, so cancelling 1_005 leaves debt to spare.
		// 1_005/5_005 = 20.08% of the coin raises the accelerator to 10.04%,
		// and the redemption pays 10.04% + 0.5%: ceil(1_005 * 0.105400) = 106.
		assert_ok!(redeem(3, DOT, PUSD, 1_005, 0, 4, 0));
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before - 1_005);
		assert_eq!(redeemer_before - Assets::balance(PUSD, 3), 1_111);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 804);
		assert_eq!(held_before - held(DOT, 1), 804);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, 106);
		// Fees are routed, so issuance falls only by the cancelled debt.
		assert_eq!(issuance_before - Assets::total_supply(PUSD), 1_005);
	});
}

#[test]
fn caller_max_steps_caps_the_loop() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		mint_stable(PUSD, 3, 1_000_000);
		let v2_before = vault_debt(DOT, PUSD, 2);

		assert_ok!(redeem(3, DOT, PUSD, 100_000, 0, 4, 1));
		assert_eq!(vault_debt(DOT, PUSD, 1), 0);
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
	});
}

fn quoted_walk(step_cap: u32, budget: Balance) -> RedemptionQuote<Balance> {
	Redemptions::preview_redeem(DOT, PUSD, budget, step_cap).expect("quote")
}

#[test]
fn walk_reports_only_cap_exhaustion_as_truncated() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));

		// A one-step cap with budget and a second target to spare: only the
		// cap guard reports truncation.
		let capped = quoted_walk(1, 100_000);
		assert_eq!(capped.steps, 1);
		assert!(capped.truncated, "the cap guard ended the walk");

		// Budget exhaustion inside the cap is a complete walk, not truncation.
		let filled = quoted_walk(20, 200);
		assert_eq!(filled.steps, 1);
		assert!(!filled.truncated);

		// Park a Dormant husk at the priority slot and sink the price: the
		// underwater Dormant head is a barrier — counted, but not truncation.
		mint_stable(PUSD, 3, 1_000);
		assert_ok!(redeem(3, DOT, PUSD, 360, 0, 4, 0));
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault 1").is_dormant());
		set_price(DOT, FixedU128::from_rational(1, 10));
		assert!(Redemptions::preview_redeem(DOT, PUSD, 100_000, 20).is_none());
	});
}

/// `max_steps` exists to bound the pre-paid weight; the dispatch then refunds
/// down to the steps actually executed.
#[test]
fn redeem_refunds_weight_to_actual_steps() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		mint_stable(PUSD, 3, 1_000_000);

		// The 201 budget is exhausted by the head vault: one executed step,
		// while the caller pre-paid for five.
		let post = redeem(3, DOT, PUSD, 201, 0, 4, 5).expect("redemption succeeds");
		assert_eq!(post.actual_weight, Some(<() as WeightInfo>::redeem(1)));
		assert!(<() as WeightInfo>::redeem(1).all_lt(<() as WeightInfo>::redeem(5)));
	});
}

#[test]
fn underwater_ordinary_vault_is_skipped() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Open the well-collateralized vault first so the thin vault 1 doesn't
		// briefly push the fresh branch into Safety mode at the genesis price.
		assert_ok!(open(2, DOT, PUSD, 3_000, 240, rate_pct(2, 100)));
		assert_ok!(open(1, DOT, PUSD, 250, 240, rate_pct(1, 100)));
		// Vault 1 stays active but underwater, so redemption must skip it.
		set_price(DOT, FixedU128::from_rational(9, 10));
		mint_stable(PUSD, 3, 1_000_000);
		let v1_before = vault_debt(DOT, PUSD, 1);
		let v1_held_before = held(DOT, 1);
		let v2_before = vault_debt(DOT, PUSD, 2);
		let v2_held_before = held(DOT, 2);
		let recipient_before = collateral_balance(DOT, 4);
		let fee_before = Assets::balance(PUSD, FEE_DEST);

		// Both vaults owe 241, so the coin carries 482 and cancelling 100 of it
		// is 20.75%: the accelerator rises to 10.37% and the redemption pays
		// 10.37% + 0.5%, ceil(100 * 0.108734) = 11. The whole 100 lands on
		// vault 2, paying floor(100/0.9) = 111 collateral.
		assert_ok!(redeem(3, DOT, PUSD, 100, 0, 4, 0));
		// The skipped underwater vault keeps its debt and its held collateral.
		assert_eq!(vault_debt(DOT, PUSD, 1), v1_before);
		assert_eq!(held(DOT, 1), v1_held_before);
		// The healthy vault behind it is redeemed across every dimension.
		assert_eq!(v2_before - vault_debt(DOT, PUSD, 2), 100);
		assert_eq!(v2_held_before - held(DOT, 2), 111);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 111);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, 11);
	});
}

#[test]
fn underwater_prefix_skipped_once_while_healthy_vaults_redeem() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Open the well-collateralized vaults first so the thin low-rate vaults
		// don't briefly push the fresh branch into Safety mode at the genesis price.
		assert_ok!(open(3, DOT, PUSD, 3_000, 240, rate_pct(3, 100)));
		assert_ok!(open(4, DOT, PUSD, 3_000, 240, rate_pct(4, 100)));
		assert_ok!(open(1, DOT, PUSD, 250, 240, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 260, 240, rate_pct(2, 100)));
		set_price(DOT, FixedU128::from_rational(9, 10));
		mint_stable(PUSD, 5, 1_000_000);
		let v1_before = vault_debt(DOT, PUSD, 1);
		let v2_before = vault_debt(DOT, PUSD, 2);
		let v3_before = vault_debt(DOT, PUSD, 3);
		let v4_before = vault_debt(DOT, PUSD, 4);
		let preview = Redemptions::preview_redeem(DOT, PUSD, 2_000, 0).expect("preview");
		assert_eq!(preview.steps, 4, "the walk visits the skipped prefix once");
		assert_eq!(preview.debt_cancelled(), v3_before + v4_before);
		assert!(!preview.truncated);

		let redeemer_before = Assets::balance(PUSD, 5);
		let recipient_before = collateral_balance(DOT, 6);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let issuance_before = Assets::total_supply(PUSD);

		assert_ok!(redeem(5, DOT, PUSD, 2_000, 0, 6, 0));
		// The underwater prefix (vaults 1-2) is skipped and left untouched.
		assert_eq!(vault_debt(DOT, PUSD, 1), v1_before);
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
		// The healthy vaults behind it drain fully.
		assert_eq!(vault_debt(DOT, PUSD, 3), 0);
		assert_eq!(vault_debt(DOT, PUSD, 4), 0);
		// Collateral is paid at face value, floored per step at price 0.9 (= debt * 10 / 9).
		let expected_collateral = v3_before * 10 / 9 + v4_before * 10 / 9;
		assert_eq!(collateral_balance(DOT, 6) - recipient_before, expected_collateral);
		// Both adapters consume the same shared walk decisions.
		assert_eq!(redeemer_before - Assets::balance(PUSD, 5), preview.stable_in);
		assert_eq!(collateral_balance(DOT, 6) - recipient_before, preview.collateral_out);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, preview.fee);
		// Issuance falls by exactly the debt burned; fees are routed, not burned.
		assert_eq!(issuance_before - Assets::total_supply(PUSD), v3_before + v4_before);
	});
}

#[test]
fn slippage_bound_reverts_without_state_change() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		let debt_before = vault_debt(DOT, PUSD, 1);
		mint_stable(PUSD, 3, 1_000_000);
		let redeemer_before = Assets::balance(PUSD, 3);
		let recipient_before = collateral_balance(DOT, 4);
		let held_before = held(DOT, 1);
		let issuance_before = Assets::total_supply(PUSD);

		// 201 pUSD would cancel 200 debt for only floor(200/1.25) = 160 collateral,
		// below the 161 floor, so the whole redemption reverts with no side effects.
		assert_noop!(redeem(3, DOT, PUSD, 201, 161, 4, 0), Error::<Test>::SlippageExceeded);
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before);
		assert_eq!(Assets::balance(PUSD, 3), redeemer_before);
		assert_eq!(collateral_balance(DOT, 4), recipient_before);
		assert_eq!(held(DOT, 1), held_before);
		assert_eq!(Assets::total_supply(PUSD), issuance_before);
	});
}

/// A partial fill scales the caller's slippage floor pro-rata to the pUSD
/// actually spent, so a floor quoted for the full budget
/// cannot spuriously revert a smaller fill.
#[test]
fn slippage_floor_scales_to_partial_fill() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000_000);

		// The sole vault owes 501 (500 + the 1-unit 7-day upfront fee) and the
		// request to cancel 1_000 over-covers it: the fill cancels all 501 and
		// pays floor(501/1.25) = 400 collateral. Only 501 of the 1_000 asked for
		// was cancelled, so the floor scales to floor(min · 501/1_000): 801
		// scales to 401 > 400 (reverts), 800 scales to 400 (fills).
		assert_noop!(redeem(3, DOT, PUSD, 1_000, 801, 4, 0), Error::<Test>::SlippageExceeded);

		let recipient_before = collateral_balance(DOT, 4);
		assert_ok!(redeem(3, DOT, PUSD, 1_000, 800, 4, 0));
		assert_eq!(vault_debt(DOT, PUSD, 1), 0);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 400);
	});
}

#[test]
fn insufficient_pusd_balance_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 50);
		assert_noop!(redeem(3, DOT, PUSD, 201, 0, 4, 0), Error::<Test>::InsufficientPusdBalance);
	});
}

#[test]
fn wallet_bound_uses_the_fee_raised_by_the_affordable_debt() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 10_000, 1_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000);
		let debt_before = vault_debt(DOT, PUSD, 1);
		let fee_before = Assets::balance(PUSD, FEE_DEST);

		assert_ok!(redeem(3, DOT, PUSD, 1_000, 0, 4, 0));

		// The wallet can afford 730 debt plus the 270 fee that 730 itself
		// raises. Pricing the unreachable 1_000-debt maximum would reserve a
		// much higher rate and cancel only 664.
		assert_eq!(debt_before - vault_debt(DOT, PUSD, 1), 730);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, 270);
		assert_eq!(Assets::balance(PUSD, 3), 0);
	});
}

#[test]
fn dynamic_fee_rises_after_ordinary_redemption() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 100_000, 50_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000_000);
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, FixedU128::zero());
		let debt_before = vault_debt(DOT, PUSD, 1);
		let stablecoin_debt_before = stablecoin_debt(PUSD);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let recipient_before = collateral_balance(DOT, 4);

		assert_ok!(redeem(3, DOT, PUSD, 10_000, 0, 4, 0));

		// 10_000 is debt to cancel, and the fee rides on top. The vault owes
		// 50_048, so this is 19.98% of the coin: the accelerator rises to 9.99%
		// and this same redemption pays 9.99% + 0.5%, ceil(10_000 * 0.104904) =
		// 1_050. Collateral is 10_000/1.25 = 8_000.
		assert_eq!(debt_before - vault_debt(DOT, PUSD, 1), 10_000);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, 1_050);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 8_000);
		// The stablecoin debt aggregate falls by exactly the cancelled debt.
		assert_eq!(stablecoin_debt_before - stablecoin_debt(PUSD), 10_000);

		// The new dynamic fee is decayed(0) + redeemed_fraction / increase_divisor,
		// computed against the stablecoin debt captured before the redemption.
		let fraction = FixedU128::checked_from_rational(10_000u128, stablecoin_debt_before)
			.expect("nonzero stablecoin debt");
		let expected = crate::fees::increased_dynamic_fee(
			FixedU128::zero(),
			fraction,
			FixedU128::from_rational(2, 1),
			FixedU128::zero(),
			FixedU128::one(),
		);
		assert!(expected > FixedU128::zero());
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, expected);
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).last_fee_operation, 1_000);
	});
}

// The fee nudges how much of a coin is redeemed, whichever collateral backs it:
// a DOT/PUSD redemption raises the fee a TOKEN_X/PUSD redeemer then pays, and
// the redeemed fraction is measured against both markets' debt, not just DOT's.
#[test]
fn redeeming_one_collateral_raises_the_fee_on_its_sibling_markets() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		register_branch(TOKEN_X, PUSD, default_branch_config());

		assert_ok!(open(1, DOT, PUSD, 100_000, 50_000, rate_pct(5, 100)));
		mint_collateral(TOKEN_X_ID, 2, 200_000);
		assert_ok!(open(2, TOKEN_X, PUSD, 100_000, 50_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000_000);

		// Both markets' debt forms the denominator, so the same redemption moves
		// the fee half as far as it would against DOT/PUSD alone.
		let both_markets = stablecoin_debt(PUSD);
		assert_eq!(both_markets, 2 * branch_outstanding(DOT, PUSD));

		let fee_before = Assets::balance(PUSD, FEE_DEST);
		assert_ok!(redeem(3, DOT, PUSD, 10_000, 0, 4, 0));
		let dot_fee = Assets::balance(PUSD, FEE_DEST) - fee_before;

		let fraction = FixedU128::checked_from_rational(10_000u128, both_markets)
			.expect("nonzero stablecoin debt");
		let raised = crate::fees::increased_dynamic_fee(
			FixedU128::zero(),
			fraction,
			FixedU128::from_rational(2, 1),
			FixedU128::zero(),
			FixedU128::one(),
		);
		assert!(raised > FixedU128::zero());
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, raised);

		// The TOKEN_X redeemer, who redeemed nothing, now pays the raised rate.
		// Both cancel the same 10_000 of debt, but the first paid
		// ceil(10_000 · (4.995% + 0.5%)) = 550 against an untouched accelerator,
		// while the second starts from that 4.995%, adds half of 10_000/90_096
		// on top, and pays ceil(10_000 · (10.545% + 0.5%)) = 1_105.
		let debt_before = vault_debt(TOKEN_X, PUSD, 2);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		assert_ok!(redeem(3, TOKEN_X, PUSD, 10_000, 0, 4, 0));
		assert_eq!(debt_before - vault_debt(TOKEN_X, PUSD, 2), 10_000);
		assert_eq!(dot_fee, 550);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, 1_105);
	});
}

// The redemption that raises the accelerator pays the raised rate, so a large
// redemption after a quiet period cannot pay only the decayed one. Splitting it
// into two halves is therefore never cheaper than doing it at once — the first
// half raises the rate the second half then pays.
#[test]
fn a_redemption_pays_the_rate_it_raises() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000_000, 500_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 2_000_000);
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, FixedU128::zero());

		// Arriving at a fully-decayed accelerator does not buy the decayed rate:
		// 20_000 of the coin's 500_048 is 4.0%, so the rate rises to 2.0% and
		// this same redemption pays 2.0% + 0.5%, ceil(20_000 * 0.024999) = 500.
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		assert_ok!(redeem(3, DOT, PUSD, 20_000, 0, 4, 0));
		let at_once = Assets::balance(PUSD, FEE_DEST) - fee_before;
		assert_eq!(at_once, 500);
		// A pre-increase charge would have been the bare base fee on the same
		// debt, which is what this redesign is meant to stop.
		assert!(at_once > 20_000 * 5 / 1_000);
	});
}

#[test]
fn dynamic_fee_decays_between_redemptions() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000_000, 500_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 2_000_000);

		assert_ok!(redeem(3, DOT, PUSD, 100_000, 0, 4, 0));
		let raised = crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee;
		assert!(raised > FixedU128::zero());

		// 24h at the 6h half-life is four whole half-lives, so the decayed fee
		// the next redemption observes is exactly `raised / 2^4`.
		advance_time(24 * HOUR_MS);
		let decayed = FixedU128::from_inner(raised.into_inner() >> 4);
		let stablecoin_debt_before = stablecoin_debt(PUSD);
		assert_ok!(redeem(3, DOT, PUSD, 1_000, 0, 4, 0));

		// The stored fee is that exact decayed value plus this redemption's own
		// increase, reproduced here with the very primitives execution uses. The
		// whole 1_000 is cancelled debt now that the fee is charged on top.
		let fraction = FixedU128::checked_from_rational(1_000u128, stablecoin_debt_before)
			.expect("nonzero stablecoin debt");
		let expected = crate::fees::increased_dynamic_fee(
			decayed,
			fraction,
			FixedU128::from_rational(2, 1),
			FixedU128::zero(),
			FixedU128::one(),
		);
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, expected);
		assert!(expected < raised, "the decay must outweigh a small re-increase");
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).last_fee_operation, Timestamp::get());
	});
}

#[test]
fn dynamic_fee_fully_decays_to_the_base_fee_after_long_idle() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000_000, 500_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 2_000_000);
		assert_ok!(redeem(3, DOT, PUSD, 100_000, 0, 4, 0));
		assert!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee > FixedU128::zero());

		// A year idle is 1_461 six-hour half-lives (≥ the 128 the shift-based
		// decay supports), so the dynamic fee saturates to exactly zero.
		advance_time(ONE_YEAR_MS);
		let stablecoin_debt_before = stablecoin_debt(PUSD);
		// The redemption pokes a year of pending interest onto the vault, so the
		// pre-poke storage value would understate the debt the cancel lands on.
		let accrued = projected_full_debt(1);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		assert_ok!(redeem(3, DOT, PUSD, 200, 0, 4, 0));

		// The requested debt is cancelled whatever the rate, so the zero-residue
		// proof lives in the fee state: it rebuilds from exactly zero, as a first
		// redemption would. A residue as small as 1e-18 would survive into the
		// stored value and fail this.
		assert_eq!(vault_debt(DOT, PUSD, 1), accrued - 200);
		let fraction = FixedU128::checked_from_rational(200u128, stablecoin_debt_before)
			.expect("nonzero stablecoin debt");
		let expected = crate::fees::increased_dynamic_fee(
			FixedU128::zero(),
			fraction,
			FixedU128::from_rational(2, 1),
			FixedU128::zero(),
			FixedU128::one(),
		);
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, expected);
		// A year of idle leaves a 500_048-debt coin, so this redemption's own
		// increase is 200/500_048/2 = 0.02%: the charged rate is 0.52% and the
		// fee ceil(200 * 0.0052) = 2.
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, 2);
	});
}

#[test]
fn dormant_target_is_redeemed_before_rate_index() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		mint_stable(PUSD, 3, 1_000_000);

		assert_ok!(redeem(3, DOT, PUSD, 360, 0, 4, 0));
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault 1").is_dormant());
		assert_eq!(
			pallet_vaults::Branches::<Test>::get(DOT, PUSD)
				.unwrap()
				.state
				.dormant_redemption_target,
			Some(1)
		);

		let v2_before = vault_debt(DOT, PUSD, 2);
		let v1_residual = vault_debt(DOT, PUSD, 1);
		let recipient_before = collateral_balance(DOT, 4);
		// Vault 1 is now Dormant (out of the rate index); the only way the second
		// redemption can reach it is via the Dormant slot, which is served before
		// the rate index. It redeems the Dormant vault and never touches ordinary
		// vault 2.
		assert_ok!(redeem(3, DOT, PUSD, 100, 0, 4, 0));
		// Cancel less than the residual so the husk survives the step and its
		// row stays readable: a full drain removes the vault, and the debt
		// cancelled could then only be inferred.
		assert_eq!(v1_residual, 141);
		assert_eq!(v1_residual - vault_debt(DOT, PUSD, 1), 100);
		// Collateral paid is the face-value amount for the debt cancelled.
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 80);
		// Priority: the ordinary vault behind the Dormant slot is untouched.
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
	});
}

#[test]
fn quote_continues_from_drained_dormant_into_rate_index() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		mint_stable(PUSD, 3, 1_000_000);

		// A sub-minimum residual parks vault 1 as the dormant redemption target.
		assert_ok!(redeem(3, DOT, PUSD, 360, 0, 4, 0));
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault 1").is_dormant());
		let v1_residual = vault_debt(DOT, PUSD, 1);
		let v2_debt = vault_debt(DOT, PUSD, 2);

		// Budget beyond both targets: the quote must drain the dormant slot,
		// then continue into the rate index rather than stopping at the
		// priority tier.
		let quote = Redemptions::preview_redeem(DOT, PUSD, 100_000, 0).expect("quote");
		assert_eq!(quote.steps, 2);
		assert!(!quote.truncated);
		assert_eq!(quote.debt_cancelled(), v1_residual + v2_debt);

		// Execution walks the same two targets and reproduces the quote exactly.
		let redeemer_before = Assets::balance(PUSD, 3);
		let recipient_before = collateral_balance(DOT, 4);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		assert_ok!(redeem(3, DOT, PUSD, 100_000, 0, 4, 0));
		assert_eq!(redeemer_before - Assets::balance(PUSD, 3), quote.stable_in);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, quote.collateral_out);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, quote.fee);
		assert_eq!(vault_debt(DOT, PUSD, 1), 0);
		assert_eq!(vault_debt(DOT, PUSD, 2), 0);
	});
}

fn setup_final_recovery(who: AccountId, coll: Balance, debt: Balance, recovery_price: FixedU128) {
	// Reset to a healthy price so the vault opens cleanly even when the branch
	// already holds FinalRecovery vaults parked at a depressed price.
	set_price(DOT, FixedU128::from_rational(5u128, 4u128));
	assert_ok!(open(who, DOT, PUSD, coll, debt, rate_pct(5, 100)));
	set_price(DOT, recovery_price);
	assert_ok!(enter_final_recovery(DOT, PUSD, who));
	assert!(Vaults::vault_status(DOT, PUSD, who).expect("fr vault").is_final_recovery());
}

#[test]
fn recovery_bonus_pays_more_than_face_value() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		mint_stable(PUSD, 3, 1_000_000);
		let recipient_before = collateral_balance(DOT, 4);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let debt_before = vault_debt(DOT, PUSD, 1);

		assert_ok!(redeem(3, DOT, PUSD, 200, 0, 4, 0));

		let collateral_out = collateral_balance(DOT, 4) - recipient_before;
		// CR = 520/501 ≈ 103.79%, so the bonus is the mid-range excess
		// CR − 100% − 1% buffer ≈ 2.79% (below the 5% penalty cap):
		// floor(floor(200 · 1.0279) / 0.52) = floor(205 / 0.52) = 394.
		assert_eq!(collateral_out, 394, "recovery bonus payout");
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before - 200);
		assert_eq!(Assets::balance(PUSD, FEE_DEST), fee_before);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
	});
}

#[test]
fn recovery_bonus_buffer_keeps_redemption_cr_improving() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Price 0.52 parks the FinalRecovery vault's CR barely above 100%, the
		// regime where an overpaid bonus could actually damage the vault.
		let price = FixedU128::from_rational(52u128, 100u128);
		setup_final_recovery(1, 1_000, 500, price);
		mint_stable(PUSD, 3, 1_000_000);

		let cr_before = collateralization_ratio(
			&Position { debt: vault_debt(DOT, PUSD, 1), collateral: held(DOT, 1) },
			price,
		)
		.expect("finite CR");
		assert!(cr_before > rate_pct(101, 100), "fixture must clear the 1% buffer");
		// The bonus this fixture produces sits strictly inside (0, penalty):
		// the mid-range case, where only the buffer bounds it.
		let bonus = recovery_pricing::recovery_bonus(
			cr_before,
			Permill::from_percent(1),
			Permill::from_percent(5),
		);
		assert!(bonus > FixedU128::zero());
		assert!(bonus < FixedU128::from(Permill::from_percent(5)));

		assert_ok!(redeem(3, DOT, PUSD, 200, 0, 4, 0));

		// The 1% buffer keeps the bonus strictly below CR − 100%, so paying it
		// must leave the vault's CR strictly better than before the redemption.
		let cr_after = collateralization_ratio(
			&Position { debt: vault_debt(DOT, PUSD, 1), collateral: held(DOT, 1) },
			price,
		)
		.expect("finite CR");
		assert!(cr_after > cr_before, "recovery-bonus redemption must improve the vault's CR");
	});
}

#[test]
fn recovery_has_priority_over_ordinary_vaults() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		// Reset to a healthy price: vault 1 is now a CR >= 100% recovery vault, so
		// the settlement uses the RecoveryBonus regime.
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		let v1_before = vault_debt(DOT, PUSD, 1);
		let v2_before = vault_debt(DOT, PUSD, 2);
		let recipient_before = collateral_balance(DOT, 4);
		mint_stable(PUSD, 3, 1_000_000);

		// At the healthy price the head's CR ≈ 249%, far beyond the 106% at
		// which the cap starts binding, so the bonus must come out clamped to
		// exactly the 5% redistribution penalty rather than the raw excess.
		let cr = collateralization_ratio(
			&Position { debt: v1_before, collateral: held(DOT, 1) },
			FixedU128::from_rational(5u128, 4u128),
		)
		.expect("finite CR");
		assert!(cr > rate_pct(106, 100), "fixture must put the raw excess above the cap");
		let bonus = recovery_pricing::recovery_bonus(
			cr,
			Permill::from_percent(1),
			Permill::from_percent(5),
		);
		assert_eq!(bonus, FixedU128::from(Permill::from_percent(5)), "bonus capped at penalty");

		assert_ok!(redeem(3, DOT, PUSD, 200, 0, 4, 0));
		// The FinalRecovery vault is served at its exact regime, before any ordinary vault.
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
		assert_eq!(v1_before - vault_debt(DOT, PUSD, 1), 200);
		// Capped 5% bonus: floor(200 * 1.05 / 1.25) = 168 collateral.
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 168);
		// The ordinary vault is untouched.
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
	});
}

#[test]
fn preview_reports_final_recovery_before_ordinary_targets() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		// Reset to a healthy price so vault 2 opens as an ordinary rate-index target.
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		let v1_before = vault_debt(DOT, PUSD, 1);
		let v2_before = vault_debt(DOT, PUSD, 2);

		let preview = Redemptions::preview_redeem(DOT, PUSD, 200, 0).expect("preview");

		assert_eq!(preview.steps, 1);
		assert_eq!(preview.stable_in, 200);
		assert_eq!(preview.collateral_out, 168);
		assert_eq!(preview.fee, 0);
		// Quoting must expose the priority target without touching the vault.
		assert_eq!(vault_debt(DOT, PUSD, 1), v1_before);
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
	});
}

#[test]
fn insurance_adjusted_settlement_with_partial_fund() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128));
		let debt = vault_debt(DOT, PUSD, 1);
		// Opening added the 7-day upfront interest fee to the 500 principal:
		// ceil(500 · 5% · 7d/1yr) = 1, so the vault owes 501.
		assert_eq!(debt, 501);
		mint_stable(PUSD, insurance_account(PUSD), 50);
		let market_cancel = debt - 50;
		mint_stable(PUSD, 3, 1_000_000);

		let if_before = Assets::balance(PUSD, insurance_account(PUSD));
		let recipient_before = collateral_balance(DOT, 4);
		let issuance_before = Assets::total_supply(PUSD);

		assert_ok!(redeem(3, DOT, PUSD, market_cancel, 0, 4, 0));

		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
		// D = 501 against C = 400 pUSD (1_000 units at 0.40) with IF = 50: the
		// fund covers 50, leaving market debt 451 at recovery rate 400/451. The
		// fixed-point rate truncates just below the true ratio, so cancelling
		// all 451 yields value floor(451·rate) = 399 (not 400) and collateral
		// floor(399/0.40) = 997 (not 1_000): both floors round against the
		// redeemer, and the 3-unit dust stays behind in the settlement.
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 997);
		assert_eq!(if_before - Assets::balance(PUSD, insurance_account(PUSD)), 50);
		assert_eq!(issuance_before - Assets::total_supply(PUSD), debt);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
	});
}

#[test]
fn insurance_adjusted_settlement_with_empty_fund() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128));
		let debt = vault_debt(DOT, PUSD, 1);
		// 500 principal + the 1-unit 7-day upfront fee (as in the test above).
		assert_eq!(debt, 501);
		assert_eq!(Assets::balance(PUSD, insurance_account(PUSD)), 0);
		mint_stable(PUSD, 3, 1_000_000);
		let recipient_before = collateral_balance(DOT, 4);
		let issuance_before = Assets::total_supply(PUSD);

		assert_ok!(redeem(3, DOT, PUSD, debt, 0, 4, 0));

		assert_eq!(vault_debt(DOT, PUSD, 1), 0);
		// Empty fund: recovery rate = C/D = 400/501, truncated in fixed point,
		// so value = floor(501·rate) = 399 and collateral = floor(399/0.40) =
		// 997 — the same two floors against the redeemer as the test above.
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 997);
		assert_eq!(issuance_before - Assets::total_supply(PUSD), debt);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
	});
}

/// Near-twin of the empty-fund settlement above; the distinct path is that a
/// *different* stablecoin's fund account is flush with pUSD, which must not
/// count as cover for the `(DOT, PUSD)` market — same floors as an empty fund.
#[test]
fn insurance_fund_of_other_stable_is_not_cover() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128));
		let debt = vault_debt(DOT, PUSD, 1);
		assert_eq!(debt, 501);
		let other_stable: StableId = PUSD + 1;
		mint_stable(PUSD, insurance_account(other_stable), 1_000_000);
		assert_eq!(Assets::balance(PUSD, insurance_account(PUSD)), 0);
		mint_stable(PUSD, 3, 1_000_000);
		let recipient_before = collateral_balance(DOT, 4);
		let issuance_before = Assets::total_supply(PUSD);

		assert_ok!(redeem(3, DOT, PUSD, debt, 0, 4, 0));

		assert_eq!(vault_debt(DOT, PUSD, 1), 0);
		// Settles exactly like the empty-fund case: value floor(501·400/501) =
		// 399, collateral floor(399/0.40) = 997.
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 997);
		assert_eq!(Assets::balance(PUSD, insurance_account(other_stable)), 1_000_000);
		assert_eq!(issuance_before - Assets::total_supply(PUSD), debt);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
	});
}

#[test]
fn set_redemption_config_updates_and_validates() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		let mut cfg = crate::RedemptionConfigs::<Test>::get(PUSD).unwrap();
		cfg.minimum_redemption_amount = 250;
		assert_ok!(Redemptions::set_redemption_config(RuntimeOrigin::root(), PUSD, cfg.clone()));
		assert_eq!(
			crate::RedemptionConfigs::<Test>::get(PUSD).unwrap().minimum_redemption_amount,
			250
		);

		// One mutation per invalid-config axis `validate` guards.
		let invalid: [fn(&mut RedemptionConfig<Balance>); 5] = [
			// `dynamic_fee_floor` above `dynamic_fee_ceiling`.
			|c| {
				c.dynamic_fee_floor = FixedU128::one();
				c.dynamic_fee_ceiling = FixedU128::zero();
			},
			// Zero `minimum_redemption_amount`.
			|c| c.minimum_redemption_amount = 0,
			// Zero `dynamic_fee_decay_period`.
			|c| c.dynamic_fee_decay_period = 0,
			// `base_fee` above `fee_ceiling`.
			|c| {
				c.base_fee = Permill::one();
				c.fee_ceiling = Permill::zero();
			},
			// Zero `dynamic_fee_increase_divisor`.
			|c| c.dynamic_fee_increase_divisor = FixedU128::zero(),
		];
		for mutate in invalid {
			let mut bad = cfg.clone();
			mutate(&mut bad);
			assert_noop!(
				Redemptions::set_redemption_config(RuntimeOrigin::root(), PUSD, bad),
				Error::<Test>::InvalidRedemptionConfig
			);
		}
	});
}

#[test]
fn stablecoin_owner_can_update_redemption_config() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		let mut cfg = crate::RedemptionConfigs::<Test>::get(PUSD).unwrap();
		cfg.minimum_redemption_amount = 250;

		// Account 1 owns the PUSD asset in genesis, so it is the coin's
		// authority under the same rule vaults uses for `create_branch`.
		assert_ok!(Redemptions::set_redemption_config(RuntimeOrigin::signed(1), PUSD, cfg));

		assert_eq!(
			crate::RedemptionConfigs::<Test>::get(PUSD).unwrap().minimum_redemption_amount,
			250
		);
	});
}

/// The complete negative origin space for `set_redemption_config`: a plain
/// user with no admin power anywhere, this market's emergency (tighten-only)
/// admin, and another market's full admin. Only Root and this market's full
/// admin pass — the two accept paths the tests above pin.
#[test]
fn set_redemption_config_rejects_market_admins() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		let other_admin: AccountId = 55;
		set_price(TOKEN_X, FixedU128::one());
		assert_ok!(Vaults::create_branch(
			RuntimeOrigin::root(),
			TOKEN_X,
			PUSD,
			branch_admins(other_admin, other_admin),
			default_branch_config(),
		));
		let cfg = crate::RedemptionConfigs::<Test>::get(PUSD).unwrap();

		// One config now governs both PUSD markets, so no single market's admin
		// may set it — otherwise the DOT/PUSD admin would price TOKEN_X/PUSD
		// redemptions, and vice versa.
		for wrong in [ADMIN, EMERGENCY_ADMIN, other_admin] {
			assert_noop!(
				Redemptions::set_redemption_config(RuntimeOrigin::signed(wrong), PUSD, cfg.clone()),
				BadOrigin
			);
		}
	});
}

#[test]
fn set_redemption_config_unregistered_branch_reverts() {
	build_and_execute(|| {
		let cfg = DefaultRedemptionConfig::get();
		assert_noop!(
			Redemptions::set_redemption_config(RuntimeOrigin::root(), PUSD, cfg),
			Error::<Test>::InvalidBranch
		);
	});
}

#[test]
fn preview_redeem_below_minimum_amount_returns_none() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));

		assert!(Redemptions::preview_redeem(DOT, PUSD, 99, 0).is_none());
	});
}

#[test]
fn preview_redeem_quotes_without_side_effects() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		let debt_before = vault_debt(DOT, PUSD, 1);

		// The quote prices the fee the same way execution does: cancelling 201 of
		// the coin's 501 debt raises the accelerator to 20.06%, so the fee is
		// ceil(201 * 0.205598) = 42 and the redeemer would spend 243 in total.
		let preview = Redemptions::preview_redeem(DOT, PUSD, 201, 0).expect("preview");
		assert_eq!(preview.steps, 1);
		assert!(!preview.truncated);
		assert_eq!(preview.stable_in, 243);
		assert_eq!(preview.debt_cancelled(), 201);
		assert_eq!(preview.collateral_out, 160);
		assert_eq!(preview.fee, 42);
		// Quoting projects the pending touch without applying it.
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before);
	});
}

#[test]
fn preview_redeem_walks_multiple_vaults() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		let v1_before = vault_debt(DOT, PUSD, 1);
		let v2_before = vault_debt(DOT, PUSD, 2);

		let preview = Redemptions::preview_redeem(DOT, PUSD, 100_000, 0).expect("preview");
		assert_eq!(preview.steps, 2);
		assert!(!preview.truncated);
		assert_eq!(preview.debt_cancelled(), v1_before + v2_before);
		// Projecting multiple targets leaves every vault untouched.
		assert_eq!(vault_debt(DOT, PUSD, 1), v1_before);
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
	});
}

#[test]
fn preview_redeem_none_when_no_target() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert!(Redemptions::preview_redeem(DOT, PUSD, 200, 0).is_none());
	});
}

#[test]
fn ordinary_redemption_end_to_end() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		set_price(DOT, FixedU128::from_rational(2, 1));
		assert_ok!(open(1, DOT, PUSD, 4_000, 5_000, rate_pct(5, 100)));
		set_dynamic_fee(PUSD, FixedU128::from_rational(15, 1_000));

		mint_stable(PUSD, 3, 1_000_000);
		let recipient_before = collateral_balance(DOT, 4);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let redeemer_before = Assets::balance(PUSD, 3);
		let debt_before = vault_debt(DOT, PUSD, 1);
		let held_before = held(DOT, 1);
		let issuance_before = Assets::total_supply(PUSD);

		assert_ok!(redeem(3, DOT, PUSD, 1_020, 0, 4, 0));

		// The vault owes 5_005, so cancelling 1_020 is 20.38% of the coin and
		// lifts the 1.5% accelerator by half of that to 11.69%. This redemption
		// pays the rate it caused: 11.69% + 0.5% = 12.19%, so the fee is
		// ceil(1_020 * 0.121898) = 125 on top of the 1_020 of debt, and the
		// collateral is 1_020/2 = 510.
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 510);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, 125);
		assert_eq!(redeemer_before - Assets::balance(PUSD, 3), 1_145);
		assert_eq!(debt_before - vault_debt(DOT, PUSD, 1), 1_020);
		assert_eq!(held_before - held(DOT, 1), 510);
		// Fees are transferred, so issuance must only fall by cancelled debt.
		assert_eq!(issuance_before - Assets::total_supply(PUSD), 1_020);
	});
}

#[test]
fn multiple_final_recovery_vaults_settle_fifo_head_with_per_head_insurance_fund() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Recovery price 0.10 → both vaults sit below 100% CR (insurance-adjusted).
		let rp = FixedU128::from_rational(1u128, 10u128);
		setup_final_recovery(1, 1_000, 500, rp);
		setup_final_recovery(2, 1_000, 500, rp);

		// Both vaults are in the FinalRecovery FIFO, oldest first; only the head
		// is exposed to redemption.
		assert_eq!(Vaults::final_recovery_queue(DOT, PUSD, 10), vec![1u64, 2u64]);

		let debt1 = vault_debt(DOT, PUSD, 1);
		let debt2 = vault_debt(DOT, PUSD, 2);
		// 500 principal + the 1-unit 7-day upfront fee each.
		assert_eq!(debt1, 501);
		assert_eq!(debt2, 501);
		// Fund the Insurance Fund to exactly cover vault 1's shortfall, so the head
		// drains it and the next head must settle against a then-empty fund.
		let collateral_value_1 = rp.saturating_mul_int(held(DOT, 1));
		let shortfall_1 = debt1 - collateral_value_1;
		mint_stable(PUSD, insurance_account(PUSD), shortfall_1);
		mint_stable(PUSD, 3, 10_000);

		let issuance_before = Assets::total_supply(PUSD);
		let redeemer_before_1 = Assets::balance(PUSD, 3);
		let recipient_before_1 = collateral_balance(DOT, 4);

		// First transaction settles only the FIFO head (vault 1).
		assert_ok!(redeem(3, DOT, PUSD, 10_000, 0, 4, 0));
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_none(), "head settled");
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		assert_eq!(Assets::balance(PUSD, insurance_account(PUSD)), 0, "head 1 drained the fund");
		// Head-only: vault 2 is untouched and becomes the new FIFO head.
		assert_eq!(vault_debt(DOT, PUSD, 2), debt2);
		assert!(Vaults::vault_status(DOT, PUSD, 2).expect("vault 2").is_final_recovery());
		assert_eq!(Vaults::final_recovery_queue(DOT, PUSD, 10), vec![2u64]);
		// The fund covered vault 1 exactly, so the redeemer paid only the
		// collateral-backed (recovery_rate == 1.0) portion.
		let pusd_in_1 = redeemer_before_1 - Assets::balance(PUSD, 3);
		assert_eq!(pusd_in_1, collateral_value_1);
		// recovery_rate == 1.0, so vault 1's entire 1_000 collateral is paid to the recipient.
		assert_eq!(collateral_balance(DOT, 4) - recipient_before_1, 1_000);

		let redeemer_before_2 = Assets::balance(PUSD, 3);
		let recipient_before_2 = collateral_balance(DOT, 4);

		// Second transaction settles vault 2 against the now-empty fund: the
		// recovery rate falls to C/D, so the redeemer covers the entire debt and
		// pUSD holders absorb the shortfall.
		assert_ok!(redeem(3, DOT, PUSD, 10_000, 0, 4, 0));
		// With the fund empty there is no insurance residual to burn, so the debt
		// is fully cancelled by the redeemer and the vault drops to zero debt (and
		// leaves the FIFO) rather than being settled-and-removed like vault 1.
		assert_eq!(vault_debt(DOT, PUSD, 2), 0, "second head fully settled");
		assert!(Vaults::final_recovery_queue(DOT, PUSD, 10).is_empty(), "FIFO drained");
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		assert_eq!(Assets::balance(PUSD, insurance_account(PUSD)), 0, "no fund left to burn");
		let pusd_in_2 = redeemer_before_2 - Assets::balance(PUSD, 3);
		assert_eq!(pusd_in_2, debt2);
		assert!(pusd_in_2 > pusd_in_1, "drained fund pushes the loss onto the redeemer");
		// Empty fund: D=501, C=100, so C/D with double flooring pays 990.
		assert_eq!(collateral_balance(DOT, 4) - recipient_before_2, 990);

		// Conservation: issuance falls by both vaults' full debt and no more
		// (redeemer burns + the atomic Insurance-Fund burn for vault 1).
		assert_eq!(issuance_before - Assets::total_supply(PUSD), debt1 + debt2);
	});
}

#[test]
fn insurance_adjusted_recovery_burns_fund_only_when_market_debt_exhausted() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Recovery price 0.40 → CR < 100% with a partial Insurance Fund cover.
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128));
		mint_stable(PUSD, insurance_account(PUSD), 50);
		mint_stable(PUSD, 3, 10_000);

		let debt_before = vault_debt(DOT, PUSD, 1);
		let if_before = Assets::balance(PUSD, insurance_account(PUSD));
		let recipient_before = collateral_balance(DOT, 4);
		let issuance_before = Assets::total_supply(PUSD);

		// First transaction cancels only part of the market-cancellable debt: the
		// fund must stay untouched until the market portion is fully exhausted.
		assert_ok!(redeem(3, DOT, PUSD, 200, 0, 4, 0));
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_some(), "still settling");
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault 1").is_final_recovery());
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		assert_eq!(
			Assets::balance(PUSD, insurance_account(PUSD)),
			if_before,
			"fund untouched mid-settlement"
		);
		// Recovery is fee-free and budget-bound: this step cancels exactly 200 debt
		// (200 < market_cancel_debt) and pays collateral at recovery_rate = C/(D-50).
		assert_eq!(debt_before - vault_debt(DOT, PUSD, 1), 200);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 442);

		// Second transaction finishes the market portion; only now does the atomic
		// Insurance-Fund burn fire, covering the residual and removing the vault.
		assert_ok!(redeem(3, DOT, PUSD, 10_000, 0, 4, 0));
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_none(), "vault settled");
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		assert_eq!(
			Assets::balance(PUSD, insurance_account(PUSD)),
			0,
			"residual burned on completion"
		);
		// Conservation: the full vault debt leaves issuance across the two txs.
		assert_eq!(issuance_before - Assets::total_supply(PUSD), debt_before);
	});
}

#[test]
fn recovery_redemption_leaves_ordinary_dynamic_fee_untouched() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// CR >= 100% → a clean recovery-bonus settlement.
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		let seeded = FixedU128::from_rational(3u128, 100u128);
		set_dynamic_fee(PUSD, seeded);
		let state_before = crate::RedemptionStates::<Test>::get(PUSD);
		mint_stable(PUSD, 3, 1_000_000);

		assert_ok!(redeem(3, DOT, PUSD, 200, 0, 4, 0));

		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
		assert!(!ordinary_event_emitted(), "recovery must not emit an ordinary redemption");
		// Recovery activity must not feed the ordinary-redemption fee accelerator.
		assert_eq!(
			crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee,
			state_before.dynamic_fee
		);
		assert_eq!(
			crate::RedemptionStates::<Test>::get(PUSD).last_fee_operation,
			state_before.last_fee_operation
		);
	});
}

#[test]
fn exact_balance_funds_the_full_fee_free_recovery_quote() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		let quote = Redemptions::preview_redeem(DOT, PUSD, 200, 0).expect("recovery quote");
		assert_eq!(quote.stable_in, 200);
		assert_eq!(quote.fee, 0);
		mint_stable(PUSD, 3, quote.stable_in);
		let debt_before = vault_debt(DOT, PUSD, 1);
		let fee_before = Assets::balance(PUSD, FEE_DEST);

		assert_ok!(redeem(3, DOT, PUSD, 200, 0, 4, 0));

		assert_eq!(debt_before - vault_debt(DOT, PUSD, 1), quote.stable_in);
		assert_eq!(Assets::balance(PUSD, 3), 0);
		assert_eq!(Assets::balance(PUSD, FEE_DEST), fee_before);
	});
}

#[test]
fn ordinary_redemption_succeeds_in_safety_mode() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// Drop the price so branch TCR falls below the 130% safety threshold while
		// the vault stays above 100% and remains redeemable — i.e. the branch is
		// in Safety mode (mode is derived from live TCR).
		set_price(DOT, FixedU128::from_rational(6u128, 10u128));
		assert!(
			branch_tcr(DOT, PUSD) < FixedU128::from_rational(130u128, 100u128),
			"fixture must put the branch below the safety threshold"
		);

		let debt_before = vault_debt(DOT, PUSD, 1);
		mint_stable(PUSD, 3, 1_000_000);
		let redeemer_before = Assets::balance(PUSD, 3);
		let recipient_before = collateral_balance(DOT, 4);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let tcr_before = branch_tcr(DOT, PUSD);

		// Cancelling 201 of the coin's 501 debt is 40.12%, lifting the accelerator
		// to 20.06%; the redemption pays that plus the 0.5% base, so
		// ceil(201 * 0.205598) = 42 on top. At price 0.6 the 201 buys
		// floor(201/0.6) = 335 collateral.
		assert_ok!(redeem(3, DOT, PUSD, 201, 0, 4, 0));
		assert_eq!(debt_before - vault_debt(DOT, PUSD, 1), 201);
		assert_eq!(redeemer_before - Assets::balance(PUSD, 3), 243);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 335);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, 42);
		// Ordinary redemptions always raise a >100% branch's TCR; that invariant
		// is exactly why they remain permitted in Safety mode.
		assert!(branch_tcr(DOT, PUSD) > tcr_before, "redemption must raise TCR");
	});
}

#[test]
fn redeem_with_oracle_down_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000);
		// The preamble reads the oracle itself, so a failing feed is refused
		// before any vault is touched.
		MockOracleAvailable::set(false);
		assert_noop!(redeem(3, DOT, PUSD, 201, 0, 4, 0), Error::<Test>::OracleUnavailable);
	});
}

#[test]
fn redeem_zero_price_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000);
		// Unlike the oracle-down test above (the feed errors), here the feed
		// answers successfully but with a degenerate zero price: the preamble's
		// explicit zero-price guard must refuse it the same way.
		set_price(DOT, FixedU128::zero());
		assert_noop!(redeem(3, DOT, PUSD, 201, 0, 4, 0), Error::<Test>::OracleUnavailable);
	});
}

/// The interest-clock value stamped on a `(DOT, PUSD)` vault at its last poke.
fn vault_interest_time(who: AccountId) -> Moment {
	pallet_vaults::Vaults::<Test>::get((DOT, PUSD, who))
		.expect("vault")
		.last_interest_time
}

/// The interest-clock value a `(DOT, PUSD)` poke at `now` writes onto a touched vault.
fn branch_interest_time(now: Moment) -> Moment {
	pallet_vaults::Branches::<Test>::get(DOT, PUSD)
		.expect("branch")
		.state
		.interest_time(now)
}

#[test]
fn ordinary_redemption_accrues_target_interest_before_cancelling() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// A full year at 100% APR makes accrued interest a visible, large integer.
		assert_ok!(open(1, DOT, PUSD, 1_000_000, 500_000, rate_pct(100, 100)));
		let debt_at_open = vault_debt(DOT, PUSD, 1);
		let stamped_at_open = vault_interest_time(1);
		mint_stable(PUSD, 3, 2_000_000);

		advance_time(ONE_YEAR_MS);

		// The projection reports the fully accrued debt the live redemption will
		// operate on without touching the vault.
		let accrued = projected_full_debt(1);
		assert!(accrued > debt_at_open, "a year of interest must grow the debt");

		let redeemer_before = Assets::balance(PUSD, 3);
		let recipient_before = collateral_balance(DOT, 4);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		assert_ok!(redeem(3, DOT, PUSD, 1_005, 0, 4, 0));

		// Redeeming poked the target: its interest clock advanced to now, and the
		// 1_000 debt cancellation landed on the accrued balance, not the stale
		// opening principal.
		assert_eq!(vault_interest_time(1), branch_interest_time(Timestamp::get()));
		assert!(vault_interest_time(1) > stamped_at_open);
		assert_eq!(vault_debt(DOT, PUSD, 1), accrued - 1_005);
		// Every dimension of the fill: 1_005 of debt cancelled, floor(1_005/1.25)
		// = 804 collateral out, and the fee on top. The denominator includes the
		// year of aggregate interest without touching the market, so the
		// increase is roughly 1_005/1_009_590/2 = 0.0498% and the charge is 6.
		assert_eq!(redeemer_before - Assets::balance(PUSD, 3), 1_011);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 804);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, 6);
	});
}

#[test]
fn ordinary_redemption_strictly_improves_branch_tcr() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// A face-value redemption removes pUSD debt and at most an equal
		// pUSD-value of collateral, so a TCR above 1 can only rise. This is why
		// the SPEC permits ordinary redemptions in Safety mode.
		assert_ok!(open(1, DOT, PUSD, 10_000, 5_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000_000);
		let tcr_before = branch_tcr(DOT, PUSD);
		assert!(tcr_before > FixedU128::one());

		assert_ok!(redeem(3, DOT, PUSD, 201, 0, 4, 0));

		assert!(branch_tcr(DOT, PUSD) > tcr_before, "redemption must improve branch TCR");
	});
}

#[test]
fn preview_matches_execution_for_partial_fill() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 10_000, 5_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000_000);

		let budget = 1_005;
		let preview = Redemptions::preview_redeem(DOT, PUSD, budget, 0).expect("preview");
		assert_eq!(preview.steps, 1);

		let redeemer_before = Assets::balance(PUSD, 3);
		let recipient_before = collateral_balance(DOT, 4);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let v1_before = vault_debt(DOT, PUSD, 1);

		assert_ok!(redeem(3, DOT, PUSD, budget, 0, 4, 0));

		// Execution reproduces the quote exactly across every dimension.
		assert_eq!(redeemer_before - Assets::balance(PUSD, 3), preview.stable_in);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, preview.collateral_out);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, preview.fee);
		assert_eq!(v1_before - vault_debt(DOT, PUSD, 1), preview.debt_cancelled());
	});
}

/// Fully redistribute `owner`'s vault: no offset, all debt and collateral go
/// to the surviving vaults.
fn liquidate_redistribute_all(owner: AccountId) {
	Vaults::execute_liquidation(&DOT, &PUSD, &owner, |_, mut collateral| {
		let owner_surplus = collateral.extract(0);
		Ok(LiquidationSettlement {
			debt_offset: 0,
			redistribution_collateral: collateral,
			owner_surplus,
		})
	})
	.expect("liquidation succeeds");
}

#[test]
fn quote_matches_execution_across_redistribution_rounds() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Distinct stakes and rates so per-stake redistribution shares floor
		// non-trivially per recipient.
		assert_ok!(open(1, DOT, PUSD, 1_100, 300, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_700, 450, rate_pct(2, 100)));
		assert_ok!(open(3, DOT, PUSD, 2_300, 600, rate_pct(3, 100)));
		mint_stable(PUSD, 5, 1_000_000);

		// Two full-redistribution rounds with the recipients untouched
		// throughout: every quoted snapshot projects its share across both
		// pending rounds against the original branch aggregate, while
		// execution decrements that aggregate touch by touch.
		for sacrificial in [9, 10] {
			assert_ok!(open(sacrificial, DOT, PUSD, 480, 480, rate_pct(4, 100)));
			set_price(DOT, FixedU128::one());
			liquidate_redistribute_all(sacrificial);
			set_price(DOT, FixedU128::from_rational(5u128, 4u128));
			advance_time(30 * 24 * 3_600 * 1_000);
		}

		let quote = Redemptions::preview_redeem(DOT, PUSD, 100_000, 0).expect("quote");
		assert_eq!(quote.steps, 3);
		assert!(!quote.truncated);

		let redeemer_before = Assets::balance(PUSD, 5);
		let recipient_before = collateral_balance(DOT, 6);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let issuance_before = Assets::total_supply(PUSD);
		assert_ok!(redeem(5, DOT, PUSD, 100_000, 0, 6, 0));

		// Execution reproduces the quote exactly across every dimension,
		// projected redistribution shares included. Per-stake flooring keeps
		// each share within the remaining branch aggregate here; the
		// documented indicative-quote drift needs caps that bind, which
		// untouched recipients cannot produce.
		assert_eq!(redeemer_before - Assets::balance(PUSD, 5), quote.stable_in);
		assert_eq!(collateral_balance(DOT, 6) - recipient_before, quote.collateral_out);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, quote.fee);
		assert_eq!(issuance_before - Assets::total_supply(PUSD), quote.debt_cancelled());
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
			register_branch(DOT, PUSD, default_branch_config());
			// Zero every fee knob so the mechanic is isolated from fee dynamics.
			let mut cfg = DefaultRedemptionConfig::get();
			cfg.dynamic_fee_ceiling = FixedU128::zero();
			cfg.base_fee = Permill::zero();
			cfg.fee_ceiling = Permill::zero();
			assert_ok!(Redemptions::set_redemption_config(RuntimeOrigin::root(), PUSD, cfg));
			assert_ok!(open(1, DOT, PUSD, 100_000, 10_000, rate_pct(5, 100)));
			mint_stable(PUSD, 3, 1_000_000);
			let debt_before = vault_debt(DOT, PUSD, 1);
			let coll_before = collateral_balance(DOT, 4);
			let issuance_before = Assets::total_supply(PUSD);
			for &amount in chunks {
				assert_ok!(redeem(3, DOT, PUSD, amount, 0, 4, 0));
			}
			result = (
				collateral_balance(DOT, 4) - coll_before,
				debt_before - vault_debt(DOT, PUSD, 1),
				issuance_before - Assets::total_supply(PUSD),
			);
		});
		result
	};

	assert_eq!(run(&[100, 100, 100]), run(&[300]));
}

#[test]
fn ordinary_redemption_at_six_decimals_scales_exactly() {
	build_and_execute(|| {
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(open(1, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		let debt_before = vault_debt(DOT, USDX, 1);
		mint_stable(USDX, 3, 1_000 * USDX_UNIT);
		let redeemer_before = Assets::balance(USDX, 3);
		let recipient_before = collateral_balance(DOT, 4);
		let held_before = held(DOT, 1);
		let issuance_before = Assets::total_supply(USDX);
		assert_eq!(Assets::balance(USDX, FEE_DEST), 0);

		assert_ok!(redeem(3, DOT, USDX, 201 * USDX_UNIT, 0, 4, 0));

		// The whole request is cancelled debt, exactly 201 USDX at six decimals,
		// and collateral is 201/1.25 = 160.8 USDX worth — both exact, no dust.
		assert_eq!(debt_before - vault_debt(DOT, USDX, 1), 201 * USDX_UNIT);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 160_800_000);
		assert_eq!(held_before - held(DOT, 1), 160_800_000);
		assert_eq!(issuance_before - Assets::total_supply(USDX), 201 * USDX_UNIT);
		// The fee is the one figure that cannot be round: the coin carries
		// 500_479_124 (500 USDX plus a 479_124 upfront fee), so cancelling
		// 201 USDX is 40.16% of it, the accelerator rises to 20.08%, and the
		// charge is ceil(201_000_000 * 0.205808) = 41_367_323 on top.
		assert_eq!(Assets::balance(USDX, FEE_DEST), 41_367_323);
		assert_eq!(redeemer_before - Assets::balance(USDX, 3), 201 * USDX_UNIT + 41_367_323);
	});
}

/// Quote a `(DOT, PUSD)` recovery offset through the pallet's view surface.
fn preview_offset(
	max_debt_to_cancel: Balance,
) -> Result<RecoveryOffsetQuote<Balance>, DispatchError> {
	Redemptions::preview_recovery_offset(&DOT, &PUSD, max_debt_to_cancel)
}

/// Execute a `(DOT, PUSD)` recovery offset: `payer` funds the payment credit
/// (capped at their whole balance, the way pool callers size their
/// withdrawals) and receives the change back; `recipient` is paid the
/// collateral. Transactional like the real callers, so an `Err` also rolls
/// the funding withdrawal back.
fn execute_offset(
	payer: AccountId,
	recipient: AccountId,
	max_debt_to_cancel: Balance,
) -> Result<RecoveryOffsetResult<Balance>, DispatchError> {
	frame::deps::frame_support::storage::with_storage_layer(|| {
		execute_offset_inner(payer, recipient, max_debt_to_cancel)
	})
}

fn execute_offset_inner(
	payer: AccountId,
	recipient: AccountId,
	max_debt_to_cancel: Balance,
) -> Result<RecoveryOffsetResult<Balance>, DispatchError> {
	use frame::traits::{
		fungibles::Balanced as FungiblesBalanced,
		tokens::{Fortitude, Precision},
	};
	let (amount, preservation) = reducible_debit::<Assets, _>(PUSD, &payer, max_debt_to_cancel);
	let payment = if amount.is_zero() {
		crate::StableCreditOf::<Test>::zero(PUSD)
	} else {
		<Assets as FungiblesBalanced<AccountId>>::withdraw(
			PUSD,
			&payer,
			amount,
			Precision::Exact,
			preservation,
			Fortitude::Polite,
		)?
	};
	let (result, change) = <Redemptions as RecoveryOffsetInterface>::execute_recovery_offset(
		&DOT, &PUSD, payment, &recipient,
	)?;
	if let Err(change) = change.drop_zero() {
		<Assets as FungiblesBalanced<AccountId>>::resolve(&payer, change)
			.map_err(|_| DispatchError::Other("change does not fit the payer account"))?;
	}
	Ok(result)
}

#[test]
fn recovery_offset_settles_fifo_head_and_matches_preview() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		setup_final_recovery(2, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		// Back at the healthy price both queued vaults sit at CR >= 100%, the
		// recovery-bonus regime offsets are restricted to.
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		assert_eq!(Vaults::final_recovery_queue(DOT, PUSD, 10), vec![1u64, 2u64]);
		let debt1 = vault_debt(DOT, PUSD, 1);
		// 500 principal + the 1-unit 7-day upfront fee.
		assert_eq!(debt1, 501);
		let debt2 = vault_debt(DOT, PUSD, 2);
		mint_stable(PUSD, 3, 10_000);
		let payer_before = Assets::balance(PUSD, 3);
		let recipient_before = collateral_balance(DOT, 4);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let issuance_before = Assets::total_supply(PUSD);

		// The quote sizes exactly the burn the execution then performs.
		assert_eq!(preview_offset(10_000), Ok(RecoveryOffsetQuote::Available { debt: debt1 }));
		// CR = 1_250/501 ≈ 249% caps the bonus at the 5% redistribution
		// penalty: collateral = floor(floor(501 · 1.05) / 1.25) = 420.
		assert_eq!(
			execute_offset(3, 4, 10_000),
			Ok(RecoveryOffsetResult::Applied { collateral_out: 420 })
		);

		// The drained head flips to Dormant and leaves the FIFO; the next
		// head is untouched.
		assert_eq!(vault_debt(DOT, PUSD, 1), 0);
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault 1").is_dormant());
		assert_eq!(Vaults::final_recovery_queue(DOT, PUSD, 10), vec![2u64]);
		assert_eq!(vault_debt(DOT, PUSD, 2), debt2);
		// The burn is fee-free, so issuance falls by exactly the cancelled
		// debt and nothing reaches the fee destination.
		assert_eq!(payer_before - Assets::balance(PUSD, 3), debt1);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 420);
		assert_eq!(Assets::balance(PUSD, FEE_DEST), fee_before);
		assert_eq!(issuance_before - Assets::total_supply(PUSD), debt1);
	});
}

#[test]
fn recovery_offset_partial_fill_keeps_head_queued() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		let debt_before = vault_debt(DOT, PUSD, 1);
		mint_stable(PUSD, 3, 10_000);
		let payer_before = Assets::balance(PUSD, 3);
		let recipient_before = collateral_balance(DOT, 4);

		// A 200 cap partially fills the 501-debt head; quote and execution
		// agree on the capped size.
		assert_eq!(preview_offset(200), Ok(RecoveryOffsetQuote::Available { debt: 200 }));
		assert_eq!(
			execute_offset(3, 4, 200),
			Ok(RecoveryOffsetResult::Applied {
				// 5%-capped bonus: floor(floor(200 · 1.05) / 1.25) = 168.
				collateral_out: 168,
			})
		);

		// The partially settled head keeps its place at the FIFO front.
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before - 200);
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault 1").is_final_recovery());
		assert_eq!(Vaults::final_recovery_queue(DOT, PUSD, 10), vec![1u64]);
		assert_eq!(payer_before - Assets::balance(PUSD, 3), 200);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 168);
	});
}

/// Regression pin for quote/execution parity: a zero `max_debt_to_cancel`
/// against a recovery-bonus head used to quote `Available { debt: 0 }` while
/// execution reported `NoTarget`.
#[test]
fn recovery_offset_zero_budget_is_no_target_in_both_paths() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		let debt_before = vault_debt(DOT, PUSD, 1);
		mint_stable(PUSD, 3, 1_000);
		let payer_before = Assets::balance(PUSD, 3);

		assert_eq!(preview_offset(0), Ok(RecoveryOffsetQuote::NoTarget));
		assert_eq!(execute_offset(3, 4, 0), Ok(RecoveryOffsetResult::NoTarget));

		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before);
		assert_eq!(Assets::balance(PUSD, 3), payer_before);
	});
}

#[test]
fn recovery_offset_below_par_head_is_refused_in_both_paths() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Price 0.40 parks the head below par (CR < 100%): settlement at a
		// discount stays exclusive to the redemption pathway.
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128));
		let debt_before = vault_debt(DOT, PUSD, 1);
		let held_before = held(DOT, 1);
		mint_stable(PUSD, 3, 10_000);
		let payer_before = Assets::balance(PUSD, 3);

		assert_eq!(preview_offset(10_000), Ok(RecoveryOffsetQuote::BelowPar));
		assert_eq!(execute_offset(3, 4, 10_000), Ok(RecoveryOffsetResult::BelowPar));

		// Refusal leaves the head fully intact.
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before);
		assert_eq!(held(DOT, 1), held_before);
		assert_eq!(Assets::balance(PUSD, 3), payer_before);
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault 1").is_final_recovery());
	});
}

#[test]
fn recovery_offset_without_recovery_head_is_no_target_in_both_paths() {
	build_and_execute(|| {
		// Unregistered market: the target-first fast path answers before the
		// config lookup could report `InvalidBranch`.
		assert_eq!(preview_offset(1_000), Ok(RecoveryOffsetQuote::NoTarget));
		assert_eq!(execute_offset(3, 4, 1_000), Ok(RecoveryOffsetResult::NoTarget));

		// An ordinary vault is not an offset target either: offsets exist
		// only for the FinalRecovery FIFO head.
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		let debt_before = vault_debt(DOT, PUSD, 1);
		mint_stable(PUSD, 3, 1_000);

		assert_eq!(preview_offset(1_000), Ok(RecoveryOffsetQuote::NoTarget));
		assert_eq!(execute_offset(3, 4, 1_000), Ok(RecoveryOffsetResult::NoTarget));
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before);
	});
}

#[test]
fn recovery_offset_underfunded_payment_partially_fills() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		let debt_before = vault_debt(DOT, PUSD, 1);
		// The payer holds less than the 501 an uncapped offset could cancel.
		// The payment credit is the budget, so the offset fills exactly what
		// it carries instead of failing for the missing remainder.
		mint_stable(PUSD, 3, 100);

		assert_eq!(
			execute_offset(3, 4, 10_000),
			Ok(RecoveryOffsetResult::Applied {
				// 5%-capped bonus: floor(floor(100 · 1.05) / 1.25) = 84.
				collateral_out: 84,
			})
		);

		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before - 100);
		assert_eq!(Assets::balance(PUSD, 3), 0);
		assert_eq!(Vaults::final_recovery_queue(DOT, PUSD, 10), vec![1u64]);
	});
}

#[test]
fn recovery_offset_frozen_branch_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		mint_stable(PUSD, 3, 1_000);
		assert_ok!(Vaults::set_governance_frozen(RuntimeOrigin::signed(ADMIN), DOT, PUSD, true));

		// Frozen-mode enforcement lives vault-side: the head is still queued,
		// so both paths reach `redeem_step` and are rejected there.
		assert_noop!(preview_offset(200), pallet_vaults::Error::<Test>::BranchFrozen);
		assert_noop!(execute_offset(3, 4, 200), pallet_vaults::Error::<Test>::BranchFrozen);
	});
}

#[test]
fn recovery_offset_wrong_coin_payment_is_refused() {
	build_and_execute(|| {
		use frame::traits::{
			fungibles::Balanced as FungiblesBalanced,
			tokens::{Fortitude, Precision, Preservation},
		};

		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));

		// Queue a settleable head in the sibling USDX market too.
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(open(2, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(52u128, 100u128));
		assert_ok!(enter_final_recovery(DOT, USDX, 2));
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));

		let pusd_debt_before = vault_debt(DOT, PUSD, 1);
		let usdx_debt_before = vault_debt(DOT, USDX, 2);
		mint_stable(USDX, 3, 100 * USDX_UNIT);
		let payer_before = Assets::balance(USDX, 3);
		let issuance_before = Assets::total_supply(USDX);

		// Use a real withdrawn payment and the same outer transaction contract
		// as production callers. The mismatch error must roll the withdrawal
		// back, not merely avoid touching either recovery head.
		let result: Result<(), DispatchError> =
			frame::deps::frame_support::storage::with_storage_layer(|| {
				let payment = <Assets as FungiblesBalanced<AccountId>>::withdraw(
					USDX,
					&3,
					100 * USDX_UNIT,
					Precision::Exact,
					Preservation::Expendable,
					Fortitude::Polite,
				)?;
				let (_result, change) =
					<Redemptions as RecoveryOffsetInterface>::execute_recovery_offset(
						&DOT, &PUSD, payment, &4,
					)?;
				drop(change);
				Ok(())
			});
		assert_eq!(result, Err(crate::Error::<Test>::RecoveryOffsetCoinMismatch.into()));
		assert_eq!(Assets::balance(USDX, 3), payer_before);
		assert_eq!(Assets::total_supply(USDX), issuance_before);
		assert_eq!(vault_debt(DOT, PUSD, 1), pusd_debt_before);
		assert_eq!(vault_debt(DOT, USDX, 2), usdx_debt_before);
	});
}

/// Near-twin of `recovery_redemption_leaves_ordinary_dynamic_fee_untouched`;
/// the distinct path is the offset surface, which must also stay silent at
/// the redemptions layer (no fee movement, no events).
#[test]
fn recovery_offset_leaves_dynamic_fee_untouched() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		set_dynamic_fee(PUSD, FixedU128::from_rational(3u128, 100u128));
		let state_before = crate::RedemptionStates::<Test>::get(PUSD);
		mint_stable(PUSD, 3, 1_000);
		System::reset_events();

		assert_eq!(
			execute_offset(3, 4, 200),
			Ok(RecoveryOffsetResult::Applied { collateral_out: 168 })
		);

		// Offsets are fee-free settlement: they neither move the dynamic fee
		// nor emit redemption events.
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD), state_before);
		let redemption_events = System::events()
			.into_iter()
			.filter(|r| matches!(r.event, RuntimeEvent::Redemptions(_)))
			.count();
		assert_eq!(redemption_events, 0);
	});
}

#[test]
fn insurance_adjusted_flooring_at_six_decimals_costs_raw_units_not_coins() {
	build_and_execute(|| {
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(open(1, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(40u128, 100u128));
		assert_ok!(enter_final_recovery(DOT, USDX, 1));
		let debt = vault_debt(DOT, USDX, 1);
		// 500 coins principal + the upfront fee ceil(500e6 · 5% · 7d/1yr) =
		// ceil(479_123.88) = 479_124 raw units (≈ 0.48 coins).
		assert_eq!(debt, 500 * USDX_UNIT + 479_124);
		mint_stable(USDX, insurance_account(USDX), 50 * USDX_UNIT);
		let market_cancel = debt - 50 * USDX_UNIT;
		mint_stable(USDX, 3, 1_000 * USDX_UNIT);
		let if_before = Assets::balance(USDX, insurance_account(USDX));
		let recipient_before = collateral_balance(DOT, 4);
		let issuance_before = Assets::total_supply(USDX);

		assert_ok!(redeem(3, DOT, USDX, market_cancel, 0, 4, 0));

		assert!(pallet_vaults::Vaults::<Test>::get((DOT, USDX, 1)).is_none(), "vault settled");
		// 400e6/450_479_124 yields value floor(market_cancel · rate) =
		// 400·USDX_UNIT − 1, then collateral floor((400·USDX_UNIT − 1)/0.40) =
		// 999_999_997 of the 1_000·USDX_UNIT held — a 3-raw-unit rounding loss.
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 999_999_997);
		assert_eq!(if_before - Assets::balance(USDX, insurance_account(USDX)), 50 * USDX_UNIT);
		assert_eq!(issuance_before - Assets::total_supply(USDX), debt);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
	});
}

/// The default config with every fee component zeroed, so a step's payment
/// need equals its budget exactly and dead-zone arithmetic stays readable.
fn zero_fee_redemption_config() -> RedemptionConfig<Balance> {
	RedemptionConfig {
		dynamic_fee_floor: FixedU128::zero(),
		dynamic_fee_ceiling: FixedU128::zero(),
		base_fee: Permill::zero(),
		fee_ceiling: Permill::zero(),
		..DefaultRedemptionConfig::get()
	}
}

#[test]
fn dead_zone_redemption_reprices_to_the_preserving_limit() {
	build_and_execute(|| {
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(Redemptions::set_redemption_config(
			RuntimeOrigin::root(),
			USDX,
			zero_fee_redemption_config(),
		));
		assert_ok!(open(1, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		let debt_before = vault_debt(DOT, USDX, 1);
		mint_stable(USDX, 3, 300 * USDX_UNIT);
		let recipient_before = collateral_balance(DOT, 4);
		let issuance_before = Assets::total_supply(USDX);

		// Spending all but 4_000 raw units would strand the wallet below the
		// 10_000-unit minimum: the step reprices once at the preserving limit
		// and commits the partial fill instead of folding the 4_000 in.
		let request = 300 * USDX_UNIT - 4_000;
		let burned = 300 * USDX_UNIT - USDX_MIN_BALANCE;
		assert_ok!(redeem(3, DOT, USDX, request, 0, 4, 0));

		assert_eq!(Assets::balance(USDX, 3), USDX_MIN_BALANCE);
		assert_eq!(debt_before - vault_debt(DOT, USDX, 1), burned);
		assert_eq!(issuance_before - Assets::total_supply(USDX), burned);
		assert_eq!(Assets::balance(USDX, FEE_DEST), 0);
		// Price 1.25: collateral out = burned / 1.25 exactly.
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, burned * 4 / 5);
	});
}

#[test]
fn full_wallet_redemption_expends_the_redeemer_account() {
	build_and_execute(|| {
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(Redemptions::set_redemption_config(
			RuntimeOrigin::root(),
			USDX,
			zero_fee_redemption_config(),
		));
		assert_ok!(open(1, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		let debt_before = vault_debt(DOT, USDX, 1);
		mint_stable(USDX, 3, 300 * USDX_UNIT);
		let issuance_before = Assets::total_supply(USDX);

		// Spending the whole wallet is the legitimate full expend: the
		// account reaps with no dust folded into the burn.
		assert_ok!(redeem(3, DOT, USDX, 300 * USDX_UNIT, 0, 4, 0));

		assert_eq!(Assets::balance(USDX, 3), 0);
		assert_eq!(debt_before - vault_debt(DOT, USDX, 1), 300 * USDX_UNIT);
		assert_eq!(issuance_before - Assets::total_supply(USDX), 300 * USDX_UNIT);
	});
}

// ---------------------------------------------------------------------------
// Numeric examples from `numeric-examples.md`, at the parameters its shared
// assumptions fix: 1 DOT = 2 pUSD, redemption_fee_floor 0.5%.
// ---------------------------------------------------------------------------

/// Numeric example 1: the redemption raises the accelerator and then pays the
/// raised rate, priced against market-wide stablecoin debt.
#[test]
fn example_1_ordinary_redemption_with_base_rate_update_and_fee() {
	build_and_execute(|| {
		// No upfront fee, so each vault's drawn principal is its debt and the
		// coin's total is exactly the example's 100_000.
		let config =
			pallet_vaults::BranchConfig { upfront_fee_period: 0, ..default_branch_config() };
		register_branch(DOT, PUSD, config);
		set_price(DOT, FixedU128::from_rational(2, 1));

		// The example's vault: 5_000 of debt against 4_000 DOT = 8_000 pUSD, a
		// 160% CR. It borrows at the floor rate so the walk reaches it first.
		assert_ok!(open(1, DOT, PUSD, 4_000, 5_000, rate_pct(1, 1_000)));
		// The rest of the coin's 100_000 of debt, on a costlier vault so it is
		// never the redemption target.
		assert_ok!(open(2, DOT, PUSD, 200_000, 95_000, rate_pct(5, 100)));
		assert_eq!(stablecoin_debt(PUSD), 100_000);

		// The example's decayed base rate.
		set_dynamic_fee(PUSD, FixedU128::from_rational(15, 1_000));

		mint_stable(PUSD, 3, 1_000_000);
		let redeemer_before = Assets::balance(PUSD, 3);
		let recipient_before = collateral_balance(DOT, 4);
		let fee_before = Assets::balance(PUSD, FEE_DEST);

		assert_ok!(redeem(3, DOT, PUSD, 1_000, 0, 4, 0));

		// redeemed_fraction = 1_000/100_000 = 1%, so the accelerator rises by
		// half of that to 2.0%, and this redemption pays 2.0% + 0.5% = 2.5%:
		// a 25 fee on top of the 1_000 cancelled, 1_025 spent in total, for
		// 1_000/2 = 500 DOT of collateral.
		assert_eq!(
			crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee,
			FixedU128::from_rational(2, 100)
		);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_before, 25);
		assert_eq!(redeemer_before - Assets::balance(PUSD, 3), 1_025);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 500);

		// The vault after redemption: 4_000 of debt against 3_500 DOT =
		// 7_000 pUSD, a 175% CR.
		assert_eq!(vault_debt(DOT, PUSD, 1), 4_000);
		assert_eq!(held(DOT, 1), 3_500);
	});
}

/// The examples are written in whole tokens but their arithmetic produces
/// fractions, which raw units express exactly. Every figure below is the
/// example's, scaled by this.
const UNIT: Balance = 1_000;

/// No upfront fee, so a vault's drawn principal is its debt.
fn example_config() -> pallet_vaults::BranchConfig<Balance> {
	pallet_vaults::BranchConfig { upfront_fee_period: 0, ..default_branch_config() }
}

/// Numeric example 2: a redemption that leaves the vault below `minimum_debt`
/// parks it as the Dormant continuation target rather than closing it.
#[test]
fn example_2_redemption_creates_a_dormant_continuation_vault() {
	build_and_execute(|| {
		let config = pallet_vaults::BranchConfig { minimum_debt: 2_000 * UNIT, ..example_config() };
		register_branch(DOT, PUSD, config);
		set_price(DOT, FixedU128::from_rational(2, 1));

		// 3_000 of debt against 2_000 DOT.
		assert_ok!(open(1, DOT, PUSD, 2_000 * UNIT, 3_000 * UNIT, rate_pct(1, 1_000)));
		mint_stable(PUSD, 3, 1_000_000 * UNIT);
		let recipient_before = collateral_balance(DOT, 4);

		assert_ok!(redeem(3, DOT, PUSD, 2_800 * UNIT, 0, 4, 0));

		// 2_800 cancelled pays 2_800/2 = 1_400 DOT, leaving 200 of debt against
		// 600 DOT — below the 2_000 minimum, so the vault goes Dormant and
		// becomes the branch's continuation target.
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 1_400 * UNIT);
		assert_eq!(vault_debt(DOT, PUSD, 1), 200 * UNIT);
		assert_eq!(held(DOT, 1), 600 * UNIT);
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault").is_dormant());
		assert_eq!(
			pallet_vaults::Branches::<Test>::get(DOT, PUSD)
				.unwrap()
				.state
				.dormant_redemption_target,
			Some(1)
		);
	});
}

/// Numeric example 3: a `FinalRecovery` head at CR >= 100% redeems at face
/// value plus a bonus capped by the redistribution penalty.
#[test]
fn example_3_final_recovery_redemption_above_par() {
	build_and_execute(|| {
		let config = pallet_vaults::BranchConfig {
			// The example's vault sits at CR 120%, so the MCR must exceed it.
			minimum_collateralization_ratio: FixedU128::from_rational(130u128, 100u128),
			initial_collateralization_ratio: FixedU128::from_rational(140u128, 100u128),
			safety_collateralization_ratio: FixedU128::from_rational(150u128, 100u128),
			redistribution_penalty: Permill::from_percent(10),
			..example_config()
		};
		register_branch(DOT, PUSD, config);

		// Open above the 140% ICR, then drop to the example's 1 DOT = 2 pUSD,
		// where 6_000 DOT = 12_000 pUSD against 10_000 of debt is CR 120%.
		set_price(DOT, FixedU128::from_rational(4, 1));
		assert_ok!(open(1, DOT, PUSD, 6_000 * UNIT, 10_000 * UNIT, rate_pct(1, 1_000)));
		set_price(DOT, FixedU128::from_rational(2, 1));
		assert_ok!(enter_final_recovery(DOT, PUSD, 1));

		mint_stable(PUSD, 3, 1_000_000 * UNIT);
		let recipient_before = collateral_balance(DOT, 4);
		let fee_before = Assets::balance(PUSD, FEE_DEST);

		assert_ok!(redeem(3, DOT, PUSD, 2_000 * UNIT, 0, 4, 0));

		// raw_bonus = 120% - 100% - 1% = 19%, capped by the 10% penalty. So
		// 2_000 * 1.10 = 2_200 pUSD of value, or 1_100 DOT.
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 1_100 * UNIT);
		assert_eq!(vault_debt(DOT, PUSD, 1), 8_000 * UNIT);
		assert_eq!(held(DOT, 1), 4_900 * UNIT);
		// Recovery redemptions are fee-free.
		assert_eq!(Assets::balance(PUSD, FEE_DEST), fee_before);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
	});
}

/// Numeric example 4: a `FinalRecovery` head below par, where partial Insurance
/// Fund cover raises the rate the market redeems at, and the fund burns the
/// residual once the market side is exhausted.
#[test]
fn example_4_final_recovery_redemption_below_par_with_insurance_cover() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, example_config());

		// Open healthy, then drop to the example's price: 4_000 DOT = 8_000 pUSD
		// against 10_000 of debt is CR 80%.
		set_price(DOT, FixedU128::from_rational(4, 1));
		assert_ok!(open(1, DOT, PUSD, 4_000 * UNIT, 10_000 * UNIT, rate_pct(1, 1_000)));
		set_price(DOT, FixedU128::from_rational(2, 1));
		assert_ok!(enter_final_recovery(DOT, PUSD, 1));
		mint_stable(PUSD, insurance_account(PUSD), 1_000 * UNIT);

		mint_stable(PUSD, 3, 1_000_000 * UNIT);
		let recipient_before = collateral_balance(DOT, 4);

		// shortfall = 10_000 - 8_000 = 2_000, cover = min(1_000, 2_000) = 1_000,
		// market_cancel_debt = 9_000, recovery_rate = 8_000/9_000 = 0.888…
		//
		// Burning 3_000 buys 3_000 * 8/9 = 2_666.66… pUSD of value, i.e.
		// 1_333.33… DOT. The doc prints 1_333.335, having rounded its own
		// intermediate 2_666.66… up to 2_666.67 before halving; the exact
		// quotient is 1_333.33…, which the pallet floors.
		assert_ok!(redeem(3, DOT, PUSD, 3_000 * UNIT, 0, 4, 0));
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 1_333_333);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));

		// Settling the rest of the market side pays out the whole 4_000 DOT and
		// leaves the fund to burn its 1_000 of cover against the residual.
		let if_before = Assets::balance(PUSD, insurance_account(PUSD));
		assert_ok!(redeem(3, DOT, PUSD, 6_000 * UNIT, 0, 4, 0));
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 4_000 * UNIT);
		assert_eq!(if_before - Assets::balance(PUSD, insurance_account(PUSD)), 1_000 * UNIT);
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
	});
}
