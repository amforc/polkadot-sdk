//! Tests for the liquidation waterfall.
//!
//! Standard scenario used throughout (prices in stablecoin per collateral):
//! vault 1 opens with 600 collateral and 500 principal at rate 0.1%; the
//! branch's weighted rate sum floors to zero at these magnitudes
//! (`floor(500 * 0.001) = 0`), so the average-rate upfront fee is zero and
//! the total debt is exactly `D = 500`. Vault 2 (2_000 collateral, 500
//! principal) keeps the branch off the last-vault guard. At price 0.9 vault
//! 1's CR is `540 / 500 = 1.08 < 1.10`.
//!
//! Waterfall constants at `D = 500`, `C = 600`, `p = 0.9`:
//! - seizable value `500 + ceil(5% * 500) = 525`, seized `ceil(525 / 0.9) = 584`, surplus `16`;
//! - keeper reward `min(584, ceil(10_000/0.9) = 11_112, ceil(100/0.9) + floor(0.1% * 584)) = 112`;
//! - resolution collateral `584 - 112 = 472`.
//!
//! At these magnitudes every offset is sub-breakeven for the pool by design:
//! the 472 resolution collateral is worth 424.8 stablecoin against 500 debt
//! cancelled, because keeper compensation is gross of the 5% penalty and the
//! flat 100-value reward alone exceeds the 25 the penalty adds. At realistic
//! scale (§7 below) the same formulas pay the pool above par.

use crate::{
	mock::*, types::BranchConfigUpdate, BranchConfig, BranchMode, DebtCollateral, Error, Event,
	LiquidationConfig, LiquidationOutcome,
};

fn liquidation_branch_config() -> BranchConfig<Balance> {
	let mut config = crate::mock::default_branch_config();
	config.liquidation.redistribution_penalty = Permill::from_percent(10);
	config
}

fn register_branch(collateral: AssetId, stable: StableId, config: BranchConfig<Balance>) {
	register_market_with(collateral, stable, FixedU128::from_rational(5u128, 4u128), config);
}

/// The standard scenario: an underwater vault 1 plus a healthy recipient.
fn setup_underwater_vault() {
	register_branch(DOT, PUSD, liquidation_branch_config());
	assert_ok!(open(1, DOT, PUSD, 600, 500, FixedU128::from_rational(1, 1_000)));
	assert_ok!(open(2, DOT, PUSD, 2_000, 500, FixedU128::from_rational(2, 1_000)));
	set_price(DOT, FixedU128::from_rational(9, 10));
}

const KEEPER: AccountId = 3;
const GENESIS_BALANCE: Balance = 1_000_000_000_000;

/// Builds the expected event outcome from per-path debt and collateral
/// arrays, ordered `[active_pool, keeper_jit, pending_pool, redistribution]`.
fn outcome(
	debt: [Balance; 4],
	collateral: [Balance; 4],
	keeper_reward: Balance,
	owner_surplus: Balance,
) -> LiquidationOutcome<Balance> {
	LiquidationOutcome {
		active_pool: DebtCollateral { debt: debt[0], collateral: collateral[0] },
		keeper_jit: DebtCollateral { debt: debt[1], collateral: collateral[1] },
		pending_pool: DebtCollateral { debt: debt[2], collateral: collateral[2] },
		redistribution: DebtCollateral { debt: debt[3], collateral: collateral[3] },
		keeper_reward,
		owner_surplus,
	}
}

/// Asserts the standard scenario's `VaultLiquidated` event: owner 1 on the
/// (DOT, PUSD) market, liquidated by [`KEEPER`].
fn assert_liquidated_event(outcome: LiquidationOutcome<Balance>) {
	System::assert_has_event(
		Event::<Test>::VaultLiquidated {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			keeper: KEEPER,
			outcome,
		}
		.into(),
	);
}

#[test]
fn market_owns_liquidation_config() {
	build_and_execute(|| {
		let config = liquidation_branch_config();
		let expected = config.liquidation;
		register_branch(DOT, PUSD, config);
		assert_eq!(
			crate::Branches::<Test>::get(DOT, PUSD)
				.expect("registered market")
				.config
				.liquidation,
			expected
		);
	});
}

