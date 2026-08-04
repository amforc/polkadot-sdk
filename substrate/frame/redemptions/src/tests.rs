use crate::{
	mock::*,
	types::{RecoveryOffsetQuote, RecoveryRegime, RedemptionConfig, RedemptionQuote},
	weights::WeightInfo,
	Error, Event,
};
use pusd_primitives::{
	collateralization_ratio, recovery_pricing, reducible_debit, CollateralRatio, DebtCollateral,
	RecoveryOffsetInterface, RecoveryOffsetResult, VaultInterface,
};

const HOUR_MS: Moment = 3_600 * 1_000;
const ONE_YEAR_MS: Moment = 31_557_600_000;

fn rate_pct(num: u128, denom: u128) -> FixedU128 {
	FixedU128::from_rational(num, denom)
}

/// Builds the ordinary-redemption fee curve for the default test policy.
///
/// The curve starts at `decayed` and uses stablecoin debt of `debt`.
fn fee_curve(decayed: FixedU128, debt: Balance) -> crate::fees::DynamicFeeCurve {
	let config = default_redemption_config();
	crate::fees::DynamicFeeCurve::try_new(decayed, debt, &config).expect("test debt fits u128")
}

/// Calculates the stable amount that buys exactly `debt` of PUSD at the current time.
///
/// The amount includes the debt and its fee. The calculation uses the stored policy, decayed
/// dynamic fee, and current debt that an ordinary redemption reads.
fn spend_for_debt(debt: Balance) -> Balance {
	let config = crate::RedemptionConfigs::<Test>::get(PUSD).expect("PUSD is registered");
	let decayed =
		crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee_at(Timestamp::get(), &config);
	let curve = crate::fees::DynamicFeeCurve::try_new(decayed, stablecoin_debt(PUSD), &config)
		.expect("test debt fits u128");
	debt + curve.fee(debt)
}

/// Quotes a `(DOT, PUSD)` redemption.
fn preview(budget: Balance, max_steps: u32) -> Result<RedemptionQuote<Balance>, DispatchError> {
	preview_redeem(DOT, PUSD, budget, max_steps)
}

/// Branch TCR as the vault pallet reports it, including pending interest.
fn branch_tcr(collateral: AssetId, stable: StableId) -> CollateralRatio {
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

/// The `(DOT, PUSD)` vault's collateralization ratio at `price`.
fn vault_cr(who: AccountId, price: FixedU128) -> FixedU128 {
	let Ok(CollateralRatio::Ratio(cr)) = collateralization_ratio(
		&DebtCollateral { debt: vault_debt(DOT, PUSD, who), collateral: held(DOT, who) },
		price,
	) else {
		panic!("fixture must carry debt with a finite CR");
	};
	cr
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
			None,
		));

		// The second market joins the coin's existing fee state untouched.
		assert_eq!(
			crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee,
			FixedU128::from_rational(3u128, 100u128)
		);
		assert!(crate::RedemptionConfigs::<Test>::contains_key(PUSD));

		assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), TOKEN_X, PUSD));
		assert!(crate::RedemptionConfigs::<Test>::contains_key(PUSD));
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
fn branch_registration_rejects_invalid_redemption_config() {
	build_and_execute(|| {
		let mut bad = default_redemption_config();
		bad.minimum_redemption_amount = 0;

		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		assert_noop!(
			Vaults::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				Some(bad),
			),
			Error::<Test>::InvalidRedemptionConfig
		);
	});
}

// The redemption policy is per stablecoin, so exactly one market registers it:
// the coin's first. Neither omitting it there nor restating it later can pass,
// or a later market would silently claim a policy it does not own.
#[test]
fn only_the_first_market_on_a_coin_carries_the_redemption_config() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		assert_noop!(
			Vaults::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				None,
			),
			Error::<Test>::RedemptionConfigRequired
		);

		register_branch(DOT, PUSD, default_branch_config());

		set_price(TOKEN_X, FixedU128::one());
		assert_noop!(
			Vaults::create_branch(
				RuntimeOrigin::root(),
				TOKEN_X,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				Some(default_redemption_config()),
			),
			Error::<Test>::RedemptionConfigNotExpected
		);
	});
}

#[test]
fn redeem_and_preview_report_below_minimum_amount() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000);
		assert_noop!(
			redeem(3, DOT, PUSD, 99, 0, 4, 0),
			Error::<Test>::BelowMinimumRedemptionAmount
		);
		assert_noop!(preview(99, 0), Error::<Test>::BelowMinimumRedemptionAmount);
	});
}

#[test]
fn redeem_and_preview_report_unregistered_stablecoin() {
	build_and_execute(|| {
		mint_stable(PUSD, 3, 1_000);
		assert_noop!(redeem(3, DOT, PUSD, 200, 0, 4, 0), Error::<Test>::StablecoinNotRegistered);
		assert_noop!(preview(200, 0), Error::<Test>::StablecoinNotRegistered);
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
fn redeem_and_preview_report_no_vault() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 3, 1_000);
		assert_noop!(redeem(3, DOT, PUSD, 200, 0, 4, 0), Error::<Test>::NoRedeemableVault);
		assert_noop!(preview(200, 0), Error::<Test>::NoRedeemableVault);
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

		// The full cost of 213 includes the fee and buys 201 debt. Each vault owes 501, which
		// includes a one-unit seven-day upfront fee.
		//
		// The stablecoin debt is 1_002, and 201 is 20.06% of it. The dynamic fee increases by
		// half of that share, to 10.03%.
		//
		// The redemption pays the 5.01% mean and the 0.5% base fee. Thus, the fee is
		// `ceil(201 * 0.055150) = 12`, and the total cost is 213.
		//
		// An amount of 202 cannot buy the same debt with its fee. The collateral output is
		// `floor(201 / 1.25) = 160`.
		assert_ok!(redeem(3, DOT, PUSD, 213, 0, 4, 0));

		// The lowest-rate vault absorbs the whole fill, in debt and in held
		// collateral; the higher-rate vault is untouched in both.
		assert_eq!(v1_before - vault_debt(DOT, PUSD, 1), 201);
		assert_eq!(v1_held_before - held(DOT, 1), 160);
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
		assert_eq!(held(DOT, 2), v2_held_before);
		// The mock verifies every balance movement against the settlement event, so pinning
		// the event's figures pins the redeemer's spend, the fee routing, the recipient's
		// collateral, and the issuance change.
		let settled = last_settlement();
		assert_eq!(settled.figures(), (201, 12, 160));
		assert_eq!(settled.ordinary_steps, Some(1));
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

#[test]
fn walk_reports_only_cap_exhaustion_as_truncated() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		// Vault 2 has a much higher collateral ratio than vault 1. Thus, a price can put vault 1
		// below par while vault 2 stays eligible for redemption.
		assert_ok!(open(2, DOT, PUSD, 10_000, 500, rate_pct(2, 100)));

		// A one-step cap with budget and a second target to spare: only the
		// cap guard reports truncation.
		let capped = preview(100_000, 1).expect("quote");
		assert_eq!(capped.steps, 1);
		assert!(capped.truncated, "the cap guard ended the walk");

		// Budget exhaustion inside the cap is a complete walk, not truncation.
		let filled = preview(200, 20).expect("quote");
		assert_eq!(filled.steps, 1);
		assert!(!filled.truncated);

		// Park a Dormant husk at the priority slot and sink the price: the
		// underwater Dormant head is a barrier — counted, but not truncation.
		mint_stable(PUSD, 3, 1_000);
		assert_ok!(redeem(3, DOT, PUSD, 360, 0, 4, 0));
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault 1").is_dormant());
		set_price(DOT, FixedU128::from_rational(15, 100));
		assert!(
			vault_cr(2, FixedU128::from_rational(15, 100)) > FixedU128::one(),
			"fixture must keep vault 2 redeemable behind the barrier"
		);
		assert_noop!(preview(100_000, 20), Error::<Test>::NoRedeemableVault);

		// The barrier does not permanently block the queue. Liquidation removes the underwater husk
		// from the priority position.
		//
		// The walk then reaches the healthy vault behind it and can redeem its debt after
		// redistribution.
		liquidate_redistribute_all(1);
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_none(), "husk gone");
		let unblocked = preview(100_000, 20).expect("quote");
		assert_eq!(unblocked.steps, 1);
		assert!(!unblocked.truncated);
		assert_eq!(unblocked.debt_cancelled, projected_full_debt(2));
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

		// Each vault owes 241, so the stablecoin debt is 482. A spend of 106 buys 100 debt, which
		// is 20.75% of this total.
		//
		// The dynamic fee increases by half of that share, to 10.37%. The redemption pays the
		// 5.19% mean and the 0.5% base fee.
		//
		// Thus, the fee is `ceil(100 * 0.056867) = 6`, and the total cost is 106. Vault 2 supplies
		// `floor(100 / 0.9) = 111` collateral.
		assert_ok!(redeem(3, DOT, PUSD, 106, 0, 4, 0));
		// The skipped underwater vault keeps its debt and its held collateral.
		assert_eq!(vault_debt(DOT, PUSD, 1), v1_before);
		assert_eq!(held(DOT, 1), v1_held_before);
		// The healthy vault behind it is redeemed.
		assert_eq!(v2_before - vault_debt(DOT, PUSD, 2), 100);
		assert_eq!(v2_held_before - held(DOT, 2), 111);
		assert_eq!(last_settlement().figures(), (100, 6, 111));
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
		let quote = preview(2_000, 0).expect("preview");
		assert_eq!(quote.steps, 4, "the walk visits the skipped prefix once");
		assert_eq!(quote.debt_cancelled, v3_before + v4_before);
		assert!(!quote.truncated);

		// The mock checks that execution reproduces this quote.
		assert_ok!(redeem(5, DOT, PUSD, 2_000, 0, 6, 0));
		// The underwater prefix (vaults 1-2) is skipped and left untouched.
		assert_eq!(vault_debt(DOT, PUSD, 1), v1_before);
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
		// The healthy vaults behind it drain fully.
		assert_eq!(vault_debt(DOT, PUSD, 3), 0);
		assert_eq!(vault_debt(DOT, PUSD, 4), 0);
		// Collateral is paid at face value, floored per step at price 0.9 (= debt * 10 / 9).
		assert_eq!(last_settlement().collateral_out, v3_before * 10 / 9 + v4_before * 10 / 9);
	});
}

