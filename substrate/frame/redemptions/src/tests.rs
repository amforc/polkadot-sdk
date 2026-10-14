use crate::{
	mock::*,
	types::{RecoveryRegime, RedemptionConfig},
	Error, Event,
};
use pallet_vaults::LiquidationSettlement;
use pusd_primitives::{
	collateralization_ratio, reducible_debit, CollateralRatio, DebtCollateral,
	RecoveryOffsetInterface, RecoveryOffsetResult, VaultInterface,
};

const HOUR_MS: Moment = 3_600 * 1_000;
const ONE_YEAR_MS: Moment = 31_557_600_000;

/// Parks a [`setup_final_recovery`] vault of 1_000 collateral and 501 debt at
/// `CR = 520 / 501 ≈ 103.79%`: in `FinalRecovery`, barely above par.
const RECOVERY_PRICE_ABOVE_PAR: FixedU128 = FixedU128::from_rational(52, 100);

/// Debt that leaves a fresh 501-debt vault a 141 residual, below the branch's 200 minimum debt,
/// so the vault turns Dormant.
const DORMANT_FILL_DEBT: Balance = 360;

/// Bound on the `FinalRecovery` queue that [`recovery_queue`] reads; no fixture queues more.
const RECOVERY_QUEUE_LEN_MAX: u32 = 10;

fn rate_pct(num: u128, denom: u128) -> FixedU128 {
	FixedU128::from_rational(num, denom)
}

/// The PUSD spend, debt plus fee, that buys exactly `debt` in an ordinary redemption now.
fn spend_for_debt(debt: Balance) -> Balance {
	let config = crate::RedemptionConfigs::<Test>::get(PUSD).expect("PUSD is registered");
	let decayed =
		crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee_at(Timestamp::get(), &config);
	let curve = crate::fees::DynamicFeeCurve::try_new(decayed, stablecoin_debt(PUSD), &config)
		.expect("test debt fits u128");
	debt + curve.fee(debt)
}

/// [`redeem`], rolled back: what a client's dry run reports, since the pallet has no quote API.
fn dry_redeem(options: RedeemOptions) -> Result<Settlement, DispatchError> {
	hypothetically!(redeem(options))
}

/// Branch TCR as the vault pallet reports it, including pending interest.
fn branch_tcr(collateral: AssetId, stable: StableId) -> CollateralRatio {
	Vaults::branch_tcr(collateral, stable).expect("branch registered")
}

/// The fully accrued debt of `who`, read through Vaults' projected snapshot.
fn projected_full_debt(who: AccountId) -> Balance {
	Vaults::project_redemption_snapshot(&DOT, &PUSD, &who).expect("snapshot").debt
}

/// The regime of the latest recovery settlement.
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

/// The first market on a coin seeds its redemption config and fee state, and only the last
/// market to leave clears them, so no deregistration wipes state its siblings still price with.
#[test]
fn config_lives_until_the_last_market_leaves() {
	build_and_execute(|| {
		assert!(crate::RedemptionConfigs::<Test>::get(PUSD).is_none());
		register_branch(DOT, PUSD, default_branch_config());
		let cfg = crate::RedemptionConfigs::<Test>::get(PUSD).expect("seeded on registration");
		assert_eq!(cfg, default_redemption_config());
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

/// Only the coin's first market carries the per-coin redemption config. Omitting it there or
/// restating it later fails, so no later market claims a policy it does not own.
#[test]
fn config_comes_only_with_the_first_market() {
	build_and_execute(|| {
		set_price(DOT, DEFAULT_PRICE);
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
		let mut invalid = default_redemption_config();
		invalid.minimum_redemption_amount = 0;
		assert_noop!(
			Vaults::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				Some(invalid),
			),
			Error::<Test>::InvalidRedemptionConfig
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

/// The terms are held to the minimum before the queue is read.
///
/// An ordinary target alone cannot show this: its own debt check reports the same error. An
/// empty queue would otherwise report `NoRedeemableVault`, and the fee-free recovery path, which
/// has no other minimum, would settle one unit below it.
#[test]
fn redeem_rejects_below_minimum() {
	build_and_execute(|| {
		let minimum = default_redemption_config().minimum_redemption_amount;
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, REDEEMER, 1_000);
		assert_noop!(
			redeem(RedeemOptions::spend(minimum - 1)),
			Error::<Test>::BelowMinimumRedemptionAmount
		);

		hypothetically!({
			assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
			assert_noop!(
				redeem(RedeemOptions::spend(minimum - 1)),
				Error::<Test>::BelowMinimumRedemptionAmount
			);
		});

		setup_final_recovery(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR);
		assert_noop!(
			redeem(RedeemOptions::spend(minimum - 1)),
			Error::<Test>::BelowMinimumRedemptionAmount
		);
		// Exactly the 100 minimum settles. CR = 520/501 ≈ 103.79% gives a bonus of 2.79%:
		// floor(floor(100 · 1.0279) / 0.52) = floor(102 / 0.52) = 196.
		let settled = redeem(RedeemOptions::spend(minimum)).expect("redemption succeeds");
		assert_eq!(settled.figures(), (minimum, 0, 196));
	});
}

#[test]
fn redeem_rejects_unregistered_coin() {
	build_and_execute(|| {
		mint_stable(PUSD, REDEEMER, 1_000);
		assert_noop!(redeem(RedeemOptions::spend(200)), Error::<Test>::StablecoinNotRegistered);
	});
}

#[test]
fn redeem_rejects_frozen_branch() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, REDEEMER, 1_000);
		assert_ok!(Vaults::set_governance_frozen(RuntimeOrigin::signed(ADMIN), DOT, PUSD, true));
		// Frozen-mode enforcement lives vault-side: projecting the first target's snapshot
		// rejects the frozen branch before any settlement, so nothing needs rolling back.
		assert_noop!(redeem(RedeemOptions::spend(200)), pallet_vaults::Error::<Test>::BranchFrozen);
	});
}

#[test]
fn redeem_rejects_empty_queue() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, REDEEMER, 1_000);
		assert_noop!(redeem(RedeemOptions::spend(200)), Error::<Test>::NoRedeemableVault);
	});
}

#[test]
fn ordinary_redeems_lowest_rate_vault_first() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		mint_stable(PUSD, REDEEMER, 1_000_000);
		let v1_before = vault_debt(DOT, PUSD, 1);
		let v1_held_before = held(DOT, 1);
		let v2_before = vault_debt(DOT, PUSD, 2);
		let v2_held_before = held(DOT, 2);
		let redeemer_before = Assets::balance(PUSD, REDEEMER);
		let fee_dest_before = Assets::balance(PUSD, FEE_DEST);
		let recipient_before = collateral_balance(DOT, RECIPIENT);
		let supply_before = Assets::total_supply(PUSD);

		// Each vault owes 501, 500 plus a one-unit upfront fee, so the coin's debt is 1_002.
		// Cancelling 201, a 20.06% share, lifts the dynamic fee by half that share to 10.03%, and
		// the fee charges the 5.01% mean plus the 0.5% base: `ceil(201 · 0.05515) = 12`. So 213
		// buys 201 debt, 212 only 200, and 201 debt pays `floor(201 / 1.25) = 160` collateral.
		let settled = redeem(RedeemOptions::spend(213)).expect("redemption succeeds");

		// The lowest-rate vault absorbs the whole fill, in debt and in held
		// collateral; the higher-rate vault is untouched in both.
		assert_eq!(v1_before - vault_debt(DOT, PUSD, 1), 201);
		assert_eq!(v1_held_before - held(DOT, 1), 160);
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
		assert_eq!(held(DOT, 2), v2_held_before);
		assert_eq!(settled.figures(), (201, 12, 160));
		assert_eq!(settled.ordinary_steps, Some(1));
		// The redeemer pays the debt and the fee, the fee reaches `FEE_DEST`, only the debt is
		// burned, and the recipient receives the collateral.
		assert_eq!(redeemer_before - Assets::balance(PUSD, REDEEMER), 213);
		assert_eq!(Assets::balance(PUSD, FEE_DEST) - fee_dest_before, 12);
		assert_eq!(supply_before - Assets::total_supply(PUSD), 201);
		assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 160);
	});
}

