//! Tests for liquidation policy and economic invariants.
//!
//! Most tests use small whole-unit amounts so expected balances stay easy to inspect.
//! Vault 2 prevents the last-vault guard, which lets each test isolate one liquidation rule.
//! The token-scale test verifies the full waterfall split with economically realistic amounts.

use crate::{
	mock::*,
	tests::{assert_event, vault_events, ONE_YEAR_MS},
	types::BranchConfigUpdate,
	BranchConfig, BranchMode, DebtCollateral, Error, Event, LiquidationConfig, LiquidationOutcome,
	LiquidationTouch,
};

const KEEPER: AccountId = 3;

fn liquidation_branch_config() -> BranchConfig<Balance> {
	let mut config = crate::mock::default_branch_config();
	config.liquidation.redistribution_penalty = Permill::from_percent(10);
	config
}

fn register_branch(collateral: AssetId, stable: StableId, config: BranchConfig<Balance>) {
	register_market_with(collateral, stable, FixedU128::from_rational(5u128, 4u128), config);
}

/// Adds a healthy recipient so waterfall tests can liquidate vault 1.
fn setup_underwater_vault() {
	register_branch(DOT, STABLE, liquidation_branch_config());
	assert_ok!(open(1, DOT, STABLE, 600, 500, FixedU128::from_rational(1, 1_000)));
	assert_ok!(open(2, DOT, STABLE, 2_000, 500, FixedU128::from_rational(2, 1_000)));
	set_price(DOT, FixedU128::from_rational(9, 10));
}

fn set_liquidation_param(who: AccountId, update: BranchConfigUpdate<Balance>) -> DispatchResult {
	Vaults::set_param(RuntimeOrigin::signed(who), DOT, STABLE, update)
}

/// Keeps exact event assertions compact by using liquidation-path order:
/// `[active_pool, keeper_jit, pending_pool, redistribution]`.
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
		touch: LiquidationTouch {
			interest: 0,
			redistribution: DebtCollateral { debt: 0, collateral: 0 },
		},
	}
}

/// The balances a liquidation of vault 1 moves, taken before the call.
struct Ledger {
	owner: Balance,
	keeper: Balance,
	keeper_stable: Balance,
	pool: Balance,
	redistribution: Balance,
}

fn ledger() -> Ledger {
	Ledger {
		owner: collateral_balance(DOT, 1),
		keeper: collateral_balance(DOT, KEEPER),
		keeper_stable: stable_balance(STABLE, KEEPER),
		pool: collateral_balance(DOT, SP_ACCOUNT),
		redistribution: held(DOT, Vaults::redistribution_account(&DOT, &STABLE)),
	}
}

/// Asserts the `VaultLiquidated` event for vault 1 and that the ledger agrees with it: each path's
/// collateral landed where the outcome says, the JIT debt left the keeper, and the owner got the
/// surplus back with the vault deposit.
fn assert_settled(before: &Ledger, outcome: LiquidationOutcome<Balance>) {
	assert_event(Event::VaultLiquidated {
		collateral_id: DOT,
		stable_id: STABLE,
		owner: 1,
		keeper: KEEPER,
		outcome: outcome.clone(),
	});
	assert!(!vault_exists(DOT, STABLE, 1));
	let after = ledger();
	assert_eq!(after.owner - before.owner, outcome.owner_surplus + VAULT_DEPOSIT);
	assert_eq!(after.keeper - before.keeper, outcome.keeper_reward + outcome.keeper_jit.collateral);
	assert_eq!(before.keeper_stable - after.keeper_stable, outcome.keeper_jit.debt);
	assert_eq!(
		after.pool - before.pool,
		outcome.active_pool.collateral + outcome.pending_pool.collateral
	);
	assert_eq!(after.redistribution - before.redistribution, outcome.redistribution.collateral);
}

/// The outcome every JIT-less liquidation of the standard fixture produces: all 500 of debt is
/// redistributed, so the whole lot is priced at the harsher 10% penalty and ceil(550/0.9) = 612
/// exceeds the 600 held. The owner keeps nothing rather than the 16 a 5%-priced seizure would
/// have left, and the 588 after the keeper's 12 follows the debt.
fn full_redistribution() -> LiquidationOutcome<Balance> {
	outcome([0, 0, 0, 500], [0, 0, 0, 588], 12, 0)
}

