use crate::{
	mock::*,
	types::{RecoveryRegime, RedemptionConfig, RedemptionState},
	Error, Event,
};
use stablecoin_primitives::{
	collateralization_ratio, reducible_debit, CollateralRatio, DebtCollateral,
	RecoveryOffsetInterface, RecoveryOffsetResult, VaultInterface,
};

const HOUR_MS: Moment = 3_600 * 1_000;
const ONE_YEAR_MS: Moment = 31_557_600_000;

/// Parks a `final_recovery_vault` of 1_000 collateral and 501 debt at
/// `CR = 520 / 501 ≈ 103.79%`: in `FinalRecovery`, barely above par.
const RECOVERY_PRICE_ABOVE_PAR: FixedU128 = FixedU128::from_rational(52, 100);

/// Debt that leaves a fresh 501-debt vault a 141 residual, below the branch's 200 minimum debt,
/// so the vault turns Dormant.
const DORMANT_FILL_DEBT: Balance = 360;

/// Bound on the `FinalRecovery` queue that [`recovery_queue`] reads; no fixture queues more.
const RECOVERY_QUEUE_LEN_MAX: u32 = 10;

const RECOVERY_BONUS: SettlementKind =
	SettlementKind::Recovery { regime: RecoveryRegime::RecoveryBonus };
const INSURANCE_ADJUSTED: SettlementKind =
	SettlementKind::Recovery { regime: RecoveryRegime::InsuranceAdjusted };

const fn percent(n: u128) -> FixedU128 {
	FixedU128::from_rational(n, 100)
}

/// The stablecoin's stored redemption config, if a branch issues it.
fn stored_config(stable: StableId) -> Option<RedemptionConfig<Balance>> {
	crate::Stablecoins::<Test>::get(stable).map(|info| info.config)
}

/// The registered stablecoin's stored dynamic fee state.
fn fee_state(stable: StableId) -> RedemptionState {
	crate::Stablecoins::<Test>::get(stable)
		.expect("the stablecoin is registered")
		.fee_state
}

/// The STABLE spend, debt plus fee, that buys exactly `debt` in an ordinary redemption now.
fn spend_for_debt(debt: Balance) -> Balance {
	let config = stored_config(STABLE).expect("STABLE is registered");
	let decayed = fee_state(STABLE).dynamic_fee_at(Timestamp::get(), &config);
	let curve = crate::fees::DynamicFeeCurve::try_new(decayed, stablecoin_debt(STABLE), &config)
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
	Vaults::project_redemption_snapshot(&DOT, &STABLE, &who).expect("snapshot").debt
}

/// The `(DOT, STABLE)` vault's collateralization ratio at `price`.
fn vault_cr(who: AccountId, price: FixedU128) -> FixedU128 {
	let Ok(CollateralRatio::Ratio(cr)) = collateralization_ratio(
		&DebtCollateral { debt: vault_debt(DOT, STABLE, who), collateral: held(DOT, who) },
		price,
	) else {
		panic!("fixture must carry debt with a finite CR");
	};
	cr
}

/// The event of a `(DOT, STABLE)` recovery settlement that [`REDEEMER`] paid.
fn recovery_executed(
	stable_burned: Balance,
	insurance_cover: Balance,
	collateral_out: Balance,
	regime: RecoveryRegime,
) -> Event<Test> {
	Event::RecoveryRedemptionExecuted {
		collateral_id: DOT,
		stable_id: STABLE,
		redeemer: REDEEMER,
		stable_burned,
		insurance_cover,
		collateral_out,
		regime,
	}
}

/// The first branch on a coin seeds its redemption config and fee state, and only the last
/// branch to leave clears them, so no deregistration wipes state its siblings still price with.
///
/// Only the coin's first branch carries the per-coin config. Omitting it there or restating it
/// later fails, so no later branch claims a policy it does not own.
#[test]
fn config_lives_until_the_last_market_leaves() {
	ExtBuilder::default()
		.stable_balance(STABLE, REDEEMER, 1_000_000)
		.build_and_execute(|| {
			assert!(stored_config(STABLE).is_none());

			// `create_branch` checks the price first, so the config errors below need a live one.
			set_price(DOT, DEFAULT_PRICE);
			assert_noop!(
				Vaults::create_branch(
					RuntimeOrigin::root(),
					DOT,
					STABLE,
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
					STABLE,
					branch_admins(ADMIN, EMERGENCY_ADMIN),
					default_branch_config(),
					Some(invalid),
				),
				Error::<Test>::InvalidRedemptionConfig
			);

			register_branch(DOT, STABLE, default_branch_config());

			let cfg = stored_config(STABLE).expect("seeded on registration");
			assert_eq!(cfg, default_redemption_config());
			assert_eq!(
				redemption_events_since_last_call(),
				vec![Event::RedemptionConfigSet { stable_id: STABLE, config: cfg }]
			);

			// A redemption drains the branch's only vault and raises the coin's fee, and the
			// owner closes the empty vault, so the branch can leave later.
			assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
			assert_ok!(redeem(RedeemOptions::spend(100_000)));
			assert_eq!(vault_debt(DOT, STABLE, 1), 0);
			assert_ok!(Vaults::close_vault(RuntimeOrigin::signed(1), DOT, STABLE, None));
			let raised = fee_state(STABLE);
			assert!(raised.dynamic_fee > FixedU128::zero());
			redemption_events_since_last_call();

			set_price(TOKEN_X, FixedU128::one());
			assert_noop!(
				Vaults::create_branch(
					RuntimeOrigin::root(),
					TOKEN_X,
					STABLE,
					branch_admins(ADMIN, EMERGENCY_ADMIN),
					default_branch_config(),
					Some(default_redemption_config()),
				),
				Error::<Test>::RedemptionConfigNotExpected
			);
			assert_ok!(Vaults::create_branch(
				RuntimeOrigin::root(),
				TOKEN_X,
				STABLE,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				None,
			));

			// The second branch joins the coin's existing fee state untouched.
			assert_eq!(fee_state(STABLE), raised);
			assert_eq!(redemption_events_since_last_call(), vec![]);

			assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), TOKEN_X, STABLE));

			assert_eq!(fee_state(STABLE), raised);
			assert_eq!(redemption_events_since_last_call(), vec![]);

			assert_ok!(Vaults::remove_branch(RuntimeOrigin::root(), DOT, STABLE));

			// The last branch out clears the config and fee state together.
			assert!(crate::Stablecoins::<Test>::get(STABLE).is_none());
			assert_eq!(
				redemption_events_since_last_call(),
				vec![Event::RedemptionConfigCleared { stable_id: STABLE }]
			);
		});
}

// The terms are held to the minimum before the queue is read. An ordinary target alone cannot
// show this: its own debt check reports the same error. An empty queue would otherwise report
// `NoRedeemableVault`, and the fee-free recovery path, which has no other minimum, would settle
// one unit below it.

/// Builds the default branch with a redeemer funded well above the minimum redemption.
fn below_minimum_fixture() -> ExtBuilder {
	ExtBuilder::default().default_branch().stable_balance(STABLE, REDEEMER, 1_000)
}

/// Asserts that a spend one unit below the minimum redemption amount is refused.
fn assert_below_minimum_rejected() {
	let minimum = default_redemption_config().minimum_redemption_amount;
	assert_noop!(
		redeem(RedeemOptions::spend(minimum - 1)),
		Error::<Test>::BelowMinimumRedemptionAmount
	);
}

#[test]
fn redeem_rejects_below_minimum_on_an_empty_queue() {
	below_minimum_fixture().build_and_execute(assert_below_minimum_rejected);
}

#[test]
fn recovery_redemption_is_held_to_the_minimum() {
	let minimum = default_redemption_config().minimum_redemption_amount;
	below_minimum_fixture()
		.final_recovery_vault(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR)
		.build_and_execute(|| {
			assert_below_minimum_rejected();

			let settled = redeem(RedeemOptions::spend(minimum)).expect("redemption succeeds");

			// Exactly the 100 minimum settles. CR = 520/501 ≈ 103.79% gives a bonus of
			// 2.79%: floor(floor(100 · 1.0279) / 0.52) = floor(102 / 0.52) = 196.
			assert_eq!(settled.figures(), (minimum, 0, 196));
		});
}

/// Each step fails one guard and passes every guard `redeem` checks before it (the coin's
/// registration, the oracle price, then the queue), so each refusal comes from the guard it
/// names. The vault-side frozen gate fires only once all of them pass.
#[test]
fn redeem_guards_reject_in_order() {
	ExtBuilder::default()
		.stable_balance(STABLE, REDEEMER, 1_000)
		.build_and_execute(|| {
			assert_noop!(redeem(RedeemOptions::spend(200)), Error::<Test>::StablecoinNotRegistered);

			// The price is live, but no vault is queued.
			register_branch(DOT, STABLE, default_branch_config());
			assert_noop!(redeem(RedeemOptions::spend(200)), Error::<Test>::NoRedeemableVault);

			assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
			clear_price(DOT);
			assert_noop!(redeem(RedeemOptions::spend(201)), Error::<Test>::OracleUnavailable);

			// A zero price is refused like a missing one.
			set_price(DOT, FixedU128::zero());
			assert_noop!(redeem(RedeemOptions::spend(201)), Error::<Test>::OracleUnavailable);

			set_price(DOT, DEFAULT_PRICE);
			assert_ok!(Vaults::set_governance_frozen(
				RuntimeOrigin::signed(ADMIN),
				DOT,
				STABLE,
				true
			));

			// Frozen-mode enforcement lives vault-side. Projecting the first
			// target's snapshot rejects the frozen branch before any settlement, so nothing
			// needs rolling back.
			assert_noop!(
				redeem(RedeemOptions::spend(200)),
				pallet_vaults::Error::<Test>::BranchFrozen
			);
		});
}

