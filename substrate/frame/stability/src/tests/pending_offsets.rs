//! `offset_pending_liquidation`: the FIFO-ordered last-resort backstop.
//! Never touches `P`/`S`/`G` (invariant 11); collateral is credited straight
//! to the consumed depositors.

use crate::mock::*;

/// Queue a pending (unactivated) deposit for `who`.
fn seed_pending(who: AccountId, amount: Balance) {
	mint_stable(PUSD, who, amount);
	assert_ok!(deposit(who, DOT, PUSD, amount));
}

#[test]
fn pending_offset_consumes_fifo_in_order() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_pending(1, 200);
		seed_pending(2, 300);

		// Step 1 (user 1, oldest): debt 200 of 350, collateral
		// floor(175 * 200 / 350) = 100 — fully consumed, leaves the FIFO.
		// Step 2 (user 2): debt min(300, 150) = 150, collateral
		// floor(75 * 150 / 150) = 75 — 150 pending remain, keeps its slot.
		let (result, leftover) = simulate_pending_offset(DOT, PUSD, 350, 175, 10);
		assert_eq!(result.debt_offset, 350);
		assert_eq!(result.collateral_to_pool, 175);
		assert_eq!(result.remaining_debt, 0);
		assert_eq!(leftover, 0);
		assert_eq!(result.iterations_used, 2);

		let row1 = deposit_row(DOT, PUSD, 1).expect("kept: it holds a claimable");
		assert!(row1.pending_deposit.is_none());
		assert_eq!(row1.claimable_collateral, 100);
		let row2 = deposit_row(DOT, PUSD, 2).expect("kept");
		assert_eq!(row2.pending_deposit.expect("partially consumed").amount, 150);
		assert_eq!(row2.claimable_collateral, 75);

		assert!(!pending_contains(DOT, PUSD, 1));
		assert!(pending_contains(DOT, PUSD, 2));
		assert_eq!(pending_oldest(DOT, PUSD), Some(2));

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_pending_deposits, 150);
		assert_eq!(state.total_collateral_gains_unclaimed, 175);
		// 350 of the 500 pool stablecoin was burned.
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 150);

		System::assert_has_event(
			crate::Event::PendingDepositOffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				debt_burned: 350,
				collateral_gain: 175,
				iterations: 2,
			}
			.into(),
		);

		// The direct credits are claimable through the normal path.
		// (Delta: DOT is native, and accounts hold genesis native balance.)
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 100);
		assert!(deposit_row(DOT, PUSD, 1).is_none());
	});
}

#[test]
fn pending_offset_respects_caller_and_pallet_caps() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_pending(1, 100);
		seed_pending(2, 100);
		seed_pending(3, 100);

		// The caller's cap stops the walk after two of three entries.
		let (result, _) = simulate_pending_offset(DOT, PUSD, 1_000, 0, 2);
		assert_eq!(result.debt_offset, 200);
		assert_eq!(result.iterations_used, 2);
		assert_eq!(result.remaining_debt, 800);
		assert_eq!(pending_count(DOT, PUSD), 1);
		assert_eq!(pending_oldest(DOT, PUSD), Some(3));

		// A zero cap walks nothing.
		let (result, _) = simulate_pending_offset(DOT, PUSD, 800, 0, 0);
		assert_eq!(result.debt_offset, 0);
		assert_eq!(result.iterations_used, 0);
		assert_eq!(pending_count(DOT, PUSD), 1);
	});
}

#[test]
fn pending_offset_noop_cases_pass_remainders_through() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());

		// Empty queue.
		let (result, leftover) = simulate_pending_offset(DOT, PUSD, 100, 50, 5);
		assert_eq!(result.debt_offset, 0);
		assert_eq!(result.remaining_debt, 100);
		assert_eq!(leftover, 50);
		assert_eq!(result.iterations_used, 0);

		// Zero remaining debt with a populated queue.
		seed_pending(1, 200);
		let (result, leftover) = simulate_pending_offset(DOT, PUSD, 0, 50, 5);
		assert_eq!(result.debt_offset, 0);
		assert_eq!(leftover, 50);
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 200);
	});
}