#[test]
fn registration_rejects_redistribution_penalty_below_offset_penalty() {
	build_and_execute(|| {
		let mut config = liquidation_branch_config();
		config.liquidation.redistribution_penalty = Permill::from_percent(4);
		set_price(DOT, FixedU128::from_rational(5, 4));
		assert_noop!(
			Vaults::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				config,
			),
			crate::Error::<Test>::ConfigOutsideEnvelope
		);
		assert!(crate::Branches::<Test>::get(DOT, PUSD).is_none());
	});
}

#[test]
fn liquidation_redistributes_without_stability_capital() {
	build_and_execute(|| {
		setup_underwater_vault();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// The vault is gone and its debt was fully redistributed.
		assert!(crate::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
		// Keeper reward 112 lands as free native balance.
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE + 112);
		// All 500 debt is redistributed, so the whole lot is priced at the
		// harsher 10% penalty: ceil(550/0.9) = 612 exceeds the 600 held, so the
		// owner keeps nothing rather than the 16 a 5%-priced seizure would have
		// left.
		assert_eq!(Balances::free_balance(1), GENESIS_BALANCE - 600);
		// Resolution collateral 488 is held by the redistribution account.
		let redistribution = Vaults::redistribution_account(&DOT, &PUSD);
		assert_eq!(held(DOT, redistribution), 488);
		// No stability capital was touched.
		assert_eq!(Balances::free_balance(SP_ACCOUNT), GENESIS_BALANCE);

		assert_liquidated_event(outcome([0, 0, 0, 500], [0, 0, 0, 488], 112, 0));
	});
}

#[test]
fn active_pool_capital_precedes_jit() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(1_000);
		mint_stable(PUSD, KEEPER, 1_000);

		// A JIT allowance is offered but the active pool covers everything.
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 200, 0));

		// Full offset: the whole resolution collateral 472 goes to the pool,
		// nothing reaches redistribution, and the JIT allowance stays unused.
		assert_eq!(Balances::free_balance(SP_ACCOUNT), GENESIS_BALANCE + 472);
		let redistribution = Vaults::redistribution_account(&DOT, &PUSD);
		assert_eq!(held(DOT, redistribution), 0);
		assert_eq!(stable_balance(PUSD, KEEPER), 1_000);
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE + 112);
		assert_eq!(Balances::free_balance(1), GENESIS_BALANCE - 600 + 16);
		// 500 of the 1_000 mocked capacity was consumed.
		assert_eq!(ActiveSpCapacity::get(), 500);

		assert_liquidated_event(outcome([500, 0, 0, 0], [472, 0, 0, 0], 112, 16));
	});
}

#[test]
fn jit_burns_after_active_pool() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(300);
		mint_stable(PUSD, KEEPER, 500);
		let issuance_before = total_stable(PUSD);

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 1_000, 0));

		// Every offset path is priced at the 5% liquidation penalty, so the
		// 500 debt seizes ceil(525/0.9) = 584 and leaves the owner 16. After the
		// 112 keeper reward, 472 is split by weight 315 : 210 (300 and 200 debt,
		// each at 1.05): active takes floor(472 * 315/525) = 283, JIT
		// floor(472 * 210/525) = 188. The 1 the flooring leaves has no
		// redistributed debt to follow, so the last non-zero offset (JIT)
		// receives it.
		assert_eq!(Balances::free_balance(SP_ACCOUNT), GENESIS_BALANCE + 283);
		assert_eq!(stable_balance(PUSD, KEEPER), 500 - 200);
		assert_eq!(total_stable(PUSD), issuance_before - 200);
		// Keeper reward 112 plus JIT collateral 189.
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE + 112 + 189);
		let redistribution = Vaults::redistribution_account(&DOT, &PUSD);
		assert_eq!(held(DOT, redistribution), 0);

		assert_liquidated_event(outcome([300, 200, 0, 0], [283, 189, 0, 0], 112, 16));
	});
}

// Same 158 : 385 weighting as `pending_deposits_precede_redistribution`,
// reached through the other clamp: JIT is sized by the keeper's reducible
// stablecoin balance, not the allowance.
#[test]
fn jit_sized_by_keeper_balance_not_allowance() {
	build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(PUSD, KEEPER, 150);

		// The 1_000 allowance is capped by the keeper's 150 of funding, and
		// the unfunded 350 redistributes.
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 1_000, 0));

		// 150 offset at 5% weighs 158 and the 350 redistributed at 10% weighs
		// 385; of the 488 left after the keeper, JIT takes
		// floor(488 * 158/543) = 141 and redistribution
		// floor(488 * 385/543) = 345 plus the 2 the flooring leaves.
		assert_eq!(stable_balance(PUSD, KEEPER), 0);
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE + 112 + 141);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 347);

		assert_liquidated_event(outcome([0, 150, 0, 350], [0, 141, 0, 347], 112, 0));
	});
}