#[test]
fn slippage_bound_reverts_without_state_change() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000_000);

		// An amount of 221 pUSD buys 200 debt and pays its fee of 21. It receives
		// `floor(200 / 1.25) = 160` collateral, below the floor of 161.
		//
		// Thus, the full redemption reverts without side effects.
		assert_noop!(redeem(3, DOT, PUSD, 221, 161, 4, 0), Error::<Test>::SlippageExceeded);
	});
}

/// Verifies that a partial fill scales the caller's slippage floor pro rata.
///
/// The scale uses the pUSD amount spent. Thus, a floor for the full budget does not incorrectly
/// reject a smaller fill.
#[test]
fn slippage_floor_scales_to_partial_fill() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000_000);

		// The vault owes 501, which includes a one-unit seven-day upfront fee. The offer of 1_000
		// exceeds the required amount.
		//
		// The fill cancels all 501 debt and pays
		// `floor(501 / 1.25) = 400` collateral.
		//
		// Full redemption has a share of one and increases the dynamic fee by `1 / 2` to 50%.
		// The mean is 25% plus the 0.5% base fee, and `ceil(501 * 0.255) = 128`.
		//
		// The redemption spends only 629 of the 1_000 offer. Thus, the floor becomes
		// `floor(min · 629 / 1_000)`. A floor of 638 becomes 401 and fails.
		//
		// A floor of 637 becomes 400 and succeeds.
		let quote = preview(1_000, 0).expect("quote");
		assert_eq!((quote.debt_cancelled, quote.fee, quote.collateral_out), (501, 128, 400));
		assert_noop!(redeem(3, DOT, PUSD, 1_000, 638, 4, 0), Error::<Test>::SlippageExceeded);

		assert_ok!(redeem(3, DOT, PUSD, 1_000, 637, 4, 0));
		assert_eq!(vault_debt(DOT, PUSD, 1), 0);
	});
}

/// Verifies that the collateral floor scales to the stable amount spent.
///
/// A fee increase between quote and execution buys less debt and collateral for the same cost. It
/// can fail the floor in the same way as a price change.
#[test]
fn slippage_floor_catches_a_fee_that_climbs_after_the_quote() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 100_000, 50_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000_000);
		mint_stable(PUSD, 5, 1_000_000);

		let quote = preview(10_000, 0).expect("quote");

		// Another redeemer acts first and increases the dynamic fee. Thus, the same 10_000 buys
		// less debt and uses more of the amount for the fee.
		assert_ok!(redeem(5, DOT, PUSD, 10_000, 0, 6, 0));
		let requoted = preview(10_000, 0).expect("requote");
		assert!(requoted.fee > quote.fee, "the fee climbed");
		assert!(requoted.debt_cancelled < quote.debt_cancelled, "the spend buys less debt");
		assert!(requoted.collateral_out < quote.collateral_out, "and less collateral");

		// The original collateral floor causes a revert. The new quoted collateral floor permits
		// the fill.
		assert_noop!(
			redeem(3, DOT, PUSD, 10_000, quote.collateral_out, 4, 0),
			Error::<Test>::SlippageExceeded
		);
		assert_ok!(redeem(3, DOT, PUSD, 10_000, requoted.collateral_out, 4, 0));
	});
}

#[test]
fn insufficient_stable_balance_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 50);
		assert_noop!(redeem(3, DOT, PUSD, 201, 0, 4, 0), Error::<Test>::InsufficientStableBalance);
	});
}

#[test]
fn balance_bound_uses_the_fee_raised_by_the_affordable_debt() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 10_000, 1_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000);
		let coin_debt_before = stablecoin_debt(PUSD);

		// The 2_000 offered exceeds the 1_000 balance. The account can pay 999 while it keeps
		// its one-unit minimum balance, and that limit binds.
		assert_ok!(redeem(3, DOT, PUSD, 2_000, 0, 4, 0));

		// The dynamic fee uses the debt that the redemption can cancel, not the requested amount.
		// The 999 supplies 824 debt and the 174 fee that this debt causes, and leaves one unit
		// that cannot buy more.
		//
		// A fee based on the unavailable 2_000 offer would reserve a much higher rate and cancel
		// much less debt.
		let settled = last_settlement();
		assert_eq!((settled.stable_burned, settled.fee), (824, 174));
		assert_eq!(Assets::balance(PUSD, 3), 2, "the minimum balance and the spare unit stay");
		// 825 debt pays one more unit of fee, and would need the whole balance.
		let curve = fee_curve(FixedU128::zero(), coin_debt_before);
		let debt: Balance = 824;
		assert!(debt + curve.fee(debt) <= 999);
		assert!(debt + 1 + curve.fee(debt + 1) > 999);
	});
}

/// Verifies that a fully funded budget buys the maximum debt whose fee also fits the budget.
///
/// The quote and execution use the same amount.
#[test]
fn budget_binds_before_a_larger_balance() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 10_000, 1_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 2_000);

		let quote = preview(1_000, 0).expect("quote");
		assert_eq!((quote.debt_cancelled, quote.fee, quote.stable_in()), (825, 175, 1_000));

		assert_ok!(redeem(3, DOT, PUSD, 1_000, 0, 4, 0));
		assert_eq!(Assets::balance(PUSD, 3), 1_000, "the rest of the balance stays put");
	});
}

/// Verifies failure when a budget cannot buy the minimum debt after inclusion of its fee.
///
/// The terms cause the failure if the balance covers the budget. The balance causes the failure if
/// it does not cover the budget.
#[test]
fn budget_that_the_fee_takes_below_the_minimum_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000);

		// The stablecoin debt is 501, so the 100 minimum is 19.96% of it. The increase is half of
		// that share, 9.98%, with a 4.99% mean.
		//
		// The 0.5% base gives `ceil(100 * 0.054900) = 6`. Thus, 106 buys exactly 100 debt. An
		// amount of 105 buys only 99 debt with the same fee of 6.
		let quote = preview(106, 0).expect("quote");
		assert_eq!((quote.debt_cancelled, quote.fee), (100, 6));
		assert_noop!(preview(105, 0), Error::<Test>::BelowMinimumRedemptionAmount);
		assert_noop!(
			redeem(3, DOT, PUSD, 105, 0, 4, 0),
			Error::<Test>::BelowMinimumRedemptionAmount
		);

		// If the balance cannot cover the budget, the balance causes the same shortfall.
		mint_stable(PUSD, 5, 105);
		assert_noop!(redeem(5, DOT, PUSD, 106, 0, 6, 0), Error::<Test>::InsufficientStableBalance);
		assert_ok!(redeem(3, DOT, PUSD, 106, 0, 4, 0));
	});
}

#[test]
fn dynamic_fee_rises_after_ordinary_redemption() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 100_000, 50_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000_000);
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, FixedU128::zero());
		let stablecoin_debt_before = stablecoin_debt(PUSD);

		assert_ok!(redeem(3, DOT, PUSD, 10_550, 0, 4, 0));

		// A spend of 10_550 buys 10_000 debt and pays the fee in addition. The vault owes 50_048,
		// so this amount is 19.98% of the stablecoin debt.
		//
		// The dynamic fee increases by half of that share, to 9.99%. The redemption pays the 5.00%
		// mean and the 0.5% base fee.
		//
		// Thus, the fee is `ceil(10_000 * 0.054952) = 550`, and the total cost is 10_550. The
		// collateral output is `10_000 / 1.25 = 8_000`.
		assert_eq!(last_settlement().figures(), (10_000, 550, 8_000));
		// The stablecoin debt aggregate falls by exactly the cancelled debt.
		assert_eq!(stablecoin_debt_before - stablecoin_debt(PUSD), 10_000);

		// The new dynamic fee is `decayed(0) + share / increase_divisor`, computed against the
		// stablecoin debt captured before the redemption.
		let expected =
			fee_curve(FixedU128::zero(), stablecoin_debt_before).raised_dynamic_fee(10_000);
		assert!(expected > FixedU128::zero());
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, expected);
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

		// Both redemptions cancel 10_000 debt. The first increases the dynamic fee from zero to
		// `10_000 / 100_096 / 2 = 4.995%`.
		//
		// It pays the mean rate, and `ceil(10_000 · (2.498% + 0.5%)) = 300`. The second starts at
		// 4.995% and increases the fee by `10_000 / 90_096 / 2 = 5.550%` to 10.545% over the final
		// 90_096 debt.
		//
		// It pays `ceil(10_000 · (7.770% + 0.5%)) = 828`. The total costs are 10_300 and 10_828.
		assert_ok!(redeem(3, DOT, PUSD, 10_300, 0, 4, 0));
		let dot = last_settlement();
		assert_eq!((dot.stable_burned, dot.fee), (10_000, 300));

		let raised = fee_curve(FixedU128::zero(), both_markets).raised_dynamic_fee(10_000);
		assert!(raised > FixedU128::zero());
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, raised);

		// The TOKEN_X redeemer, who redeemed nothing, now pays the raised rate.
		let debt_before = vault_debt(TOKEN_X, PUSD, 2);
		assert_ok!(redeem(3, TOKEN_X, PUSD, 10_828, 0, 4, 0));
		assert_eq!(debt_before - vault_debt(TOKEN_X, PUSD, 2), 10_000);
		let token_x = last_settlement();
		assert_eq!((token_x.stable_burned, token_x.fee), (10_000, 828));
	});
}