// Without Stability capital, redistribution must settle all debt and apply its higher borrower
// penalty.
#[test]
fn liquidation_without_jit_allowance_falls_back_to_full_redistribution() {
	ExtBuilder::default().build_and_execute(|| {
		setup_underwater_vault();
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0));

		assert_settled(&before, full_redistribution());
	});
}

// The optional JIT leg fails open: an allowance below the branch minimum skips it, liquidation
// still runs, and the keeper's stablecoin is untouched.
#[test]
fn jit_allowance_below_branch_minimum_falls_back_to_full_redistribution() {
	ExtBuilder::default().build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(STABLE, KEEPER, 500);
		let before = ledger();

		// A 50-unit allowance sits below the 100-unit `minimum_jit_contribution`.
		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 50, 0));

		assert_settled(&before, full_redistribution());
	});
}

// The optional JIT leg fails open: when the keeper's funding clamps the contribution below the
// branch minimum, liquidation still runs and the keeper's stablecoin is untouched.
#[test]
fn jit_clamped_below_minimum_by_funding_falls_back_to_full_redistribution() {
	ExtBuilder::default().build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(STABLE, KEEPER, 50);
		let before = ledger();

		// The allowance clears the minimum, but the keeper's funding clamps it below.
		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 1_000, 0));

		assert_settled(&before, full_redistribution());
	});
}

// An allowance does not reserve stablecoin, so an unfunded keeper resolves to no JIT rather than a
// failed withdrawal, and liquidation still runs.
#[test]
fn unfunded_keeper_jit_falls_back_to_full_redistribution() {
	ExtBuilder::default().build_and_execute(|| {
		setup_underwater_vault();
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 200, 0));

		assert_settled(&before, full_redistribution());
	});
}

// The floor protects an executed trade only; without one it is inert even when no collateral could
// satisfy it, so liquidation still runs.
#[test]
fn unsatisfiable_slippage_floor_without_a_trade_falls_back_to_full_redistribution() {
	ExtBuilder::default().build_and_execute(|| {
		setup_underwater_vault();
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 200, 1_000));

		assert_settled(&before, full_redistribution());
	});
}

// Equal penalties must produce the same borrower loss for offset and redistributed debt.
#[test]
fn full_redistribution_with_surplus() {
	ExtBuilder::default().build_and_execute(|| {
		register_branch(DOT, STABLE, default_branch_config());
		assert_ok!(open(1, DOT, STABLE, 600, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, STABLE, 2_000, 500, FixedU128::from_rational(2, 1_000)));
		set_price(DOT, FixedU128::from_rational(9, 10));
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0));

		// At 5% the full 500 of redistributed debt weighs 525, seizing
		// ceil(525/0.9) = 584 of the 600 held — the owner keeps 16 even though
		// every unit of debt lands on other vaults.
		assert_settled(&before, outcome([0, 0, 0, 500], [0, 0, 0, 572], 12, 16));
	});
}

// Liquidation must settle current debt so accrued interest cannot escape the waterfall, down to
// the single unit a touch rounds a sub-unit accrual up to.
#[test]
fn debt_includes_accrued_interest() {
	ExtBuilder::default().build_and_execute(|| {
		setup_underwater_vault();

		hypothetically!({
			advance_time(1);
			let before = ledger();

			assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0));

			// The touch rounds the millisecond of interest up to one unit, which the liquidation
			// settles.
			let mut expected = outcome([0, 0, 0, 501], [0, 0, 0, 588], 12, 0);
			expected.touch.interest = 1;
			assert_settled(&before, expected);
		});

		advance_time(10 * ONE_YEAR_MS);
		let before = ledger();

		System::reset_events();
		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0));

		// Ten years at 0.1% accrue floor(500 * 0.001 * 10) = 5, so 505 of debt
		// redistributes. Its 10%-penalized debt is 505 + ceil(50.5) = 556, and
		// ceil(556/0.9) = 618 exceeds the 600 held: no owner surplus, and the
		// 588 left after the keeper all follows the redistributed debt.
		let mut expected = outcome([0, 0, 0, 505], [0, 0, 0, 588], 12, 0);
		// The accrued interest rides on the liquidation event instead of its own.
		expected.touch.interest = 5;
		// The branch issues the touch's 5 together with ten years of the other vaults' interest.
		assert_eq!(
			vault_events(),
			vec![
				Event::InterestIssued { collateral_id: DOT, stable_id: STABLE, amount: 15 },
				Event::VaultLiquidated {
					collateral_id: DOT,
					stable_id: STABLE,
					owner: 1,
					keeper: KEEPER,
					outcome: expected.clone(),
				},
			]
		);
		assert_settled(&before, expected);
	});
}