#[test]
fn ordinary_redeems_lowest_rate_vault_first() {
	ExtBuilder::default()
		.default_branch()
		.vault(1, 1_000, 500, percent(1))
		.vault(2, 1_000, 500, percent(2))
		.stable_balance(STABLE, REDEEMER, 1_000_000)
		.build_and_execute(|| {
			let v1_before = vault_debt(DOT, STABLE, 1);
			let v1_held_before = held(DOT, 1);
			let v2_before = vault_debt(DOT, STABLE, 2);
			let v2_held_before = held(DOT, 2);
			let redeemer_before = Assets::balance(STABLE, REDEEMER);
			let fee_dest_before = Assets::balance(STABLE, FEE_DEST);
			let recipient_before = collateral_balance(DOT, RECIPIENT);
			let supply_before = Assets::total_supply(STABLE);

			// Each vault owes 501, 500 plus a one-unit upfront fee, so the coin's debt is
			// 1_002. Cancelling 201, a 20.06% share, lifts the dynamic fee by half that share to
			// 10.03%, and the fee charges the 5.01% mean plus the 0.5% base:
			// `ceil(201 · 0.05515) = 12`. So 213 buys 201 debt, 212 only 200, and 201 debt pays
			// `floor(201 / 1.25) = 160` collateral.
			let settled = redeem(RedeemOptions::spend(213)).expect("redemption succeeds");

			// The lowest-rate vault absorbs the whole fill, in debt and in held
			// collateral; the higher-rate vault is untouched in both.
			assert_eq!(v1_before - vault_debt(DOT, STABLE, 1), 201);
			assert_eq!(v1_held_before - held(DOT, 1), 160);
			assert_eq!(vault_debt(DOT, STABLE, 2), v2_before);
			assert_eq!(held(DOT, 2), v2_held_before);
			assert_eq!(settled.figures(), (201, 12, 160));
			assert_eq!(settled.kind, SettlementKind::Ordinary { steps: 1 });
			// The redeemer pays the debt and the fee, the fee reaches `FEE_DEST`, only the debt
			// is burned, and the recipient receives the collateral.
			assert_eq!(redeemer_before - Assets::balance(STABLE, REDEEMER), 213);
			assert_eq!(Assets::balance(STABLE, FEE_DEST) - fee_dest_before, 12);
			assert_eq!(supply_before - Assets::total_supply(STABLE), 201);
			assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 160);
			// `floor(floor(201e18 / 1_002) / 2)`.
			assert_eq!(
				redemption_events_since_last_call(),
				vec![
					Event::RedemptionDynamicFeeUpdated {
						stable_id: STABLE,
						old_dynamic_fee: FixedU128::zero(),
						new_dynamic_fee: FixedU128::from_inner(100_299_401_197_604_790),
					},
					Event::OrdinaryRedemptionExecuted {
						collateral_id: DOT,
						stable_id: STABLE,
						redeemer: REDEEMER,
						stable_burned: 201,
						collateral_out: 160,
						fee: 12,
						steps: 1,
					},
				]
			);
		});
}

#[test]
fn ordinary_walk_stops_at_step_cap_or_barrier() {
	// Vault 2 has a much higher collateral ratio than vault 1. Thus, a price can put
	// vault 1 below par while vault 2 stays eligible for redemption.
	ExtBuilder::default()
		.default_branch()
		.vault(1, 1_000, 500, percent(1))
		.vault(2, 10_000, 500, percent(2))
		.stable_balance(STABLE, REDEEMER, 1_000_000)
		.build_and_execute(|| {
			// A one-step cap ends the walk with budget and a second target to spare.
			let capped =
				dry_redeem(RedeemOptions { max_steps: 1, ..RedeemOptions::spend(100_000) })
					.expect("vault 1 is redeemable");

			// A caller sees it as a walk that used every step and spent less than its
			// budget.
			assert_eq!(capped.kind, SettlementKind::Ordinary { steps: 1 });
			assert_eq!(capped.stable_burned, projected_full_debt(1));
			assert!(capped.stable_burned + capped.fee < 100_000);

			let filled = dry_redeem(RedeemOptions::spend(200)).expect("vault 1 is redeemable");

			// Budget exhaustion inside the cap is a complete walk that spends the whole
			// budget.
			assert_eq!(filled.kind, SettlementKind::Ordinary { steps: 1 });
			assert_eq!(filled.stable_burned + filled.fee, 200);

			// A Dormant husk at the priority slot and a sunk price.
			assert_ok!(redeem(RedeemOptions::spend(spend_for_debt(DORMANT_FILL_DEBT))));
			assert!(vault_status(DOT, STABLE, 1).expect("vault 1").is_dormant());
			set_price(DOT, FixedU128::from_rational(15, 100));
			assert!(
				vault_cr(2, FixedU128::from_rational(15, 100)) > FixedU128::one(),
				"fixture must keep vault 2 redeemable behind the barrier"
			);

			// The underwater Dormant head is a barrier that ends the walk below
			// the cap.
			assert_noop!(redeem(RedeemOptions::spend(100_000)), Error::<Test>::NoRedeemableVault);

			// Liquidating the husk clears the priority slot.
			liquidate_redistribute_all(1);

			// The barrier is not permanent, and the walk reaches the healthy vault behind
			// it.
			assert!(pallet_vaults::Vaults::<Test>::get((DOT, STABLE, 1)).is_none(), "husk gone");
			let unblocked =
				dry_redeem(RedeemOptions::spend(100_000)).expect("vault 2 is redeemable");
			assert_eq!(unblocked.kind, SettlementKind::Ordinary { steps: 1 });
			assert_eq!(unblocked.stable_burned, projected_full_debt(2));
		});
}

#[test]
fn ordinary_walk_skips_underwater_vaults() {
	// The well-collateralized vaults open first so the thin low-rate vaults don't
	// briefly push the fresh branch into Safety mode at the genesis price.
	ExtBuilder::default()
		.default_branch()
		.vault(3, 3_000, 240, percent(3))
		.vault(4, 3_000, 240, percent(4))
		.vault(1, 250, 240, percent(1))
		.vault(2, 260, 240, percent(2))
		.price(DOT, FixedU128::from_rational(9, 10))
		.stable_balance(STABLE, OTHER_REDEEMER, 1_000_000)
		.build_and_execute(|| {
			let v1_before = vault_debt(DOT, STABLE, 1);
			let v1_held_before = held(DOT, 1);
			let v2_before = vault_debt(DOT, STABLE, 2);
			let v2_held_before = held(DOT, 2);
			let v3_before = vault_debt(DOT, STABLE, 3);
			let v4_before = vault_debt(DOT, STABLE, 4);
			let redeemer_before = Assets::balance(STABLE, OTHER_REDEEMER);
			let supply_before = Assets::total_supply(STABLE);

			let settled = redeem(RedeemOptions {
				who: OTHER_REDEEMER,
				recipient: OTHER_RECIPIENT,
				..RedeemOptions::spend(2_000)
			})
			.expect("redemption succeeds");

			assert_eq!(
				settled.kind,
				SettlementKind::Ordinary { steps: 4 },
				"the walk visits the skipped prefix once"
			);
			assert_eq!(settled.stable_burned, v3_before + v4_before);
			// The underwater prefix (vaults 1-2) is skipped: its debt and held collateral are
			// untouched.
			assert_eq!(vault_debt(DOT, STABLE, 1), v1_before);
			assert_eq!(held(DOT, 1), v1_held_before);
			assert_eq!(vault_debt(DOT, STABLE, 2), v2_before);
			assert_eq!(held(DOT, 2), v2_held_before);
			// The healthy vaults behind it drain fully.
			assert_eq!(vault_debt(DOT, STABLE, 3), 0);
			assert_eq!(vault_debt(DOT, STABLE, 4), 0);
			// Collateral is paid at face value, floored per step at price 0.9 (= debt * 10 / 9).
			assert_eq!(settled.collateral_out, v3_before * 10 / 9 + v4_before * 10 / 9);
			// The walk ends well inside the 2_000 budget, so the redeemer is refunded
			// everything but the burned debt and its fee.
			assert!(settled.stable_burned + settled.fee < 2_000);
			assert_eq!(
				redeemer_before - Assets::balance(STABLE, OTHER_REDEEMER),
				settled.stable_burned + settled.fee
			);
			assert_eq!(supply_before - Assets::total_supply(STABLE), settled.stable_burned);
		});
}

/// A partial fill scales the slippage floor by the share of the budget spent, so a floor set for
/// the full budget does not reject a smaller fill.
#[test]
fn slippage_floor_scales_to_partial_fill() {
	ExtBuilder::default()
		.default_branch()
		.vault(1, 1_000, 500, percent(5))
		.stable_balance(STABLE, REDEEMER, 1_000_000)
		.build_and_execute(|| {
			// The 1_000 offer fills the whole 501 debt for `floor(501 / 1.25) = 400`
			// collateral. Redeeming the coin's whole debt lifts the dynamic fee by 1/2 to 50%,
			// so the fee is `ceil(501 · (25% + 0.5%)) = 128` and the fill spends 629. The floor
			// scales to `floor(min · 629 / 1_000)`: 638 becomes 401 and fails, 637 becomes 400
			// and passes.
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

			assert_eq!(vault_debt(DOT, STABLE, 1), 0);
		});
}