/// Runs consecutive DOT/PUSD redemptions of `debts` against the fixture of
/// [`a_redemption_in_halves_pays_the_climb_that_the_first_half_causes`] and returns the fees
/// paid and the dynamic fee left behind. Storage is rolled back afterwards, so one fixture
/// serves every sequence.
///
/// Each redemption spends exactly what its debt and fee cost, so the mock verifies the quote and
/// settlement of every call. Each `idle_ms` elapses before its redemption.
fn redeem_in_sequence(debts: &[(Balance, Moment)]) -> (Balance, FixedU128) {
	hypothetically!({
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let collateral_before = collateral_balance(DOT, 4);
		let total: Balance = debts.iter().map(|&(debt, _)| debt).sum();
		for &(debt, idle_ms) in debts {
			advance_time(idle_ms);
			assert_ok!(redeem(3, DOT, PUSD, spend_for_debt(debt), 0, 4, 0));
		}
		// Every unit of debt buys the same collateral at the 1.25 price, however it is split.
		assert_eq!(collateral_balance(DOT, 4) - collateral_before, total * 4 / 5);
		(
			Assets::balance(PUSD, FEE_DEST) - fee_before,
			crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee,
		)
	})
}

/// Verifies the worked example through the pallet: 20_000 of the 500_480 stablecoin debt, at
/// once and in halves.
///
/// The curve values are those of `fees::tests::the_worked_example_at_once_and_in_halves`: 300 at
/// once, and 100 plus 201 in halves, which leave a higher dynamic fee. The pallet feeds the curve
/// the stablecoin debt and the stored dynamic fee that each redemption leaves.
///
/// Time makes a split cheaper. After one idle half-life, the second half starts at half the
/// dynamic fee that the first half caused: it pays `ceil(10_000 · (0.4995% + 0.5097% + 0.5%)) =
/// 151` instead of 201.
#[test]
fn a_redemption_in_halves_pays_the_climb_that_the_first_half_causes() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// One vault owes 500_480: 500_000 plus a 480-unit seven-day upfront fee.
		assert_ok!(open(1, DOT, PUSD, 1_000_000, 500_000, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 2_000_000);
		assert_eq!(vault_debt(DOT, PUSD, 1), 500_480);

		let (at_once_fee, at_once_dynamic_fee) = redeem_in_sequence(&[(20_000, 0)]);
		assert_eq!(at_once_fee, 300);
		assert_eq!(
			at_once_dynamic_fee,
			fee_curve(FixedU128::zero(), 500_480).raised_dynamic_fee(20_000)
		);

		let (halves_fee, halves_dynamic_fee) = redeem_in_sequence(&[(10_000, 0), (10_000, 0)]);
		assert_eq!(halves_fee, 301);
		assert!(halves_dynamic_fee > at_once_dynamic_fee);

		let (rested_fee, rested_dynamic_fee) =
			redeem_in_sequence(&[(10_000, 0), (10_000, 6 * HOUR_MS)]);
		assert_eq!(rested_fee, 251);
		assert!(rested_dynamic_fee < at_once_dynamic_fee);
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
		assert_ok!(redeem(3, DOT, PUSD, spend_for_debt(1_000), 0, 4, 0));

		// The stored fee is the exact decayed value plus this redemption's increase. This
		// calculation uses the same primitives as execution.
		//
		// The spend buys 1_000 canceled debt and pays the fee in addition.
		let expected = fee_curve(decayed, stablecoin_debt_before).raised_dynamic_fee(1_000);
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, expected);
		assert!(expected < raised, "the decay must outweigh a small re-increase");
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
		// An amount of 202 buys 200 debt. The calculation below gives the fee of 2.
		assert_ok!(redeem(3, DOT, PUSD, 202, 0, 4, 0));

		// Debt cancellation does not depend on the rate. Thus, the fee state proves that no residue
		// remains. It starts again from exactly zero, as it does for a first redemption.
		//
		// A residue as small as `1e-18` would remain in the stored value and fail this test.
		assert_eq!(vault_debt(DOT, PUSD, 1), accrued - 200);
		let expected = fee_curve(FixedU128::zero(), stablecoin_debt_before).raised_dynamic_fee(200);
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, expected);
		// One year of idle interest gives stablecoin debt of 420_504. This redemption increases the
		// dynamic fee by `200 / 420_504 / 2 = 0.024%`.
		//
		// It pays the 0.012% mean and the 0.5% base fee. Thus, `ceil(200 * 0.005119) = 2`, and the
		// total cost is 202.
		assert_eq!(last_settlement().fee, 2);
	});
}

#[test]
fn dormant_target_is_redeemed_before_rate_index() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		mint_stable(PUSD, 3, 1_000_000);

		// The redemption cancels 360 of the 501 debt in vault 1. Its sub-minimum residual makes it
		// Dormant.
		assert_ok!(redeem(3, DOT, PUSD, spend_for_debt(360), 0, 4, 0));
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
		// Vault 1 is now Dormant (out of the rate index); the only way the second
		// redemption can reach it is via the Dormant slot, which is served before
		// the rate index. It redeems the Dormant vault and never touches ordinary
		// vault 2.
		assert_ok!(redeem(3, DOT, PUSD, spend_for_debt(100), 0, 4, 0));
		// Cancel less than the residual so the husk survives the step and its
		// row stays readable: a full drain removes the vault, and the debt
		// cancelled could then only be inferred.
		assert_eq!(v1_residual, 141);
		assert_eq!(v1_residual - vault_debt(DOT, PUSD, 1), 100);
		// Collateral paid is the face-value amount for the debt cancelled.
		assert_eq!(last_settlement().collateral_out, 80);
		// Priority: the ordinary vault behind the Dormant slot is untouched.
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
	});
}

#[test]
fn preview_redeem_continues_from_drained_dormant_into_rate_index() {
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
		let quote = preview(100_000, 0).expect("quote");
		assert_eq!(quote.steps, 2);
		assert!(!quote.truncated);
		assert_eq!(quote.debt_cancelled, v1_residual + v2_debt);

		// Execution walks the same two targets; the mock checks it reproduces the quote.
		assert_ok!(redeem(3, DOT, PUSD, 100_000, 0, 4, 0));
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
fn recovery_bonus_pays_more_than_face_value_and_improves_the_vault_cr() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Price 0.52 parks the FinalRecovery vault's CR barely above 100%, the
		// regime where an overpaid bonus could actually damage the vault.
		let price = FixedU128::from_rational(52u128, 100u128);
		setup_final_recovery(1, 1_000, 500, price);
		// A live dynamic fee makes the mock's "recovery leaves the fee state alone" check
		// meaningful.
		set_dynamic_fee(PUSD, FixedU128::from_rational(3u128, 100u128));
		mint_stable(PUSD, 3, 1_000_000);
		let debt_before = vault_debt(DOT, PUSD, 1);

		let cr_before = vault_cr(1, price);
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

		// CR = 520/501 ≈ 103.79%, so the bonus is the mid-range excess
		// CR − 100% − 1% buffer ≈ 2.79% (below the 5% penalty cap):
		// floor(floor(200 · 1.0279) / 0.52) = floor(205 / 0.52) = 394. Recovery is fee-free.
		let settled = last_settlement();
		assert_eq!(settled.figures(), (200, 0, 394), "recovery bonus payout");
		assert_eq!(settled.ordinary_steps, None, "recovery must not settle as ordinary");
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before - 200);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));

		// The 1% buffer keeps the bonus strictly below CR − 100%, so paying it
		// must leave the vault's CR strictly better than before the redemption.
		assert!(vault_cr(1, price) > cr_before, "recovery-bonus redemption must improve the CR");
	});
}