// Active deposits have priority over an optional keeper contribution. This prevents an unnecessary
// JIT burn.
#[test]
fn active_pool_precedes_jit() {
	ExtBuilder::default().active_sp_capacity(1_000).build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(STABLE, KEEPER, 1_000);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 200, 0));

		// Full offset: the whole resolution collateral 572 goes to the pool,
		// nothing reaches redistribution, and the JIT allowance stays unused.
		assert_settled(&before, outcome([500, 0, 0, 0], [572, 0, 0, 0], 12, 16));
		// 500 of the 1_000 mocked capacity was consumed.
		assert_eq!(ActiveSpCapacity::get(), 500);
	});
}

// JIT must cover only the debt left by the active pool to preserve waterfall priority.
#[test]
fn jit_burns_after_active_pool() {
	ExtBuilder::default().active_sp_capacity(300).build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(STABLE, KEEPER, 500);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 1_000, 0));

		// Every offset path is priced at the 5% liquidation penalty, so the
		// 500 debt seizes ceil(525/0.9) = 584 and leaves the owner 16. After the
		// 12 keeper reward, 572 is split by penalized debt 315 : 210 (300 and 200 debt,
		// each at 1.05): active takes floor(572 * 315/525) = 343, JIT
		// floor(572 * 210/525) = 228. The 1 the flooring leaves has no
		// redistributed debt to follow, so the last non-zero offset (JIT)
		// receives it.
		assert_settled(&before, outcome([300, 200, 0, 0], [343, 229, 0, 0], 12, 16));
	});
}

// The keeper's stablecoin balance must limit JIT exposure because an allowance does not reserve
// stablecoin.
#[test]
fn jit_clamped_by_keeper_balance() {
	ExtBuilder::default().build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(STABLE, KEEPER, 150);
		let before = ledger();

		// The 1_000 allowance is capped by the keeper's 150 of funding, and
		// the unfunded 350 redistributes.
		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 1_000, 0));

		// 150 offset at 5% weighs 158 and the 350 redistributed at 10% weighs
		// 385; of the 588 left after the keeper, JIT takes
		// floor(588 * 158/543) = 171 and redistribution
		// floor(588 * 385/543) = 416 plus the 1 the flooring leaves.
		assert_settled(&before, outcome([0, 150, 0, 350], [0, 171, 0, 417], 12, 0));
	});
}

// Pending deposits must absorb debt before redistribution to preserve the specified waterfall.
#[test]
fn pending_precedes_redistribution() {
	ExtBuilder::default().build_and_execute(|| {
		setup_underwater_vault();
		PendingSpCapacity::set(&150);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0));

		// 150 debt offset at 5% weighs 158 and the 350 redistributed at 10%
		// weighs 385, so the 543 of value seizes the whole 600 held. Of the 588
		// left after the keeper, pending takes floor(588 * 158/543) = 171 and
		// redistribution floor(588 * 385/543) = 416, plus the 1 the flooring
		// leaves — redistribution has debt, so the remainder follows it.
		assert_settled(&before, outcome([0, 0, 150, 350], [0, 0, 171, 417], 12, 0));
		assert_eq!(PendingSpCapacity::get(), 0);
	});
}

// The last nonzero offset path must receive the rounding remainder. This preserves collateral when
// no debt goes to redistribution.
#[test]
fn remainder_follows_last_offset() {
	ExtBuilder::default().active_sp_capacity(300).build_and_execute(|| {
		setup_underwater_vault();
		PendingSpCapacity::set(&200);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0));

		// Active takes floor(572 * 315/525) = 343, pending
		// floor(572 * 210/525) = 228 plus the 1 the flooring leaves.
		assert_settled(&before, outcome([300, 0, 200, 0], [343, 0, 229, 0], 12, 16));
		assert_eq!((ActiveSpCapacity::get(), PendingSpCapacity::get()), (0, 0));
	});
}

// The branch minimum gates only the keeper's side of a JIT trade, inclusively: exactly funded JIT
// must remain available.
#[test]
fn jit_executes_when_funding_equals_minimum() {
	ExtBuilder::default().build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(STABLE, KEEPER, 100);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 1_000, 0));

		// The 100 JIT at 5% weighs 105 and the 400 redistributed at 10% weighs 440, so the 545 of
		// value asks for ceil(545/0.9) = 606 and the clamp seizes the whole 600. Of the 588 left
		// after the keeper, JIT takes floor(588 * 105/545) = 113 and redistribution
		// floor(588 * 440/545) = 474 plus the 1 the flooring leaves.
		assert_settled(&before, outcome([0, 100, 0, 400], [0, 113, 0, 475], 12, 0));
		assert_eq!(ActiveSpCapacity::get(), 0);
	});
}

