//! Tests for liquidation policy and economic invariants.
//!
//! Most tests use small whole-unit amounts so expected balances stay easy to inspect.
//! Vault 2 prevents the last-vault guard, which lets each test isolate one liquidation rule.
//! The token-scale tests verify the same formulas with economically realistic amounts.

use crate::{
	mock::*,
	tests::{assert_event, ONE_YEAR_MS},
	types::BranchConfigUpdate,
	BranchConfig, BranchMode, DebtCollateral, Error, Event, LiquidationConfig, LiquidationOutcome,
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
	register_branch(DOT, PUSD, liquidation_branch_config());
	assert_ok!(open(1, DOT, PUSD, 600, 500, FixedU128::from_rational(1, 1_000)));
	assert_ok!(open(2, DOT, PUSD, 2_000, 500, FixedU128::from_rational(2, 1_000)));
	set_price(DOT, FixedU128::from_rational(9, 10));
}

fn set_liquidation_param(who: AccountId, update: BranchConfigUpdate<Balance>) -> DispatchResult {
	Vaults::set_param(RuntimeOrigin::signed(who), DOT, PUSD, update)
}

fn liquidation_config() -> LiquidationConfig<Balance> {
	crate::Branches::<Test>::get(DOT, PUSD)
		.expect("registered branch")
		.config
		.liquidation
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
		keeper_stable: stable_balance(PUSD, KEEPER),
		pool: collateral_balance(DOT, SP_ACCOUNT),
		redistribution: held(DOT, Vaults::redistribution_account(&DOT, &PUSD)),
	}
}

/// Asserts the `VaultLiquidated` event for vault 1 and that the ledger agrees with it: each path's
/// collateral landed where the outcome says, the JIT debt left the keeper, and the owner got the
/// surplus back with the vault deposit.
fn assert_settled(before: &Ledger, outcome: LiquidationOutcome<Balance>) {
	assert_event(Event::VaultLiquidated {
		collateral_id: DOT,
		stable_id: PUSD,
		owner: 1,
		keeper: KEEPER,
		outcome: outcome.clone(),
	});
	assert!(!vault_exists(DOT, PUSD, 1));
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
// penalty. The optional JIT leg fails open: whatever makes it unusable, liquidation still runs and
// the keeper's stablecoin is untouched.
#[test]
fn jit_skipped_falls_back_to_full_redistribution() {
	build_and_execute(|| {
		setup_underwater_vault();

		let cases = [
			(0, 0, 0, "no allowance"),
			// A 50-unit allowance sits below the 100-unit `minimum_jit_contribution`.
			(500, 50, 0, "allowance below the market minimum"),
			// The allowance clears the minimum, but the keeper's funding clamps it below.
			(50, 1_000, 0, "funding clamps the contribution below the minimum"),
			// An allowance does not reserve stablecoin, so an unfunded keeper resolves to no JIT
			// rather than a failed withdrawal.
			(0, 200, 0, "unfunded keeper"),
			// The floor protects an executed trade only; without one it is inert even when no
			// collateral could satisfy it.
			(0, 200, 1_000, "unsatisfiable floor without a trade"),
		];
		for (funding, max_jit, min_out, reason) in cases {
			hypothetically!({
				if funding > 0 {
					mint_stable(PUSD, KEEPER, funding);
				}
				let before = ledger();
				assert_eq!(liquidate(KEEPER, DOT, PUSD, 1, max_jit, min_out), Ok(()), "{reason}");
				assert_settled(&before, full_redistribution());
			});
		}
		assert_eq!(SpOffsetCalls::get(), 0, "a pure redistribution must not call the pool");
	});
}

// Equal penalties must produce the same borrower loss for offset and redistributed debt.
#[test]
fn full_redistribution_with_surplus() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_ok!(open(1, DOT, PUSD, 600, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, PUSD, 2_000, 500, FixedU128::from_rational(2, 1_000)));
		set_price(DOT, FixedU128::from_rational(9, 10));
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// At 5% the full 500 of redistributed debt weighs 525, seizing
		// ceil(525/0.9) = 584 of the 600 held — the owner keeps 16 even though
		// every unit of debt lands on other vaults.
		assert_settled(&before, outcome([0, 0, 0, 500], [0, 0, 0, 572], 12, 16));
	});
}