#[test]
fn ordinary_walk_stops_at_step_cap_or_barrier() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		// Vault 2 has a much higher collateral ratio than vault 1. Thus, a price can put vault 1
		// below par while vault 2 stays eligible for redemption.
		assert_ok!(open(2, DOT, PUSD, 10_000, 500, rate_pct(2, 100)));
		mint_stable(PUSD, REDEEMER, 1_000_000);

		// A one-step cap ends the walk with budget and a second target to spare. A caller sees
		// it as a walk that used every step and spent less than its budget.
		let capped = dry_redeem(RedeemOptions { max_steps: 1, ..RedeemOptions::spend(100_000) })
			.expect("vault 1 is redeemable");
		assert_eq!(capped.ordinary_steps, Some(1));
		assert_eq!(capped.stable_burned, projected_full_debt(1));
		assert!(capped.stable_burned + capped.fee < 100_000);

		// Budget exhaustion inside the cap is a complete walk: it spends the whole budget.
		let filled = dry_redeem(RedeemOptions::spend(200)).expect("vault 1 is redeemable");
		assert_eq!(filled.ordinary_steps, Some(1));
		assert_eq!(filled.stable_burned + filled.fee, 200);

		// Park a Dormant husk at the priority slot and sink the price: the
		// underwater Dormant head is a barrier that ends the walk below the cap.
		assert_ok!(redeem(RedeemOptions::spend(spend_for_debt(DORMANT_FILL_DEBT))));
		assert!(vault_status(DOT, PUSD, 1).expect("vault 1").is_dormant());
		set_price(DOT, FixedU128::from_rational(15, 100));
		assert!(
			vault_cr(2, FixedU128::from_rational(15, 100)) > FixedU128::one(),
			"fixture must keep vault 2 redeemable behind the barrier"
		);
		assert_noop!(redeem(RedeemOptions::spend(100_000)), Error::<Test>::NoRedeemableVault);

		// The barrier is not permanent: liquidating the husk clears the priority slot, and the
		// walk reaches the healthy vault behind it.
		liquidate_redistribute_all(1);
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_none(), "husk gone");
		let unblocked = dry_redeem(RedeemOptions::spend(100_000)).expect("vault 2 is redeemable");
		assert_eq!(unblocked.ordinary_steps, Some(1));
		assert_eq!(unblocked.stable_burned, projected_full_debt(2));
	});
}

#[test]
fn ordinary_walk_skips_underwater_vaults() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Open the well-collateralized vaults first so the thin low-rate vaults
		// don't briefly push the fresh branch into Safety mode at the genesis price.
		assert_ok!(open(3, DOT, PUSD, 3_000, 240, rate_pct(3, 100)));
		assert_ok!(open(4, DOT, PUSD, 3_000, 240, rate_pct(4, 100)));
		assert_ok!(open(1, DOT, PUSD, 250, 240, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 260, 240, rate_pct(2, 100)));
		set_price(DOT, FixedU128::from_rational(9, 10));
		mint_stable(PUSD, OTHER_REDEEMER, 1_000_000);
		let v1_before = vault_debt(DOT, PUSD, 1);
		let v1_held_before = held(DOT, 1);
		let v2_before = vault_debt(DOT, PUSD, 2);
		let v2_held_before = held(DOT, 2);
		let v3_before = vault_debt(DOT, PUSD, 3);
		let v4_before = vault_debt(DOT, PUSD, 4);
		let redeemer_before = Assets::balance(PUSD, OTHER_REDEEMER);
		let supply_before = Assets::total_supply(PUSD);
		let settled = redeem(RedeemOptions {
			who: OTHER_REDEEMER,
			recipient: OTHER_RECIPIENT,
			..RedeemOptions::spend(2_000)
		})
		.expect("redemption succeeds");
		assert_eq!(settled.ordinary_steps, Some(4), "the walk visits the skipped prefix once");
		assert_eq!(settled.stable_burned, v3_before + v4_before);
		// The underwater prefix (vaults 1-2) is skipped: its debt and held collateral are
		// untouched.
		assert_eq!(vault_debt(DOT, PUSD, 1), v1_before);
		assert_eq!(held(DOT, 1), v1_held_before);
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
		assert_eq!(held(DOT, 2), v2_held_before);
		// The healthy vaults behind it drain fully.
		assert_eq!(vault_debt(DOT, PUSD, 3), 0);
		assert_eq!(vault_debt(DOT, PUSD, 4), 0);
		// Collateral is paid at face value, floored per step at price 0.9 (= debt * 10 / 9).
		assert_eq!(settled.collateral_out, v3_before * 10 / 9 + v4_before * 10 / 9);
		// The walk ends well inside the 2_000 budget, so the redeemer is refunded everything
		// but the burned debt and its fee.
		assert!(settled.stable_burned + settled.fee < 2_000);
		assert_eq!(
			redeemer_before - Assets::balance(PUSD, OTHER_REDEEMER),
			settled.stable_burned + settled.fee
		);
		assert_eq!(supply_before - Assets::total_supply(PUSD), settled.stable_burned);
	});
}

/// A partial fill scales the slippage floor by the share of the budget spent, so a floor set for
/// the full budget does not reject a smaller fill.
#[test]
fn slippage_floor_scales_to_partial_fill() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, REDEEMER, 1_000_000);

		// The 1_000 offer fills the whole 501 debt for `floor(501 / 1.25) = 400` collateral.
		// Redeeming the coin's whole debt lifts the dynamic fee by 1/2 to 50%, so the fee is
		// `ceil(501 · (25% + 0.5%)) = 128` and the fill spends 629. The floor scales to
		// `floor(min · 629 / 1_000)`: 638 becomes 401 and fails, 637 becomes 400 and passes.
		let quote = dry_redeem(RedeemOptions::spend(1_000)).expect("quote");
		assert_eq!((quote.stable_burned, quote.fee, quote.collateral_out), (501, 128, 400));
		assert_noop!(
			redeem(RedeemOptions { min_collateral_out: 638, ..RedeemOptions::spend(1_000) }),
			Error::<Test>::SlippageExceeded
		);

		assert_ok!(redeem(RedeemOptions {
			min_collateral_out: 637,
			..RedeemOptions::spend(1_000)
		}));
		assert_eq!(vault_debt(DOT, PUSD, 1), 0);
	});
}

/// A fee that climbs between quote and execution buys less collateral for the same spend, and
/// the slippage floor catches it as it would a price move.
#[test]
fn slippage_floor_catches_fee_climb() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 100_000, 50_000, rate_pct(5, 100)));
		mint_stable(PUSD, REDEEMER, 1_000_000);
		mint_stable(PUSD, OTHER_REDEEMER, 1_000_000);

		let quote = dry_redeem(RedeemOptions::spend(10_000)).expect("quote");

		// Another redeemer goes first and raises the dynamic fee.
		assert_ok!(redeem(RedeemOptions {
			who: OTHER_REDEEMER,
			recipient: OTHER_RECIPIENT,
			..RedeemOptions::spend(10_000)
		}));
		let requoted = dry_redeem(RedeemOptions::spend(10_000)).expect("requote");
		assert!(requoted.fee > quote.fee, "the fee climbed");
		assert!(requoted.stable_burned < quote.stable_burned, "the spend buys less debt");
		assert!(requoted.collateral_out < quote.collateral_out, "and less collateral");

		// The stale quote's floor now reverts; the fresh quote's floor passes.
		assert_noop!(
			redeem(RedeemOptions {
				min_collateral_out: quote.collateral_out,
				..RedeemOptions::spend(10_000)
			}),
			Error::<Test>::SlippageExceeded
		);
		assert_ok!(redeem(RedeemOptions {
			min_collateral_out: requoted.collateral_out,
			..RedeemOptions::spend(10_000)
		}));
	});
}

/// A budget whose fee leaves less than the minimum debt reverts: on the terms when the balance
/// covers the budget, on the balance when it does not.
#[test]
fn redeem_rejects_fee_leaving_debt_below_minimum() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, REDEEMER, 1_000);

		// The 100 minimum is a 19.96% share of the 501 coin debt, so the dynamic fee rises 9.98%
		// and the fee is `ceil(100 · (4.99% + 0.5%)) = 6`: 106 buys exactly 100 debt, 105 only 99.
		let quote = dry_redeem(RedeemOptions::spend(106)).expect("quote");
		assert_eq!((quote.stable_burned, quote.fee), (100, 6));
		assert_noop!(
			redeem(RedeemOptions::spend(105)),
			Error::<Test>::BelowMinimumRedemptionAmount
		);

		// With a balance short of the budget, the balance is what fails.
		mint_stable(PUSD, OTHER_REDEEMER, 105);
		assert_noop!(
			redeem(RedeemOptions {
				who: OTHER_REDEEMER,
				recipient: OTHER_RECIPIENT,
				..RedeemOptions::spend(106)
			}),
			Error::<Test>::InsufficientStableBalance
		);
		assert_ok!(redeem(RedeemOptions::spend(106)));
	});
}