// A system ask below the branch minimum must still execute, because the small residual is the
// waterfall's choice, not the keeper's.
#[test]
fn jit_executes_when_the_ask_is_below_minimum() {
	ExtBuilder::default().build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(&450);
		mint_stable(STABLE, KEEPER, 500);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 500, 0));

		// The active pool leaves only 50 debt, below the 100-unit minimum, and the JIT burns it
		// anyway. Seizure prices the whole 500 of offset debt at 5%: ceil(525/0.9) = 584 leaves
		// the owner 16. Of the 572 after the keeper, the path penalized debts 473 : 53
		// (450 + ceil(22.5) and 50 + ceil(2.5)) give active floor(572 * 473/526) = 514 and JIT
		// floor(572 * 53/526) = 57 plus the 1 the flooring leaves.
		assert_settled(&before, outcome([450, 50, 0, 0], [514, 58, 0, 0], 12, 16));
		assert_eq!(ActiveSpCapacity::get(), 0);
	});
}

// A system ask exactly at the branch minimum must execute: the minimum is inclusive.
#[test]
fn jit_executes_when_the_ask_equals_minimum() {
	ExtBuilder::default().build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(&400);
		mint_stable(STABLE, KEEPER, 500);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 1_000, 0));

		// The active pool leaves exactly the minimum, so JIT burns 100. Both offsets weigh at
		// 5% — 420 : 105 — and 525 of value seizes the standard 584. Of the 572 after the keeper,
		// active takes floor(572 * 420/525) = 457 and JIT floor(572 * 105/525) = 114 plus the 1
		// the flooring leaves.
		assert_settled(&before, outcome([400, 100, 0, 0], [457, 115, 0, 0], 12, 16));
		assert_eq!(ActiveSpCapacity::get(), 0);
	});
}

// The slippage floor must protect the keeper's trade without blocking the liquidation. The
// protection must not exceed the keeper's request: the exact share is accepted, and one above it
// drops the trade while the waterfall proceeds without it. Only the keeper's leg falls out of the
// plan: a pool settlement beside it survives, and the JIT debt moves down the waterfall.
#[test]
fn slippage_floor_drops_only_the_jit_leg() {
	ExtBuilder::default().build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(STABLE, KEEPER, 200);

		hypothetically!({
			let before = ledger();
			assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 200, 228));
			// The 540 of value seizes exactly ceil(540/0.9) = 600: no surplus, and
			// redistribution takes floor(588 * 330/540) = 359 plus the 1 remainder.
			assert_settled(&before, outcome([0, 200, 0, 300], [0, 228, 0, 360], 12, 0));
		});

		hypothetically!({
			ActiveSpCapacity::set(&300);
			mint_stable(STABLE, KEEPER, 300);
			let before = ledger();

			// The same split as `jit_burns_after_active_pool` would pay the JIT
			// 229 of collateral, so a 230 floor drops the trade and its 200 debt
			// redistributes instead.
			assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 1_000, 230));

			// The re-plan weighs 315 : 220 (300 at 1.05 and 200 at 1.10), so 535 of
			// value seizes ceil(535/0.9) = 595 and leaves the owner 5. Of the 583
			// left after the keeper, active takes floor(583 * 315/535) = 343 and
			// redistribution floor(583 * 220/535) = 239 plus the 1 the flooring
			// leaves.
			assert_settled(&before, outcome([300, 0, 0, 200], [343, 0, 0, 240], 12, 5));
			assert_eq!(ActiveSpCapacity::get(), 0, "the active leg settled");
		});

		let before = ledger();
		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 200, 229));
		assert_settled(&before, full_redistribution());
	});
}

// The exact MCR boundary protects healthy debt and permits liquidation below the boundary.
#[test]
fn liquidatable_only_below_mcr() {
	ExtBuilder::default().build_and_execute(|| {
		register_branch(DOT, STABLE, liquidation_branch_config());
		assert_ok!(open(1, DOT, STABLE, 550, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, STABLE, 2_000, 500, FixedU128::from_rational(2, 1_000)));

		// Liquidatability is `CR < MCR`, strictly: at par the vault sits
		// exactly on the 1.10 MCR (550 / 500) and stays safe.
		set_price(DOT, FixedU128::from_u32(1));
		assert_noop!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0), Error::<Test>::VaultNotLiquidatable);

		// One tick below, floor(0.999 * 550) = 549 gives CR 1.098 < 1.10.
		set_price(DOT, FixedU128::from_rational(999, 1_000));
		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0));
		assert!(!vault_exists(DOT, STABLE, 1));
	});
}