/// A budget whose fee leaves less than the minimum debt reverts: on the terms when the balance
/// covers the budget, on the balance when it does not.
#[test]
fn redeem_rejects_fee_leaving_debt_below_minimum() {
	// `OTHER_REDEEMER` holds less than the budget below.
	ExtBuilder::default()
		.default_branch()
		.vault(1, 1_000, 500, percent(5))
		.stable_balance(STABLE, REDEEMER, 1_000)
		.stable_balance(STABLE, OTHER_REDEEMER, 105)
		.build_and_execute(|| {
			// The 100 minimum is a 19.96% share of the 501 coin debt, so the dynamic fee rises
			// 9.98% and the fee is `ceil(100 · (4.99% + 0.5%)) = 6`: 106 buys exactly 100 debt,
			// 105 only 99.
			let quote = dry_redeem(RedeemOptions::spend(106)).expect("quote");
			assert_eq!((quote.stable_burned, quote.fee), (100, 6));

			assert_noop!(
				redeem(RedeemOptions::spend(105)),
				Error::<Test>::BelowMinimumRedemptionAmount
			);
			// With a balance short of the budget, the balance is what fails.
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
	ExtBuilder::default()
		.stable_balance(STABLE, REDEEMER, 2_000_000)
		.build_and_execute(|| {
			assert_eq!(Redemptions::current_fee_rate(STABLE), None, "no branch issues STABLE yet");

			register_branch(DOT, STABLE, default_branch_config());
			assert_ok!(open(1, DOT, STABLE, 1_000_000, 500_000, percent(5)));

			let base_fee = FixedU128::from(default_redemption_config().base_fee);
			assert_eq!(Redemptions::current_fee_rate(STABLE), Some(base_fee));

			// The 100_000 buys 95_015 of the 500_480 debt. The dynamic fee rises by half of
			// that share, `floor(floor(95_015e18 / 500_480) / 2)`, to 9.4924%. The fee on the
			// 4.7462% mean and the 0.5% base is `ceil(95_015 · 0.0524619) = ceil(4_984.67) =
			// 4_985`. One unit more of debt pays the same fee and costs 100_001.
			let settled = redeem(RedeemOptions::spend(100_000)).expect("redemption succeeds");

			assert_eq!(settled.figures(), (95_015, 4_985, 76_012));
			let raised = FixedU128::from_inner(94_923_873_081_841_432);
			assert_eq!(fee_state(STABLE).dynamic_fee, raised);
			assert_eq!(Redemptions::current_fee_rate(STABLE), Some(raised + base_fee));

			advance_time(24 * HOUR_MS);

			// 24h at the 6h half-life is four whole half-lives:
			// `floor(94_923_873_081_841_432 / 16) = 5_932_742_067_615_089`, plus the base fee.
			let rate = FixedU128::from_inner(5_932_742_067_615_089) + base_fee;
			assert_eq!(Redemptions::current_fee_rate(STABLE), Some(rate));

			set_price(DOT, FixedU128::from_rational(1, 100));

			// The view needs no redeemable vault, which a redemption does.
			assert_noop!(redeem(RedeemOptions::spend(100_000)), Error::<Test>::NoRedeemableVault);
			assert_eq!(Redemptions::current_fee_rate(STABLE), Some(rate));
		});
}

/// The fee belongs to the coin, whichever collateral backs it: a DOT/STABLE redemption raises the
/// fee a TOKEN_X/STABLE redeemer pays next, and the share redeemed counts both branches' debt.
#[test]
fn fee_is_shared_across_markets_on_a_coin() {
	ExtBuilder::default()
		.default_branch()
		.vault(1, 100_000, 50_000, percent(5))
		.branch(TOKEN_X, STABLE, default_branch_config())
		.token_x_balance(2, 200_000)
		.vault(2, 100_000, 50_000, percent(5))
		.stable_balance(STABLE, REDEEMER, 1_000_000)
		.build_and_execute(|| {
			// Both branches' debt is the denominator, so a redemption moves the fee half as far as
			// it would against DOT/STABLE alone.
			assert_eq!(stablecoin_debt(STABLE), 2 * 50_048);

			// Each redemption cancels 10_000. The first lifts the dynamic fee from zero by
			// `10_000 / 100_096 / 2 = 4.995%` and pays `ceil(10_000 · (2.498% + 0.5%)) = 300`.
			// The second starts there, adds `10_000 / 90_096 / 2 = 5.550%`, and pays
			// `ceil(10_000 · (7.770% + 0.5%)) = 828`.
			let dot = redeem(RedeemOptions::spend(10_300)).expect("redemption succeeds");
			let debt_before = vault_debt(TOKEN_X, STABLE, 2);
			let token_x =
				redeem(RedeemOptions { collateral: TOKEN_X, ..RedeemOptions::spend(10_828) })
					.expect("redemption succeeds");

			// The TOKEN_X redeemer, who redeemed nothing, pays the raised rate.
			assert_eq!((dot.stable_burned, dot.fee), (10_000, 300));
			assert_eq!(debt_before - vault_debt(TOKEN_X, STABLE, 2), 10_000);
			assert_eq!((token_x.stable_burned, token_x.fee), (10_000, 828));
		});
}

/// The events of a `(DOT, STABLE)` ordinary redemption by [`REDEEMER`] that settles one vault at
/// the 1.25 default price: the dynamic fee moves from `old` to `new` and `debt` pays `fee`.
fn ordinary_one_step_events(
	(old, new): (FixedU128, FixedU128),
	debt: Balance,
	fee: Balance,
) -> [Event<Test>; 2] {
	[
		Event::RedemptionDynamicFeeUpdated {
			stable_id: STABLE,
			old_dynamic_fee: old,
			new_dynamic_fee: new,
		},
		Event::OrdinaryRedemptionExecuted {
			collateral_id: DOT,
			stable_id: STABLE,
			redeemer: REDEEMER,
			stable_burned: debt,
			collateral_out: debt * 4 / 5,
			fee,
			steps: 1,
		},
	]
}

/// Redeems each `(debt, idle_ms)` in turn, after `idle_ms`, spending exactly its debt plus fee,
/// then rolls everything back. Returns the fees paid, the dynamic fee left, and the events the
/// redemptions deposited.
fn redeem_in_sequence(debts: &[(Balance, Moment)]) -> (Balance, FixedU128, Vec<Event<Test>>) {
	hypothetically!({
		redemption_events_since_last_call();
		let fee_before = Assets::balance(STABLE, FEE_DEST);
		let collateral_before = collateral_balance(DOT, RECIPIENT);
		let total: Balance = debts.iter().map(|&(debt, _)| debt).sum();
		for &(debt, idle_ms) in debts {
			advance_time(idle_ms);
			assert_ok!(redeem(RedeemOptions::spend(spend_for_debt(debt))));
		}
		// Every unit of debt buys the same collateral at the 1.25 price, however it is split.
		assert_eq!(collateral_balance(DOT, RECIPIENT) - collateral_before, total * 4 / 5);
		(
			Assets::balance(STABLE, FEE_DEST) - fee_before,
			fee_state(STABLE).dynamic_fee,
			redemption_events_since_last_call(),
		)
	})
}

/// A worked example through the pallet: 20_000 of the 500_480 coin debt, at once and in halves.
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
	// One vault owes 500_480, 500_000 plus a 480-unit seven-day upfront fee.
	ExtBuilder::default()
		.default_branch()
		.vault(1, 1_000_000, 500_000, percent(5))
		.stable_balance(STABLE, REDEEMER, 2_000_000)
		.build_and_execute(|| {
			assert_eq!(vault_debt(DOT, STABLE, 1), 500_480);
			let zero = FixedU128::zero();

			let (at_once_fee, at_once_dynamic_fee, at_once_events) =
				redeem_in_sequence(&[(20_000, 0)]);

			// `20_000 / 500_480 / 2`, rounded down to `1e-18` at each step.
			let at_once = FixedU128::from_inner(19_980_818_414_322_250);
			assert_eq!(at_once_fee, 300);
			assert_eq!(at_once_dynamic_fee, at_once);
			assert_eq!(at_once_events, ordinary_one_step_events((zero, at_once), 20_000, 300));

			let (halves_fee, halves_dynamic_fee, halves_events) =
				redeem_in_sequence(&[(10_000, 0), (10_000, 0)]);

			// Without idle time, the second half starts from the fee that the first half
			// left and adds `floor(floor(10_000e18 / 490_480) / 2) = 10_194_095_579_840_156` to
			// the first half's `floor(floor(10_000e18 / 500_480) / 2)`.
			let first_half = FixedU128::from_inner(9_990_409_207_161_125);
			let halves = FixedU128::from_inner(20_184_504_787_001_281);
			assert_eq!(halves_fee, 301);
			assert!(halves_dynamic_fee > at_once_dynamic_fee);
			assert_eq!(halves_dynamic_fee, halves);
			assert_eq!(
				halves_events,
				[
					ordinary_one_step_events((zero, first_half), 10_000, 100),
					ordinary_one_step_events((first_half, halves), 10_000, 201),
				]
				.concat()
			);

			let (rested_fee, rested_dynamic_fee, rested_events) =
				redeem_in_sequence(&[(10_000, 0), (10_000, 6 * HOUR_MS)]);

			// The first half's rise decays to 0.4995% over the half-life, then the second
			// half adds `10_000 / ~490_480 / 2`, rounded down to `1e-18` at each step.
			let rested = FixedU128::from_inner(15_188_946_869_078_618);
			assert_eq!(rested_fee, 251);
			assert_eq!(rested_dynamic_fee, rested);
			assert!(rested_dynamic_fee < at_once_dynamic_fee);
			// The second half reports the halved fee, `floor(9_990_409_207_161_125 / 2)`, not
			// the stored one. Six hours of interest on the remaining 490_480 are
			// `ceil(16.79) = 17`, so it adds
			// `floor(floor(10_000e18 / 490_497) / 2) = 10_193_742_265_498_056`.
			let decayed_first_half = FixedU128::from_inner(4_995_204_603_580_562);
			assert_eq!(
				rested_events,
				[
					ordinary_one_step_events((zero, first_half), 10_000, 100),
					ordinary_one_step_events((decayed_first_half, rested), 10_000, 151),
				]
				.concat()
			);
		});
}

#[test]
fn ordinary_walk_serves_the_dormant_slot_first_then_continues() {
	ExtBuilder::default()
		.default_branch()
		.vault(1, 1_000, 500, percent(1))
		.vault(2, 1_000, 500, percent(2))
		.stable_balance(STABLE, REDEEMER, 1_000_000)
		.build_and_execute(|| {
			// A sub-minimum residual parks vault 1 as the dormant redemption target.
			assert_ok!(redeem(RedeemOptions::spend(spend_for_debt(DORMANT_FILL_DEBT))));
			assert!(vault_status(DOT, STABLE, 1).expect("vault 1").is_dormant());
			let v2_before = vault_debt(DOT, STABLE, 2);
			let v1_residual = vault_debt(DOT, STABLE, 1);
			// Cancelling less than the residual keeps the husk's row readable.
			assert_eq!(v1_residual, 141);

			// Dormant vault 1 has left the rate index, so only the Dormant slot, served
			// first, can reach it.
			let settled =
				redeem(RedeemOptions::spend(spend_for_debt(100))).expect("redemption succeeds");

			assert_eq!(v1_residual - vault_debt(DOT, STABLE, 1), 100);
			// Collateral paid is the face-value amount for the debt cancelled.
			assert_eq!(settled.collateral_out, 80);
			// Priority: the ordinary vault behind the Dormant slot is untouched.
			assert_eq!(vault_debt(DOT, STABLE, 2), v2_before);

			assert!(vault_status(DOT, STABLE, 1).expect("vault 1").is_dormant());
			let v1_residual = vault_debt(DOT, STABLE, 1);
			let v2_debt = vault_debt(DOT, STABLE, 2);

			// A budget beyond both targets.
			let settled = redeem(RedeemOptions::spend(100_000)).expect("redemption succeeds");

			// The walk drains the dormant slot and continues into the rate index.
			assert_eq!(settled.kind, SettlementKind::Ordinary { steps: 2 });
			assert_eq!(settled.stable_burned, v1_residual + v2_debt);
			assert_eq!(vault_debt(DOT, STABLE, 1), 0);
			assert_eq!(vault_debt(DOT, STABLE, 2), 0);
		});
}

/// The builder that [`raise_fee_through_token_x`] needs: a TOKEN_X/STABLE branch with one
/// ordinary vault, registered before the `(DOT, STABLE)` branch that the test adds next.
fn token_x_branch_with_vault() -> ExtBuilder {
	ExtBuilder::default()
		.branch(TOKEN_X, STABLE, default_branch_config())
		.token_x_balance(2, 2_000)
		.vault(2, 1_000, 500, percent(5))
		.stable_balance(STABLE, REDEEMER, 1_000_000)
}

/// Raises STABLE's dynamic fee through an ordinary TOKEN_X/STABLE redemption, which a
/// `(DOT, STABLE)` recovery head cannot intercept, and returns the fee state it left.
fn raise_fee_through_token_x() -> RedemptionState {
	assert_ok!(redeem(RedeemOptions { collateral: TOKEN_X, ..RedeemOptions::spend(213) }));
	let raised = fee_state(STABLE);
	assert!(raised.dynamic_fee > FixedU128::zero(), "the redemption raised the fee");
	redemption_events_since_last_call();
	raised
}

#[test]
fn recovery_bonus_improves_vault_cr() {
	// Price 0.52 parks the FinalRecovery vault's CR barely above 100%, the regime where an
	// overpaid bonus could actually damage the vault.
	let price = RECOVERY_PRICE_ABOVE_PAR;
	token_x_branch_with_vault()
		.default_branch()
		.final_recovery_vault(1, 1_000, 500, price)
		.build_and_execute(|| {
			// A live dynamic fee makes "recovery leaves the fee state alone" meaningful.
			let fee_state_before = raise_fee_through_token_x();
			let debt_before = vault_debt(DOT, STABLE, 1);
			let recipient_before = collateral_balance(DOT, RECIPIENT);
			let cr_before = vault_cr(1, price);
			assert!(cr_before > percent(101), "fixture must clear the 1% buffer");

			let settled = redeem(RedeemOptions::spend(200)).expect("redemption succeeds");

			// CR = 520/501 ≈ 103.79%, so the bonus is the mid-range excess
			// CR − 100% − 1% buffer ≈ 2.79% (below the 5% penalty cap):
			// floor(floor(200 · 1.0279) / 0.52) = floor(205 / 0.52) = 394. Recovery is
			// fee-free and leaves the fee state alone.
			assert_eq!(settled.figures(), (200, 0, 394), "recovery bonus payout");
			assert_eq!(settled.kind, RECOVERY_BONUS);
			assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 394);
			assert_eq!(fee_state(STABLE), fee_state_before);
			assert_eq!(vault_debt(DOT, STABLE, 1), debt_before - 200);
			assert_eq!(
				redemption_events_since_last_call(),
				vec![recovery_executed(200, 0, 394, RecoveryRegime::RecoveryBonus)]
			);
			// The 1% buffer keeps the bonus strictly below CR − 100%, so paying it must leave
			// the vault's CR strictly better than before the redemption.
			assert!(
				vault_cr(1, price) > cr_before,
				"recovery-bonus redemption must improve the CR"
			);
		});
}