/// The head settles at a bonus up to the 120% ICR, including the band above
/// the 110% MCR where it could already exit: the bonus keeps redeemers pushing
/// it out of recovery with a buffer rather than one tick above MCR. Above ICR,
/// redemption and preview refuse the head and offsets see no target. After the
/// exit it is an ordinary vault again.
#[test]
fn recovery_settlement_stops_above_icr() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		mint_stable(PUSD, 3, 1_000_000);

		// CR = 550/501 ≈ 109.8%: the capped 5% bonus pays floor(210 / 0.55) = 381.
		set_price(DOT, FixedU128::from_rational(55u128, 100u128));
		assert_ok!(redeem(3, DOT, PUSD, 200, 0, 4, 0));
		assert_eq!(last_settlement().collateral_out, 381);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
		let debt_before = vault_debt(DOT, PUSD, 1);

		// CR = 619 · 0.60 / 301 ≈ 123.4%: above the ICR, only the exit remains.
		set_price(DOT, FixedU128::from_rational(60u128, 100u128));
		assert_noop!(redeem(3, DOT, PUSD, 200, 0, 4, 0), Error::<Test>::FinalRecoveryExitRequired);
		assert_noop!(preview(200, 0), Error::<Test>::FinalRecoveryExitRequired);
		assert_ok!(preview_offset(200), RecoveryOffsetQuote::NoTarget);
		assert_ok!(execute_offset(3, 4, 200), RecoveryOffsetResult::NoTarget);
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before);

		// CR = 619 · 0.56 / 301 ≈ 115.2%: above MCR but not ICR. Offsets see a target and a
		// redemption still pays the capped 5% bonus, floor(105 / 0.56) = 187, with no fee.
		set_price(DOT, FixedU128::from_rational(56u128, 100u128));
		assert_ok!(preview_offset(100), RecoveryOffsetQuote::Available { debt: 100 });
		assert_ok!(redeem(3, DOT, PUSD, 100, 0, 4, 0));
		assert_eq!(last_settlement().figures(), (100, 0, 187));
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
		assert_eq!(vault_debt(DOT, PUSD, 1), 201);
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault 1").is_final_recovery());

		// CR = 432 · 0.56 / 201 ≈ 120.4%: the head exits and is ordinary again.
		assert_ok!(Vaults::exit_final_recovery(
			RuntimeOrigin::signed(3),
			DOT,
			PUSD,
			1,
			pallet_linked_list::Position::endpoints_only()
		));
		assert_ok!(redeem(3, DOT, PUSD, spend_for_debt(200), 0, 4, 0));
		// Ordinary again: face value floor(200 / 0.56) = 357, and the fee is charged.
		let settled = last_settlement();
		assert!(settled.ordinary_steps.is_some());
		assert_eq!(settled.collateral_out, 357);
		assert!(settled.fee > 0);
	});
}

/// A sub-minimum head past the gate may be unable to exit while another vault
/// holds the Dormant slot, so it stays settleable, at face value.
#[test]
fn sub_minimum_head_past_the_exit_gate_settles_at_face_value() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		mint_stable(PUSD, 3, 1_000_000);
		let debt = vault_debt(DOT, PUSD, 1);
		assert_ok!(redeem(3, DOT, PUSD, debt - 150, 0, 4, 0));
		assert_eq!(vault_debt(DOT, PUSD, 1), 150);

		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		assert_ok!(preview_offset(100), RecoveryOffsetQuote::Available { debt: 100 });
		assert_ok!(redeem(3, DOT, PUSD, 100, 0, 4, 0));
		// No bonus: floor(100 / 1.25) = 80, and no fee.
		assert_eq!(last_settlement().figures(), (100, 0, 80));
		assert_eq!(vault_debt(DOT, PUSD, 1), 50);
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault 1").is_final_recovery());
	});
}

#[test]
fn recovery_head_is_quoted_and_served_before_ordinary_vaults() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		// Open the ordinary vault at a healthy price, then settle at 0.54, where the
		// head's CR ≈ 107.8% is below the 110% exit gate.
		set_price(DOT, FixedU128::from_rational(5u128, 4u128));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		let price = FixedU128::from_rational(54u128, 100u128);
		set_price(DOT, price);
		let v1_before = vault_debt(DOT, PUSD, 1);
		let v2_before = vault_debt(DOT, PUSD, 2);
		mint_stable(PUSD, 3, 1_000_000);

		// Beyond the 106% at which the cap starts binding, the bonus must come out
		// clamped to exactly the 5% redistribution penalty rather than the raw excess.
		let cr = vault_cr(1, price);
		assert!(cr > rate_pct(106, 100), "fixture must put the raw excess above the cap");
		assert!(cr < rate_pct(110, 100), "fixture must keep the head below the exit gate");
		let bonus = recovery_pricing::recovery_bonus(
			cr,
			Permill::from_percent(1),
			Permill::from_percent(5),
		);
		assert_eq!(bonus, FixedU128::from(Permill::from_percent(5)), "bonus capped at penalty");

		// The quote exposes the priority target: capped 5% bonus, floor(200 * 1.05 / 0.54) = 388
		// collateral, no fee.
		let quote = preview(200, 0).expect("quote");
		assert_eq!(quote.steps, 1);
		assert_eq!((quote.stable_in(), quote.fee, quote.collateral_out), (200, 0, 388));

		// The FinalRecovery vault is served at its exact regime, before any ordinary vault; the
		// mock checks execution reproduces the quote.
		assert_ok!(redeem(3, DOT, PUSD, 200, 0, 4, 0));
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
		assert_eq!(v1_before - vault_debt(DOT, PUSD, 1), 200);
		// The ordinary vault is untouched.
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
	});
}

/// A partial settlement with zero market debt must not draw Insurance Fund cover.
#[test]
fn partial_fill_with_zero_market_cancel_debt_pays_no_cover() {
	build_and_execute(|| {
		let snapshot = pusd_primitives::RedemptionStepSnapshot {
			status: pusd_primitives::VaultStatus::FinalRecovery,
			debt: 400,
			terminal_interest_charge: 1,
			collateral: 100,
			redistribution_penalty: Permill::zero(),
			initial_collateralization_ratio: FixedU128::from_rational(120u128, 100u128),
			minimum_debt: 200,
		};
		// A zero budget selects the partial branch after collateral value floors to zero.
		mint_stable(PUSD, insurance_account(PUSD), 400);
		let price = FixedU128::from_rational(1u128, 1_000_000u128);
		let plan =
			Redemptions::price_recovery(&PUSD, &snapshot, price, 0, &default_redemption_config())
				.expect("prices");
		assert_eq!(plan.debt(), 0);
		assert_eq!(plan.insurance_cover(), 0, "partial fill must leave the fund untouched");
		assert_eq!(plan.regime(), RecoveryRegime::InsuranceAdjusted);
	});
}

/// With no cover for the coin, the market debt is the whole debt. Another stablecoin's fund
/// account may be flush with pUSD, but it is not cover for the `(DOT, PUSD)` market.
#[test]
fn insurance_adjusted_settlement_with_empty_fund_ignores_other_coins_funds() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128));
		let debt = vault_debt(DOT, PUSD, 1);
		// 500 principal + the 1-unit 7-day upfront fee.
		assert_eq!(debt, 501);
		let other_stable: StableId = PUSD + 1;
		mint_stable(PUSD, insurance_account(other_stable), 1_000_000);
		assert_eq!(Assets::balance(PUSD, insurance_account(PUSD)), 0);
		mint_stable(PUSD, 3, 1_000_000);

		assert_ok!(redeem(3, DOT, PUSD, debt, 0, 4, 0));

		// Empty fund: the market debt is the whole 501, at an effective rate of
		// C/D = 400/501. Cancelling all of it pays the entire 1_000 collateral
		// and closes the row.
		let settled = last_settlement();
		assert_eq!(settled.figures(), (debt, 0, 1_000));
		assert_eq!(settled.insurance_cover, 0);
		assert_eq!(Vaults::vault_status(DOT, PUSD, 1), None, "vault settled and closed");
		assert_eq!(held(DOT, 1), 0);
		assert_eq!(Assets::balance(PUSD, insurance_account(other_stable)), 1_000_000);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
	});
}

/// The collateral value that sizes the Insurance Fund cover rounds up: the fund covers no more
/// than the true shortfall, and the redeemer pays no less than the collateral is worth.
#[test]
fn insurance_cover_rounds_the_collateral_value_up() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// 1_000 units at 0.4005 are worth 400.5 pUSD against 501 of debt.
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(4_005u128, 10_000u128));
		assert_eq!(vault_debt(DOT, PUSD, 1), 501);
		mint_stable(PUSD, insurance_account(PUSD), 1_000);
		mint_stable(PUSD, 3, 1_000_000);

		// The value rounds up to 401, so the flush fund covers a shortfall of 100 rather than
		// 101, and the redeemer pays 401 for collateral worth 400.5: the half unit rounds
		// against the redeemer, never against the fund.
		assert_ok!(redeem(3, DOT, PUSD, 1_000, 0, 4, 0));
		let settled = last_settlement();
		assert_eq!(settled.figures(), (401, 0, 1_000));
		assert_eq!(settled.insurance_cover, 100);
		assert_eq!(Vaults::vault_status(DOT, PUSD, 1), None, "vault settled and closed");
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

		// The `pallet-assets` owner is the stablecoin authority. Vaults uses the same rule for
		// `create_branch`.
		//
		// A market full admin is not sufficient because one policy controls all markets for the
		// stablecoin.
		use frame::traits::fungibles::roles::Inspect as _;
		assert_eq!(Assets::owner(PUSD), Some(STABLECOIN_OWNER));
		assert_ne!(STABLECOIN_OWNER, ADMIN);
		assert_ok!(Redemptions::set_redemption_config(
			RuntimeOrigin::signed(STABLECOIN_OWNER),
			PUSD,
			cfg
		));

		assert_eq!(
			crate::RedemptionConfigs::<Test>::get(PUSD).unwrap().minimum_redemption_amount,
			250
		);
	});
}

/// Verifies all unauthorized origins for `set_redemption_config`.
///
/// They are this market's full admin, its emergency admin, and another market's full admin. Only
/// Root and the stablecoin owner have authority.
#[test]
fn set_redemption_config_rejects_market_admins() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		let other_admin: AccountId = 55;
		// A Root-created market charges its full admin the custody seed.
		mint_collateral(TOKEN_X_ID, other_admin, 2);
		set_price(TOKEN_X, FixedU128::one());
		assert_ok!(Vaults::create_branch(
			RuntimeOrigin::root(),
			TOKEN_X,
			PUSD,
			branch_admins(other_admin, other_admin),
			default_branch_config(),
			None,
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
		let cfg = default_redemption_config();
		assert_noop!(
			Redemptions::set_redemption_config(RuntimeOrigin::root(), PUSD, cfg),
			Error::<Test>::StablecoinNotRegistered
		);
	});
}