// Safety mode must preserve ordinary liquidation so unsafe vaults can improve branch health.
#[test]
fn safety_mode_allows_liquidation() {
	ExtBuilder::default().build_and_execute(|| {
		register_branch(DOT, STABLE, liquidation_branch_config());
		assert_ok!(open(1, DOT, STABLE, 600, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, STABLE, 800, 500, FixedU128::from_rational(2, 1_000)));
		set_price(DOT, FixedU128::from_rational(9, 10));
		let before = ledger();

		// The tighter vault 2 pulls branch TCR to (540 + 720) / 1_000 = 1.26,
		// below the 1.30 safety threshold, while staying healthy itself at
		// CR 1.44.
		assert_eq!(branch_mode(DOT, STABLE), Some(BranchMode::Safety));

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0));

		assert_settled(&before, full_redistribution());
	});
}

// A branch whose every vault is under water must still clear its riskiest vault: liquidation
// works when the recipients are themselves below par.
#[test]
fn branch_below_par_still_liquidates() {
	ExtBuilder::default().build_and_execute(|| {
		register_branch(DOT, STABLE, liquidation_branch_config());
		assert_ok!(open(1, DOT, STABLE, 600, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, STABLE, 600, 500, FixedU128::from_rational(2, 1_000)));
		// Both vaults sit at CR 600 * 0.8 / 500 = 0.96, so branch TCR is 0.96.
		set_price(DOT, FixedU128::from_rational(4, 5));
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0));

		// The 10%-penalty ask ceil(550/0.8) = 688 exceeds the vault's collateral:
		// the whole 600 is seized, the keeper takes ceil(10/0.8) = 13, and the
		// remaining 587 follows the 500 of redistributed debt.
		assert_settled(&before, outcome([0, 0, 0, 500], [0, 0, 0, 587], 13, 0));

		// The sole recipient must receive the complete redistributed debt and collateral.
		assert_ok!(poke(KEEPER, DOT, STABLE, 2));

		let vault = vault(DOT, STABLE, 2);
		assert_eq!(vault.debt.total(), 501 + 500);
		assert_eq!(vault.collateral, 600 + 587);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &STABLE)), 0);

		// Vault 2 is now deeper under water, but as the last stake-bearer it
		// takes the final-recovery path, not another liquidation.
		assert_noop!(
			liquidate(KEEPER, DOT, STABLE, 2, 0, 0),
			Error::<Test>::LastVaultCannotBeLiquidated
		);
	});
}

// Liquidation must reject an unknown branch, a missing vault, a frozen branch, and an unavailable
// oracle before it can change protocol state.
#[test]
fn preconditions_rejected_without_side_effects() {
	ExtBuilder::default().build_and_execute(|| {
		assert_noop!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0), Error::<Test>::BranchNotFound);

		// The branch exists but account 1 never opened a vault on it.
		register_branch(DOT, STABLE, liquidation_branch_config());

		assert_noop!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0), Error::<Test>::VaultNotFound);

		assert_ok!(open(1, DOT, STABLE, 600, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, STABLE, 2_000, 500, FixedU128::from_rational(2, 1_000)));
		set_price(DOT, FixedU128::from_rational(9, 10));

		// Vault 1 is liquidatable here, so only the freeze can reject the call: the protocol must
		// not change branch balances until the freeze ends.
		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, true));
		assert_noop!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0), Error::<Test>::BranchFrozen);
		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, false));

		MockOracleAvailable::set(&false);

		// The pallet propagates the oracle's own error.
		assert_noop!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0), DispatchError::Unavailable);
	});
}