/// The head settles at a bonus up to the 120% ICR, even above the 110% MCR where it could
/// already exit, so redeemers push it out with a margin rather than one tick above the MCR.
/// Above the ICR, redemptions refuse it and offsets see no target. Once out, it is ordinary.
#[test]
fn recovery_stops_above_icr() {
	ExtBuilder::default()
		.default_branch()
		.final_recovery_vault(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR)
		.stable_balance(STABLE, REDEEMER, 1_000_000)
		.build_and_execute(|| {
			// CR = 550/501 ≈ 109.8%.
			set_price(DOT, FixedU128::from_rational(55u128, 100u128));
			let settled = redeem(RedeemOptions::spend(200)).expect("redemption succeeds");

			// The capped 5% bonus pays floor(210 / 0.55) = 381.
			assert_eq!(settled.collateral_out, 381);
			assert_eq!(settled.kind, RECOVERY_BONUS);
			let debt_before = vault_debt(DOT, STABLE, 1);

			// CR = 619 · 0.60 / 301 ≈ 123.4%, above the ICR.
			set_price(DOT, FixedU128::from_rational(60u128, 100u128));

			// Only the exit remains.
			assert_noop!(
				redeem(RedeemOptions::spend(200)),
				Error::<Test>::FinalRecoveryExitRequired
			);
			assert_ok!(execute_offset(STABLE, 200), RecoveryOffsetResult::NotOffsettable);
			assert_eq!(vault_debt(DOT, STABLE, 1), debt_before);

			// CR = 619 · 0.56 / 301 ≈ 115.2%, above MCR but not ICR.
			set_price(DOT, FixedU128::from_rational(56u128, 100u128));

			// Offsets see a target and a redemption still pays the capped 5% bonus,
			// floor(105 / 0.56) = 187, with no fee.
			assert_eq!(
				hypothetically!(execute_offset(STABLE, 100)),
				Ok(RecoveryOffsetResult::Applied { collateral_out: 187 })
			);
			let settled = redeem(RedeemOptions::spend(100)).expect("redemption succeeds");
			assert_eq!(settled.figures(), (100, 0, 187));
			assert_eq!(settled.kind, RECOVERY_BONUS);
			assert_eq!(vault_debt(DOT, STABLE, 1), 201);
			assert!(vault_status(DOT, STABLE, 1).expect("vault 1").is_final_recovery());

			// CR = 432 · 0.56 / 201 ≈ 120.4%, so the head exits.
			assert_ok!(Vaults::exit_final_recovery(
				RuntimeOrigin::signed(REDEEMER),
				DOT,
				STABLE,
				1,
				pallet_linked_list::Position::endpoints_only()
			));
			let settled =
				redeem(RedeemOptions::spend(spend_for_debt(200))).expect("redemption succeeds");

			// Ordinary again, the branch's one vault settles at face value
			// floor(200 / 0.56) = 357, and the fee is charged.
			assert_eq!(settled.kind, SettlementKind::Ordinary { steps: 1 });
			assert_eq!(settled.collateral_out, 357);
			assert!(settled.fee > 0);
		});
}

/// A sub-minimum head past the gate may be unable to exit while another vault
/// holds the Dormant slot, so it stays settleable, at face value.
#[test]
fn recovery_sub_minimum_head_above_mcr_settles_at_face_value() {
	ExtBuilder::default()
		.default_branch()
		.final_recovery_vault(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR)
		.stable_balance(STABLE, REDEEMER, 1_000_000)
		.build_and_execute(|| {
			let debt = vault_debt(DOT, STABLE, 1);
			assert_ok!(redeem(RedeemOptions::spend(debt - 150)));
			assert_eq!(vault_debt(DOT, STABLE, 1), 150);
			set_price(DOT, DEFAULT_PRICE);
			// Offsets reach the head too, at the same price as the redemption below.
			assert_eq!(
				hypothetically!(execute_offset(STABLE, 100)),
				Ok(RecoveryOffsetResult::Applied { collateral_out: 80 })
			);

			let settled = redeem(RedeemOptions::spend(100)).expect("redemption succeeds");

			// No bonus, floor(100 / 1.25) = 80, and no fee.
			assert_eq!(settled.figures(), (100, 0, 80));
			assert_eq!(settled.kind, RECOVERY_BONUS);
			assert_eq!(vault_debt(DOT, STABLE, 1), 50);
			assert!(vault_status(DOT, STABLE, 1).expect("vault 1").is_final_recovery());
		});
}

