//! Tests for liquidation policy and economic invariants.
//!
//! Most tests use small whole-unit amounts so expected balances stay easy to inspect.
//! Vault 2 prevents the last-vault guard, which lets each test isolate one liquidation rule.
//! The token-scale tests verify the same formulas with economically realistic amounts.

use crate::{
	mock::*, types::BranchConfigUpdate, BranchConfig, BranchMode, DebtCollateral, Error, Event,
	LiquidationConfig, LiquidationOutcome,
};
use pusd_primitives::MILLIS_PER_YEAR;

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

const KEEPER: AccountId = 3;
const GENESIS_BALANCE: Balance = 1_000_000_000_000;

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

/// Fixes the common market and actors so each test can focus on its outcome.
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

// Branch creation must reject a penalty order that makes redistribution cheaper than an offset.
#[test]
fn create_with_inverted_penalties_rejected() {
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

// Without Stability capital, redistribution must settle all debt and apply its higher borrower
// penalty.
#[test]
fn full_redistribution_without_surplus() {
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

// The public liquidation path must preserve the free asset minimum that branch
// registration placed in issued-collateral redistribution custody. Without
// that seed, resolving 488 and then holding all 488 fails under `Protect`.
#[test]
fn issued_collateral_full_redistribution_preserves_custody_seed() {
	build_and_execute(|| {
		register_branch(TOKEN_X, PUSD, liquidation_branch_config());
		assert_ok!(open(1, TOKEN_X, PUSD, 600, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, TOKEN_X, PUSD, 2_000, 500, FixedU128::from_rational(2, 1_000)));
		set_price(TOKEN_X, FixedU128::from_rational(9, 10));

		let redistribution = Vaults::redistribution_account(&TOKEN_X, &PUSD);
		assert_eq!(collateral_balance(TOKEN_X, redistribution), 1, "free branch seed");
		assert_eq!(held(TOKEN_X, redistribution), 0);

		assert_ok!(liquidate(KEEPER, TOKEN_X, PUSD, 1, 0, 0));

		assert!(crate::Vaults::<Test>::get((TOKEN_X, PUSD, 1)).is_none());
		assert_eq!(collateral_balance(TOKEN_X, redistribution), 1, "seed remains free");
		assert_eq!(held(TOKEN_X, redistribution), 488, "redistribution held above seed");
		System::assert_has_event(
			Event::<Test>::VaultLiquidated {
				collateral_id: TOKEN_X,
				stable_id: PUSD,
				owner: 1,
				keeper: KEEPER,
				outcome: outcome([0, 0, 0, 500], [0, 0, 0, 488], 112, 0),
			}
			.into(),
		);
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

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// At 5% the full 500 of redistributed debt weighs 525, seizing
		// ceil(525/0.9) = 584 of the 600 held — the owner keeps 16 even though
		// every unit of debt lands on other vaults.
		assert_eq!(Balances::free_balance(1), GENESIS_BALANCE - 600 + 16);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 472);

		assert_liquidated_event(outcome([0, 0, 0, 500], [0, 0, 0, 472], 112, 16));
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

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// The 10%-penalty ask ceil(550/0.7) = 786 dwarfs the 600 held, so the
		// owner keeps nothing and the keeper takes the flat ceil(100/0.7) = 143
		// (floor(0.1% * 600) adds nothing).
		assert_eq!(Balances::free_balance(1), GENESIS_BALANCE - 600);
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE + 143);
		assert_liquidated_event(outcome([0, 0, 0, 500], [0, 0, 0, 457], 143, 0));

		// The recipient inherits 500 of debt against 457 of collateral worth
		// only floor(457 * 0.7) = 319; both shares divide the 2_000 stake
		// exactly, and no bad debt is recorded — vault 2's CR carries the loss.
		// Its own debt is 501: 500 of principal plus the 1 upfront fee its
		// 0.2% rate paid at open.
		assert_ok!(Vaults::poke(RuntimeOrigin::signed(KEEPER), DOT, PUSD, 2));
		let vault = crate::Vaults::<Test>::get((DOT, PUSD, 2)).expect("recipient remains");
		assert_eq!(vault.debt.total(), 501 + 500);
		assert_eq!(vault.collateral, 2_457);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 0);
		assert_eq!(branch_state(DOT, PUSD).expect("branch state").debt.bad_debt, 0);
	});
}