// Redistribution posts debt per unit of the remaining stake. A huge vault liquidated onto a
// one-unit stake needs a per-stake increment beyond `FixedU128`, and the liquidation must fail
// whole rather than post a truncated share that would orphan debt.
#[test]
fn redistribution_beyond_the_per_stake_range_is_rejected() {
	ExtBuilder::default().build_and_execute(|| {
		const DEBT: Balance = 400_000_000_000_000_000_000;
		const COLLATERAL: Balance = 600_000_000_000_000;
		let config = BranchConfig { debt_ceiling: Balance::MAX, ..liquidation_branch_config() };
		register_market_with(DOT, STABLE, FixedU128::from_u32(1_000_000), config);
		assert_ok!(Vaults::set_global_debt_ceiling(RuntimeOrigin::root(), STABLE, Balance::MAX));
		mint_collateral(DOT, 1, COLLATERAL);
		// One unit of collateral is the whole stake left once vault 1 is detached.
		assert_ok!(open(2, DOT, STABLE, 1, 200, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(1, DOT, STABLE, COLLATERAL, DEBT, FixedU128::from_rational(1, 1_000)));
		// Vault 1 drops to a 105% ratio; vault 2 stays far above every threshold.
		set_price(DOT, FixedU128::from_u32(700_000));
		assert!(DEBT > Balance::MAX / FixedU128::DIV, "the per-stake increment must overflow");

		assert_noop!(
			liquidate(KEEPER, DOT, STABLE, 1, 0, 0),
			Error::<Test>::RedistributionWouldOverflow
		);
	});
}

// The offset penalty must not exceed the redistribution penalty, on creation and on either
// update: a higher value would invert the borrower-loss order. Equality is valid because it
// does not, and the accepted update lands in the branch's liquidation config. An administrator
// cannot create policy for a branch that is not registered.
#[test]
fn penalty_order_enforced_on_create_and_update() {
	ExtBuilder::default().build_and_execute(|| {
		assert_noop!(
			set_liquidation_param(
				ADMIN,
				BranchConfigUpdate::OffsetPenalty(Permill::from_percent(10))
			),
			Error::<Test>::BranchNotFound
		);

		let mut config = liquidation_branch_config();
		config.liquidation.redistribution_penalty = Permill::from_percent(4);
		set_price(DOT, FixedU128::from_rational(5, 4));

		assert_noop!(
			Vaults::create_branch(
				RuntimeOrigin::root(),
				DOT,
				STABLE,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				config,
				(),
			),
			Error::<Test>::InvalidBranchConfig(
				BranchConfigDefect::OffsetPenaltyAboveRedistribution
			)
		);

		register_branch(DOT, STABLE, liquidation_branch_config());

		assert_noop!(
			set_liquidation_param(
				ADMIN,
				BranchConfigUpdate::OffsetPenalty(Permill::from_percent(11))
			),
			Error::<Test>::InvalidBranchConfig(
				BranchConfigDefect::OffsetPenaltyAboveRedistribution
			)
		);
		assert_noop!(
			set_liquidation_param(
				ADMIN,
				BranchConfigUpdate::RedistributionPenalty(Permill::from_percent(4))
			),
			Error::<Test>::InvalidBranchConfig(
				BranchConfigDefect::OffsetPenaltyAboveRedistribution
			)
		);

		assert_ok!(set_liquidation_param(
			ADMIN,
			BranchConfigUpdate::OffsetPenalty(Permill::from_percent(10))
		));
		assert_eq!(
			market_config(DOT, STABLE).liquidation,
			LiquidationConfig {
				offset_penalty: Permill::from_percent(10),
				..liquidation_branch_config().liquidation
			}
		);
	});
}

// The keeper is paid out of the offset penalty, so the compensation a liquidation can actually
// hand over may not outgrow what that penalty funds — otherwise an offset burns pool stablecoin
// the seizure no longer covers, with the branch's own administrator naming who collects the
// difference.
#[test]
fn keeper_compensation_above_the_penalty_rejected() {
	ExtBuilder::default().build_and_execute(|| {
		// The mock's smallest vault owes 200 at a 5% offset penalty, so a liquidation seizes
		// 210 and leaves exactly 10 spare.
		register_branch(DOT, STABLE, liquidation_branch_config());
		let exceeds_penalty = || {
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::KeeperCompensationExceedsPenalty)
		};

		assert_noop!(
			set_liquidation_param(ADMIN, BranchConfigUpdate::KeeperFlatCompensationValue(11)),
			exceeds_penalty()
		);
		assert_ok!(set_liquidation_param(
			ADMIN,
			BranchConfigUpdate::KeeperFlatCompensationValue(10)
		));

		// A liquidation pays the capped sum, so a cap inside the penalty is what the keeper
		// collects and the fee above it is unreachable.
		assert_ok!(set_liquidation_param(
			ADMIN,
			BranchConfigUpdate::KeeperCompensationCapValue(10)
		));
		assert_ok!(set_liquidation_param(
			ADMIN,
			BranchConfigUpdate::KeeperFlatCompensationValue(11)
		));

		// The cap is the amount that gets paid, so it must fit the penalty itself.
		assert_noop!(
			set_liquidation_param(ADMIN, BranchConfigUpdate::KeeperCompensationCapValue(11)),
			exceeds_penalty()
		);
	});
}