#[test]
fn preview_redeem_quotes_the_fee_curve_without_side_effects() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));

		// The quote and execution use the same fee calculation. An amount of 223 buys 201 of the
		// 501 stablecoin debt, a 40.12% share.
		//
		// The dynamic fee increases by half of that share, to 20.06%. Its mean is 10.03%, so the
		// fee is `ceil(201 * 0.105299) = 22`.
		//
		// Thus, the redeemer spends all 223. The mock proves the quote projects the pending touch
		// without applying it.
		let quote = preview(223, 0).expect("preview");
		assert_eq!(quote.steps, 1);
		assert!(!quote.truncated);
		assert_eq!(quote.stable_in(), 223);
		assert_eq!((quote.debt_cancelled, quote.fee, quote.collateral_out), (201, 22, 160));
	});
}

#[test]
fn preview_and_execution_include_terminal_charge_only_for_full_step() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 2_000, 500, rate_pct(10, 100)));
		mint_stable(PUSD, 5, 10_000);
		advance_time(1);

		let snapshot = Vaults::project_redemption_snapshot(&DOT, &PUSD, &1).unwrap();
		assert_eq!(snapshot.terminal_interest_charge, 1);
		let quote = preview(10_000, 1).unwrap();
		assert_eq!(quote.steps, 1);
		assert_eq!(quote.debt_cancelled, snapshot.debt + 1);

		assert_ok!(redeem(5, DOT, PUSD, 10_000, quote.collateral_out, 6, 1));
		assert_eq!(vault_debt(DOT, PUSD, 1), 0);
	});
}

// A partial payment must preserve the terminal charge for the surviving vault.
#[test]
fn partial_redemption_with_terminal_remainder_caps_below_base_debt() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 2_000, 500, rate_pct(10, 100)));
		mint_stable(PUSD, 5, 10_000);
		advance_time(1);

		let snapshot = Vaults::project_redemption_snapshot(&DOT, &PUSD, &1).unwrap();
		assert_eq!(snapshot.terminal_interest_charge, 1);
		// A spend that buys the whole base debt but not the terminal charge.
		let spend = spend_for_debt(snapshot.debt);
		let quote = preview(spend, 1).unwrap();
		assert_eq!(quote.steps, 1);
		assert_eq!(quote.debt_cancelled, snapshot.debt - 1);

		assert_ok!(redeem(5, DOT, PUSD, spend, 0, 6, 1));
		assert_eq!(vault_debt(DOT, PUSD, 1), 1);
		// The final settlement must collect the preserved terminal charge.
		let after = Vaults::project_redemption_snapshot(&DOT, &PUSD, &1).unwrap();
		assert_eq!(after.terminal_interest_charge, 1);
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

		let quote = preview(100_000, 0).expect("preview");
		assert_eq!(quote.steps, 2);
		assert!(!quote.truncated);
		assert_eq!(quote.debt_cancelled, v1_before + v2_before);
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

		// First transaction settles only the FIFO head (vault 1). The full payout leaves
		// neither debt nor collateral, so the vault closes and its row is removed.
		assert_ok!(redeem(3, DOT, PUSD, 10_000, 0, 4, 0));
		assert_eq!(Vaults::vault_status(DOT, PUSD, 1), None, "head settled and closed");
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
		assert_eq!(held(DOT, 1), 0);
		// A below-par owner has no equity claim on any of the collateral.
		System::assert_has_event(RuntimeEvent::Vaults(pallet_vaults::Event::VaultClosed {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			recipient: 1,
			collateral: 0,
		}));
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		// The fund covered vault 1 exactly, so the redeemer paid only the
		// collateral-backed portion, at par, and the full settlement pays the
		// vault's entire 1_000 collateral.
		let first = last_settlement();
		assert_eq!(first.figures(), (collateral_value_1, 0, 1_000));
		assert_eq!(first.insurance_cover, shortfall_1);
		assert_eq!(Assets::balance(PUSD, insurance_account(PUSD)), 0, "head 1 drained the fund");
		// Head-only: vault 2 is untouched and becomes the new FIFO head.
		assert_eq!(vault_debt(DOT, PUSD, 2), debt2);
		assert!(Vaults::vault_status(DOT, PUSD, 2).expect("vault 2").is_final_recovery());
		assert_eq!(Vaults::final_recovery_queue(DOT, PUSD, 10), vec![2u64]);

		// Second transaction settles vault 2 against the now-empty fund: the
		// recovery rate falls to C/D, so the redeemer covers the entire debt and
		// pUSD holders absorb the shortfall. A full settlement pays the whole
		// collateral, so this vault closes exactly like vault 1 did.
		assert_ok!(redeem(3, DOT, PUSD, 10_000, 0, 4, 0));
		assert_eq!(Vaults::vault_status(DOT, PUSD, 2), None, "second head settled and closed");
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 2)).is_none());
		assert_eq!(held(DOT, 2), 0);
		assert!(Vaults::final_recovery_queue(DOT, PUSD, 10).is_empty(), "FIFO drained");
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		// Empty fund: D = 501 against C = 100, so the redeemer pays all 501 for
		// the vault's 1_000 units.
		let second = last_settlement();
		assert_eq!(second.figures(), (debt2, 0, 1_000));
		assert_eq!(second.insurance_cover, 0, "no fund left to burn");
		assert!(
			second.stable_burned > first.stable_burned,
			"drained fund pushes the loss onto the redeemer"
		);
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

		// The first transaction spends 200 against a vault with much more debt. Preview and
		// execution cancel exactly 200 without a fee.
		//
		// The Insurance Fund must not change until the transaction cancels all market-side debt.
		let partial = preview(200, 0).expect("partial quote");
		assert_eq!((partial.debt_cancelled, partial.fee), (200, 0));
		assert_ok!(redeem(3, DOT, PUSD, 200, 0, 4, 0));
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_some(), "still settling");
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault 1").is_final_recovery());
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		// Recovery has no fee and cannot exceed the spend. This step cancels exactly 200 debt
		// because `200 < market_cancel_debt = 451`.
		//
		// The market debt is `D − 50 = 451`, so 200 buys `floor(200 · 1_000 / 451) = 443` of
		// the 1_000 collateral units.
		let first = last_settlement();
		assert_eq!(first.figures(), (200, 0, 443));
		assert_eq!(first.insurance_cover, 0, "fund untouched mid-settlement");
		assert_eq!(debt_before - vault_debt(DOT, PUSD, 1), 200);

		// The price changes before the second transaction, so the split is re-read from live
		// state: at 0.30 the remaining 557 collateral units are valued at 168 (167.1 rounded
		// up) against 301 of debt. The shortfall rises to 133, the fund still covers only 50,
		// and the market side cancels the other 251. Supplying all of it is the full settlement: it
		// burns the cover in the same step, pays every remaining collateral unit, and closes the
		// row.
		set_price(DOT, FixedU128::from_rational(30u128, 100u128));
		assert_ok!(redeem(3, DOT, PUSD, 10_000, 0, 4, 0));
		let second = last_settlement();
		assert_eq!(second.figures(), (251, 0, 557));
		assert_eq!(second.insurance_cover, 50, "cover burned on completion");
		assert_eq!(Vaults::vault_status(DOT, PUSD, 1), None, "vault settled and closed");
		assert_eq!(held(DOT, 1), 0);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		assert_eq!(Assets::balance(PUSD, insurance_account(PUSD)), 0);
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
		let tcr_before = branch_tcr(DOT, PUSD);

		// An amount of 223 buys 201 of the 501 stablecoin debt, which is 40.12%. The dynamic fee
		// increases by half of that share, to 20.06%.
		//
		// The redemption pays the 10.03% mean and the 0.5% base fee. Thus,
		// `ceil(201 * 0.105299) = 22`, and the total cost is 223.
		//
		// At a price of 0.6, the 201 debt buys `floor(201 / 0.6) = 335` collateral.
		assert_ok!(redeem(3, DOT, PUSD, 223, 0, 4, 0));
		assert_eq!(debt_before - vault_debt(DOT, PUSD, 1), 201);
		assert_eq!(last_settlement().figures(), (201, 22, 335));
		// A face-value redemption removes pUSD debt and at most an equal pUSD-value of
		// collateral, so a TCR above 1 can only rise; that invariant is exactly why ordinary
		// redemptions remain permitted in Safety mode.
		assert!(branch_tcr(DOT, PUSD) > tcr_before, "redemption must raise TCR");
	});
}

#[test]
fn redeem_and_preview_report_oracle_down_or_zero_price() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 3, 1_000);

		// The preamble reads the oracle itself, so a failing feed is refused
		// before any vault is touched.
		MockOracleAvailable::set(false);
		assert_noop!(redeem(3, DOT, PUSD, 201, 0, 4, 0), Error::<Test>::OracleUnavailable);
		assert_noop!(preview(201, 0), Error::<Test>::OracleUnavailable);

		// A feed that answers successfully but with a degenerate zero price hits
		// the preamble's explicit zero-price guard, which refuses it the same way.
		MockOracleAvailable::set(true);
		set_price(DOT, FixedU128::zero());
		assert_noop!(redeem(3, DOT, PUSD, 201, 0, 4, 0), Error::<Test>::OracleUnavailable);
		assert_noop!(preview(201, 0), Error::<Test>::OracleUnavailable);
	});
}