// Liquidation must settle current debt so accrued interest cannot escape the waterfall.
#[test]
fn debt_includes_accrued_interest() {
	build_and_execute(|| {
		setup_underwater_vault();
		advance_time(10 * MILLIS_PER_YEAR);

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// Ten years at 0.1% accrue floor(500 * 0.001 * 10) = 5, so 505 of debt
		// redistributes. Its 10%-penalty weight is 505 + ceil(50.5) = 556, and
		// ceil(556/0.9) = 618 exceeds the 600 held: no owner surplus, and the
		// 488 left after the keeper all follows the redistributed debt.
		assert!(crate::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 488);

		assert_liquidated_event(outcome([0, 0, 0, 505], [0, 0, 0, 488], 112, 0));
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

// An offset cannot create borrower surplus when penalty-weighted debt exceeds the held collateral.
#[test]
fn full_offset_without_surplus() {
	build_and_execute(|| {
		setup_underwater_vault();
		// Deeper underwater than the standard 0.9: at 0.8 the 525 of offset
		// value asks for ceil(525/0.8) = 657, more than the 600 held.
		set_price(DOT, FixedU128::from_rational(4, 5));
		ActiveSpCapacity::set(1_000);

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// Seized = 600, surplus 0. The keeper takes ceil(100/0.8) = 125 flat
		// (floor(0.1% * 600) adds nothing) and the pool the remaining 475.
		assert_eq!(Balances::free_balance(1), GENESIS_BALANCE - 600);
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE + 125);
		assert_eq!(Balances::free_balance(SP_ACCOUNT), GENESIS_BALANCE + 475);
		assert_eq!(ActiveSpCapacity::get(), 500);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 0);

		assert_liquidated_event(outcome([500, 0, 0, 0], [475, 0, 0, 0], 125, 0));
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

// The keeper's stablecoin balance must limit JIT exposure because an allowance does not reserve
// stablecoin.
#[test]
fn jit_clamped_by_keeper_balance() {
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

// Pending deposits must absorb debt before redistribution to preserve the specified waterfall.
#[test]
fn pending_precedes_redistribution() {
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

// The last nonzero offset path must receive the rounding remainder. This preserves collateral when
// no debt goes to redistribution.
#[test]
fn remainder_follows_last_offset() {
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

// A keeper must not bypass the market minimum with a deliberately small JIT allowance.
#[test]
fn jit_below_minimum_rejected() {
	build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(PUSD, KEEPER, 500);

		// A 50-unit allowance sits below the 100-unit `minimum_jit_contribution`.
		// The keeper is fully funded, so the allowance alone triggers the reject.
		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 50, 0), Error::<Test>::JitBelowMinimum);
	});
}

// A keeper must not bypass the market minimum by underfunding a large allowance either.
#[test]
fn jit_underfunded_below_minimum_rejected() {
	build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(PUSD, KEEPER, 50);

		// The 1_000 allowance clears the minimum, but the keeper's 50 of
		// funding would clamp the contribution below it — the funding shortfall
		// is keeper-side, so it rejects like a small allowance.
		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 1_000, 0), Error::<Test>::JitBelowMinimum);
	});
}