// Unlike the tests above, which sit between par and the penalty band, this vault is genuinely
// under water: its collateral is worth less than its debt, and the recipients — not the
// protocol — absorb the shortfall.
#[test]
fn redistribution_with_collateral_below_debt() {
	build_and_execute(|| {
		setup_underwater_vault();
		// 600 * 0.7 = 420 of value against 500 of debt: CR 0.84.
		set_price(DOT, FixedU128::from_rational(7, 10));
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// The 10%-penalty ask ceil(550/0.7) = 786 dwarfs the 600 held, so the
		// owner keeps nothing and the keeper takes the flat ceil(10/0.7) = 15
		// (floor(0.1% * 600) adds nothing).
		assert_settled(&before, outcome([0, 0, 0, 500], [0, 0, 0, 585], 15, 0));

		// The recipient must own the complete redistributed shortfall after its touch.
		assert_ok!(poke(KEEPER, DOT, PUSD, 2));
		let vault = vault(DOT, PUSD, 2);
		assert_eq!(vault.debt.total(), 501 + 500);
		assert_eq!(vault.collateral, 2_585);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 0);
	});
}

// Liquidation must settle current debt so accrued interest cannot escape the waterfall.
#[test]
fn debt_includes_accrued_interest() {
	build_and_execute(|| {
		setup_underwater_vault();
		advance_time(10 * ONE_YEAR_MS);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// Ten years at 0.1% accrue floor(500 * 0.001 * 10) = 5, so 505 of debt
		// redistributes. Its 10%-penalty weight is 505 + ceil(50.5) = 556, and
		// ceil(556/0.9) = 618 exceeds the 600 held: no owner surplus, and the
		// 588 left after the keeper all follows the redistributed debt.
		assert_settled(&before, outcome([0, 0, 0, 505], [0, 0, 0, 588], 12, 0));
	});
}

#[test]
fn terminal_interest_enters_the_waterfall_before_redistribution() {
	build_and_execute(|| {
		SpFeeShare::set(Permill::from_percent(100));
		setup_underwater_vault();
		advance_time(1);
		let fee_before = stable_balance(PUSD, FEE_DEST);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// Liquidation must include the terminal interest unit.
		assert_settled(&before, outcome([0, 0, 0, 501], [0, 0, 0, 588], 12, 0));
		// Only terminal interest increases fee-account revenue.
		assert_eq!(stable_balance(PUSD, FEE_DEST), fee_before + 1);
	});
}

// Active deposits have priority over an optional keeper contribution. This prevents an unnecessary
// JIT burn.
#[test]
fn active_pool_precedes_jit() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(1_000);
		mint_stable(PUSD, KEEPER, 1_000);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 200, 0));

		// Full offset: the whole resolution collateral 572 goes to the pool,
		// nothing reaches redistribution, and the JIT allowance stays unused.
		assert_settled(&before, outcome([500, 0, 0, 0], [572, 0, 0, 0], 12, 16));
		// 500 of the 1_000 mocked capacity was consumed.
		assert_eq!(ActiveSpCapacity::get(), 500);
		assert_eq!(
			PendingSpInspectionCalls::get(),
			0,
			"a full active offset must not inspect pending capacity"
		);
		assert_eq!(SpOffsetCalls::get(), 1, "the non-zero active leg settles once");
	});
}