/// The view reports the rate that the next redemption starts from: the base fee alone before any
/// redemption, the raised dynamic fee after one, and the decayed fee after idle time.
#[test]
fn fee_rate_view_reports_next_starting_rate() {
	build_and_execute(|| {
		assert_eq!(Redemptions::current_fee_rate(PUSD), None, "no market issues PUSD yet");

		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000_000, 500_000, rate_pct(5, 100)));
		mint_stable(PUSD, REDEEMER, 2_000_000);
		let base_fee = FixedU128::from(default_redemption_config().base_fee);
		assert_eq!(Redemptions::current_fee_rate(PUSD), Some(base_fee));

		// The 100_000 buys 95_015 of the 500_480 debt. The dynamic fee rises by half of that
		// share, `floor(floor(95_015e18 / 500_480) / 2)`, to 9.4924%. The fee on the 4.7462%
		// mean and the 0.5% base is `ceil(95_015 · 0.0524619) = ceil(4_984.67) = 4_985`. One
		// unit more of debt pays the same fee and costs 100_001.
		let settled = redeem(RedeemOptions::spend(100_000)).expect("redemption succeeds");
		assert_eq!(settled.figures(), (95_015, 4_985, 76_012));
		let raised = FixedU128::from_inner(94_923_873_081_841_432);
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee, raised);
		assert_eq!(Redemptions::current_fee_rate(PUSD), Some(raised + base_fee));

		// 24h at the 6h half-life is four whole half-lives:
		// `floor(94_923_873_081_841_432 / 16) = 5_932_742_067_615_089`, plus the base fee.
		advance_time(24 * HOUR_MS);
		let rate = FixedU128::from_inner(5_932_742_067_615_089) + base_fee;
		assert_eq!(Redemptions::current_fee_rate(PUSD), Some(rate));

		// A sized redemption climbs from that rate, so it pays more than the rate alone charges.
		// A day of interest on the remaining 405_465 is `ceil(55.51) = 56`, so the debt is
		// 405_521. The 100_000 buys 93_578 of it, which raises the dynamic fee by 11.5380% from
		// 0.5933%. The fee on the 6.3623% mean and the 0.5% base is
		// `ceil(93_578 · 0.0686227) = ceil(6_421.58) = 6_422`. The starting rate alone would
		// charge `ceil(93_578 · 0.0109327) = 1_024`.
		let quote = dry_redeem(RedeemOptions::spend(100_000)).expect("the vault is redeemable");
		assert_eq!((quote.stable_burned, quote.fee), (93_578, 6_422));

		// The view needs no redeemable vault, which a redemption does.
		set_price(DOT, FixedU128::from_rational(1, 100));
		assert_noop!(redeem(RedeemOptions::spend(100_000)), Error::<Test>::NoRedeemableVault);
		assert_eq!(Redemptions::current_fee_rate(PUSD), Some(rate));
	});
}

/// The fee belongs to the coin, whichever collateral backs it: a DOT/PUSD redemption raises the
/// fee a TOKEN_X/PUSD redeemer pays next, and the share redeemed counts both markets' debt.
#[test]
fn fee_is_shared_across_markets_on_a_coin() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		register_branch(TOKEN_X, PUSD, default_branch_config());

		assert_ok!(open(1, DOT, PUSD, 100_000, 50_000, rate_pct(5, 100)));
		mint_collateral(TOKEN_X_ID, 2, 200_000);
		assert_ok!(open(2, TOKEN_X, PUSD, 100_000, 50_000, rate_pct(5, 100)));
		mint_stable(PUSD, REDEEMER, 1_000_000);

		// Both markets' debt is the denominator, so a redemption moves the fee half as far as it
		// would against DOT/PUSD alone.
		assert_eq!(stablecoin_debt(PUSD), 2 * 50_048);

		// Each redemption cancels 10_000. The first lifts the dynamic fee from zero by
		// `10_000 / 100_096 / 2 = 4.995%` and pays `ceil(10_000 · (2.498% + 0.5%)) = 300`. The
		// second starts there, adds `10_000 / 90_096 / 2 = 5.550%`, and pays
		// `ceil(10_000 · (7.770% + 0.5%)) = 828`.
		let dot = redeem(RedeemOptions::spend(10_300)).expect("redemption succeeds");
		assert_eq!((dot.stable_burned, dot.fee), (10_000, 300));

		// The TOKEN_X redeemer, who redeemed nothing, now pays the raised rate.
		let debt_before = vault_debt(TOKEN_X, PUSD, 2);
		let token_x = redeem(RedeemOptions { collateral: TOKEN_X, ..RedeemOptions::spend(10_828) })
			.expect("redemption succeeds");
		assert_eq!(debt_before - vault_debt(TOKEN_X, PUSD, 2), 10_000);
		assert_eq!((token_x.stable_burned, token_x.fee), (10_000, 828));
	});
}

/// The `(old, new)` dynamic fees that each PUSD `RedemptionDynamicFeeUpdated` event reports, in
/// order.
fn dynamic_fee_updates() -> Vec<(FixedU128, FixedU128)> {
	System::events()
		.into_iter()
		.filter_map(|record| match record.event {
			RuntimeEvent::Redemptions(Event::RedemptionDynamicFeeUpdated {
				stable_id: PUSD,
				old_dynamic_fee,
				new_dynamic_fee,
			}) => Some((old_dynamic_fee, new_dynamic_fee)),
			_ => None,
		})
		.collect()
}

/// Redeems each `(debt, idle_ms)` in turn, after `idle_ms`, spending exactly its debt plus fee,
/// then rolls everything back. Returns the fees paid, the dynamic fee left, and the dynamic-fee
/// updates reported.
fn redeem_in_sequence(
	debts: &[(Balance, Moment)],
) -> (Balance, FixedU128, Vec<(FixedU128, FixedU128)>) {
	hypothetically!({
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let collateral_before = collateral_balance(DOT, RECIPIENT);
		let total: Balance = debts.iter().map(|&(debt, _)| debt).sum();
		for &(debt, idle_ms) in debts {
			advance_time(idle_ms);
			assert_ok!(redeem(RedeemOptions::spend(spend_for_debt(debt))));
		}
		// Every unit of debt buys the same collateral at the 1.25 price, however it is split.
		assert_eq!(collateral_balance(DOT, RECIPIENT) - collateral_before, total * 4 / 5);
		(
			Assets::balance(PUSD, FEE_DEST) - fee_before,
			crate::RedemptionStates::<Test>::get(PUSD).dynamic_fee,
			dynamic_fee_updates(),
		)
	})
}

/// The worked example of `fees::tests::the_worked_example_at_once_and_in_halves`, through the
/// pallet: 20_000 of the 500_480 coin debt, at once and in halves.
///
/// The default divisor of 2 halves every climb: 300 at once, 100 plus 201 in halves, which also
/// leave a higher dynamic fee. Time makes a split cheaper: after one idle half-life, the second
/// half starts from half the first half's climb and pays
/// `ceil(10_000 · (0.4995% + 0.5097% + 0.5%)) = 151` instead of 201.
///
/// Each update event reports the dynamic fee the redemption started from, decayed to its block,
/// and the one it left. The share and its division by 2 both round down.
#[test]
fn fee_split_redemption_pays_first_half_climb() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// One vault owes 500_480: 500_000 plus a 480-unit seven-day upfront fee.
		assert_ok!(open(1, DOT, PUSD, 1_000_000, 500_000, rate_pct(5, 100)));
		mint_stable(PUSD, REDEEMER, 2_000_000);
		assert_eq!(vault_debt(DOT, PUSD, 1), 500_480);

		let zero = FixedU128::zero();
		let (at_once_fee, at_once_dynamic_fee, at_once_updates) =
			redeem_in_sequence(&[(20_000, 0)]);
		assert_eq!(at_once_fee, 300);
		// `20_000 / 500_480 / 2`, rounded down to `1e-18` at each step.
		assert_eq!(at_once_dynamic_fee, FixedU128::from_inner(19_980_818_414_322_250));
		assert_eq!(at_once_updates, [(zero, FixedU128::from_inner(19_980_818_414_322_250))]);

		// `floor(floor(10_000e18 / 500_480) / 2)`.
		let first_half = FixedU128::from_inner(9_990_409_207_161_125);
		let (halves_fee, halves_dynamic_fee, halves_updates) =
			redeem_in_sequence(&[(10_000, 0), (10_000, 0)]);
		assert_eq!(halves_fee, 301);
		assert!(halves_dynamic_fee > at_once_dynamic_fee);
		// Without idle time, the second half starts from the fee that the first half left and
		// adds `floor(floor(10_000e18 / 490_480) / 2) = 10_194_095_579_840_156`.
		assert_eq!(
			halves_updates,
			[(zero, first_half), (first_half, FixedU128::from_inner(20_184_504_787_001_281)),]
		);

		let (rested_fee, rested_dynamic_fee, rested_updates) =
			redeem_in_sequence(&[(10_000, 0), (10_000, 6 * HOUR_MS)]);
		assert_eq!(rested_fee, 251);
		// The first half's rise decays to 0.4995% over the half-life, then the second half adds
		// `10_000 / ~490_480 / 2`, rounded down to `1e-18` at each step.
		assert_eq!(rested_dynamic_fee, FixedU128::from_inner(15_188_946_869_078_618));
		assert!(rested_dynamic_fee < at_once_dynamic_fee);
		// The second half reports the halved fee, `floor(9_990_409_207_161_125 / 2)`, not the
		// stored one. Six hours of interest on the remaining 490_480 are `ceil(16.79) = 17`, so
		// it adds `floor(floor(10_000e18 / 490_497) / 2) = 10_193_742_265_498_056`.
		assert_eq!(
			rested_updates,
			[
				(zero, first_half),
				(
					FixedU128::from_inner(4_995_204_603_580_562),
					FixedU128::from_inner(15_188_946_869_078_618)
				),
			]
		);
	});
}