#[test]
fn pending_deposits_precede_redistribution() {
	build_and_execute(|| {
		setup_underwater_vault();
		PendingSpCapacity::set(150);

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// 150 debt offset at 5% weighs 158 and the 350 redistributed at 10%
		// weighs 385, so the 543 of value seizes the whole 600 held. Of the 488
		// left after the keeper, pending takes floor(488 * 158/543) = 141 and
		// redistribution floor(488 * 385/543) = 345, plus the 2 the flooring
		// leaves — redistribution has debt, so the remainder follows it.
		assert_eq!(Balances::free_balance(SP_ACCOUNT), GENESIS_BALANCE + 141);
		let redistribution = Vaults::redistribution_account(&DOT, &PUSD);
		assert_eq!(held(DOT, redistribution), 347);
		assert_eq!(PendingSpCapacity::get(), 0);

		assert_liquidated_event(outcome([0, 0, 150, 350], [0, 0, 141, 347], 112, 0));
	});
}

// Same 315 : 210 split as `jit_burns_after_active_pool`, but the junior leg is
// pending capital: with no redistributed debt, the flooring remainder follows
// the last non-zero offset path — pending — instead.
#[test]
fn flooring_remainder_follows_pending_when_no_redistribution() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(300);
		PendingSpCapacity::set(200);

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// Active takes floor(472 * 315/525) = 283, pending
		// floor(472 * 210/525) = 188 plus the 1 the flooring leaves.
		assert_eq!(Balances::free_balance(SP_ACCOUNT), GENESIS_BALANCE + 283 + 189);
		assert_eq!((ActiveSpCapacity::get(), PendingSpCapacity::get()), (0, 0));
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 0);

		assert_liquidated_event(outcome([300, 0, 200, 0], [283, 0, 189, 0], 112, 16));
	});
}

#[test]
fn jit_below_minimum_rejected() {
	build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(PUSD, KEEPER, 50);

		// A 50-unit allowance sits below the 100-unit `minimum_jit_contribution`.
		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 50, 0), Error::<Test>::JitBelowMinimum);
	});
}

#[test]
fn jit_skips_when_system_ask_is_below_minimum() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(450);
		mint_stable(PUSD, KEEPER, 500);
		let keeper_stable_before = stable_balance(PUSD, KEEPER);

		// The active pool leaves only 50 debt, below the market's 100-unit
		// minimum JIT contribution. The keeper is willing and funded, but the
		// system ask itself is dust, so JIT is skipped and liquidation proceeds.
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 500, 1_000));

		assert_eq!(ActiveSpCapacity::get(), 0, "the active leg settled");
		assert_eq!(stable_balance(PUSD, KEEPER), keeper_stable_before, "no dust JIT burn");
		assert!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)) > 0);
		assert!(crate::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
	});
}

#[test]
fn jit_slippage_enforced() {
	build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(PUSD, KEEPER, 200);

		// 200 of the debt is offset by JIT at 5% and the other 300 redistributed
		// at 10%, so the lot is weighted 210 : 330 and the keeper's share is
		// floor(488 * 210/540) = 189 collateral; a 190 floor must revert.
		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 200, 190), Error::<Test>::JitSlippageExceeded);
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 200, 189));
	});
}

// Unlike `jit_slippage_enforced`, the active pool participates here, so
// `settle_active` has already paid the pool 283 of collateral when the JIT
// floor fails: the noop witnesses the transactional unwind of a partial
// settlement. (The mock capacity static is host state, not storage — it stays
// consumed, so no follow-up liquidation is attempted.)
#[test]
fn failed_jit_slippage_rolls_back_pool_settlement() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(300);
		mint_stable(PUSD, KEEPER, 500);

		// The same split as `jit_burns_after_active_pool`: the JIT leg carries
		// 189 of collateral, so a 190 floor fails after the pool settlement.
		assert_noop!(
			liquidate(KEEPER, DOT, PUSD, 1, 1_000, 190),
			Error::<Test>::JitSlippageExceeded
		);
		assert_eq!(Balances::free_balance(SP_ACCOUNT), GENESIS_BALANCE);
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE);
		assert!(crate::Vaults::<Test>::get((DOT, PUSD, 1)).is_some());
	});
}