// An offset cannot create borrower surplus when penalty-weighted debt exceeds the held collateral.
#[test]
fn full_offset_without_surplus() {
	build_and_execute(|| {
		setup_underwater_vault();
		// Deeper underwater than the standard 0.9: at 0.8 the 525 of offset
		// value asks for ceil(525/0.8) = 657, more than the 600 held.
		set_price(DOT, FixedU128::from_rational(4, 5));
		ActiveSpCapacity::set(1_000);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// Seized = 600, surplus 0. The keeper takes ceil(10/0.8) = 13 flat
		// (floor(0.1% * 600) adds nothing) and the pool the remaining 587.
		assert_settled(&before, outcome([500, 0, 0, 0], [587, 0, 0, 0], 13, 0));
		assert_eq!(ActiveSpCapacity::get(), 500);
	});
}

// JIT must cover only the debt left by the active pool to preserve waterfall priority.
#[test]
fn jit_burns_after_active_pool() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(300);
		mint_stable(PUSD, KEEPER, 500);
		let issuance_before = total_stable(PUSD);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 1_000, 0));

		// Every offset path is priced at the 5% liquidation penalty, so the
		// 500 debt seizes ceil(525/0.9) = 584 and leaves the owner 16. After the
		// 12 keeper reward, 572 is split by weight 315 : 210 (300 and 200 debt,
		// each at 1.05): active takes floor(572 * 315/525) = 343, JIT
		// floor(572 * 210/525) = 228. The 1 the flooring leaves has no
		// redistributed debt to follow, so the last non-zero offset (JIT)
		// receives it.
		assert_settled(&before, outcome([300, 200, 0, 0], [343, 229, 0, 0], 12, 16));
		assert_eq!(total_stable(PUSD), issuance_before - 200, "the JIT stablecoin burned");
	});
}

// The keeper's stablecoin balance must limit JIT exposure because an allowance does not reserve
// stablecoin.
#[test]
fn jit_clamped_by_keeper_balance() {
	build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(PUSD, KEEPER, 150);
		let before = ledger();

		// The 1_000 allowance is capped by the keeper's 150 of funding, and
		// the unfunded 350 redistributes.
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 1_000, 0));

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
	build_and_execute(|| {
		setup_underwater_vault();
		PendingSpCapacity::set(150);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

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
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(300);
		PendingSpCapacity::set(200);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// Active takes floor(572 * 315/525) = 343, pending
		// floor(572 * 210/525) = 228 plus the 1 the flooring leaves.
		assert_settled(&before, outcome([300, 0, 200, 0], [343, 0, 229, 0], 12, 16));
		assert_eq!((ActiveSpCapacity::get(), PendingSpCapacity::get()), (0, 0));
	});
}

// The market minimum is inclusive, so exactly funded JIT must remain available.
#[test]
fn jit_executes_at_minimum_funding() {
	build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(PUSD, KEEPER, 100);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 1_000, 0));

		// The 100 JIT at 5% weighs 105 and the 400 redistributed at 10% weighs
		// 440, so the 545 of value asks for ceil(545/0.9) = 606 and the clamp
		// seizes the whole 600. Of the 588 left after the keeper, JIT takes
		// floor(588 * 105/545) = 113 and redistribution floor(588 * 440/545) = 474
		// plus the 1 the flooring leaves.
		assert_settled(&before, outcome([0, 100, 0, 400], [0, 113, 0, 475], 12, 0));
	});
}

// A system ask below the market minimum must still execute: the keeper's allowance and funding
// clear the minimum, and the small residual is the waterfall's choice, not the keeper's.
#[test]
fn jit_executes_below_minimum_ask() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(450);
		mint_stable(PUSD, KEEPER, 500);
		let before = ledger();

		// The active pool leaves only 50 debt, below the market's 100-unit
		// minimum JIT contribution — the JIT burns it anyway.
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 500, 0));

		// Seizure prices the whole 500 of offset debt at 5%: ceil(525/0.9) =
		// 584 leaves the owner 16. Of the 572 after the keeper, the path
		// weights 473 : 53 (450 + ceil(22.5) and 50 + ceil(2.5)) give active
		// floor(572 * 473/526) = 514 and JIT floor(572 * 53/526) = 57 plus the
		// 1 the flooring leaves.
		assert_settled(&before, outcome([450, 50, 0, 0], [514, 58, 0, 0], 12, 16));
		assert_eq!(ActiveSpCapacity::get(), 0);
	});
}