/// The interest-clock value stamped on a `(DOT, PUSD)` vault at its last poke.
fn vault_interest_time(who: AccountId) -> Moment {
	pallet_vaults::Vaults::<Test>::get((DOT, PUSD, who))
		.expect("vault")
		.vault
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

		// An amount of 1_011 buys 1_005 debt. The calculation below gives the fee of 6.
		assert_ok!(redeem(3, DOT, PUSD, 1_011, 0, 4, 0));

		// The redemption touches the target and advances its interest clock to the current time.
		// Cancellation of 1_005 debt applies to the accrued balance, not the opening principal.
		assert_eq!(vault_interest_time(1), branch_interest_time(Timestamp::get()));
		assert!(vault_interest_time(1) > stamped_at_open);
		assert_eq!(vault_debt(DOT, PUSD, 1), accrued - 1_005);
		// The fill cancels 1_005 debt and pays `floor(1_005 / 1.25) = 804` collateral, plus the
		// fee. The denominator includes one year of aggregate interest without a market touch.
		//
		// Thus, the increase is approximately `1_005 / 1_009_583 / 2 = 0.0498%`.
		//
		// Its mean is half that value, and the charge is `ceil(1_005 * 0.005249) = 6`. The total
		// cost is 1_011.
		assert_eq!(last_settlement().figures(), (1_005, 6, 804));
	});
}

fn liquidate_redistribute_all(owner: AccountId) {
	Vaults::liquidate(
		RuntimeOrigin::signed(99),
		DOT,
		PUSD,
		owner,
		pallet_vaults::JitTerms { max_stable: 0, min_collateral_out: 0 },
	)
	.expect("liquidation succeeds");
}

// Debt-free husks stay redistribution-eligible, so a full wipe never leaves a sole stake bearer
// to absorb sub-resolution residue: the complement stays pending until only one stake bearer
// remains.
#[test]
fn full_wipe_leaves_the_pending_complement_with_the_husks() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Distinct stakes and rates create allocation residue.
		assert_ok!(open(1, DOT, PUSD, 1_100, 300, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_700, 450, rate_pct(2, 100)));
		assert_ok!(open(3, DOT, PUSD, 2_300, 600, rate_pct(3, 100)));
		mint_stable(PUSD, 5, 1_000_000);

		// Two redistributions leave two debt units and two collateral units pending.
		for sacrificial in [9, 10] {
			assert_ok!(open(sacrificial, DOT, PUSD, 480, 480, rate_pct(4, 100)));
			set_price(DOT, FixedU128::one());
			liquidate_redistribute_all(sacrificial);
			set_price(DOT, FixedU128::from_rational(5u128, 4u128));
			advance_time(30 * 24 * 3_600 * 1_000);
		}

		let quote = preview(100_000, 0).expect("quote");
		assert_eq!(quote.steps, 3);
		assert!(!quote.truncated);

		// The mock checks that execution reproduces the quote exactly.
		assert_ok!(redeem(5, DOT, PUSD, 100_000, 0, 6, 0));

		// The husks own no debt but keep their stake, so the complement stays pending.
		let vault =
			|owner| pallet_vaults::pallet::Vaults::<Test>::get((DOT, PUSD, owner)).unwrap().vault;
		let state = || pallet_vaults::pallet::Branches::<Test>::get(DOT, PUSD).unwrap().state;
		assert_eq!(state().debt.principal, 0);
		assert_eq!(state().debt.pending_redistribution_principal, 2);
		assert_eq!(state().pending_redistribution_collateral, 2);
		assert_eq!(
			state().stakes.total,
			vault(1).redistribution_stake +
				vault(2).redistribution_stake +
				vault(3).redistribution_stake
		);

		// Once the other husks close, the last stake bearer receives the exact complement.
		assert_ok!(Vaults::close_vault(RuntimeOrigin::signed(1), DOT, PUSD, None));
		assert_ok!(Vaults::close_vault(RuntimeOrigin::signed(2), DOT, PUSD, None));
		let collateral_before = vault(3).collateral;
		assert_ok!(Vaults::poke(RuntimeOrigin::signed(5), DOT, PUSD, 3));
		assert_eq!(vault(3).debt.principal, 2);
		assert_eq!(vault(3).collateral, collateral_before + 2);
		assert_eq!(state().debt.pending_redistribution_principal, 0);
		assert_eq!(state().pending_redistribution_collateral, 0);
	});
}

#[test]
fn ordinary_redemption_at_six_decimals_scales_exactly() {
	build_and_execute(|| {
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(open(1, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		let debt_before = vault_debt(DOT, USDX, 1);
		mint_stable(USDX, 3, 1_000 * USDX_UNIT);
		let held_before = held(DOT, 1);

		assert_ok!(redeem(3, DOT, USDX, 222_186_162, 0, 4, 0));

		// The spend buys exactly 201 USDX of debt at six decimals, plus the fee calculated below.
		// The collateral value is `201 / 1.25 = 160.8` USDX.
		//
		// Both values are exact and have no dust.
		assert_eq!(debt_before - vault_debt(DOT, USDX, 1), 201 * USDX_UNIT);
		assert_eq!(held_before - held(DOT, 1), 160_800_000);
		// The fee is the one figure that cannot be round: the coin carries 500_479_124 (500 USDX
		// plus a 479_124 upfront fee), so 201 USDX is 40.16% of the stablecoin debt. The dynamic
		// fee increases by half of that share, to 20.08%, with a mean of 10.04%.
		//
		// The fee is `ceil(201_000_000 * 0.1054037883) = 21_186_162`. Thus, the total cost is
		// 222_186_162.
		assert_eq!(last_settlement().figures(), (201 * USDX_UNIT, 21_186_162, 160_800_000));
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
		&DOT, payment, &recipient,
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
		// At 0.54 both queued vaults sit at CR ≈ 107.8%: inside the recovery-bonus
		// regime offsets are restricted to, and below the 110% exit gate.
		set_price(DOT, FixedU128::from_rational(54u128, 100u128));
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
		assert_ok!(preview_offset(10_000), RecoveryOffsetQuote::Available { debt: debt1 });
		// CR = 540/501 ≈ 107.8% caps the bonus at the 5% redistribution
		// penalty: collateral = floor(floor(501 · 1.05) / 0.54) = 974.
		assert_ok!(
			execute_offset(3, 4, 10_000),
			RecoveryOffsetResult::Applied { collateral_out: 974 }
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
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 974);
		assert_eq!(Assets::balance(PUSD, FEE_DEST), fee_before);
		assert_eq!(issuance_before - Assets::total_supply(PUSD), debt1);
	});
}

#[test]
fn recovery_offset_partial_fill_keeps_head_queued() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		set_price(DOT, FixedU128::from_rational(54u128, 100u128));
		let debt_before = vault_debt(DOT, PUSD, 1);
		mint_stable(PUSD, 3, 10_000);
		let payer_before = Assets::balance(PUSD, 3);
		let recipient_before = collateral_balance(DOT, 4);

		// A 200 cap partially fills the 501-debt head; quote and execution
		// agree on the capped size.
		assert_ok!(preview_offset(200), RecoveryOffsetQuote::Available { debt: 200 });
		// 5%-capped bonus: floor(floor(200 · 1.05) / 0.54) = 388.
		assert_ok!(
			execute_offset(3, 4, 200),
			RecoveryOffsetResult::Applied { collateral_out: 388 }
		);

		// The partially settled head keeps its place at the FIFO front.
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before - 200);
		assert!(Vaults::vault_status(DOT, PUSD, 1).expect("vault 1").is_final_recovery());
		assert_eq!(Vaults::final_recovery_queue(DOT, PUSD, 10), vec![1u64]);
		assert_eq!(payer_before - Assets::balance(PUSD, 3), 200);
		assert_eq!(collateral_balance(DOT, 4) - recipient_before, 388);
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
		set_price(DOT, FixedU128::from_rational(54u128, 100u128));
		mint_stable(PUSD, 3, 1_000);

		// A zero budget withdraws nothing, so both paths are pure storage no-ops.
		assert_storage_noop!({
			assert_ok!(preview_offset(0), RecoveryOffsetQuote::NoTarget);
		});
		assert_storage_noop!({
			assert_ok!(execute_offset(3, 4, 0), RecoveryOffsetResult::NoTarget);
		});
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

		assert_storage_noop!({
			assert_ok!(preview_offset(10_000), RecoveryOffsetQuote::BelowPar);
		});
		// Execution withdraws and refunds the payment, which leaves asset events behind, so
		// the refusal is checked field by field rather than by storage root.
		assert_ok!(execute_offset(3, 4, 10_000), RecoveryOffsetResult::BelowPar);
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
		// config lookup could report `StablecoinNotRegistered`.
		assert_ok!(preview_offset(1_000), RecoveryOffsetQuote::NoTarget);
		assert_ok!(execute_offset(3, 4, 1_000), RecoveryOffsetResult::NoTarget);

		// An ordinary vault is not an offset target either: offsets exist
		// only for the FinalRecovery FIFO head.
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		let debt_before = vault_debt(DOT, PUSD, 1);
		mint_stable(PUSD, 3, 1_000);

		assert_ok!(preview_offset(1_000), RecoveryOffsetQuote::NoTarget);
		assert_ok!(execute_offset(3, 4, 1_000), RecoveryOffsetResult::NoTarget);
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before);
	});
}