// The keeper's percentage is charged on the whole seizure while only the penalty inside it pays
// for the keeper, so the two rates — not any one vault's amounts — decide whether the branch
// works. `min_debt` is 200 here, small enough that rounding hides a rate that leaks at scale.
#[test]
fn keeper_percentage_above_the_penalty_rate_rejected() {
	ExtBuilder::default().build_and_execute(|| {
		register_branch(DOT, STABLE, liquidation_branch_config());
		let percent = |numerator: u32| {
			BranchConfigUpdate::KeeperPercentCompensation(Permill::from_rational(
				numerator,
				1_000_000u32,
			))
		};
		let exceeds_penalty =
			|| Error::<Test>::InvalidBranchConfig(BranchConfigDefect::KeeperPercentExceedsPenalty);
		// Price the rate on its own, with no flat fee competing for the same penalty.
		assert_ok!(set_liquidation_param(
			ADMIN,
			BranchConfigUpdate::KeeperFlatCompensationValue(0)
		));

		// A seizure is 1 + 5% per unit of debt and 5% of it is spare, so the rate that exactly
		// exhausts the penalty is 5/105 = 4.7619%.
		assert_ok!(set_liquidation_param(ADMIN, percent(47_619)));
		assert_noop!(set_liquidation_param(ADMIN, percent(47_620)), exceeds_penalty());

		// 5% of a seizure is past the 5% penalty inside it: on a 2_000 debt the keeper would
		// take 105 against a 100 penalty. On the 200 the branch floors at, the same rate takes
		// floor(10.5) = 10 out of a 10 penalty and looks payable.
		assert_noop!(set_liquidation_param(ADMIN, percent(50_000)), exceeds_penalty());
	});
}

/// Uses six-decimal raw units so tests can represent fractional-token outcomes exactly.
const UNIT: Balance = 1_000_000;

/// Raises the MCR above 120% so token-scale fixtures become liquidatable after the price change.
fn token_scale_branch_config(liquidation: LiquidationConfig<Balance>) -> BranchConfig<Balance> {
	BranchConfig {
		minimum_collateralization_ratio: FixedU128::from_rational(130u128, 100u128),
		initial_collateralization_ratio: FixedU128::from_rational(140u128, 100u128),
		safety_collateralization_ratio: FixedU128::from_rational(150u128, 100u128),
		// Token-scale, like every other amount here: the inherited raw-unit floor would put the
		// smallest vault's offset penalty below the keeper's flat fee.
		minimum_debt: 100 * UNIT,
		// This isolates liquidation because the drawn principal equals debt.
		upfront_fee_period: 0,
		liquidation,
		..liquidation_branch_config()
	}
}

/// Opens vault 1 above the 140% ICR at 1 DOT = 4 stablecoin, adds a large recipient vault, then
/// drops the price to 1 DOT = 2 stablecoin.
fn setup_token_scale_vault(
	liquidation: LiquidationConfig<Balance>,
	collateral: Balance,
	debt: Balance,
) {
	register_branch(DOT, STABLE, token_scale_branch_config(liquidation));
	set_price(DOT, FixedU128::from_u32(4));
	assert_ok!(open(1, DOT, STABLE, collateral, debt, FixedU128::from_rational(1, 1_000)));
	assert_ok!(open(
		2,
		DOT,
		STABLE,
		100_000 * UNIT,
		1_000 * UNIT,
		FixedU128::from_rational(2, 1_000)
	));
	set_price(DOT, FixedU128::from_u32(2));
}