#[test]
fn recovery_head_served_before_ordinary_vaults() {
	// The ordinary vault opens at a healthy price, and the settlement runs at 0.54, where
	// the head's CR ≈ 107.8% is below the 110% exit gate.
	let price = FixedU128::from_rational(54u128, 100u128);
	ExtBuilder::default()
		.default_branch()
		.final_recovery_vault(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR)
		.price(DOT, DEFAULT_PRICE)
		.vault(2, 1_000, 500, percent(5))
		.price(DOT, price)
		.stable_balance(STABLE, REDEEMER, 1_000_000)
		.build_and_execute(|| {
			let v1_before = vault_debt(DOT, STABLE, 1);
			let v2_before = vault_debt(DOT, STABLE, 2);
			// Beyond the 106% at which the cap starts binding, the bonus must come out clamped
			// to exactly the 5% redistribution penalty rather than the raw excess.
			let cr = vault_cr(1, price);
			assert!(cr > percent(106), "fixture must put the raw excess above the cap");
			assert!(cr < percent(110), "fixture must keep the head below the exit gate");

			let settled = redeem(RedeemOptions::spend(200)).expect("redemption succeeds");

			// The FinalRecovery vault is served at its exact regime, before any ordinary
			// vault: capped 5% bonus, floor(200 * 1.05 / 0.54) = 388 collateral, no fee.
			assert_eq!(settled.kind, RECOVERY_BONUS);
			assert_eq!(settled.figures(), (200, 0, 388));
			assert_eq!(v1_before - vault_debt(DOT, STABLE, 1), 200);
			// The ordinary vault is untouched.
			assert_eq!(vault_debt(DOT, STABLE, 2), v2_before);
		});
}

/// The collateral value that sizes the Insurance Fund cover rounds up: the fund covers no more
/// than the true shortfall, and the redeemer pays no less than the collateral is worth.
#[test]
fn insurance_cover_rounds_collateral_value_up() {
	// 1_000 units at 0.4005 are worth 400.5 STABLE against 501 of debt.
	ExtBuilder::default()
		.default_branch()
		.final_recovery_vault(1, 1_000, 500, FixedU128::from_rational(4_005u128, 10_000u128))
		.stable_balance(STABLE, insurance_account(STABLE), 1_000)
		.stable_balance(STABLE, REDEEMER, 1_000_000)
		.build_and_execute(|| {
			assert_eq!(vault_debt(DOT, STABLE, 1), 501);
			let recipient_before = collateral_balance(DOT, RECIPIENT);
			let supply_before = Assets::total_supply(STABLE);

			let settled = redeem(RedeemOptions::spend(1_000)).expect("redemption succeeds");

			// The value rounds up to 401, so the flush fund covers a shortfall of 100
			// rather than 101, and the redeemer pays 401 for collateral worth 400.5: the half
			// unit rounds against the redeemer, never against the fund.
			assert_eq!(settled.figures(), (401, 0, 1_000));
			assert_eq!(settled.insurance_cover, 100);
			assert_eq!(settled.kind, INSURANCE_ADJUSTED);
			// The redeemer pays the 401 and the fund the 100 cover; both are burned.
			assert_eq!(Assets::balance(STABLE, REDEEMER), 1_000_000 - 401);
			assert_eq!(Assets::balance(STABLE, insurance_account(STABLE)), 900);
			assert_eq!(supply_before - Assets::total_supply(STABLE), 501);
			assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 1_000);
			assert_eq!(vault_status(DOT, STABLE, 1), None, "vault settled and closed");
		});
}

#[test]
fn config_update_stores_and_validates() {
	ExtBuilder::default().build_and_execute(|| {
		assert_noop!(
			Redemptions::set_redemption_config(
				RuntimeOrigin::root(),
				STABLE,
				default_redemption_config()
			),
			Error::<Test>::StablecoinNotRegistered
		);
		register_branch(DOT, STABLE, default_branch_config());
		redemption_events_since_last_call();

		// Every field differs from the seeded default, so a field that the call failed to store
		// would show.
		let cfg = RedemptionConfig {
			minimum_redemption_amount: 250,
			dynamic_fee_decay_period: 12 * HOUR_MS,
			dynamic_fee_floor: percent(1),
			dynamic_fee_ceiling: percent(50),
			base_fee: Permill::from_percent(1),
			fee_ceiling: Permill::from_percent(60),
			dynamic_fee_increase_divisor: FixedU128::from_u32(4),
			final_recovery_bonus_buffer: Permill::from_percent(2),
		};

		assert_ok!(Redemptions::set_redemption_config(RuntimeOrigin::root(), STABLE, cfg.clone()));

		assert_eq!(stored_config(STABLE), Some(cfg.clone()));
		assert_eq!(
			redemption_events_since_last_call(),
			vec![Event::RedemptionConfigUpdated {
				stable_id: STABLE,
				old_config: default_redemption_config(),
				new_config: cfg.clone(),
			}]
		);

		// One mutation per invalid-config axis `is_valid` guards.
		let assert_invalid = |bad: RedemptionConfig<Balance>| {
			assert_noop!(
				Redemptions::set_redemption_config(RuntimeOrigin::root(), STABLE, bad),
				Error::<Test>::InvalidRedemptionConfig
			);
		};
		assert_invalid(RedemptionConfig {
			dynamic_fee_floor: FixedU128::one(),
			dynamic_fee_ceiling: FixedU128::zero(),
			..cfg.clone()
		});
		assert_invalid(RedemptionConfig { minimum_redemption_amount: 0, ..cfg.clone() });
		assert_invalid(RedemptionConfig { dynamic_fee_decay_period: 0, ..cfg.clone() });
		assert_invalid(RedemptionConfig {
			base_fee: Permill::one(),
			fee_ceiling: Permill::zero(),
			..cfg.clone()
		});
		assert_invalid(RedemptionConfig { dynamic_fee_increase_divisor: FixedU128::zero(), ..cfg });
	});
}

/// Only Root and the coin's `pallet-assets` owner set the redemption config, as Vaults requires
/// for `create_branch`. The config governs every branch on the coin, so no branch admin may:
/// the DOT/STABLE admin would otherwise price TOKEN_X/STABLE redemptions.
#[test]
fn config_update_requires_coin_owner_or_root() {
	let other_admin: AccountId = 55;
	// A Root-created branch charges its full admin the custody seed.
	ExtBuilder::default()
		.default_branch()
		.token_x_balance(other_admin, 2)
		.build_and_execute(|| {
			set_price(TOKEN_X, FixedU128::one());
			assert_ok!(Vaults::create_branch(
				RuntimeOrigin::root(),
				TOKEN_X,
				STABLE,
				branch_admins(other_admin, other_admin),
				default_branch_config(),
				None,
			));
			let mut cfg = stored_config(STABLE).unwrap();
			cfg.minimum_redemption_amount = 250;

			// This branch's full admin, its emergency admin, and the sibling
			// branch's full admin.
			assert_noop!(
				Redemptions::set_redemption_config(
					RuntimeOrigin::signed(ADMIN),
					STABLE,
					cfg.clone()
				),
				BadOrigin
			);
			assert_noop!(
				Redemptions::set_redemption_config(
					RuntimeOrigin::signed(EMERGENCY_ADMIN),
					STABLE,
					cfg.clone()
				),
				BadOrigin
			);
			assert_noop!(
				Redemptions::set_redemption_config(
					RuntimeOrigin::signed(other_admin),
					STABLE,
					cfg.clone()
				),
				BadOrigin
			);

			use frame::traits::fungibles::roles::Inspect as _;
			assert_eq!(Assets::owner(STABLE), Some(STABLECOIN_OWNER));
			assert_ok!(Redemptions::set_redemption_config(
				RuntimeOrigin::signed(STABLECOIN_OWNER),
				STABLE,
				cfg
			));

			assert_eq!(stored_config(STABLE).unwrap().minimum_redemption_amount, 250);
		});
}