#[test]
fn recovery_offset_underfunded_payment_partially_fills() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		set_price(DOT, FixedU128::from_rational(54u128, 100u128));
		let debt_before = vault_debt(DOT, PUSD, 1);
		// The payer holds less than the 501 an uncapped offset could cancel.
		// The payment credit is the budget, so the offset fills exactly what
		// it carries instead of failing for the missing remainder.
		mint_stable(PUSD, 3, 100);

		// 5%-capped bonus: floor(floor(100 · 1.05) / 0.54) = 194.
		assert_ok!(
			execute_offset(3, 4, 10_000),
			RecoveryOffsetResult::Applied { collateral_out: 194 }
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
fn recovery_offset_payment_asset_selects_the_market() {
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
		// Both heads settle at 0.54, inside the bonus regime and below the exit gate.
		set_price(DOT, FixedU128::from_rational(54u128, 100u128));

		let pusd_debt_before = vault_debt(DOT, PUSD, 1);
		let usdx_debt_before = vault_debt(DOT, USDX, 2);
		mint_stable(USDX, 3, 100 * USDX_UNIT);
		let issuance_before = Assets::total_supply(USDX);

		// The payment's own asset names the market: a USDX payment against
		// `DOT` collateral settles the (DOT, USDX) head, and a coin mismatch
		// with some other named market is unrepresentable.
		let result: Result<RecoveryOffsetResult<Balance>, DispatchError> =
			frame::deps::frame_support::storage::with_storage_layer(|| {
				let payment = <Assets as FungiblesBalanced<AccountId>>::withdraw(
					USDX,
					&3,
					100 * USDX_UNIT,
					Precision::Exact,
					Preservation::Expendable,
					Fortitude::Polite,
				)?;
				let (result, change) =
					<Redemptions as RecoveryOffsetInterface>::execute_recovery_offset(
						&DOT, payment, &4,
					)?;
				change
					.drop_zero()
					.map_err(|_| DispatchError::Other("full budget must be consumed"))?;
				Ok(result)
			});
		assert!(matches!(result, Ok(RecoveryOffsetResult::Applied { .. })));

		// The USDX head absorbed the full budget; the PUSD market never moved.
		assert_eq!(vault_debt(DOT, USDX, 2), usdx_debt_before - 100 * USDX_UNIT);
		assert_eq!(vault_debt(DOT, PUSD, 1), pusd_debt_before);
		assert_eq!(Assets::balance(USDX, 3), 0);
		assert_eq!(issuance_before - Assets::total_supply(USDX), 100 * USDX_UNIT);
	});
}

/// Offsets bypass the `redeem` mock, so this checks by hand what it checks for
/// recovery redemptions: the offset surface stays silent at the redemptions
/// layer (no fee movement, no events).
#[test]
fn recovery_offset_leaves_dynamic_fee_untouched() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(52u128, 100u128));
		set_price(DOT, FixedU128::from_rational(54u128, 100u128));
		set_dynamic_fee(PUSD, FixedU128::from_rational(3u128, 100u128));
		let state_before = crate::RedemptionStates::<Test>::get(PUSD);
		mint_stable(PUSD, 3, 1_000);
		System::reset_events();

		assert_ok!(
			execute_offset(3, 4, 200),
			RecoveryOffsetResult::Applied { collateral_out: 388 }
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

/// At six decimals, a partial below-par step floors raw units, never coins, and
/// the full settlement pays every raw unit that is left.
#[test]
fn insurance_adjusted_flooring_at_six_decimals_costs_raw_units_only_on_partial_steps() {
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
		mint_stable(USDX, 3, 1_000 * USDX_UNIT);

		// C = 400e6 against D = 500_479_124 with 50e6 of cover leaves market
		// debt 450_479_124. A partial 225e6 buys its share of the 1e9 units:
		// floor(225e6 · 1e9 / 450_479_124) = floor(499_468_206.6) = 499_468_206.
		assert_ok!(redeem(3, DOT, USDX, 225 * USDX_UNIT, 0, 4, 0));
		let partial = last_settlement();
		assert_eq!(partial.figures(), (225 * USDX_UNIT, 0, 499_468_206));
		assert_eq!(partial.insurance_cover, 0, "fund untouched");
		assert_eq!(vault_debt(DOT, USDX, 1), debt - 225 * USDX_UNIT);
		assert_eq!(held(DOT, 1), 500_531_794);

		// The 500_531_794 units left are valued at 200_212_718 against 275_479_124
		// of debt; the fund covers 50e6 of the shortfall, so the market side
		// cancels 225_479_124. Supplying it is the full settlement: it pays all
		// 500_531_794 units and closes the row.
		assert_ok!(redeem(3, DOT, USDX, 1_000 * USDX_UNIT, 0, 4, 0));
		let full = last_settlement();
		assert_eq!(full.figures(), (225_479_124, 0, 500_531_794));
		assert_eq!(full.insurance_cover, 50 * USDX_UNIT);
		assert_eq!(Vaults::vault_status(DOT, USDX, 1), None, "vault settled and closed");
		assert_eq!(held(DOT, 1), 0);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
	});
}

/// The default config with every fee component zeroed.
fn zero_fee_redemption_config() -> RedemptionConfig<Balance> {
	RedemptionConfig {
		dynamic_fee_floor: FixedU128::zero(),
		dynamic_fee_ceiling: FixedU128::zero(),
		base_fee: Permill::zero(),
		fee_ceiling: Permill::zero(),
		..default_redemption_config()
	}
}

/// Verifies that an ordinary redemption spends at most what keeps the redeemer at or above
/// the stablecoin's minimum balance, whatever the request.
///
/// The fee is the last debit and the walk may cancel less than planned. Only a plan that
/// keeps the account alive is certain to be payable in full: every step and the fee are then
/// preserving debits, so no request can strand a sub-minimum remainder that the last debit
/// could not take.
///
/// The redeemer holds 300 USDX with a 10_000-raw-unit minimum balance, so the payable limit is
/// 299_990_000. It buys 263_885_993 debt: `263_885_993 / 500_479_124 / 2` raises the dynamic fee
/// by 26.36%, its mean is 13.18%, and with the 0.5% base the fee is
/// `ceil(263_885_993 · 0.1368166834) = 36_104_007`, exactly the limit in total. The collateral
/// is `floor(263_885_993 / 1.25) = 211_108_794`.
///
/// A request that would leave less than the minimum, the whole balance, and more than the
/// balance all fill to this same limit, and the redeemer keeps exactly the minimum.
#[test]
fn ordinary_redemption_keeps_the_redeemer_at_or_above_the_minimum_balance() {
	build_and_execute(|| {
		let balance = 300 * USDX_UNIT;
		let payable = balance - USDX_MIN_BALANCE;
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(open(1, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		mint_stable(USDX, 3, balance);
		let debt_before = vault_debt(DOT, USDX, 1);

		let quote = preview_redeem(DOT, USDX, payable, 0).expect("quote");
		assert_eq!(quote.debt_cancelled, 263_885_993);
		assert_eq!(quote.fee, 36_104_007);
		assert_eq!(quote.stable_in(), payable);
		assert_eq!(quote.collateral_out, 211_108_794);

		// Every request runs against the same fixture and is rolled back afterwards.
		for request in [payable + 1, balance - 4_000, balance, balance + 1, Balance::MAX] {
			hypothetically!({
				// The quote does not know the balance, so it prices the request itself, and the
				// request buys more than the account can pay while it keeps its minimum balance.
				let requested = preview_redeem(DOT, USDX, request, 0).expect("quote");
				assert!(requested.stable_in() > payable, "request {request} breaches the minimum");

				assert_ok!(redeem(3, DOT, USDX, request, 0, 4, 0));

				assert_eq!(
					last_settlement().figures(),
					(quote.debt_cancelled, quote.fee, quote.collateral_out),
					"request {request}"
				);
				assert_eq!(debt_before - vault_debt(DOT, USDX, 1), quote.debt_cancelled);
				assert_eq!(Assets::balance(USDX, 3), USDX_MIN_BALANCE, "request {request}");
			});
		}
	});
}

/// Verifies that a fee-free policy follows the same rule.
///
/// Without a fee the debt is the whole spend, but the account still keeps its minimum
/// balance: the limit comes from the plan, not from the fee.
#[test]
fn fee_free_redemption_also_keeps_the_minimum_balance() {
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

		let burned = 300 * USDX_UNIT - USDX_MIN_BALANCE;
		assert_ok!(redeem(3, DOT, USDX, 300 * USDX_UNIT, 0, 4, 0));

		assert_eq!(Assets::balance(USDX, 3), USDX_MIN_BALANCE);
		assert_eq!(debt_before - vault_debt(DOT, USDX, 1), burned);
		let settled = last_settlement();
		assert_eq!((settled.stable_burned, settled.fee), (burned, 0));
	});
}