// The system must use JIT when residual debt equals the inclusive market minimum.
#[test]
fn jit_executes_at_minimum_ask() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(400);
		mint_stable(PUSD, KEEPER, 500);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 1_000, 0));

		// The active pool leaves exactly the 100-unit minimum, so JIT burns 100.
		// Both offsets weigh at 5% — 420 : 105 — and 525 of value seizes the
		// standard 584. Of the 572 after the keeper, active takes
		// floor(572 * 420/525) = 457 and JIT floor(572 * 105/525) = 114 plus the
		// 1 the flooring leaves.
		assert_settled(&before, outcome([400, 100, 0, 0], [457, 115, 0, 0], 12, 16));
		assert_eq!(ActiveSpCapacity::get(), 0);
	});
}

// The slippage floor must protect the keeper's trade without blocking the liquidation. The
// protection must not exceed the keeper's request: the exact share is accepted, and one above it
// drops the trade while the waterfall proceeds without it.
#[test]
fn slippage_floor_binds_at_the_jit_share() {
	build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(PUSD, KEEPER, 200);

		hypothetically!({
			let before = ledger();
			assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 200, 228));
			// The 540 of value seizes exactly ceil(540/0.9) = 600: no surplus, and
			// redistribution takes floor(588 * 330/540) = 359 plus the 1 remainder.
			assert_settled(&before, outcome([0, 200, 0, 300], [0, 228, 0, 360], 12, 0));
		});

		let before = ledger();
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 200, 229));
		assert_settled(&before, full_redistribution());
	});
}

// Dropping a JIT trade for slippage must keep the pool settlement: only the keeper's leg falls
// out of the plan, and its debt moves down the waterfall.
#[test]
fn slippage_skip_preserves_pool_settlement() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(300);
		mint_stable(PUSD, KEEPER, 500);
		let before = ledger();

		// The same split as `jit_burns_after_active_pool` would pay the JIT
		// 229 of collateral, so a 230 floor drops the trade and its 200 debt
		// redistributes instead.
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 1_000, 230));

		// The re-plan weighs 315 : 220 (300 at 1.05 and 200 at 1.10), so 535 of
		// value seizes ceil(535/0.9) = 595 and leaves the owner 5. Of the 583
		// left after the keeper, active takes floor(583 * 315/535) = 343 and
		// redistribution floor(583 * 220/535) = 239 plus the 1 the flooring
		// leaves.
		assert_settled(&before, outcome([300, 0, 0, 200], [343, 0, 0, 240], 12, 5));
		assert_eq!(ActiveSpCapacity::get(), 0, "the active leg settled");
	});
}

// The exact MCR boundary protects healthy debt and permits liquidation below the boundary.
#[test]
fn liquidatable_only_below_mcr() {
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
		assert!(!vault_exists(DOT, PUSD, 1));
	});
}

// Safety mode must preserve ordinary liquidation so unsafe vaults can improve branch health.
#[test]
fn safety_mode_allows_liquidation() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());
		assert_ok!(open(1, DOT, PUSD, 600, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, PUSD, 800, 500, FixedU128::from_rational(2, 1_000)));
		set_price(DOT, FixedU128::from_rational(9, 10));
		let before = ledger();

		// The tighter vault 2 pulls branch TCR to (540 + 720) / 1_000 = 1.26,
		// below the 1.30 safety threshold, while staying healthy itself at
		// CR 1.44.
		assert_eq!(branch_mode(DOT, PUSD), Some(BranchMode::Safety));
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		assert_settled(&before, full_redistribution());
	});
}