#[test]
fn insurance_fund_drains_head_by_head() {
	// At recovery price 0.10 both vaults sit below 100% CR (insurance-adjusted). Another
	// coin's fund account holds STABLE too, but it is no cover for this branch.
	let rp = FixedU128::from_rational(1u128, 10u128);
	ExtBuilder::default()
		.default_branch()
		.final_recovery_vault(1, 1_000, 500, rp)
		.final_recovery_vault(2, 1_000, 500, rp)
		.stable_balance(STABLE, insurance_account(USDX), 1_000_000)
		.stable_balance(STABLE, REDEEMER, 10_000)
		.build_and_execute(|| {
			// Both vaults are in the FinalRecovery FIFO, oldest first; only the head is exposed
			// to redemption.
			assert_eq!(recovery_queue(), vec![1, 2]);
			let debt1 = vault_debt(DOT, STABLE, 1);
			let debt2 = vault_debt(DOT, STABLE, 2);
			// 500 principal + the 1-unit 7-day upfront fee each.
			assert_eq!(debt1, 501);
			assert_eq!(debt2, 501);
			// Fund the Insurance Fund to exactly cover vault 1's shortfall, so the head drains
			// it and the next head must settle against a then-empty fund.
			let collateral_value_1 = rp.saturating_mul_int(held(DOT, 1));
			let shortfall_1 = debt1 - collateral_value_1;
			mint_stable(STABLE, insurance_account(STABLE), shortfall_1);

			// The first transaction settles only the FIFO head (vault 1).
			let first = redeem(RedeemOptions::spend(10_000)).expect("redemption succeeds");

			// The full payout leaves neither debt nor collateral, so the vault closes and
			// its row is removed.
			assert_eq!(vault_status(DOT, STABLE, 1), None, "head settled and closed");
			assert!(pallet_vaults::Vaults::<Test>::get((DOT, STABLE, 1)).is_none());
			assert_eq!(held(DOT, 1), 0);
			// A below-par owner has no equity claim on any of the collateral.
			System::assert_has_event(RuntimeEvent::Vaults(pallet_vaults::Event::VaultClosed {
				collateral_id: DOT,
				stable_id: STABLE,
				owner: 1,
				recipient: 1,
				collateral: 0,
			}));
			// The fund covered vault 1 exactly, so the redeemer paid only the collateral-backed
			// portion, at par, and the full settlement pays the vault's entire 1_000
			// collateral.
			assert_eq!(first.figures(), (collateral_value_1, 0, 1_000));
			assert_eq!(first.insurance_cover, shortfall_1);
			assert_eq!(first.kind, INSURANCE_ADJUSTED);
			assert_eq!(
				Assets::balance(STABLE, insurance_account(STABLE)),
				0,
				"head 1 drained the fund"
			);
			// Head-only: vault 2 is untouched and becomes the new FIFO head.
			assert_eq!(vault_debt(DOT, STABLE, 2), debt2);
			assert!(vault_status(DOT, STABLE, 2).expect("vault 2").is_final_recovery());
			assert_eq!(recovery_queue(), vec![2]);

			// The second transaction settles vault 2 against the now-empty fund.
			let second = redeem(RedeemOptions::spend(10_000)).expect("redemption succeeds");

			// The recovery rate falls to C/D, so the redeemer covers the entire debt and
			// STABLE holders absorb the shortfall. A full settlement pays the whole collateral,
			// so this vault closes exactly like vault 1 did.
			assert_eq!(vault_status(DOT, STABLE, 2), None, "second head settled and closed");
			assert!(pallet_vaults::Vaults::<Test>::get((DOT, STABLE, 2)).is_none());
			assert_eq!(held(DOT, 2), 0);
			assert!(recovery_queue().is_empty(), "FIFO drained");
			// Empty fund: D = 501 against C = 100, so the redeemer pays all 501 for the vault's
			// 1_000 units.
			assert_eq!(second.figures(), (debt2, 0, 1_000));
			assert_eq!(second.insurance_cover, 0, "no fund left to burn");
			assert_eq!(second.kind, INSURANCE_ADJUSTED);
			assert_eq!(Assets::balance(STABLE, insurance_account(USDX)), 1_000_000);
			assert!(
				second.stable_burned > first.stable_burned,
				"drained fund pushes the loss onto the redeemer"
			);
			assert_eq!(
				redemption_events_since_last_call(),
				vec![
					recovery_executed(
						collateral_value_1,
						shortfall_1,
						1_000,
						RecoveryRegime::InsuranceAdjusted
					),
					recovery_executed(debt2, 0, 1_000, RecoveryRegime::InsuranceAdjusted),
				]
			);
		});
}

#[test]
fn insurance_fund_burned_only_on_full_settlement() {
	// Recovery price 0.40 puts the CR below 100% with a partial Insurance Fund cover.
	ExtBuilder::default()
		.default_branch()
		.final_recovery_vault(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128))
		.stable_balance(STABLE, insurance_account(STABLE), 50)
		.stable_balance(STABLE, REDEEMER, 10_000)
		.build_and_execute(|| {
			let debt_before = vault_debt(DOT, STABLE, 1);
			let recipient_before = collateral_balance(DOT, RECIPIENT);

			let first = redeem(RedeemOptions::spend(200)).expect("redemption succeeds");

			// The fund stays untouched until the market-side debt is gone.
			assert!(
				pallet_vaults::Vaults::<Test>::get((DOT, STABLE, 1)).is_some(),
				"still settling"
			);
			assert!(vault_status(DOT, STABLE, 1).expect("vault 1").is_final_recovery());
			assert_eq!(first.kind, INSURANCE_ADJUSTED);
			// Recovery is fee-free, so 200 cancels 200 of the `D − 50 = 451` market-side debt
			// and buys `floor(200 · 1_000 / 451) = 443` of the 1_000 collateral units.
			assert_eq!(first.figures(), (200, 0, 443));
			assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 443);
			assert_eq!(first.insurance_cover, 0, "fund untouched mid-settlement");
			assert_eq!(Assets::balance(STABLE, insurance_account(STABLE)), 50);
			assert_eq!(debt_before - vault_debt(DOT, STABLE, 1), 200);

			// The price changes before the second transaction.
			set_price(DOT, FixedU128::from_rational(30u128, 100u128));
			// The fill below spends 251 of the 10_000 budget, so the slippage floor scales to
			// `floor(min · 251 / 10_000)`: 22_232 becomes 558 and fails, 22_231 becomes 557,
			// exactly the collateral out, and passes.
			assert_noop!(
				redeem(RedeemOptions {
					min_collateral_out: 22_232,
					..RedeemOptions::spend(10_000)
				}),
				Error::<Test>::SlippageExceeded
			);
			let second = redeem(RedeemOptions {
				min_collateral_out: 22_231,
				..RedeemOptions::spend(10_000)
			})
			.expect("redemption succeeds");

			// The split is re-read from live state. At 0.30 the remaining 557 collateral
			// units are valued at 168 (167.1 rounded up) against 301 of debt. The shortfall
			// rises to 133, the fund still covers only 50, and the market side cancels the
			// other 251. Supplying all of it is the full settlement: it burns the cover in the
			// same step, pays every remaining collateral unit, and closes the row.
			assert_eq!(second.figures(), (251, 0, 557));
			assert_eq!(second.insurance_cover, 50, "cover burned on completion");
			assert_eq!(second.kind, INSURANCE_ADJUSTED);
			assert_eq!(vault_status(DOT, STABLE, 1), None, "vault settled and closed");
			assert_eq!(held(DOT, 1), 0);
			assert_eq!(Assets::balance(STABLE, insurance_account(STABLE)), 0);
		});
}

/// The cover is priced against the fund's balance before the redeemer pays. When the redeemer
/// is the fund itself, its payment drains what the cover was priced against, so the cover
/// withdrawal fails and the whole settlement rolls back.
#[test]
fn insurance_fund_paying_its_own_cover_refuses_settlement() {
	let fund = insurance_account(STABLE);
	// 1_000 units at 0.40 are worth 400 against 501 of debt, a shortfall of 101. The fund
	// covers the 101 shortfall and the 400 market side apart, but not both.
	ExtBuilder::default()
		.default_branch()
		.final_recovery_vault(1, 1_000, 500, FixedU128::from_rational(40u128, 100u128))
		.stable_balance(STABLE, fund, 450)
		.stable_balance(STABLE, REDEEMER, 1_000)
		.build_and_execute(|| {
			// Another redeemer settles in full: the fund pays the whole 101 cover.
			let settled = dry_redeem(RedeemOptions::spend(1_000)).expect("redemption succeeds");
			assert_eq!(settled.figures(), (400, 0, 1_000));
			assert_eq!(settled.insurance_cover, 101);

			// The fund as redeemer prices the same full fill, then pays 400 of its
			// 450 and has only 50 left for the 101 cover.
			assert_noop!(
				redeem(RedeemOptions { who: fund, ..RedeemOptions::spend(1_000) }),
				Error::<Test>::InsuranceFundWithdrawFailed
			);
		});
}

#[test]
fn ordinary_allowed_in_safety_mode() {
	// The branch sits in Safety mode (TCR below 130%) while the vault stays above par.
	ExtBuilder::default()
		.default_branch()
		.vault(1, 1_000, 500, percent(5))
		.price(DOT, FixedU128::from_rational(6u128, 10u128))
		.stable_balance(STABLE, REDEEMER, 1_000_000)
		.build_and_execute(|| {
			assert!(
				branch_tcr(DOT, STABLE) < FixedU128::from_rational(130u128, 100u128),
				"fixture must put the branch below the safety threshold"
			);
			let debt_before = vault_debt(DOT, STABLE, 1);
			let tcr_before = branch_tcr(DOT, STABLE);

			// 223 buys 201 of the 501 coin debt, a 40.12% share, so the dynamic fee rises
			// 20.06% and the fee is `ceil(201 · (10.03% + 0.5%)) = 22`. At 0.6, 201 debt pays
			// `floor(201 / 0.6) = 335` collateral.
			let settled = redeem(RedeemOptions::spend(223)).expect("redemption succeeds");

			assert_eq!(debt_before - vault_debt(DOT, STABLE, 1), 201);
			assert_eq!(settled.figures(), (201, 22, 335));
			// A face-value redemption removes debt and at most equal-value collateral, so a TCR
			// above 1 can only rise: why Safety mode still permits it.
			assert!(branch_tcr(DOT, STABLE) > tcr_before, "redemption must raise TCR");
		});
}

#[test]
fn ordinary_accrues_interest_before_cancelling() {
	// A year at 100% APR makes the accrued interest large.
	ExtBuilder::default()
		.default_branch()
		.vault(1, 1_000_000, 500_000, percent(100))
		.stable_balance(STABLE, REDEEMER, 2_000_000)
		.build_and_execute(|| {
			let debt_at_open = vault_debt(DOT, STABLE, 1);
			advance_time(ONE_YEAR_MS);
			let accrued = projected_full_debt(1);
			assert!(accrued > debt_at_open, "a year of interest must grow the debt");

			let settled = redeem(RedeemOptions::spend(1_011)).expect("redemption succeeds");

			// The redemption cancels from the accrued debt.
			assert_eq!(vault_debt(DOT, STABLE, 1), accrued - 1_005);
			// 1_011 buys 1_005 debt for `floor(1_005 / 1.25) = 804` collateral. The coin debt
			// counts a year of interest accrued without a branch touch, so the dynamic fee rises
			// about `1_005 / 1_009_583 / 2 = 0.0498%` and the fee is
			// `ceil(1_005 · (0.0249% + 0.5%)) = 6`.
			assert_eq!(settled.figures(), (1_005, 6, 804));
		});
}