#[test]
fn unfunded_keeper_contributes_no_jit() {
	build_and_execute(|| {
		setup_underwater_vault();

		// The keeper holds no stablecoin at all, so the allowance quietly
		// resolves to no JIT rather than a failed withdrawal — and the 1_000
		// slippage floor, unsatisfiable against 600 of collateral, is inert:
		// it applies only to an executed JIT trade.
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 200, 1_000));
		let redistribution = Vaults::redistribution_account(&DOT, &PUSD);
		assert_eq!(held(DOT, redistribution), 488);
	});
}

#[test]
fn healthy_vault_rejected() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());
		assert_ok!(open(1, DOT, PUSD, 600, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, PUSD, 2_000, 500, FixedU128::from_rational(2, 1_000)));

		// At the registration price 1.25, vault 1's CR is 1.49 >= MCR.
		assert_noop!(
			liquidate(KEEPER, DOT, PUSD, 1, 0, 0),
			crate::Error::<Test>::VaultNotLiquidatable
		);
	});
}

#[test]
fn cr_exactly_at_mcr_is_not_liquidatable() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());
		assert_ok!(open(1, DOT, PUSD, 550, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, PUSD, 2_000, 500, FixedU128::from_rational(2, 1_000)));

		// Liquidatability is `CR < MCR`, strictly: at par the vault sits
		// exactly on the 1.10 MCR (550 / 500) and stays safe.
		set_price(DOT, FixedU128::from_rational(1, 1));
		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0), Error::<Test>::VaultNotLiquidatable);

		// One tick below, floor(0.999 * 550) = 549 gives CR 1.098 < 1.10.
		set_price(DOT, FixedU128::from_rational(999, 1_000));
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));
		assert!(crate::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
	});
}

// §9 mode rules allow ordinary liquidation in Safety mode; only Frozen blocks
// it (`last_vault.rs`).
#[test]
fn liquidation_allowed_in_safety_mode() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());
		assert_ok!(open(1, DOT, PUSD, 600, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, PUSD, 800, 500, FixedU128::from_rational(2, 1_000)));
		set_price(DOT, FixedU128::from_rational(9, 10));

		// The tighter vault 2 pulls branch TCR to (540 + 720) / 1_000 = 1.26,
		// below the 1.30 safety threshold, while staying healthy itself at
		// CR 1.44.
		assert_eq!(branch_mode(DOT, PUSD), Some(BranchMode::Safety));
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// The waterfall is the standard full-redistribution outcome.
		assert_liquidated_event(outcome([0, 0, 0, 500], [0, 0, 0, 488], 112, 0));
	});
}

#[test]
fn unknown_market_rejected() {
	build_and_execute(|| {
		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0), crate::Error::<Test>::BranchNotFound);
	});
}

#[test]
fn oracle_outage_halts_liquidation() {
	build_and_execute(|| {
		setup_underwater_vault();
		MockOracleAvailable::set(false);

		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0), Error::<Test>::OraclePriceNotAvailable);
	});
}

#[test]
fn market_admin_updates_liquidation_config() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());
		let update = LiquidationConfig {
			offset_penalty: Permill::from_percent(10),
			..liquidation_branch_config().liquidation
		};

		assert_noop!(
			Vaults::set_param(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				BranchConfigUpdate::Liquidation(update)
			),
			crate::Error::<Test>::NotBranchAdmin
		);

		assert_ok!(Vaults::set_param(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			BranchConfigUpdate::Liquidation(update)
		));
		assert_eq!(
			crate::Branches::<Test>::get(DOT, PUSD)
				.expect("registered market")
				.config
				.liquidation,
			update
		);

		assert_noop!(
			Vaults::set_param(
				RuntimeOrigin::signed(ADMIN),
				TOKEN_X,
				PUSD,
				BranchConfigUpdate::Liquidation(liquidation_branch_config().liquidation)
			),
			crate::Error::<Test>::BranchNotFound
		);
	});
}