// The market minimum is inclusive, so exactly funded JIT must remain available.
#[test]
fn jit_executes_at_minimum_funding() {
	build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(PUSD, KEEPER, 100);

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 1_000, 0));

		// The 100 JIT at 5% weighs 105 and the 400 redistributed at 10% weighs
		// 440, so the 545 of value asks for ceil(545/0.9) = 606 and the clamp
		// seizes the whole 600. Of the 488 left after the keeper, JIT takes
		// floor(488 * 105/545) = 94 and redistribution floor(488 * 440/545) = 393
		// plus the 1 the flooring leaves.
		assert_eq!(stable_balance(PUSD, KEEPER), 0);
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE + 112 + 94);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 394);

		assert_liquidated_event(outcome([0, 100, 0, 400], [0, 94, 0, 394], 112, 0));
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

		// The active pool leaves only 50 debt, below the market's 100-unit
		// minimum JIT contribution — the JIT burns it anyway.
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 500, 0));

		// Seizure prices the whole 500 of offset debt at 5%: ceil(525/0.9) =
		// 584 leaves the owner 16. Of the 472 after the keeper, the path
		// weights 473 : 53 (450 + ceil(22.5) and 50 + ceil(2.5)) give active
		// floor(472 * 473/526) = 424 and JIT floor(472 * 53/526) = 47 plus the
		// 1 the flooring leaves.
		assert_eq!(ActiveSpCapacity::get(), 0);
		assert_eq!(stable_balance(PUSD, KEEPER), 500 - 50, "the dust residual burned");
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE + 112 + 48);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 0);
		assert!(crate::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());

		assert_liquidated_event(outcome([450, 50, 0, 0], [424, 48, 0, 0], 112, 16));
	});
}

// The system must use JIT when residual debt equals the inclusive market minimum.
#[test]
fn jit_executes_at_minimum_ask() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(400);
		mint_stable(PUSD, KEEPER, 500);

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 1_000, 0));

		// The active pool leaves exactly the 100-unit minimum, so JIT burns 100.
		// Both offsets weigh at 5% — 420 : 105 — and 525 of value seizes the
		// standard 584. Of the 472 after the keeper, active takes
		// floor(472 * 420/525) = 377 and JIT floor(472 * 105/525) = 94 plus the
		// 1 the flooring leaves.
		assert_eq!(ActiveSpCapacity::get(), 0);
		assert_eq!(stable_balance(PUSD, KEEPER), 400);
		assert_eq!(Balances::free_balance(SP_ACCOUNT), GENESIS_BALANCE + 377);
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE + 112 + 95);

		assert_liquidated_event(outcome([400, 100, 0, 0], [377, 95, 0, 0], 112, 16));
	});
}

// The slippage floor must protect the keeper's trade without blocking the liquidation: an
// unmet floor skips the JIT contribution and the waterfall proceeds.
#[test]
fn slippage_above_share_skips_jit() {
	build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(PUSD, KEEPER, 200);

		// One above the 189 the trade would pay: the trade is dropped, no
		// stablecoin burns, and the standard full redistribution follows.
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 200, 190));

		assert_eq!(stable_balance(PUSD, KEEPER), 200, "no JIT burn under the floor");
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 488);
		assert_liquidated_event(outcome([0, 0, 0, 500], [0, 0, 0, 488], 112, 0));
	});
}

// The exact slippage boundary must succeed. The protection must not exceed the keeper's request.
#[test]
fn slippage_at_share_accepted() {
	build_and_execute(|| {
		setup_underwater_vault();
		mint_stable(PUSD, KEEPER, 200);

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 200, 189));

		// The 540 of value seizes exactly ceil(540/0.9) = 600: no surplus, and
		// redistribution takes floor(488 * 330/540) = 298 plus the 1 remainder.
		assert_liquidated_event(outcome([0, 200, 0, 300], [0, 189, 0, 299], 112, 0));
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

		// The same split as `jit_burns_after_active_pool` would pay the JIT
		// 189 of collateral, so a 190 floor drops the trade and its 200 debt
		// redistributes instead.
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 1_000, 190));

		// The re-plan weighs 315 : 220 (300 at 1.05 and 200 at 1.10), so 535 of
		// value seizes ceil(535/0.9) = 595 and leaves the owner 5. Of the 483
		// left after the keeper, active takes floor(483 * 315/535) = 284 and
		// redistribution floor(483 * 220/535) = 198 plus the 1 the flooring
		// leaves.
		assert_eq!(ActiveSpCapacity::get(), 0, "the active leg settled");
		assert_eq!(Balances::free_balance(SP_ACCOUNT), GENESIS_BALANCE + 284);
		assert_eq!(stable_balance(PUSD, KEEPER), 500, "no JIT burn under the floor");
		assert_eq!(Balances::free_balance(KEEPER), GENESIS_BALANCE + 112);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 199);
		assert!(crate::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());

		assert_liquidated_event(outcome([300, 0, 0, 200], [284, 0, 0, 199], 112, 5));
	});
}