/// Liquidates `owner`'s DOT/STABLE vault entirely by redistribution.
fn liquidate_redistribute_all(owner: AccountId) {
	Vaults::liquidate(
		RuntimeOrigin::signed(99),
		DOT,
		STABLE,
		owner,
		pallet_vaults::JitTerms { max_stable: 0, min_collateral_out: 0 },
	)
	.expect("liquidation succeeds");
}

/// The `(DOT, STABLE)` `FinalRecovery` FIFO, head first.
fn recovery_queue() -> Vec<AccountId> {
	let queue =
		LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), RECOVERY_QUEUE_LEN_MAX);
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
		&DOT, payment, amount, &RECIPIENT,
	)?;
	if let Err(change) = change.drop_zero() {
		<Assets as FungiblesBalanced<AccountId>>::resolve(&REDEEMER, change)
			.map_err(|_| DispatchError::Other("change does not fit the payer account"))?;
	}
	Ok(result)
}

/// An offset settles the `FinalRecovery` head only while it is offsettable: a zero payment, a
/// below-par head, and a frozen branch settle nothing. A partial offset keeps the head queued, a
/// full one drains it to Dormant and out of the FIFO. Offsets are fee-free: they neither move
/// the dynamic fee nor emit redemption events, and issuance falls by exactly the cancelled debt.
#[test]
fn execute_recovery_offset_settles_only_an_offsettable_head() {
	// At 0.54 both queued vaults sit at CR ≈ 107.8%, inside the recovery-bonus regime
	// offsets are restricted to, and below the 110% exit gate.
	token_x_branch_with_vault()
		.default_branch()
		.final_recovery_vault(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR)
		.final_recovery_vault(3, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR)
		.price(DOT, FixedU128::from_rational(54u128, 100u128))
		.build_and_execute(|| {
			assert_eq!(recovery_queue(), vec![1, 3]);
			// 500 principal + the 1-unit 7-day upfront fee.
			assert_eq!(vault_debt(DOT, STABLE, 1), 501);
			// A live dynamic fee makes "the offset leaves the fee state alone" meaningful.
			let state_before = raise_fee_through_token_x();

			// A zero budget withdraws nothing, so it is a pure storage no-op even though a
			// recovery bonus applies.
			assert_storage_noop!({
				assert_ok!(execute_offset(STABLE, 0), RecoveryOffsetResult::NotOffsettable);
			});

			let debt_before = vault_debt(DOT, STABLE, 1);
			let payer_before = Assets::balance(STABLE, REDEEMER);
			let recipient_before = collateral_balance(DOT, RECIPIENT);

			// A 200 cap partially fills the 501-debt head. 5%-capped bonus:
			// floor(floor(200 · 1.05) / 0.54) = 388.
			assert_ok!(
				execute_offset(STABLE, 200),
				RecoveryOffsetResult::Applied { collateral_out: 388 }
			);

			assert_eq!(fee_state(STABLE), state_before);
			assert_eq!(redemption_events_since_last_call(), vec![]);
			// The partially settled head keeps its place at the FIFO front.
			assert_eq!(vault_debt(DOT, STABLE, 1), debt_before - 200);
			assert!(vault_status(DOT, STABLE, 1).expect("vault 1").is_final_recovery());
			assert_eq!(recovery_queue(), vec![1, 3]);
			assert_eq!(payer_before - Assets::balance(STABLE, REDEEMER), 200);
			assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 388);

			let debt3 = vault_debt(DOT, STABLE, 3);
			let payer_before = Assets::balance(STABLE, REDEEMER);
			let recipient_before = collateral_balance(DOT, RECIPIENT);
			let fee_before = Assets::balance(STABLE, FEE_DEST);
			let issuance_before = Assets::total_supply(STABLE);

			// The 301 left on 612 collateral is CR ≈ 109.8%, still below the exit gate, so the
			// bonus stays capped at 5%: floor(floor(301 · 1.05) / 0.54) = floor(316 / 0.54) = 585.
			assert_ok!(
				execute_offset(STABLE, 10_000),
				RecoveryOffsetResult::Applied { collateral_out: 585 }
			);

			// The drained head flips to Dormant and leaves the FIFO; the next head is
			// untouched.
			assert_eq!(vault_debt(DOT, STABLE, 1), 0);
			assert!(vault_status(DOT, STABLE, 1).expect("vault 1").is_dormant());
			assert_eq!(recovery_queue(), vec![3]);
			assert_eq!(vault_debt(DOT, STABLE, 3), debt3);
			// The burn is fee-free, so issuance falls by exactly the cancelled debt and nothing
			// reaches the fee destination.
			assert_eq!(payer_before - Assets::balance(STABLE, REDEEMER), 301);
			assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 585);
			assert_eq!(Assets::balance(STABLE, FEE_DEST), fee_before);
			assert_eq!(issuance_before - Assets::total_supply(STABLE), 301);

			// Price 0.40 parks the new head below par (CR < 100%), and settlement at a discount
			// stays exclusive to the redemption pathway.
			set_price(DOT, FixedU128::from_rational(40u128, 100u128));
			let held_before = held(DOT, 3);
			let payer_before = Assets::balance(STABLE, REDEEMER);

			// Execution withdraws and refunds the payment, which leaves asset events behind, so
			// the refusal is checked field by field rather than by storage root.
			assert_ok!(execute_offset(STABLE, 10_000), RecoveryOffsetResult::BelowPar);
			assert_eq!(vault_debt(DOT, STABLE, 3), debt3);
			assert_eq!(held(DOT, 3), held_before);
			assert_eq!(Assets::balance(STABLE, REDEEMER), payer_before);
			assert!(vault_status(DOT, STABLE, 3).expect("vault 3").is_final_recovery());

			assert_ok!(Vaults::set_governance_frozen(
				RuntimeOrigin::signed(ADMIN),
				DOT,
				STABLE,
				true
			));

			// Frozen-mode enforcement lives vault-side. The head is still queued, so the offset
			// projects its snapshot, and the projection rejects the frozen branch.
			assert_noop!(execute_offset(STABLE, 200), pallet_vaults::Error::<Test>::BranchFrozen);
		});
}

#[test]
fn offset_ignores_ordinary_vaults() {
	ExtBuilder::default()
		.stable_balance(STABLE, REDEEMER, 1_000)
		.build_and_execute(|| {
			// On an unregistered branch, the target-first fast path answers before the
			// config lookup could report `StablecoinNotRegistered`.
			assert_ok!(execute_offset(STABLE, 1_000), RecoveryOffsetResult::NoTarget);

			register_branch(DOT, STABLE, default_branch_config());
			assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
			let debt_before = vault_debt(DOT, STABLE, 1);

			// An ordinary vault is not an offset target either; offsets exist only for
			// the FinalRecovery FIFO head.
			assert_ok!(execute_offset(STABLE, 1_000), RecoveryOffsetResult::NoTarget);
			assert_eq!(vault_debt(DOT, STABLE, 1), debt_before);
		});
}