#[test]
fn ordinary_serves_dormant_slot_first() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		mint_stable(PUSD, REDEEMER, 1_000_000);

		assert_ok!(redeem(RedeemOptions::spend(spend_for_debt(DORMANT_FILL_DEBT))));
		assert!(vault_status(DOT, PUSD, 1).expect("vault 1").is_dormant());

		let v2_before = vault_debt(DOT, PUSD, 2);
		let v1_residual = vault_debt(DOT, PUSD, 1);
		// Dormant vault 1 has left the rate index, so only the Dormant slot, served first, can
		// reach it.
		let settled =
			redeem(RedeemOptions::spend(spend_for_debt(100))).expect("redemption succeeds");
		// Cancelling less than the residual keeps the husk's row readable.
		assert_eq!(v1_residual, 141);
		assert_eq!(v1_residual - vault_debt(DOT, PUSD, 1), 100);
		// Collateral paid is the face-value amount for the debt cancelled.
		assert_eq!(settled.collateral_out, 80);
		// Priority: the ordinary vault behind the Dormant slot is untouched.
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
	});
}

#[test]
fn ordinary_continues_past_drained_dormant_slot() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		mint_stable(PUSD, REDEEMER, 1_000_000);

		// A sub-minimum residual parks vault 1 as the dormant redemption target.
		assert_ok!(redeem(RedeemOptions::spend(spend_for_debt(DORMANT_FILL_DEBT))));
		assert!(vault_status(DOT, PUSD, 1).expect("vault 1").is_dormant());
		let v1_residual = vault_debt(DOT, PUSD, 1);
		let v2_debt = vault_debt(DOT, PUSD, 2);

		// A budget beyond both targets drains the dormant slot and continues into the rate index.
		let settled = redeem(RedeemOptions::spend(100_000)).expect("redemption succeeds");
		assert_eq!(settled.ordinary_steps, Some(2));
		assert_eq!(settled.stable_burned, v1_residual + v2_debt);
		assert_eq!(vault_debt(DOT, PUSD, 1), 0);
		assert_eq!(vault_debt(DOT, PUSD, 2), 0);
	});
}

/// Opens `who`'s DOT/PUSD vault and parks it in `FinalRecovery` at `recovery_price`.
fn setup_final_recovery(who: AccountId, coll: Balance, debt: Balance, recovery_price: FixedU128) {
	// Earlier `FinalRecovery` vaults may have left the price depressed.
	set_price(DOT, DEFAULT_PRICE);
	assert_ok!(open(who, DOT, PUSD, coll, debt, rate_pct(5, 100)));
	set_price(DOT, recovery_price);
	assert_ok!(enter_final_recovery(DOT, PUSD, who));
	assert!(vault_status(DOT, PUSD, who).expect("fr vault").is_final_recovery());
}

#[test]
fn recovery_bonus_improves_vault_cr() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Price 0.52 parks the FinalRecovery vault's CR barely above 100%, the
		// regime where an overpaid bonus could actually damage the vault.
		let price = RECOVERY_PRICE_ABOVE_PAR;
		setup_final_recovery(1, 1_000, 500, price);
		// A live dynamic fee makes "recovery leaves the fee state alone" meaningful.
		set_dynamic_fee(PUSD, FixedU128::from_rational(3u128, 100u128));
		mint_stable(PUSD, REDEEMER, 1_000_000);
		let debt_before = vault_debt(DOT, PUSD, 1);
		let fee_state_before = crate::RedemptionStates::<Test>::get(PUSD);
		let recipient_before = collateral_balance(DOT, RECIPIENT);

		let cr_before = vault_cr(1, price);
		assert!(cr_before > rate_pct(101, 100), "fixture must clear the 1% buffer");

		let settled = redeem(RedeemOptions::spend(200)).expect("redemption succeeds");

		// CR = 520/501 ≈ 103.79%, so the bonus is the mid-range excess
		// CR − 100% − 1% buffer ≈ 2.79% (below the 5% penalty cap):
		// floor(floor(200 · 1.0279) / 0.52) = floor(205 / 0.52) = 394. Recovery is fee-free.
		assert_eq!(settled.figures(), (200, 0, 394), "recovery bonus payout");
		assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 394);
		assert_eq!(settled.ordinary_steps, None, "recovery must not settle as ordinary");
		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD), fee_state_before);
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before - 200);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));

		// The 1% buffer keeps the bonus strictly below CR − 100%, so paying it
		// must leave the vault's CR strictly better than before the redemption.
		assert!(vault_cr(1, price) > cr_before, "recovery-bonus redemption must improve the CR");
	});
}

/// The head settles at a bonus up to the 120% ICR, even above the 110% MCR where it could
/// already exit, so redeemers push it out with a margin rather than one tick above the MCR.
/// Above the ICR, redemptions refuse it and offsets see no target. Once out, it is ordinary.
#[test]
fn recovery_stops_above_icr() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR);
		mint_stable(PUSD, REDEEMER, 1_000_000);

		// CR = 550/501 ≈ 109.8%: the capped 5% bonus pays floor(210 / 0.55) = 381.
		set_price(DOT, FixedU128::from_rational(55u128, 100u128));
		let settled = redeem(RedeemOptions::spend(200)).expect("redemption succeeds");
		assert_eq!(settled.collateral_out, 381);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
		let debt_before = vault_debt(DOT, PUSD, 1);

		// CR = 619 · 0.60 / 301 ≈ 123.4%: above the ICR, only the exit remains.
		set_price(DOT, FixedU128::from_rational(60u128, 100u128));
		assert_noop!(redeem(RedeemOptions::spend(200)), Error::<Test>::FinalRecoveryExitRequired);
		assert_ok!(preview_offset(200), RecoveryOffsetQuote::NoTarget);
		assert_ok!(execute_offset(PUSD, 200), RecoveryOffsetResult::NoTarget);
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before);

		// CR = 619 · 0.56 / 301 ≈ 115.2%: above MCR but not ICR. Offsets see a target and a
		// redemption still pays the capped 5% bonus, floor(105 / 0.56) = 187, with no fee.
		set_price(DOT, FixedU128::from_rational(56u128, 100u128));
		assert_ok!(preview_offset(100), RecoveryOffsetQuote::Available { debt: 100 });
		let settled = redeem(RedeemOptions::spend(100)).expect("redemption succeeds");
		assert_eq!(settled.figures(), (100, 0, 187));
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
		assert_eq!(vault_debt(DOT, PUSD, 1), 201);
		assert!(vault_status(DOT, PUSD, 1).expect("vault 1").is_final_recovery());

		// CR = 432 · 0.56 / 201 ≈ 120.4%: the head exits and is ordinary again.
		assert_ok!(Vaults::exit_final_recovery(
			RuntimeOrigin::signed(REDEEMER),
			DOT,
			PUSD,
			1,
			pallet_linked_list::Position::endpoints_only()
		));
		let settled =
			redeem(RedeemOptions::spend(spend_for_debt(200))).expect("redemption succeeds");
		// Ordinary again: face value floor(200 / 0.56) = 357, and the fee is charged.
		assert!(settled.ordinary_steps.is_some());
		assert_eq!(settled.collateral_out, 357);
		assert!(settled.fee > 0);
	});
}