// A branch whose every vault is under water must still clear its riskiest vault: liquidation
// works when the recipients are themselves below par.
#[test]
fn branch_below_par_still_liquidates() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());
		assert_ok!(open(1, DOT, PUSD, 600, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, PUSD, 600, 500, FixedU128::from_rational(2, 1_000)));
		// Both vaults sit at CR 600 * 0.8 / 500 = 0.96, so branch TCR is 0.96.
		set_price(DOT, FixedU128::from_rational(4, 5));
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// The 10%-penalty ask ceil(550/0.8) = 688 exceeds the vault's collateral:
		// the whole 600 is seized, the keeper takes ceil(10/0.8) = 13, and the
		// remaining 587 follows the 500 of redistributed debt.
		assert_settled(&before, outcome([0, 0, 0, 500], [0, 0, 0, 587], 13, 0));

		// The sole recipient must receive the complete redistributed debt and collateral.
		assert_ok!(poke(KEEPER, DOT, PUSD, 2));
		let vault = vault(DOT, PUSD, 2);
		assert_eq!(vault.debt.total(), 501 + 500);
		assert_eq!(vault.collateral, 600 + 587);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 0);

		// Vault 2 is now deeper under water, but as the last stake-bearer it
		// takes the final-recovery path, not another liquidation.
		assert_noop!(
			liquidate(KEEPER, DOT, PUSD, 2, 0, 0),
			Error::<Test>::LastVaultCannotBeLiquidated
		);
	});
}

// Liquidation must reject an unknown market, a missing vault, and an unavailable oracle before it
// can change protocol state.
#[test]
fn preconditions_rejected_without_side_effects() {
	build_and_execute(|| {
		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0), Error::<Test>::BranchNotFound);

		// The market exists but account 1 never opened a vault on it.
		register_branch(DOT, PUSD, liquidation_branch_config());
		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0), Error::<Test>::VaultNotFound);

		assert_ok!(open(1, DOT, PUSD, 600, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, PUSD, 2_000, 500, FixedU128::from_rational(2, 1_000)));
		set_price(DOT, FixedU128::from_rational(9, 10));
		MockOracleAvailable::set(false);
		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0), Error::<Test>::OraclePriceNotAvailable);
	});
}

// Liquidation policy is branch governance state: only that branch's administrator may change it,
// and authority on one market must not create policy for an unregistered one.
#[test]
fn config_update_requires_the_branch_admin() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());
		let update = BranchConfigUpdate::OffsetPenalty(Permill::from_percent(10));

		assert_noop!(set_liquidation_param(1, update.clone()), Error::<Test>::NotBranchAdmin);
		assert_noop!(
			Vaults::set_param(RuntimeOrigin::signed(ADMIN), TOKEN_X, PUSD, update.clone()),
			Error::<Test>::BranchNotFound
		);

		assert_ok!(set_liquidation_param(ADMIN, update));
		assert_eq!(
			liquidation_config(),
			LiquidationConfig {
				offset_penalty: Permill::from_percent(10),
				..liquidation_branch_config().liquidation
			}
		);
	});
}

// The offset penalty must not exceed the redistribution penalty, on creation and on either
// update: a higher value would invert the borrower-loss order. Equality is valid because it
// does not.
#[test]
fn penalty_order_enforced_on_create_and_update() {
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
				(),
			),
			Error::<Test>::InvalidBranchConfig(
				BranchConfigDefect::OffsetPenaltyAboveRedistribution
			)
		);

		register_branch(DOT, PUSD, liquidation_branch_config());
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
	});
}

// The keeper is paid out of the offset penalty, so the compensation a liquidation can actually
// hand over may not outgrow what that penalty funds — otherwise an offset burns pool stablecoin
// the seizure no longer covers, with the market's own administrator naming who collects the
// difference.
#[test]
fn keeper_compensation_above_the_penalty_rejected() {
	build_and_execute(|| {
		// The mock's smallest vault owes 200 at a 5% offset penalty, so a liquidation seizes
		// 210 and leaves exactly 10 spare.
		register_branch(DOT, PUSD, liquidation_branch_config());
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
// for the keeper, so the two rates — not any one vault's amounts — decide whether the market
// works. `min_debt` is 200 here, small enough that rounding hides a rate that leaks at scale.
#[test]
fn keeper_percentage_above_the_penalty_rate_rejected() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());
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
		// take 105 against a 100 penalty. On the 200 the market floors at, the same rate takes
		// floor(10.5) = 10 out of a 10 penalty and looks payable.
		assert_noop!(set_liquidation_param(ADMIN, percent(50_000)), exceeds_penalty());
	});
}