#[test]
fn offset_payment_asset_selects_market() {
	// A settleable head in both the STABLE and the sibling USDX branch, both at 0.54,
	// inside the bonus regime and below the exit gate.
	ExtBuilder::default()
		.default_branch()
		.final_recovery_vault(1, 1_000, 500, RECOVERY_PRICE_ABOVE_PAR)
		.branch(DOT, USDX, usdx_branch_config())
		.final_recovery_vault(2, 1_000 * USDX_UNIT, 500 * USDX_UNIT, RECOVERY_PRICE_ABOVE_PAR)
		.price(DOT, FixedU128::from_rational(54u128, 100u128))
		.stable_balance(USDX, REDEEMER, 100 * USDX_UNIT)
		.build_and_execute(|| {
			let stable_debt_before = vault_debt(DOT, STABLE, 1);
			let usdx_debt_before = vault_debt(DOT, USDX, 2);
			let issuance_before = Assets::total_supply(USDX);
			let recipient_before = collateral_balance(DOT, RECIPIENT);

			// The payment's own asset names the branch, so a USDX payment against `DOT`
			// collateral settles the (DOT, USDX) head, and a coin mismatch with some other
			// named branch is unrepresentable.
			let result = execute_offset(USDX, 100 * USDX_UNIT);

			// The USDX head's CR = 540 / 500.48 ≈ 107.9% caps the bonus at 5%:
			// floor(floor(100e6 · 1.05) / 0.54) = floor(194_444_444.4) = 194_444_444.
			assert_eq!(result, Ok(RecoveryOffsetResult::Applied { collateral_out: 194_444_444 }));
			assert_eq!(collateral_balance(DOT, RECIPIENT) - recipient_before, 194_444_444);
			// The USDX head absorbed the full budget; the STABLE branch never moved.
			assert_eq!(vault_debt(DOT, USDX, 2), usdx_debt_before - 100 * USDX_UNIT);
			assert_eq!(vault_debt(DOT, STABLE, 1), stable_debt_before);
			assert_eq!(Assets::balance(USDX, REDEEMER), 0);
			assert_eq!(issuance_before - Assets::total_supply(USDX), 100 * USDX_UNIT);
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
///
/// A fee-free policy keeps the minimum balance too: the limit comes from the plan, not the fee.
#[test]
fn min_balance_kept_by_ordinary_redemption() {
	let balance = 300 * USDX_UNIT;
	let payable = balance - USDX_MIN_BALANCE;
	ExtBuilder::default()
		.branch(DOT, USDX, usdx_branch_config())
		.vault(1, 1_000 * USDX_UNIT, 500 * USDX_UNIT, percent(5))
		.stable_balance(USDX, REDEEMER, balance)
		.build_and_execute(|| {
			let debt_before = vault_debt(DOT, USDX, 1);
			let limit = dry_redeem(RedeemOptions { stable: USDX, ..RedeemOptions::spend(payable) })
				.expect("the limit is payable");
			assert_eq!(limit.stable_burned, 263_885_993);
			assert_eq!(limit.fee, 36_104_007);
			assert_eq!(limit.stable_burned + limit.fee, payable);
			assert_eq!(limit.collateral_out, 211_108_794);

			// Every request runs against the same fixture and is rolled back afterwards.
			let assert_fills_to_limit = |request: Balance| {
				hypothetically!({
					let settled =
						redeem(RedeemOptions { stable: USDX, ..RedeemOptions::spend(request) })
							.expect("redemption succeeds");

					assert_eq!(settled.figures(), limit.figures(), "request {request}");
					assert_eq!(debt_before - vault_debt(DOT, USDX, 1), limit.stable_burned);
					assert_eq!(
						Assets::balance(USDX, REDEEMER),
						USDX_MIN_BALANCE,
						"request {request}"
					);
				});
			};
			// Requests that would leave less than the minimum.
			assert_fills_to_limit(payable + 1);
			assert_fills_to_limit(balance - USDX_MIN_BALANCE / 2);
			// The whole balance.
			assert_fills_to_limit(balance);
			// More than the balance.
			assert_fills_to_limit(balance + 1);
			assert_fills_to_limit(Balance::MAX);

			assert_ok!(Redemptions::set_redemption_config(
				RuntimeOrigin::root(),
				USDX,
				zero_fee_redemption_config(),
			));

			let settled = redeem(RedeemOptions { stable: USDX, ..RedeemOptions::spend(balance) })
				.expect("redemption succeeds");

			assert_eq!(Assets::balance(USDX, REDEEMER), USDX_MIN_BALANCE);
			assert_eq!(debt_before - vault_debt(DOT, USDX, 1), payable);
			assert_eq!((settled.stable_burned, settled.fee), (payable, 0));
		});
}

/// A recovery redemption may spend the whole balance: its single, fee-free debit can consume the
/// account. A spend that would leave a sub-minimum remainder stops at the minimum instead.
///
/// A spend the balance cannot fund at all reports the balance, not the queue: at exactly the
/// minimum balance, a smaller request can neither preserve nor consume the account.
#[test]
fn min_balance_may_be_consumed_by_recovery() {
	ExtBuilder::default()
		.branch(DOT, USDX, usdx_branch_config())
		.final_recovery_vault(1, 1_000 * USDX_UNIT, 500 * USDX_UNIT, RECOVERY_PRICE_ABOVE_PAR)
		.build_and_execute(|| {
			let debt_before = vault_debt(DOT, USDX, 1);
			let request = 200 * USDX_UNIT;
			let assert_recovery_burns = |balance: Balance, burned: Balance| {
				hypothetically!({
					mint_stable(USDX, REDEEMER, balance);

					let settled =
						redeem(RedeemOptions { stable: USDX, ..RedeemOptions::spend(request) })
							.expect("redemption succeeds");

					assert_eq!(settled.kind, RECOVERY_BONUS);
					assert_eq!(settled.stable_burned, burned);
					assert_eq!(debt_before - vault_debt(DOT, USDX, 1), burned);
					assert_eq!(Assets::balance(USDX, REDEEMER), balance - burned);
				});
			};
			// The request is the whole balance and drains the account.
			assert_recovery_burns(request, request);
			// The request would leave 4_000 raw units, less than the 10_000-unit minimum, so the
			// spend stops at the minimum instead.
			assert_recovery_burns(request + 4_000, request - 6_000);

			hypothetically!({
				mint_stable(USDX, REDEEMER, USDX_MIN_BALANCE);

				assert_noop!(
					redeem(RedeemOptions {
						stable: USDX,
						..RedeemOptions::spend(USDX_MIN_BALANCE / 2)
					}),
					Error::<Test>::InsufficientStableBalance
				);
			});
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
	let mut config = example_config();
	// The vault will sit at CR 120%, so the MCR must exceed it.
	config.minimum_collateralization_ratio = FixedU128::from_rational(130u128, 100u128);
	config.initial_collateralization_ratio = FixedU128::from_rational(140u128, 100u128);
	config.safety_collateralization_ratio = FixedU128::from_rational(150u128, 100u128);
	config.liquidation.redistribution_penalty = Permill::from_percent(10);
	// Keeper terms stay on here, so the settlement below prices the collateral the entry reward
	// left in the vault.
	config.liquidation.keeper_flat_compensation_value = 10;
	config.liquidation.keeper_percent_compensation = Permill::from_rational(1u32, 1_000u32);
	// The vault opens above the 140% ICR, and then 1 DOT = 2 STABLE. At this
	// price, 6_000 DOT has a value of 12_000 STABLE against 10_000 debt, which gives a 120% CR.
	ExtBuilder::default()
		.branch(DOT, STABLE, config)
		.price(DOT, FixedU128::from_rational(4, 1))
		.vault(1, 6_000 * UNIT, 10_000 * UNIT, FixedU128::from_rational(1, 1_000))
		.price(DOT, FixedU128::from_rational(2, 1))
		.stable_balance(STABLE, REDEEMER, 1_000_000 * UNIT)
		.build_and_execute(|| {
			// The keeper terms are in raw units, so the entry pays its keeper the cap: a 10_000
			// value is 5_000 units, or 5 DOT, at 2. The 5-unit flat plus 0.1% of the
			// 5_250_000-unit seizure would be more.
			assert_ok!(enter_final_recovery(DOT, STABLE, 1));
			assert_eq!(held(DOT, 1), 6_000 * UNIT - 5_000);

			let settled = redeem(RedeemOptions::spend(2_000 * UNIT)).expect("redemption succeeds");

			// Raw_bonus = 120% - 100% - 1% = 19%, capped by the 10% penalty. So
			// 2_000 * 1.10 = 2_200 STABLE of value, or 1_100 DOT. Recovery redemptions are
			// fee-free.
			assert_eq!(settled.figures(), (2_000 * UNIT, 0, 1_100 * UNIT));
			assert_eq!(settled.kind, RECOVERY_BONUS);
			assert_eq!(vault_debt(DOT, STABLE, 1), 8_000 * UNIT);
			assert_eq!(held(DOT, 1), 4_900 * UNIT - 5_000);
		});
}

fn redeem_actual_weight(max_stable_to_spend: Balance, max_steps: u32) -> frame::prelude::Weight {
	Redemptions::redeem(
		RuntimeOrigin::signed(REDEEMER),
		DOT,
		STABLE,
		crate::RedemptionTerms { max_stable_to_spend, min_collateral_out: 0 },
		RECIPIENT,
		max_steps,
	)
	.expect("the redemption succeeds")
	.actual_weight
	.expect("a redemption reports its actual weight")
}

// A walk that runs out of targets still paid for the lookup that found none, so it is weighed one
// step above the vaults it visited; a walk that stops at its cap does no such lookup. A cap above
// `MaxRedemptionSteps`, or the `0` default, is clamped to it, and the declared weight covers the
// clamped walk or a recovery settlement, whichever weighs more.
#[test]
fn ordinary_walk_weighs_a_trailing_lookup_as_a_step() {
	use crate::{Config, WeightInfo};
	use frame::{deps::frame_support::dispatch::GetDispatchInfo, traits::Get};
	let cap = <Test as Config>::MaxRedemptionSteps::get();
	// One vault more than a walk can reach.
	let vault_count = cap + 1;
	ExtBuilder::default()
		.default_branch()
		.vault(1, 1_000, 500, percent(1))
		.vault(2, 10_000, 500, percent(2))
		.stable_balance(STABLE, REDEEMER, 1_000_000)
		.build_and_execute(|| {
			// Rising rates queue the extra vaults behind vaults 1 and 2, the last opened last.
			let extra_owners = 11..=AccountId::from(vault_count) + 8;
			for (owner, rate) in extra_owners.clone().zip(30u128..) {
				assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), owner, 1_000_000));
				assert_ok!(open(
					owner,
					DOT,
					STABLE,
					1_000,
					500,
					FixedU128::from_rational(rate, 1_000)
				));
			}
			let last = *extra_owners.end();
			let last_debt = vault_debt(DOT, STABLE, last);

			let declared = |max_steps: u32| {
				crate::Call::<Test>::redeem {
					collateral_id: DOT,
					stable_id: STABLE,
					terms: crate::RedemptionTerms {
						max_stable_to_spend: 100_000,
						min_collateral_out: 0,
					},
					recipient: RECIPIENT,
					max_steps,
				}
				.get_dispatch_info()
				.call_weight
			};
			let capped_weight =
				<() as WeightInfo>::redeem(cap).max(<() as WeightInfo>::redeem_recovery());
			assert_eq!(declared(u32::MAX), capped_weight);
			assert_eq!(declared(0), capped_weight);
			assert_eq!(
				declared(1),
				<() as WeightInfo>::redeem(1).max(<() as WeightInfo>::redeem_recovery())
			);

			let capped = hypothetically!(redeem_actual_weight(100_000, 1));
			assert_eq!(capped, <() as WeightInfo>::redeem(1));

			// A cap above the ceiling walks only `cap` vaults with budget to spare.
			let clamped = hypothetically!(redeem(RedeemOptions {
				max_steps: u32::MAX,
				..RedeemOptions::spend(100_000)
			}))
			.expect("the walk is redeemable");
			assert_eq!(clamped.kind, SettlementKind::Ordinary { steps: cap });
			assert_eq!(redeem_actual_weight(100_000, u32::MAX), <() as WeightInfo>::redeem(cap));
			assert_eq!(vault_debt(DOT, STABLE, last), last_debt);

			// The one vault left is a step, and the lookup past it another.
			let exhausted = redeem_actual_weight(100_000, 0);
			assert_eq!(exhausted, <() as WeightInfo>::redeem(2));
			assert!(vault_status(DOT, STABLE, last).is_none_or(|status| status.is_dormant()));
		});
}