/// A sub-minimum head past the gate may be unable to exit while another vault
/// holds the Dormant slot, so it stays settleable, at face value.
#[test]
fn recovery_sub_minimum_head_above_mcr_settles_at_face_value() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR);
		mint_stable(PUSD, REDEEMER, 1_000_000);
		let debt = vault_debt(DOT, PUSD, 1);
		assert_ok!(redeem(RedeemOptions::spend(debt - 150)));
		assert_eq!(vault_debt(DOT, PUSD, 1), 150);

		set_price(DOT, DEFAULT_PRICE);
		assert_ok!(preview_offset(100), RecoveryOffsetQuote::Available { debt: 100 });
		let settled = redeem(RedeemOptions::spend(100)).expect("redemption succeeds");
		// No bonus: floor(100 / 1.25) = 80, and no fee.
		assert_eq!(settled.figures(), (100, 0, 80));
		assert_eq!(vault_debt(DOT, PUSD, 1), 50);
		assert!(vault_status(DOT, PUSD, 1).expect("vault 1").is_final_recovery());
	});
}

#[test]
fn recovery_head_served_before_ordinary_vaults() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR);
		// Open the ordinary vault at a healthy price, then settle at 0.54, where the
		// head's CR ≈ 107.8% is below the 110% exit gate.
		set_price(DOT, DEFAULT_PRICE);
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		let price = FixedU128::from_rational(54u128, 100u128);
		set_price(DOT, price);
		let v1_before = vault_debt(DOT, PUSD, 1);
		let v2_before = vault_debt(DOT, PUSD, 2);
		mint_stable(PUSD, REDEEMER, 1_000_000);

		// Beyond the 106% at which the cap starts binding, the bonus must come out
		// clamped to exactly the 5% redistribution penalty rather than the raw excess.
		let cr = vault_cr(1, price);
		assert!(cr > rate_pct(106, 100), "fixture must put the raw excess above the cap");
		assert!(cr < rate_pct(110, 100), "fixture must keep the head below the exit gate");

		// The FinalRecovery vault is served at its exact regime, before any ordinary vault:
		// capped 5% bonus, floor(200 * 1.05 / 0.54) = 388 collateral, no fee.
		let settled = redeem(RedeemOptions::spend(200)).expect("redemption succeeds");
		assert_eq!(settled.ordinary_steps, None, "a recovery settlement");
		assert_eq!(settled.figures(), (200, 0, 388));
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
		assert_eq!(v1_before - vault_debt(DOT, PUSD, 1), 200);
		// The ordinary vault is untouched.
		assert_eq!(vault_debt(DOT, PUSD, 2), v2_before);
	});
}

/// The collateral value that sizes the Insurance Fund cover rounds up: the fund covers no more
/// than the true shortfall, and the redeemer pays no less than the collateral is worth.
#[test]
fn insurance_cover_rounds_collateral_value_up() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// 1_000 units at 0.4005 are worth 400.5 pUSD against 501 of debt.
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(4_005u128, 10_000u128));
		assert_eq!(vault_debt(DOT, PUSD, 1), 501);
		mint_stable(PUSD, insurance_account(PUSD), 1_000);
		mint_stable(PUSD, REDEEMER, 1_000_000);
		let recipient_before = collateral_balance(DOT, RECIPIENT);
		let supply_before = Assets::total_supply(PUSD);

		// The value rounds up to 401, so the flush fund covers a shortfall of 100 rather than
		// 101, and the redeemer pays 401 for collateral worth 400.5: the half unit rounds
		// against the redeemer, never against the fund.
		let settled = redeem(RedeemOptions::spend(1_000)).expect("redemption succeeds");
		assert_eq!(settled.figures(), (401, 0, 1_000));
		assert_eq!(settled.insurance_cover, 100);
		// The redeemer pays the 401 and the fund the 100 cover; both are burned.
		assert_eq!(Assets::balance(PUSD, REDEEMER), 1_000_000 - 401);
		assert_eq!(Assets::balance(PUSD, insurance_account(PUSD)), 900);
		assert_eq!(supply_before - Assets::total_supply(PUSD), 501);
		assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 1_000);
		assert_eq!(vault_status(DOT, PUSD, 1), None, "vault settled and closed");
	});
}

#[test]
fn config_update_stores_and_validates() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Every field differs from the seeded default, so a field that the call failed to store
		// would show.
		let cfg = RedemptionConfig {
			minimum_redemption_amount: 250,
			dynamic_fee_decay_period: 12 * HOUR_MS,
			dynamic_fee_floor: rate_pct(1, 100),
			dynamic_fee_ceiling: rate_pct(50, 100),
			base_fee: Permill::from_percent(1),
			fee_ceiling: Permill::from_percent(60),
			dynamic_fee_increase_divisor: FixedU128::from_u32(4),
			final_recovery_bonus_buffer: Permill::from_percent(2),
		};
		assert_ok!(Redemptions::set_redemption_config(RuntimeOrigin::root(), PUSD, cfg.clone()));
		assert_eq!(crate::RedemptionConfigs::<Test>::get(PUSD), Some(cfg.clone()));
		System::assert_last_event(RuntimeEvent::Redemptions(Event::RedemptionConfigUpdated {
			stable_id: PUSD,
		}));

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

/// Only Root and the coin's `pallet-assets` owner set the redemption config, as Vaults requires
/// for `create_branch`. The config governs every market on the coin, so no market admin may:
/// the DOT/PUSD admin would otherwise price TOKEN_X/PUSD redemptions.
#[test]
fn config_update_requires_coin_owner_or_root() {
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
		let mut cfg = crate::RedemptionConfigs::<Test>::get(PUSD).unwrap();
		cfg.minimum_redemption_amount = 250;

		// This market's full admin, its emergency admin, and the sibling market's full admin.
		for wrong in [ADMIN, EMERGENCY_ADMIN, other_admin] {
			assert_noop!(
				Redemptions::set_redemption_config(RuntimeOrigin::signed(wrong), PUSD, cfg.clone()),
				BadOrigin
			);
		}

		use frame::traits::fungibles::roles::Inspect as _;
		assert_eq!(Assets::owner(PUSD), Some(STABLECOIN_OWNER));
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

#[test]
fn config_update_rejects_unregistered_coin() {
	build_and_execute(|| {
		let cfg = default_redemption_config();
		assert_noop!(
			Redemptions::set_redemption_config(RuntimeOrigin::root(), PUSD, cfg),
			Error::<Test>::StablecoinNotRegistered
		);
	});
}

#[test]
fn insurance_fund_drains_head_by_head() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Recovery price 0.10 → both vaults sit below 100% CR (insurance-adjusted).
		let rp = FixedU128::from_rational(1u128, 10u128);
		setup_final_recovery(1, 1_000, 500, rp);
		setup_final_recovery(2, 1_000, 500, rp);

		// Both vaults are in the FinalRecovery FIFO, oldest first; only the head
		// is exposed to redemption.
		assert_eq!(recovery_queue(), vec![1u64, 2u64]);

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
		// Another coin's fund account holds pUSD too, but it is no cover for this market.
		mint_stable(PUSD, insurance_account(USDX), 1_000_000);
		mint_stable(PUSD, REDEEMER, 10_000);

		// First transaction settles only the FIFO head (vault 1). The full payout leaves
		// neither debt nor collateral, so the vault closes and its row is removed.
		let first = redeem(RedeemOptions::spend(10_000)).expect("redemption succeeds");
		assert_eq!(vault_status(DOT, PUSD, 1), None, "head settled and closed");
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
		assert_eq!(first.figures(), (collateral_value_1, 0, 1_000));
		assert_eq!(first.insurance_cover, shortfall_1);
		assert_eq!(Assets::balance(PUSD, insurance_account(PUSD)), 0, "head 1 drained the fund");
		// Head-only: vault 2 is untouched and becomes the new FIFO head.
		assert_eq!(vault_debt(DOT, PUSD, 2), debt2);
		assert!(vault_status(DOT, PUSD, 2).expect("vault 2").is_final_recovery());
		assert_eq!(recovery_queue(), vec![2u64]);

		// Second transaction settles vault 2 against the now-empty fund: the
		// recovery rate falls to C/D, so the redeemer covers the entire debt and
		// pUSD holders absorb the shortfall. A full settlement pays the whole
		// collateral, so this vault closes exactly like vault 1 did.
		let second = redeem(RedeemOptions::spend(10_000)).expect("redemption succeeds");
		assert_eq!(vault_status(DOT, PUSD, 2), None, "second head settled and closed");
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 2)).is_none());
		assert_eq!(held(DOT, 2), 0);
		assert!(recovery_queue().is_empty(), "FIFO drained");
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		// Empty fund: D = 501 against C = 100, so the redeemer pays all 501 for
		// the vault's 1_000 units.
		assert_eq!(second.figures(), (debt2, 0, 1_000));
		assert_eq!(second.insurance_cover, 0, "no fund left to burn");
		assert_eq!(Assets::balance(PUSD, insurance_account(USDX)), 1_000_000);
		assert!(
			second.stable_burned > first.stable_burned,
			"drained fund pushes the loss onto the redeemer"
		);
	});
}