// Redistribution is the harsher outcome for the borrower, so it must never be
// priced below an offset. Governance cannot raise a market's offset penalty
// above its redistribution penalty.
#[test]
fn offset_penalty_may_not_exceed_redistribution_penalty() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());
		let above = LiquidationConfig {
			// The fixture's redistribution penalty is 10%.
			offset_penalty: Permill::from_percent(11),
			..liquidation_branch_config().liquidation
		};
		assert_noop!(
			Vaults::set_param(
				RuntimeOrigin::signed(ADMIN),
				DOT,
				PUSD,
				BranchConfigUpdate::Liquidation(above)
			),
			crate::Error::<Test>::ConfigOutsideEnvelope
		);

		// Equal is allowed: the two outcomes may cost the borrower the same.
		let equal = LiquidationConfig {
			offset_penalty: Permill::from_percent(10),
			..liquidation_branch_config().liquidation
		};
		assert_ok!(Vaults::set_param(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			BranchConfigUpdate::Liquidation(equal)
		));
	});
}

#[test]
fn redistribution_penalty_may_not_be_lowered_below_offset_penalty() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());

		assert_noop!(
			Vaults::set_param(
				RuntimeOrigin::signed(ADMIN),
				DOT,
				PUSD,
				BranchConfigUpdate::RedistributionPenalty(Permill::from_percent(4)),
			),
			crate::Error::<Test>::ConfigOutsideEnvelope
		);
		assert_eq!(
			crate::Branches::<Test>::get(DOT, PUSD)
				.expect("registered branch")
				.config
				.liquidation
				.redistribution_penalty,
			Permill::from_percent(10)
		);

		assert_ok!(Vaults::set_param(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			BranchConfigUpdate::RedistributionPenalty(Permill::from_percent(5)),
		));
	});
}

// A mixed liquidation prices each path by its own penalty, so redistribution
// recipients receive more collateral per unit of debt than the offsetters do.
#[test]
fn redistribution_earns_more_collateral_per_debt_than_offsets() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(200);

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// 200 offset at 5% weighs 210, 300 redistributed at 10% weighs 330, so
		// the 540 of value seizes exactly ceil(540/0.9) = 600. Of the 488 left
		// after the keeper, active takes floor(488 * 210/540) = 189 and
		// redistribution floor(488 * 330/540) = 298 plus the 1 remainder.
		let sp = Balances::free_balance(SP_ACCOUNT) - GENESIS_BALANCE;
		let redistribution = held(DOT, Vaults::redistribution_account(&DOT, &PUSD));
		assert_eq!((sp, redistribution), (189, 299));

		// Per unit of debt the redistribution recipients do strictly better,
		// which is the whole point of the harsher penalty: 299/300 against
		// 189/200 in collateral, i.e. 0.9967 against 0.945.
		assert!(redistribution * 200 > sp * 300);
	});
}

/// The examples are written in whole tokens, but their arithmetic produces
/// fractions (§8 allocates 262.5 DOT, §7 pays a 6.25 DOT keeper reward). A
/// chain works in raw units, where token decimals make those exact, so every
/// figure is scaled by one six-decimal token's worth of them rather than left
/// at a granularity that cannot express half of one.
const UNIT: Balance = 1_000_000;

/// §8's market: the example's vault sits at CR 120%, so the MCR has to be above
/// that for it to be liquidatable at all. Opening happens at a higher price and
/// the price is dropped to the example's 2 afterwards.
fn example_branch_config() -> crate::BranchConfig<Balance> {
	crate::BranchConfig {
		minimum_collateralization_ratio: FixedU128::from_rational(130u128, 100u128),
		initial_collateralization_ratio: FixedU128::from_rational(140u128, 100u128),
		safety_collateralization_ratio: FixedU128::from_rational(150u128, 100u128),
		minimum_debt: 100,
		// The example carries no upfront fee, so the drawn principal is the debt.
		upfront_fee_period: 0,
		..liquidation_branch_config()
	}
}