// An allowance does not reserve stablecoin. Thus, an unfunded keeper must not block fallback
// redistribution.
#[test]
fn jit_skipped_for_unfunded_keeper() {
	build_and_execute(|| {
		setup_underwater_vault();

		// The keeper holds no stablecoin, so the 200 allowance quietly resolves
		// to no JIT rather than a failed withdrawal, and the standard full
		// redistribution follows.
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 200, 0));

		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 488);
		assert_liquidated_event(outcome([0, 0, 0, 500], [0, 0, 0, 488], 112, 0));
	});
}

// Slippage protects an executed JIT trade only. It must not block liquidation when no trade occurs.
#[test]
fn slippage_floor_inert_without_trade() {
	build_and_execute(|| {
		setup_underwater_vault();

		// A 1_000 floor is unsatisfiable against the 600 of collateral held,
		// but the unfunded keeper executes no JIT trade and the floor applies
		// only to one — the liquidation settles regardless.
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 200, 1_000));
		assert!(crate::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
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
		assert!(crate::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
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

		// The tighter vault 2 pulls branch TCR to (540 + 720) / 1_000 = 1.26,
		// below the 1.30 safety threshold, while staying healthy itself at
		// CR 1.44.
		assert_eq!(branch_mode(DOT, PUSD), Some(BranchMode::Safety));
		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		assert_liquidated_event(outcome([0, 0, 0, 500], [0, 0, 0, 488], 112, 0));
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

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		// The 10%-penalty ask ceil(550/0.8) = 688 exceeds the vault's collateral:
		// the whole 600 is seized, the keeper takes ceil(100/0.8) = 125, and the
		// remaining 475 follows the 500 of redistributed debt.
		assert!(crate::Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
		assert_liquidated_event(outcome([0, 0, 0, 500], [0, 0, 0, 475], 125, 0));

		// Over vault 2's 600 stake the per-stake fixed-point floors once each
		// way: it inherits floor(500/600 * 600) = 499 of debt (1 goes to
		// ownerless debt) and floor(475/600 * 600) = 474 of collateral (1 stays
		// with the redistribution account). Its own debt is 501: 500 of
		// principal plus the 1 upfront fee its 0.2% rate paid at open.
		assert_ok!(Vaults::poke(RuntimeOrigin::signed(KEEPER), DOT, PUSD, 2));
		let vault = crate::Vaults::<Test>::get((DOT, PUSD, 2)).expect("recipient remains");
		assert_eq!(vault.debt.total(), 501 + 499);
		assert_eq!(vault.collateral, 600 + 474);
		assert_eq!(held(DOT, Vaults::redistribution_account(&DOT, &PUSD)), 1);
		assert_eq!(branch_state(DOT, PUSD).expect("branch state").ownerless_debt, 1);

		// Vault 2 is now deeper under water, but as the last stake-bearer it
		// takes the final-recovery path, not another liquidation.
		assert_noop!(
			liquidate(KEEPER, DOT, PUSD, 2, 0, 0),
			Error::<Test>::LastVaultCannotBeLiquidated
		);
	});
}

// Liquidation must reject an unknown market before it can change protocol state.
#[test]
fn unknown_market_rejected() {
	build_and_execute(|| {
		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0), crate::Error::<Test>::BranchNotFound);
	});
}