#[test]
fn insurance_fund_burned_only_on_full_settlement() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Recovery price 0.40 → CR < 100% with a partial Insurance Fund cover.
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128));
		mint_stable(PUSD, insurance_account(PUSD), 50);
		mint_stable(PUSD, REDEEMER, 10_000);
		let debt_before = vault_debt(DOT, PUSD, 1);
		let recipient_before = collateral_balance(DOT, RECIPIENT);

		// The fund stays untouched until the market-side debt is gone.
		let first = redeem(RedeemOptions::spend(200)).expect("redemption succeeds");
		assert!(pallet_vaults::Vaults::<Test>::get((DOT, PUSD, 1)).is_some(), "still settling");
		assert!(vault_status(DOT, PUSD, 1).expect("vault 1").is_final_recovery());
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		// Recovery is fee-free, so 200 cancels 200 of the `D − 50 = 451` market-side debt and
		// buys `floor(200 · 1_000 / 451) = 443` of the 1_000 collateral units.
		assert_eq!(first.figures(), (200, 0, 443));
		assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 443);
		assert_eq!(first.insurance_cover, 0, "fund untouched mid-settlement");
		assert_eq!(Assets::balance(PUSD, insurance_account(PUSD)), 50);
		assert_eq!(debt_before - vault_debt(DOT, PUSD, 1), 200);

		// The price changes before the second transaction, so the split is re-read from live
		// state: at 0.30 the remaining 557 collateral units are valued at 168 (167.1 rounded
		// up) against 301 of debt. The shortfall rises to 133, the fund still covers only 50,
		// and the market side cancels the other 251. Supplying all of it is the full settlement: it
		// burns the cover in the same step, pays every remaining collateral unit, and closes the
		// row.
		set_price(DOT, FixedU128::from_rational(30u128, 100u128));
		let second = redeem(RedeemOptions::spend(10_000)).expect("redemption succeeds");
		assert_eq!(second.figures(), (251, 0, 557));
		assert_eq!(second.insurance_cover, 50, "cover burned on completion");
		assert_eq!(vault_status(DOT, PUSD, 1), None, "vault settled and closed");
		assert_eq!(held(DOT, 1), 0);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::InsuranceAdjusted));
		assert_eq!(Assets::balance(PUSD, insurance_account(PUSD)), 0);
	});
}

#[test]
fn ordinary_allowed_in_safety_mode() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// Put the branch in Safety mode (TCR below 130%) while the vault stays above par.
		set_price(DOT, FixedU128::from_rational(6u128, 10u128));
		assert!(
			branch_tcr(DOT, PUSD) < FixedU128::from_rational(130u128, 100u128),
			"fixture must put the branch below the safety threshold"
		);

		let debt_before = vault_debt(DOT, PUSD, 1);
		mint_stable(PUSD, REDEEMER, 1_000_000);
		let tcr_before = branch_tcr(DOT, PUSD);

		// 223 buys 201 of the 501 coin debt, a 40.12% share, so the dynamic fee rises 20.06% and
		// the fee is `ceil(201 · (10.03% + 0.5%)) = 22`. At 0.6, 201 debt pays
		// `floor(201 / 0.6) = 335` collateral.
		let settled = redeem(RedeemOptions::spend(223)).expect("redemption succeeds");
		assert_eq!(debt_before - vault_debt(DOT, PUSD, 1), 201);
		assert_eq!(settled.figures(), (201, 22, 335));
		// A face-value redemption removes debt and at most equal-value collateral, so a TCR above
		// 1 can only rise: why Safety mode still permits it.
		assert!(branch_tcr(DOT, PUSD) > tcr_before, "redemption must raise TCR");
	});
}

#[test]
fn redeem_rejects_missing_or_zero_price() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, REDEEMER, 1_000);

		clear_price(DOT);
		assert_noop!(redeem(RedeemOptions::spend(201)), Error::<Test>::OracleUnavailable);

		// A zero price is refused like a missing one.
		set_price(DOT, FixedU128::zero());
		assert_noop!(redeem(RedeemOptions::spend(201)), Error::<Test>::OracleUnavailable);
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
fn ordinary_accrues_interest_before_cancelling() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// A year at 100% APR makes the accrued interest large.
		assert_ok!(open(1, DOT, PUSD, 1_000_000, 500_000, rate_pct(100, 100)));
		let debt_at_open = vault_debt(DOT, PUSD, 1);
		let stamped_at_open = vault_interest_time(1);
		mint_stable(PUSD, REDEEMER, 2_000_000);

		advance_time(ONE_YEAR_MS);

		let accrued = projected_full_debt(1);
		assert!(accrued > debt_at_open, "a year of interest must grow the debt");

		let settled = redeem(RedeemOptions::spend(1_011)).expect("redemption succeeds");

		// The redemption advances the target's interest clock and cancels from the accrued debt.
		assert_eq!(vault_interest_time(1), branch_interest_time(Timestamp::get()));
		assert!(vault_interest_time(1) > stamped_at_open);
		assert_eq!(vault_debt(DOT, PUSD, 1), accrued - 1_005);
		// 1_011 buys 1_005 debt for `floor(1_005 / 1.25) = 804` collateral. The coin debt counts
		// a year of interest accrued without a market touch, so the dynamic fee rises about
		// `1_005 / 1_009_583 / 2 = 0.0498%` and the fee is `ceil(1_005 · (0.0249% + 0.5%)) = 6`.
		assert_eq!(settled.figures(), (1_005, 6, 804));
	});
}

/// Liquidates `owner`'s DOT/PUSD vault entirely by redistribution.
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

/// `OffsetDecision` without the owner and collateral these tests do not assert.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RecoveryOffsetQuote {
	NoTarget,
	BelowPar,
	Available { debt: Balance },
}

/// Quotes a `(DOT, PUSD)` recovery offset through the decision that execution prices with.
fn preview_offset(max_debt_to_cancel: Balance) -> Result<RecoveryOffsetQuote, DispatchError> {
	use crate::recovery::OffsetDecision;
	Ok(match Redemptions::offset_decision(&DOT, &PUSD, max_debt_to_cancel)? {
		OffsetDecision::NoTarget => RecoveryOffsetQuote::NoTarget,
		OffsetDecision::BelowPar => RecoveryOffsetQuote::BelowPar,
		OffsetDecision::Available { debt, .. } => RecoveryOffsetQuote::Available { debt },
	})
}

/// The `(DOT, PUSD)` `FinalRecovery` FIFO, head first.
fn recovery_queue() -> Vec<AccountId> {
	let queue =
		LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, PUSD), RECOVERY_QUEUE_LEN_MAX);
	assert!(queue.len() < RECOVERY_QUEUE_LEN_MAX as usize, "the read must see the whole queue");
	queue
}

/// Status of `owner`'s vault through the public view, `None` once the vault is gone.
fn vault_status(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
) -> Option<pallet_vaults::types::VaultStatus> {
	match Vaults::vault_after_touch(collateral, stable, owner) {
		Ok((_, status)) => Some(status),
		Err(error) => {
			assert_eq!(error, pallet_vaults::Error::<Test>::VaultNotFound.into());
			None
		},
	}
}

/// Executes a `(DOT, stable)` recovery offset as a pool caller would: [`REDEEMER`] funds the
/// payment up to its reducible balance and gets the change back, and [`RECIPIENT`] receives the
/// collateral. An `Err` also rolls the withdrawal back.
fn execute_offset(
	stable: StableId,
	max_debt_to_cancel: Balance,
) -> Result<RecoveryOffsetResult<Balance>, DispatchError> {
	frame::deps::frame_support::storage::with_storage_layer(|| {
		execute_offset_inner(stable, max_debt_to_cancel)
	})
}

fn execute_offset_inner(
	stable: StableId,
	max_debt_to_cancel: Balance,
) -> Result<RecoveryOffsetResult<Balance>, DispatchError> {
	use frame::traits::{
		fungibles::Balanced as FungiblesBalanced,
		tokens::{Fortitude, Precision},
	};
	let (amount, preservation) =
		reducible_debit::<Assets, _>(stable, &REDEEMER, max_debt_to_cancel);
	let payment = if amount.is_zero() {
		crate::StableCreditOf::<Test>::zero(stable)
	} else {
		<Assets as FungiblesBalanced<AccountId>>::withdraw(
			stable,
			&REDEEMER,
			amount,
			Precision::Exact,
			preservation,
			Fortitude::Polite,
		)?
	};
	let (result, change) = <Redemptions as RecoveryOffsetInterface>::execute_recovery_offset(
		&DOT, payment, &RECIPIENT,
	)?;
	if let Err(change) = change.drop_zero() {
		<Assets as FungiblesBalanced<AccountId>>::resolve(&REDEEMER, change)
			.map_err(|_| DispatchError::Other("change does not fit the payer account"))?;
	}
	Ok(result)
}