/// Numeric example 8: one liquidation split across all four resolution paths,
/// with keeper compensation switched off as the example omits it. Every figure
/// below is the example's, scaled by [`UNIT`].
#[test]
fn example_8_active_pool_jit_pending_and_redistribution() {
	build_and_execute(|| {
		let mut config = example_branch_config();
		config.liquidation = LiquidationConfig {
			offset_penalty: Permill::from_percent(5),
			keeper_flat_compensation_value: 0,
			keeper_percent_compensation: Permill::zero(),
			keeper_compensation_cap_value: 0,
			minimum_jit_contribution: 100,
			redistribution_penalty: Permill::from_percent(10),
		};
		register_branch(DOT, PUSD, config);

		// Open above the 140% ICR, then drop to the example's 1 DOT = 2 pUSD.
		set_price(DOT, FixedU128::from_rational(4, 1));
		// The example's 1_000 pUSD of debt against 600 DOT.
		assert_ok!(open(
			1,
			DOT,
			PUSD,
			600 * UNIT,
			1_000 * UNIT,
			FixedU128::from_rational(1, 1_000)
		));
		// A second vault to receive the redistributed share.
		assert_ok!(open(
			2,
			DOT,
			PUSD,
			100_000 * UNIT,
			1_000 * UNIT,
			FixedU128::from_rational(2, 1_000)
		));
		set_price(DOT, FixedU128::from_rational(2, 1));

		ActiveSpCapacity::set(500 * UNIT);
		PendingSpCapacity::set(100 * UNIT);
		mint_stable(PUSD, KEEPER, 200 * UNIT);
		let sp_before = Balances::free_balance(SP_ACCOUNT);
		let owner_before = Balances::free_balance(1);

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 200 * UNIT, 0));

		// Debt splits 500 active / 200 JIT / 100 pending / 200 redistributed.
		// The three offsets weigh 525 + 210 + 105 = 840 at 1.05 and
		// redistribution 220 at 1.10, so 1_060 pUSD of value seizes
		// 1_060/2 = 530 DOT and leaves the owner 70. With no keeper cut the
		// whole 530 is allocated: 262.5 / 105 / 52.5 / 110, which sums back to
		// 530 exactly — the example has no rounding remainder to assign.
		assert_eq!(Balances::free_balance(1) - owner_before, 70 * UNIT);
		assert_eq!(Balances::free_balance(SP_ACCOUNT) - sp_before, 262_500_000 + 52_500_000);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 110 * UNIT);
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE + 105 * UNIT);
		assert_eq!(stable_balance(PUSD, KEEPER), 0);

		assert_liquidated_event(outcome(
			[500 * UNIT, 200 * UNIT, 100 * UNIT, 200 * UNIT],
			[262_500_000, 105 * UNIT, 52_500_000, 110 * UNIT],
			0,
			70 * UNIT,
		));
	});
}

/// Numeric example 7: the active pool covers the whole debt, so the only other
/// claims on the seized lot are keeper compensation and the owner's surplus.
/// Unlike §8 this one exercises the keeper's flat-plus-percentage formula.
#[test]
fn example_7_liquidation_fully_covered_by_active_stability_pool() {
	build_and_execute(|| {
		let mut config = example_branch_config();
		config.liquidation = LiquidationConfig {
			offset_penalty: Permill::from_percent(5),
			keeper_flat_compensation_value: 2 * UNIT,
			keeper_percent_compensation: Permill::from_rational(1u32, 1_000u32),
			keeper_compensation_cap_value: 100 * UNIT,
			minimum_jit_contribution: 100,
			redistribution_penalty: Permill::from_percent(10),
		};
		register_branch(DOT, PUSD, config);

		set_price(DOT, FixedU128::from_rational(4, 1));
		// The example's 10_000 pUSD of debt against 6_000 DOT.
		assert_ok!(open(
			1,
			DOT,
			PUSD,
			6_000 * UNIT,
			10_000 * UNIT,
			FixedU128::from_rational(1, 1_000)
		));
		assert_ok!(open(
			2,
			DOT,
			PUSD,
			100_000 * UNIT,
			1_000 * UNIT,
			FixedU128::from_rational(2, 1_000)
		));
		set_price(DOT, FixedU128::from_rational(2, 1));

		// 20_000 of active deposits, twice what the debt needs.
		ActiveSpCapacity::set(20_000 * UNIT);
		let sp_before = Balances::free_balance(SP_ACCOUNT);
		let owner_before = Balances::free_balance(1);

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// max_seizable_value = 10_000 * 1.05 = 10_500 pUSD, so 5_250 DOT; the
		// vault holds 6_000, leaving the owner 750. The keeper takes
		// 2/2 = 1 DOT flat plus 0.1% of 5_250 = 5.25 DOT, and the pool takes
		// the remaining 5_243.75 DOT for the whole 10_000 of debt.
		assert_eq!(Balances::free_balance(1) - owner_before, 750 * UNIT);
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE + 6_250_000);
		assert_eq!(Balances::free_balance(SP_ACCOUNT) - sp_before, 5_243_750_000);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 0);

		assert_liquidated_event(outcome(
			[10_000 * UNIT, 0, 0, 0],
			[5_243_750_000, 0, 0, 0],
			6_250_000,
			750 * UNIT,
		));
	});
}