#[test]
fn pending_offset_ignores_active_deposits_and_accumulators() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 600);
		assert_ok!(deposit(1, DOT, PUSD, 600));
		advance_time(5_000);
		assert_ok!(activate(1, DOT, PUSD));
		seed_pending(2, 300);
		drop(distribute_yield(DOT, PUSD, 60));

		let before = pool_state(DOT, PUSD);
		let sums_before = crate::PoolSumsStore::<Test>::get((DOT, PUSD, 0u32, 0u32));

		let (result, _) = simulate_pending_offset(DOT, PUSD, 200, 100, 5);
		assert_eq!(result.debt_offset, 200);
		assert_eq!(result.collateral_to_pool, 100);

		// Only pending capital moved: the accumulators and the active side
		// are bit-identical (invariant 11).
		let after = pool_state(DOT, PUSD);
		assert_eq!(after.p, before.p);
		assert_eq!(after.epoch, before.epoch);
		assert_eq!(after.scale, before.scale);
		assert_eq!(after.total_active_deposits, 600);
		assert_eq!(after.total_pending_deposits, 100);
		assert_eq!(crate::PoolSumsStore::<Test>::get((DOT, PUSD, 0u32, 0u32)), sums_before);

		// The active depositor's yield claim is untouched.
		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		System::assert_has_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 60,
			}
			.into(),
		);
	});
}

#[test]
fn pending_offset_flooring_credits_zero_and_prunes() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_pending(1, 100);

		// floor(1 * 100 / 1000) = 0: the whole pending amount burns for a
		// zero collateral credit, and the emptied row is pruned.
		let (result, leftover) = simulate_pending_offset(DOT, PUSD, 1_000, 1, 5);
		assert_eq!(result.debt_offset, 100);
		assert_eq!(result.collateral_to_pool, 0);
		assert_eq!(result.remaining_debt, 900);
		assert_eq!(leftover, 1);

		assert!(deposit_row(DOT, PUSD, 1).is_none());
		assert!(!pending_contains(DOT, PUSD, 1));
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_pending_deposits, 0);
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 0);
	});
}

#[test]
fn pending_offset_with_sub_minimum_collateral_gain_stops_before_the_step() {
	build_and_execute(|| {
		// Same sub-minimum first-gain guard as the active offset: the walk
		// stops before the step commits anything (roll-forward semantics).
		assert_ok!(Assets::force_create(RuntimeOrigin::root(), 77, 1, true, 1_000));
		let coll = AssetId::WithId(77);
		register_branch(coll.clone(), PUSD, default_branch_config());
		mint_stable(PUSD, 1, 200);
		assert_ok!(deposit(1, coll.clone(), PUSD, 200));

		// Gain floor(500 * 200 / 200) = 500 < the 1_000 minimum on an empty
		// pool account: the step is attempted but nothing of it applies.
		let (result, leftover) = simulate_pending_offset(coll.clone(), PUSD, 200, 500, 5);
		assert_eq!(result.debt_offset, 0);
		assert_eq!(result.remaining_debt, 200);
		assert_eq!(result.iterations_used, 1);
		assert_eq!(leftover, 500);
		let row = deposit_row(coll.clone(), PUSD, 1).expect("kept");
		assert_eq!(row.pending_deposit.expect("untouched").amount, 200);
		assert_eq!(pool_state(coll, PUSD).total_pending_deposits, 200);
	});
}

#[test]
fn pending_offset_on_unregistered_branch_noops_and_returns_the_credit() {
	build_and_execute(|| {
		let (result, leftover) = simulate_pending_offset(DOT, PUSD, 100, 50, 5);
		assert_eq!(result.debt_offset, 0);
		assert_eq!(result.remaining_debt, 100);
		assert_eq!(leftover, 50);
	});
}