#[test]
fn offset_settles_head_as_previewed() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR);
		setup_final_recovery(2, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR);
		// At 0.54 both queued vaults sit at CR ≈ 107.8%: inside the recovery-bonus
		// regime offsets are restricted to, and below the 110% exit gate.
		set_price(DOT, FixedU128::from_rational(54u128, 100u128));
		assert_eq!(recovery_queue(), vec![1u64, 2u64]);
		let debt1 = vault_debt(DOT, PUSD, 1);
		// 500 principal + the 1-unit 7-day upfront fee.
		assert_eq!(debt1, 501);
		let debt2 = vault_debt(DOT, PUSD, 2);
		mint_stable(PUSD, REDEEMER, 10_000);
		let payer_before = Assets::balance(PUSD, REDEEMER);
		let recipient_before = collateral_balance(DOT, RECIPIENT);
		let fee_before = Assets::balance(PUSD, FEE_DEST);
		let issuance_before = Assets::total_supply(PUSD);

		// The quote sizes exactly the burn the execution then performs.
		assert_ok!(preview_offset(10_000), RecoveryOffsetQuote::Available { debt: debt1 });
		// CR = 540/501 ≈ 107.8% caps the bonus at the 5% redistribution
		// penalty: collateral = floor(floor(501 · 1.05) / 0.54) = 974.
		assert_ok!(
			execute_offset(PUSD, 10_000),
			RecoveryOffsetResult::Applied { collateral_out: 974 }
		);

		// The drained head flips to Dormant and leaves the FIFO; the next
		// head is untouched.
		assert_eq!(vault_debt(DOT, PUSD, 1), 0);
		assert!(vault_status(DOT, PUSD, 1).expect("vault 1").is_dormant());
		assert_eq!(recovery_queue(), vec![2u64]);
		assert_eq!(vault_debt(DOT, PUSD, 2), debt2);
		// The burn is fee-free, so issuance falls by exactly the cancelled
		// debt and nothing reaches the fee destination.
		assert_eq!(payer_before - Assets::balance(PUSD, REDEEMER), debt1);
		assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 974);
		assert_eq!(Assets::balance(PUSD, FEE_DEST), fee_before);
		assert_eq!(issuance_before - Assets::total_supply(PUSD), debt1);
	});
}

/// A partial offset keeps the head queued. Offsets are fee-free: they neither move the dynamic
/// fee nor emit redemption events.
#[test]
fn offset_partial_fill_keeps_head_queued() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR);
		set_price(DOT, FixedU128::from_rational(54u128, 100u128));
		// A live dynamic fee makes "the offset leaves the fee state alone" meaningful.
		set_dynamic_fee(PUSD, FixedU128::from_rational(3u128, 100u128));
		let state_before = crate::RedemptionStates::<Test>::get(PUSD);
		let debt_before = vault_debt(DOT, PUSD, 1);
		mint_stable(PUSD, REDEEMER, 10_000);
		let payer_before = Assets::balance(PUSD, REDEEMER);
		let recipient_before = collateral_balance(DOT, RECIPIENT);
		System::reset_events();

		// A 200 cap partially fills the 501-debt head; quote and execution
		// agree on the capped size.
		assert_ok!(preview_offset(200), RecoveryOffsetQuote::Available { debt: 200 });
		// 5%-capped bonus: floor(floor(200 · 1.05) / 0.54) = 388.
		assert_ok!(
			execute_offset(PUSD, 200),
			RecoveryOffsetResult::Applied { collateral_out: 388 }
		);

		assert_eq!(crate::RedemptionStates::<Test>::get(PUSD), state_before);
		let redemption_events = System::events()
			.into_iter()
			.filter(|r| matches!(r.event, RuntimeEvent::Redemptions(_)))
			.count();
		assert_eq!(redemption_events, 0);

		// The partially settled head keeps its place at the FIFO front.
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before - 200);
		assert!(vault_status(DOT, PUSD, 1).expect("vault 1").is_final_recovery());
		assert_eq!(recovery_queue(), vec![1u64]);
		assert_eq!(payer_before - Assets::balance(PUSD, REDEEMER), 200);
		assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 388);
	});
}

/// Regression pin for quote/execution parity: a zero `max_debt_to_cancel`
/// against a recovery-bonus head used to quote `Available { debt: 0 }` while
/// execution reported `NoTarget`.
#[test]
fn offset_zero_budget_has_no_target() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR);
		set_price(DOT, FixedU128::from_rational(54u128, 100u128));
		mint_stable(PUSD, REDEEMER, 1_000);

		// A zero budget withdraws nothing, so both paths are pure storage no-ops.
		assert_storage_noop!({
			assert_ok!(preview_offset(0), RecoveryOffsetQuote::NoTarget);
		});
		assert_storage_noop!({
			assert_ok!(execute_offset(PUSD, 0), RecoveryOffsetResult::NoTarget);
		});
	});
}

#[test]
fn offset_refuses_below_par_head() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Price 0.40 parks the head below par (CR < 100%): settlement at a
		// discount stays exclusive to the redemption pathway.
		setup_final_recovery(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128));
		let debt_before = vault_debt(DOT, PUSD, 1);
		let held_before = held(DOT, 1);
		mint_stable(PUSD, REDEEMER, 10_000);
		let payer_before = Assets::balance(PUSD, REDEEMER);

		assert_storage_noop!({
			assert_ok!(preview_offset(10_000), RecoveryOffsetQuote::BelowPar);
		});
		// Execution withdraws and refunds the payment, which leaves asset events behind, so
		// the refusal is checked field by field rather than by storage root.
		assert_ok!(execute_offset(PUSD, 10_000), RecoveryOffsetResult::BelowPar);
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before);
		assert_eq!(held(DOT, 1), held_before);
		assert_eq!(Assets::balance(PUSD, REDEEMER), payer_before);
		assert!(vault_status(DOT, PUSD, 1).expect("vault 1").is_final_recovery());
	});
}

#[test]
fn offset_ignores_ordinary_vaults() {
	build_and_execute(|| {
		// Unregistered market: the target-first fast path answers before the
		// config lookup could report `StablecoinNotRegistered`.
		assert_ok!(preview_offset(1_000), RecoveryOffsetQuote::NoTarget);
		assert_ok!(execute_offset(PUSD, 1_000), RecoveryOffsetResult::NoTarget);

		// An ordinary vault is not an offset target either: offsets exist
		// only for the FinalRecovery FIFO head.
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		let debt_before = vault_debt(DOT, PUSD, 1);
		mint_stable(PUSD, REDEEMER, 1_000);

		assert_ok!(preview_offset(1_000), RecoveryOffsetQuote::NoTarget);
		assert_ok!(execute_offset(PUSD, 1_000), RecoveryOffsetResult::NoTarget);
		assert_eq!(vault_debt(DOT, PUSD, 1), debt_before);
	});
}

#[test]
fn offset_rejects_frozen_branch() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR);
		set_price(DOT, DEFAULT_PRICE);
		mint_stable(PUSD, REDEEMER, 1_000);
		assert_ok!(Vaults::set_governance_frozen(RuntimeOrigin::signed(ADMIN), DOT, PUSD, true));

		// Frozen-mode enforcement lives vault-side: the head is still queued, so both paths
		// project its snapshot, and the projection rejects the frozen branch.
		assert_noop!(preview_offset(200), pallet_vaults::Error::<Test>::BranchFrozen);
		assert_noop!(execute_offset(PUSD, 200), pallet_vaults::Error::<Test>::BranchFrozen);
	});
}

#[test]
fn offset_payment_asset_selects_market() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		setup_final_recovery(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR);

		// Queue a settleable head in the sibling USDX market too.
		set_price(DOT, DEFAULT_PRICE);
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(open(2, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		set_price(DOT, RECOVERY_PRICE_ABOVE_PAR);
		assert_ok!(enter_final_recovery(DOT, USDX, 2));
		// Both heads settle at 0.54, inside the bonus regime and below the exit gate.
		set_price(DOT, FixedU128::from_rational(54u128, 100u128));

		let pusd_debt_before = vault_debt(DOT, PUSD, 1);
		let usdx_debt_before = vault_debt(DOT, USDX, 2);
		mint_stable(USDX, REDEEMER, 100 * USDX_UNIT);
		let issuance_before = Assets::total_supply(USDX);

		// The payment's own asset names the market: a USDX payment against
		// `DOT` collateral settles the (DOT, USDX) head, and a coin mismatch
		// with some other named market is unrepresentable.
		assert!(matches!(
			execute_offset(USDX, 100 * USDX_UNIT),
			Ok(RecoveryOffsetResult::Applied { .. })
		));

		// The USDX head absorbed the full budget; the PUSD market never moved.
		assert_eq!(vault_debt(DOT, USDX, 2), usdx_debt_before - 100 * USDX_UNIT);
		assert_eq!(vault_debt(DOT, PUSD, 1), pusd_debt_before);
		assert_eq!(Assets::balance(USDX, REDEEMER), 0);
		assert_eq!(issuance_before - Assets::total_supply(USDX), 100 * USDX_UNIT);
	});
}