// Market existence must not hide that the target vault does not exist.
#[test]
fn missing_vault_rejected() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());

		// The market exists but account 1 never opened a vault on it.
		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0), crate::Error::<Test>::VaultNotFound);
	});
}

// Liquidation must stop when the protocol cannot value collateral safely.
#[test]
fn oracle_outage_halts_liquidation() {
	build_and_execute(|| {
		setup_underwater_vault();
		MockOracleAvailable::set(false);

		assert_noop!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0), Error::<Test>::OraclePriceNotAvailable);
	});
}

// A branch administrator must be able to update liquidation policy for that branch.
#[test]
fn admin_updates_config() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());
		let update = LiquidationConfig {
			offset_penalty: Permill::from_percent(10),
			..liquidation_branch_config().liquidation
		};

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
	});
}

// Liquidation policy is branch governance state, so an ordinary account must not change it.
#[test]
fn non_admin_config_update_rejected() {
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
	});
}

// Authority on one market must not create liquidation policy for an unregistered market.
#[test]
fn unknown_market_config_update_rejected() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());

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

// The offset penalty must not exceed the redistribution penalty. A higher value would invert the
// borrower-loss order.
#[test]
fn offset_penalty_above_redistribution_rejected() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, liquidation_branch_config());
		let above = LiquidationConfig {
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

		// Equality is valid because it does not invert the borrower-loss order.
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

// Governance must not lower the redistribution penalty below the current offset penalty.
#[test]
fn redistribution_penalty_below_offset_rejected() {
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

// A higher redistribution penalty must give recipients more collateral for each unit of inherited
// debt.
#[test]
fn redistribution_outpays_offsets() {
	build_and_execute(|| {
		setup_underwater_vault();
		ActiveSpCapacity::set(200);

		assert_ok!(liquidate(KEEPER, DOT, PUSD, 1, 0, 0));

		let sp = Balances::free_balance(SP_ACCOUNT) - GENESIS_BALANCE;
		let redistribution = held(DOT, Vaults::redistribution_account(&DOT, &PUSD));
		assert_eq!((sp, redistribution), (189, 299));

		assert!(redistribution * 200 > sp * 300);
	});
}

/// Uses six-decimal raw units so tests can represent fractional-token outcomes exactly.
const UNIT: Balance = 1_000_000;

/// Raises the MCR above 120% so token-scale fixtures become liquidatable after the price change.
fn token_scale_branch_config() -> crate::BranchConfig<Balance> {
	crate::BranchConfig {
		minimum_collateralization_ratio: FixedU128::from_rational(130u128, 100u128),
		initial_collateralization_ratio: FixedU128::from_rational(140u128, 100u128),
		safety_collateralization_ratio: FixedU128::from_rational(150u128, 100u128),
		minimum_debt: 100,
		// This isolates liquidation because the drawn principal equals debt.
		upfront_fee_period: 0,
		..liquidation_branch_config()
	}
}

// The complete waterfall must preserve relative path allocation and collateral when keeper
// compensation is zero.
#[test]
fn four_way_split() {
	build_and_execute(|| {
		let mut config = token_scale_branch_config();
		config.liquidation = LiquidationConfig {
			offset_penalty: Permill::from_percent(5),
			keeper_flat_compensation_value: 0,
			keeper_percent_compensation: Permill::zero(),
			keeper_compensation_cap_value: 0,
			minimum_jit_contribution: 100,
			redistribution_penalty: Permill::from_percent(10),
		};
		register_branch(DOT, PUSD, config);

		// Open above the 140% ICR, then drop to 1 DOT = 2 stablecoin.
		set_price(DOT, FixedU128::from_rational(4, 1));
		// 1_000 of stablecoin debt against 600 DOT.
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
		// 530 exactly — no rounding remainder to assign.
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

// A token-scale full offset protects keeper compensation and borrower surplus from small-fixture
// distortion.
#[test]
fn full_offset_with_keeper_compensation() {
	build_and_execute(|| {
		let mut config = token_scale_branch_config();
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
		// 10_000 of stablecoin debt against 6_000 DOT.
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