// A higher redistribution penalty must give recipients more collateral for each unit of inherited
// debt.
#[test]
fn redistribution_outpays_offsets() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(200);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// 200 offset at 5% weighs 210 and 300 redistributed at 10% weighs 330, so
		// the 540 of value seizes exactly ceil(540/0.9) = 600 and leaves no
		// surplus. Of the 588 left after the keeper, active takes
		// floor(588 * 210/540) = 228 and redistribution floor(588 * 330/540) = 359
		// plus the 1 the flooring leaves: 1.14 against 1.20 of collateral per unit
		// of debt.
		assert_settled(&before, outcome([200, 0, 0, 300], [228, 0, 0, 360], 12, 0));
		let after = ledger();
		let pool = after.pool - before.pool;
		let redistribution = after.redistribution - before.redistribution;
		assert!(redistribution * 200 > pool * 300);
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
	register_branch(DOT, PUSD, token_scale_branch_config(liquidation));
	set_price(DOT, FixedU128::from_rational(4, 1));
	assert_ok!(open(1, DOT, PUSD, collateral, debt, FixedU128::from_rational(1, 1_000)));
	assert_ok!(open(
		2,
		DOT,
		PUSD,
		100_000 * UNIT,
		1_000 * UNIT,
		FixedU128::from_rational(2, 1_000)
	));
	set_price(DOT, FixedU128::from_rational(2, 1));
}

// The complete waterfall must preserve relative path allocation and collateral when keeper
// compensation is zero.
#[test]
fn four_way_split() {
	build_and_execute(|| {
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
		ActiveSpCapacity::set(500 * UNIT);
		PendingSpCapacity::set(100 * UNIT);
		mint_stable(PUSD, KEEPER, 200 * UNIT);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 200 * UNIT, 0));

		// Debt splits 500 active / 200 JIT / 100 pending / 200 redistributed.
		// The three offsets weigh 525 + 210 + 105 = 840 at 1.05 and
		// redistribution 220 at 1.10, so 1_060 pUSD of value seizes
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

// A token-scale full offset protects keeper compensation and borrower surplus from small-fixture
// distortion.
#[test]
fn full_offset_with_keeper_compensation() {
	build_and_execute(|| {
		let liquidation = LiquidationConfig {
			offset_penalty: Permill::from_percent(5),
			keeper_flat_compensation_value: 2 * UNIT,
			keeper_percent_compensation: Permill::from_rational(1u32, 1_000u32),
			keeper_compensation_cap_value: 100 * UNIT,
			minimum_jit_contribution: 100,
			redistribution_penalty: Permill::from_percent(10),
		};
		// 10_000 of stablecoin debt against 6_000 DOT.
		setup_token_scale_vault(liquidation, 6_000 * UNIT, 10_000 * UNIT);
		// 20_000 of active deposits, twice what the debt needs.
		ActiveSpCapacity::set(20_000 * UNIT);
		let before = ledger();

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// max_seizable_value = 10_000 * 1.05 = 10_500 pUSD, so 5_250 DOT; the
		// vault holds 6_000, leaving the owner 750. The keeper takes
		// 2/2 = 1 DOT flat plus 0.1% of 5_250 = 5.25 DOT, and the pool takes
		// the remaining 5_243.75 DOT for the whole 10_000 of debt.
		assert_settled(
			&before,
			outcome([10_000 * UNIT, 0, 0, 0], [5_243_750_000, 0, 0, 0], 6_250_000, 750 * UNIT),
		);
	});
}