/// At six decimals, a partial below-par step floors raw units, never coins, and
/// the full settlement pays every raw unit that is left.
#[test]
fn insurance_partial_steps_floor_raw_units() {
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
		mint_stable(USDX, REDEEMER, 1_000 * USDX_UNIT);

		// C = 400e6 against D = 500_479_124 with 50e6 of cover leaves market
		// debt 450_479_124. A partial 225e6 buys its share of the 1e9 units:
		// floor(225e6 · 1e9 / 450_479_124) = floor(499_468_206.6) = 499_468_206.
		let partial =
			redeem(RedeemOptions { stable: USDX, ..RedeemOptions::spend(225 * USDX_UNIT) })
				.expect("redemption succeeds");
		assert_eq!(partial.figures(), (225 * USDX_UNIT, 0, 499_468_206));
		assert_eq!(partial.insurance_cover, 0, "fund untouched");
		assert_eq!(vault_debt(DOT, USDX, 1), debt - 225 * USDX_UNIT);
		assert_eq!(held(DOT, 1), 500_531_794);

		// The 500_531_794 units left are valued at 200_212_718 against 275_479_124
		// of debt; the fund covers 50e6 of the shortfall, so the market side
		// cancels 225_479_124. Supplying it is the full settlement: it pays all
		// 500_531_794 units and closes the row.
		let full =
			redeem(RedeemOptions { stable: USDX, ..RedeemOptions::spend(1_000 * USDX_UNIT) })
				.expect("redemption succeeds");
		assert_eq!(full.figures(), (225_479_124, 0, 500_531_794));
		assert_eq!(full.insurance_cover, 50 * USDX_UNIT);
		assert_eq!(vault_status(DOT, USDX, 1), None, "vault settled and closed");
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

/// An ordinary redemption never takes the redeemer below the minimum balance, whatever it
/// requests. The fee is the last debit and the walk may cancel less than planned, so only a plan
/// that keeps the account alive is certain to be payable in full.
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
fn min_balance_kept_by_ordinary_redemption() {
	build_and_execute(|| {
		let balance = 300 * USDX_UNIT;
		let payable = balance - USDX_MIN_BALANCE;
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(open(1, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		mint_stable(USDX, REDEEMER, balance);
		let debt_before = vault_debt(DOT, USDX, 1);

		let limit = dry_redeem(RedeemOptions { stable: USDX, ..RedeemOptions::spend(payable) })
			.expect("the limit is payable");
		assert_eq!(limit.stable_burned, 263_885_993);
		assert_eq!(limit.fee, 36_104_007);
		assert_eq!(limit.stable_burned + limit.fee, payable);
		assert_eq!(limit.collateral_out, 211_108_794);

		// Every request runs against the same fixture and is rolled back afterwards.
		for request in
			[payable + 1, balance - USDX_MIN_BALANCE / 2, balance, balance + 1, Balance::MAX]
		{
			hypothetically!({
				let settled =
					redeem(RedeemOptions { stable: USDX, ..RedeemOptions::spend(request) })
						.expect("redemption succeeds");

				assert_eq!(settled.figures(), limit.figures(), "request {request}");
				assert_eq!(debt_before - vault_debt(DOT, USDX, 1), limit.stable_burned);
				assert_eq!(Assets::balance(USDX, REDEEMER), USDX_MIN_BALANCE, "request {request}");
			});
		}
	});
}

/// A fee-free policy keeps the minimum balance too: the limit comes from the plan, not the fee.
#[test]
fn min_balance_kept_without_fee() {
	build_and_execute(|| {
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(Redemptions::set_redemption_config(
			RuntimeOrigin::root(),
			USDX,
			zero_fee_redemption_config(),
		));
		assert_ok!(open(1, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		let debt_before = vault_debt(DOT, USDX, 1);
		mint_stable(USDX, REDEEMER, 300 * USDX_UNIT);

		let burned = 300 * USDX_UNIT - USDX_MIN_BALANCE;
		let settled =
			redeem(RedeemOptions { stable: USDX, ..RedeemOptions::spend(300 * USDX_UNIT) })
				.expect("redemption succeeds");

		assert_eq!(Assets::balance(USDX, REDEEMER), USDX_MIN_BALANCE);
		assert_eq!(debt_before - vault_debt(DOT, USDX, 1), burned);
		assert_eq!((settled.stable_burned, settled.fee), (burned, 0));
	});
}

/// A recovery redemption may spend the whole balance: its single, fee-free debit can consume the
/// account. A spend that would leave a sub-minimum remainder stops at the minimum instead.
#[test]
fn min_balance_may_be_consumed_by_recovery() {
	build_and_execute(|| {
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(open(1, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		set_price(DOT, RECOVERY_PRICE_ABOVE_PAR);
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
				mint_stable(USDX, REDEEMER, balance);

				assert_ok!(redeem(RedeemOptions { stable: USDX, ..RedeemOptions::spend(request) }));

				assert_eq!(debt_before - vault_debt(DOT, USDX, 1), burned);
				assert_eq!(Assets::balance(USDX, REDEEMER), balance - burned);
				assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
			});
		}
	});
}

/// A recovery spend the balance cannot fund at all reports the balance, not the queue: at
/// exactly the minimum balance, a smaller request can neither preserve nor consume the account.
#[test]
fn min_balance_alone_cannot_fund_recovery() {
	build_and_execute(|| {
		register_branch(DOT, USDX, usdx_branch_config());
		assert_ok!(open(1, DOT, USDX, 1_000 * USDX_UNIT, 500 * USDX_UNIT, rate_pct(5, 100)));
		set_price(DOT, RECOVERY_PRICE_ABOVE_PAR);
		assert_ok!(enter_final_recovery(DOT, USDX, 1));
		mint_stable(USDX, REDEEMER, USDX_MIN_BALANCE);
		let request = USDX_MIN_BALANCE / 2;

		assert_noop!(
			redeem(RedeemOptions { stable: USDX, ..RedeemOptions::spend(request) }),
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

/// A `FinalRecovery` head at or above par redeems at face value plus a bonus capped by the
/// redistribution penalty.
#[test]
fn recovery_bonus_capped_by_penalty() {
	build_and_execute(|| {
		let mut config = example_config();
		// The vault will sit at CR 120%, so the MCR must exceed it.
		config.minimum_collateralization_ratio = FixedU128::from_rational(130u128, 100u128);
		config.initial_collateralization_ratio = FixedU128::from_rational(140u128, 100u128);
		config.safety_collateralization_ratio = FixedU128::from_rational(150u128, 100u128);
		config.redistribution_penalty = Permill::from_percent(10);
		register_branch(DOT, PUSD, config);

		// The test opens above the 140% ICR and then sets 1 DOT = 2 pUSD. At this price, 6_000 DOT
		// has a value of 12_000 pUSD against 10_000 debt, which gives a 120% CR.
		set_price(DOT, FixedU128::from_rational(4, 1));
		assert_ok!(open(1, DOT, PUSD, 6_000 * UNIT, 10_000 * UNIT, rate_pct(1, 1_000)));
		set_price(DOT, FixedU128::from_rational(2, 1));
		assert_ok!(enter_final_recovery(DOT, PUSD, 1));

		mint_stable(PUSD, REDEEMER, 1_000_000 * UNIT);

		let settled = redeem(RedeemOptions::spend(2_000 * UNIT)).expect("redemption succeeds");

		// raw_bonus = 120% - 100% - 1% = 19%, capped by the 10% penalty. So
		// 2_000 * 1.10 = 2_200 pUSD of value, or 1_100 DOT. Recovery redemptions are fee-free.
		assert_eq!(settled.figures(), (2_000 * UNIT, 0, 1_100 * UNIT));
		assert_eq!(vault_debt(DOT, PUSD, 1), 8_000 * UNIT);
		assert_eq!(held(DOT, 1), 4_900 * UNIT);
		assert_eq!(last_recovery_regime(), Some(RecoveryRegime::RecoveryBonus));
	});
}