/// Verifies that a recovery redemption may spend the whole balance.
///
/// Recovery charges no fee, so its single debit is the whole spend and can consume the
/// account. A spend that would strand a sub-minimum remainder is repriced to the amount that
/// keeps the minimum balance instead.
#[test]
fn recovery_redemption_may_consume_the_redeemer_account() {
	build_and_execute(|| {
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(open(1, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(52u128, 100u128));
		assert_ok!(enter_final_recovery(DOT, USDX, 1));
		let debt_before = vault_debt(DOT, USDX, 1);

		let request = 200 * USDX_UNIT;
		for (balance, burned) in [
			// The request is the whole balance and drains the account.
			(request, request),
			// The request would leave 4_000 raw units, less than the 10_000-unit minimum, so the
			// spend stops at the minimum instead.
			(request + 4_000, request - 6_000),
		] {
			hypothetically!({
				mint_stable(USDX, 3, balance);

				assert_ok!(redeem(3, DOT, USDX, request, 0, 4, 0));

				assert_eq!(debt_before - vault_debt(DOT, USDX, 1), burned);
				assert_eq!(Assets::balance(USDX, 3), balance - burned);
				assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
			});
		}
	});
}

/// A recovery spend the balance cannot fund at all reports the balance, not the queue: at
/// exactly the minimum balance, a smaller request can neither preserve nor consume the account.
#[test]
fn recovery_redemption_at_the_minimum_balance_reports_insufficient_balance() {
	build_and_execute(|| {
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(open(1, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(52u128, 100u128));
		assert_ok!(enter_final_recovery(DOT, USDX, 1));
		mint_stable(USDX, 3, USDX_MIN_BALANCE);
		let request = USDX_MIN_BALANCE / 2;

		let quote = preview_redeem(DOT, USDX, request, 0).expect("recovery quote");
		assert_eq!(quote.debt_cancelled, request);
		assert_noop!(
			redeem(3, DOT, USDX, request, 0, 4, 0),
			Error::<Test>::InsufficientStableBalance
		);
	});
}

/// Raw units give an exact representation of the fractional token amounts in the examples.
const UNIT: Balance = 1_000;

/// Builds a branch without an upfront fee, so a vault's principal equals its debt.
fn example_config() -> pallet_vaults::BranchConfig<Balance> {
	pallet_vaults::BranchConfig { upfront_fee_period: 0, ..default_branch_config() }
}

/// Verifies that an ordinary redemption prices its dynamic fee against stablecoin-wide debt.
///
/// The redemption increases the dynamic fee and pays the mean rate during that increase. It does
/// not use only the target vault's debt.
#[test]
fn ordinary_redemption_prices_its_fee_against_the_stablecoin_wide_debt() {
	build_and_execute(|| {
		// Without an upfront fee, each vault's principal equals its debt. Thus, the stablecoin debt
		// is exactly 100_000.
		register_branch(DOT, PUSD, example_config());
		set_price(DOT, FixedU128::from_rational(2, 1));

		// The target has 5_000 debt and 4_000 DOT, with a value of 8_000 pUSD and a 160% CR. It
		// uses the floor borrow rate, so the walk reaches it first.
		assert_ok!(open(1, DOT, PUSD, 4_000, 5_000, rate_pct(1, 1_000)));
		// The rest of the coin's 100_000 of debt, on a costlier vault so it is
		// never the redemption target.
		assert_ok!(open(2, DOT, PUSD, 200_000, 95_000, rate_pct(5, 100)));
		assert_eq!(stablecoin_debt(PUSD), 100_000);

		// The setup sets the decayed dynamic fee at which the redemption starts.
		set_dynamic_fee(PUSD, FixedU128::from_rational(15, 1_000));

		mint_stable(PUSD, 3, 1_000_000);

		assert_ok!(redeem(3, DOT, PUSD, 1_023, 0, 4, 0));

		// The 1_023 spent buys 1_000 debt. This is `1_000 / 100_000 = 1%` of the stablecoin debt,
		// not 20% of the target-vault debt.
		//
		// The dynamic fee increases by `1% / 2 = 0.5%` to 2.0%. The redemption pays the mean
		// dynamic fee of `1.5% + 0.25%` and the 0.5% base fee.
		//
		// Therefore, the fee is `ceil(1_000 * 0.0225) = 23`, and the total cost is 1_023. The
		// collateral output is `1_000 / 2 = 500` DOT.
		let dynamic_fee = crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee;
		assert_eq!(dynamic_fee, fee_curve(rate_pct(15, 1_000), 100_000).raised_dynamic_fee(1_000));
		assert_eq!(dynamic_fee, rate_pct(2, 100));
		assert_eq!(last_settlement().figures(), (1_000, 23, 500));

		// The vault after redemption: 4_000 of debt against 3_500 DOT =
		// 7_000 pUSD, a 175% CR.
		assert_eq!(vault_debt(DOT, PUSD, 1), 4_000);
		assert_eq!(held(DOT, 1), 3_500);
	});
}

// The next three examples do not test the redemption fee. The ordinary example checks only the
// vault result, and recovery settlements have no fee.

/// Verifies that a vault below `minimum_debt` becomes the Dormant continuation target.
///
/// The redemption does not close the vault.
#[test]
fn redemption_below_minimum_debt_leaves_a_dormant_continuation_vault() {
	build_and_execute(|| {
		let config = pallet_vaults::BranchConfig { minimum_debt: 2_000 * UNIT, ..example_config() };
		register_branch(DOT, PUSD, config);
		set_price(DOT, FixedU128::from_rational(2, 1));

		// 3_000 of debt against 2_000 DOT.
		assert_ok!(open(1, DOT, PUSD, 2_000 * UNIT, 3_000 * UNIT, rate_pct(1, 1_000)));
		mint_stable(PUSD, 3, 1_000_000 * UNIT);

		// The test spends the amount that cancels exactly 2_800 debt. It does not check the fee.
		assert_ok!(redeem(3, DOT, PUSD, spend_for_debt(2_800 * UNIT), 0, 4, 0));

		// 2_800 cancelled pays 2_800/2 = 1_400 DOT, leaving 200 of debt against
		// 600 DOT — below the 2_000 minimum, so the vault goes Dormant and
		// becomes the branch's continuation target.
		assert_eq!(last_settlement().collateral_out, 1_400 * UNIT);
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

/// Verifies a `FinalRecovery` head with `CR >= 100%`.
///
/// The redemption uses face value plus a bonus that the redistribution penalty limits.
#[test]
fn final_recovery_redemption_above_par_pays_the_capped_bonus() {
	build_and_execute(|| {
		let mut config = example_config();
		// The vault will sit at CR 120%, so the MCR must exceed it.
		config.minimum_collateralization_ratio = FixedU128::from_rational(130u128, 100u128);
		config.initial_collateralization_ratio = FixedU128::from_rational(140u128, 100u128);
		config.safety_collateralization_ratio = FixedU128::from_rational(150u128, 100u128);
		config.liquidation.redistribution_penalty = Permill::from_percent(10);
		register_branch(DOT, PUSD, config);

		// The test opens above the 140% ICR and then sets 1 DOT = 2 pUSD. At this price, 6_000 DOT
		// has a value of 12_000 pUSD against 10_000 debt, which gives a 120% CR.
		set_price(DOT, FixedU128::from_rational(4, 1));
		assert_ok!(open(1, DOT, PUSD, 6_000 * UNIT, 10_000 * UNIT, rate_pct(1, 1_000)));
		set_price(DOT, FixedU128::from_rational(2, 1));
		assert_ok!(enter_final_recovery(DOT, PUSD, 1));

		mint_stable(PUSD, 3, 1_000_000 * UNIT);

		assert_ok!(redeem(3, DOT, PUSD, 2_000 * UNIT, 0, 4, 0));

		// raw_bonus = 120% - 100% - 1% = 19%, capped by the 10% penalty. So
		// 2_000 * 1.10 = 2_200 pUSD of value, or 1_100 DOT. Recovery redemptions are fee-free.
		assert_eq!(last_settlement().figures(), (2_000 * UNIT, 0, 1_100 * UNIT));
		assert_eq!(vault_debt(DOT, PUSD, 1), 8_000 * UNIT);
		assert_eq!(held(DOT, 1), 4_900 * UNIT);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
	});
}

/// Verifies a `FinalRecovery` head below par with partial Insurance Fund cover.
///
/// The cover increases the market-side redemption rate. After cancellation of all market-side
/// debt, the final settlement includes the Insurance Fund cover.
#[test]
fn final_recovery_redemption_below_par_settles_with_insurance_cover() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, example_config());

		// The test opens a healthy vault and then sets 1 DOT = 2 pUSD. Thus, 4_000 DOT has a value
		// of 8_000 pUSD against 10_000 debt, which gives an 80% CR.
		set_price(DOT, FixedU128::from_rational(4, 1));
		assert_ok!(open(1, DOT, PUSD, 4_000 * UNIT, 10_000 * UNIT, rate_pct(1, 1_000)));
		set_price(DOT, FixedU128::from_rational(2, 1));
		assert_ok!(enter_final_recovery(DOT, PUSD, 1));
		mint_stable(PUSD, insurance_account(PUSD), 1_000 * UNIT);

		mint_stable(PUSD, 3, 1_000_000 * UNIT);

		// The shortfall is `10_000 - 8_000 = 2_000`. The cover is
		// `min(1_000, 2_000) = 1_000`.
		//
		// Thus, `market_cancel_debt = 9_000`, an effective rate of `8_000 / 9_000 = 0.888…`.
		// A burn of 3_000 buys `3_000 / 9_000` of the 4_000 DOT: 1_333.33… DOT, which the
		// pallet rounds down to 1_333.333.
		assert_ok!(redeem(3, DOT, PUSD, 3_000 * UNIT, 0, 4, 0));
		let partial = last_settlement();
		assert_eq!(partial.figures(), (3_000 * UNIT, 0, 1_333_333));
		assert_eq!(partial.insurance_cover, 0);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));

		// Settlement of the other market-side debt pays the rest of the 4_000 DOT. The same
		// settlement includes the Insurance Fund cover of 1_000.
		assert_ok!(redeem(3, DOT, PUSD, 6_000 * UNIT, 0, 4, 0));
		let full = last_settlement();
		assert_eq!(full.figures(), (6_000 * UNIT, 0, 4_000 * UNIT - 1_333_333));
		assert_eq!(full.insurance_cover, 1_000 * UNIT);
		// The settlement leaves neither debt nor collateral, so the vault closes and the
		// branch empties.
		assert_eq!(Vaults::vault_status(DOT, PUSD, 1), None, "vault fully settled and closed");
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
		assert_eq!(held(DOT, 1), 0);
		assert_eq!(
			pallet_vaults::Branches::<Test>::get(DOT, PUSD)
				.expect("branch")
				.state
				.vault_count,
			0
		);
	});
}