// The complete waterfall must preserve relative path allocation and collateral when keeper
// compensation is zero.
#[test]
fn four_way_split() {
	ExtBuilder::default().build_and_execute(|| {
		let liquidation = LiquidationConfig {
			offset_penalty: Permill::from_percent(5),
			keeper_flat_compensation_value: 0,
			keeper_percent_compensation: Permill::zero(),
			keeper_compensation_cap_value: 0,
			minimum_jit_contribution: 100,
			redistribution_penalty: Permill::from_percent(10),
		};
		// 1_000 of stablecoin debt against 600 DOT.
		setup_token_scale_vault(liquidation, 600 * UNIT, 1_000 * UNIT);
		ActiveSpCapacity::set(&(500 * UNIT));
		PendingSpCapacity::set(&(100 * UNIT));
		mint_stable(STABLE, KEEPER, 200 * UNIT);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 200 * UNIT, 0));

		// Debt splits 500 active / 200 JIT / 100 pending / 200 redistributed.
		// The three offsets weigh 525 + 210 + 105 = 840 at 1.05 and
		// redistribution 220 at 1.10, so 1_060 STABLE of value seizes
		// 1_060/2 = 530 DOT and leaves the owner 70. With no keeper cut the
		// whole 530 is allocated: 262.5 / 105 / 52.5 / 110, which sums back to
		// 530 exactly — no rounding remainder to assign.
		assert_settled(
			&before,
			outcome(
				[500 * UNIT, 200 * UNIT, 100 * UNIT, 200 * UNIT],
				[262_500_000, 105 * UNIT, 52_500_000, 110 * UNIT],
				0,
				70 * UNIT,
			),
		);
	});
}

// A liquidated vault's pending redistribution share never moves through its owner: the
// redistribution account ends up holding exactly the new leg either way, whether the leg outgrows
// the share and custody takes the difference, or a pool offset leaves no leg and custody releases
// the share into the seized lot.
#[test]
fn pending_share_settles_in_custody_without_an_owner_round_trip() {
	ExtBuilder::default().build_and_execute(|| {
		register_branch(DOT, STABLE, liquidation_branch_config());
		assert_ok!(open(1, DOT, STABLE, 600, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, STABLE, 2_000, 500, FixedU128::from_rational(2, 1_000)));
		assert_ok!(open(3, DOT, STABLE, 2_000, 500, FixedU128::from_rational(2, 1_000)));
		assert_ok!(open(4, DOT, STABLE, 20_000, 500, FixedU128::from_rational(2, 1_000)));
		set_price(DOT, FixedU128::from_rational(9, 10));
		let custody = Vaults::redistribution_account(&DOT, &STABLE);
		let vault_events_len = vault_events().len();

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0));

		assert_eq!(held(DOT, custody), 588);

		// Vault 2's share is on hold in custody, and larger than the top-up its own liquidation
		// leaves behind: the whole vault redistributes, so custody gains the leg less the share.
		set_price(DOT, FixedU128::from_rational(1, 4));
		let (touched_2, _) = Vaults::vault_after_touch(DOT, STABLE, 2).expect("vault 2 exists");
		let share_2 = touched_2.collateral - vault(DOT, STABLE, 2).collateral;
		assert!(share_2 > 0);
		let owner_held_2 = held(DOT, 2);
		assert_eq!(owner_held_2 + share_2, touched_2.collateral);

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 2, 0, 0));

		let leg_2 = vault_events()
			.into_iter()
			.skip(vault_events_len)
			.find_map(|event| match event {
				Event::VaultLiquidated { owner: 2, outcome, .. } => Some(outcome),
				_ => None,
			})
			.expect("vault 2 liquidated")
			.redistribution
			.collateral;
		assert!(leg_2 > share_2);
		assert_eq!(held(DOT, custody), 588 - share_2 + leg_2);
		assert_eq!(held(DOT, 2), 0);

		// Vault 3's share is also in custody, and a pool covering all of its debt leaves no leg,
		// so custody releases the share into the seized lot instead of receiving anything.
		ActiveSpCapacity::set(&10_000);
		let (touched_3, _) = Vaults::vault_after_touch(DOT, STABLE, 3).expect("vault 3 exists");
		let share_3 = touched_3.collateral - vault(DOT, STABLE, 3).collateral;
		assert!(share_3 > 0);
		let custody_before = held(DOT, custody);
		let pool_before = collateral_balance(DOT, SP_ACCOUNT);

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 3, 0, 0));

		let outcome_3 = vault_events()
			.into_iter()
			.skip(vault_events_len)
			.find_map(|event| match event {
				Event::VaultLiquidated { owner: 3, outcome, .. } => Some(outcome),
				_ => None,
			})
			.expect("vault 3 liquidated");
		assert_eq!(outcome_3.redistribution, DebtCollateral { debt: 0, collateral: 0 });
		assert_eq!(held(DOT, custody), custody_before - share_3);
		assert_eq!(
			collateral_balance(DOT, SP_ACCOUNT) - pool_before,
			outcome_3.active_pool.collateral
		);
	});
}
