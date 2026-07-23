//! `offset_pending_liquidation`: the FIFO-ordered last-resort backstop.
//! Never touches `P`/`S`/`G` (invariant 11); collateral is credited straight
//! to the consumed depositors.

use crate::{math::pro_rata_floor, mock::*, Error};

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
		let (result, leftover) = simulate_pending_offset(DOT, PUSD, 350, 175);
		assert_eq!(result.debt_offset, 350);
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
fn pending_offset_stops_at_pallet_cap_and_resumes_at_next_entry() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		for who in 1..=9 {
			seed_pending(who, 100);
		}

		// Exactly the oldest eight rows are consumed by the configured pallet
		// maximum.
		let (result, _) = simulate_pending_offset(DOT, PUSD, 1_000, 0);
		assert_eq!(result.debt_offset, 800);
		assert_eq!(result.iterations_used, 8);
		assert_eq!(pending_count(DOT, PUSD), 1);
		assert_eq!(pending_oldest(DOT, PUSD), Some(9));
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 100);

		// A later call resumes from the first untouched row.
		let (result, _) = simulate_pending_offset(DOT, PUSD, 200, 0);
		assert_eq!(result.debt_offset, 100);
		assert_eq!(result.iterations_used, 1);
		assert_eq!(pending_count(DOT, PUSD), 0);
		assert_eq!(pending_oldest(DOT, PUSD), None);
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 0);
	});
}

#[test]
fn pending_offset_noop_cases_pass_remainders_through() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());

		// Empty queue.
		let (result, leftover) = simulate_pending_offset(DOT, PUSD, 100, 50);
		assert_eq!(result.debt_offset, 0);
		assert_eq!(leftover, 50);
		assert_eq!(result.iterations_used, 0);

		// Zero remaining debt with a populated queue.
		seed_pending(1, 200);
		let (result, leftover) = simulate_pending_offset(DOT, PUSD, 0, 50);
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
		assert_ok!(poke(1, 1, DOT, PUSD));
		seed_pending(2, 300);
		drop(distribute_yield(DOT, PUSD, 60));

		let before = pool_state(DOT, PUSD);
		let sums_before = crate::PoolSumsStore::<Test>::get((DOT, PUSD, 0u32, 0u32));

		let (result, leftover) = simulate_pending_offset(DOT, PUSD, 200, 100);
		assert_eq!(result.debt_offset, 200);
		assert_eq!(leftover, 0);

		// Only pending capital moved: the accumulators and the active side
		// are bit-identical (invariant 11).
		let after = pool_state(DOT, PUSD);
		assert_eq!(after.coords.p, before.coords.p);
		assert_eq!(after.coords.epoch, before.coords.epoch);
		assert_eq!(after.coords.scale, before.coords.scale);
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
		// zero collateral credit, and the emptied row is pruned. The
		// flooring loss is bounded by one collateral base unit per step and
		// only visible at all when the credit is nearly worthless relative
		// to the debt (1 collateral against 1_000 debt here) — in practice
		// the error is negligible.
		let (result, leftover) = simulate_pending_offset(DOT, PUSD, 1_000, 1);
		assert_eq!(result.debt_offset, 100);
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
		let (result, leftover) = simulate_pending_offset(coll.clone(), PUSD, 200, 500);
		assert_eq!(result.debt_offset, 0);
		assert_eq!(result.iterations_used, 1);
		assert_eq!(leftover, 500);
		let row = deposit_row(coll.clone(), PUSD, 1).expect("kept");
		assert_eq!(row.pending_deposit.expect("untouched").amount, 200);
		assert_eq!(pool_state(coll.clone(), PUSD).total_pending_deposits, 200);
		let pool = Stability::pool_account(&coll, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 200);
	});
}

#[test]
fn pending_offset_on_unregistered_branch_noops_and_returns_the_credit() {
	build_and_execute(|| {
		let (result, leftover) = simulate_pending_offset(DOT, PUSD, 100, 50);
		assert_eq!(result.debt_offset, 0);
		assert_eq!(leftover, 50);
	});
}

/// One liquidation cascading through the full waterfall —
/// active-pool offset, keeper JIT, the pending-deposit backstop, and the
/// redistribution residual. Only the two offset stages are pallet calls; the
/// keeper JIT and the final redistribution belong to the external liquidation
/// orchestrator and are modelled here as plain arithmetic between the calls.
///
/// Every stage prices at the same credit-wide ratio 1152.845 / 2200
/// = 0.52402045… DOT per pUSD — each split is pro-rata against the debt
/// still standing, so stages differ only by flooring.
#[test]
fn full_liquidation_waterfall_active_jit_pending_and_residual() {
	build_and_execute(|| {
		const DOT_E10: Balance = 10_000_000_000;

		register_branch(DOT, PUSD, default_branch_config());
		seed_deposit(1, 1_501); // active pool = 1501 pUSD, P = 1, epoch 0.
		activate_all(&[1]);
		seed_pending(2, 250); // pending FIFO, user 2 oldest ...
		seed_pending(3, 100); // ... then user 3.

		// Stage 1 — active offset. The vault owes 2200 pUSD; its post-keeper
		// resolution collateral is 1152.845 DOT (keeper comp is external, so we
		// take it as the input). The offered 2200 exceeds the 1501 pool, so the
		// offset depletes the pool exactly and keeps only the collateral backing
		// the 1501 it burns; the rest flows on to the next stage.
		let c0 = 1_152_845 * (DOT_E10 / 1_000); // 11_528_450_000_000
		let (debt_offset1, leftover1) = simulate_offset(DOT, PUSD, 2_200, c0);
		assert_eq!(debt_offset1, 1_501);
		// The pool kept floor(11_528_450_000_000 * 1501 / 2200) =
		// 7_865_547_022_727 (786.5547022727 DOT) of the credit —
		// 786.5547 / 1501 = 0.52402 DOT per pUSD burned.
		assert_eq!(leftover1, 3_662_902_977_273); // 366.2902977273 DOT.

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.coords.epoch, 1);
		assert_eq!(state.coords.scale, 0);
		assert_eq!(state.coords.p, FixedU128::one());
		let sums = crate::PoolSumsStore::<Test>::get((DOT, PUSD, 0u32, 0u32));
		assert_eq!(sums.s_collateral, FixedU128::from_rational(7_865_547_022_727, 1_501));

		System::assert_has_event(
			crate::Event::PoolOffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				debt_burned: 1_501,
				collateral_gain: 7_865_547_022_727,
				epoch: 1,
				scale: 0,
			}
			.into(),
		);

		// Stage 2 — keeper JIT (external orchestrator, not a pallet call). It
		// burns 300 pUSD with keeper liquidity and takes the matching collateral
		// share off the remainder before the pending backstop runs.
		let jit_debt = 300;
		let jit_collateral = pro_rata_floor(leftover1, jit_debt, 699);
		// 157.2061 / 300 = 0.52402 DOT per pUSD — the same stage price.
		assert_eq!(jit_collateral, 1_572_061_363_636); // 157.2061363636 DOT.
		let after_jit_debt = 699 - jit_debt; // 399 pUSD.
		let after_jit_collat = leftover1 - jit_collateral; // 2_090_841_613_637 = 209.0841613637 DOT.
		assert_eq!(after_jit_collat, 2_090_841_613_637);

		// Stage 3+4 — pending-deposit FIFO backstop. User 2 (250) is consumed in
		// full, then user 3 (100), each priced against the debt still standing:
		//   floor(2_090_841_613_637 * 250 / 399) = 1_310_051_136_364
		//   floor(  780_790_477_273 * 100 / 149) =   524_020_454_545
		// (131.0051 / 250 and 52.4020 / 100: again 0.52402 DOT per pUSD.)
		let (r2, leftover2) = simulate_pending_offset(DOT, PUSD, after_jit_debt, after_jit_collat);
		assert_eq!(r2.debt_offset, 350);
		assert_eq!(r2.iterations_used, 2);
		// The residual — the 49 pUSD debt still standing + this collateral —
		// is what the orchestrator hands to redistribution (external).
		assert_eq!(leftover2, 256_770_022_728); // 25.6770022728 DOT.

		let row2 = deposit_row(DOT, PUSD, 2).expect("kept: holds a claimable");
		assert!(row2.pending_deposit.is_none());
		assert_eq!(row2.claimable_collateral, 1_310_051_136_364);
		let row3 = deposit_row(DOT, PUSD, 3).expect("kept");
		assert!(row3.pending_deposit.is_none());
		assert_eq!(row3.claimable_collateral, 524_020_454_545);
		assert!(!pending_contains(DOT, PUSD, 2));
		assert!(!pending_contains(DOT, PUSD, 3));
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 0);

		System::assert_has_event(
			crate::Event::PendingDepositOffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				debt_burned: 350,
				collateral_gain: 1_834_071_590_909,
				iterations: 2,
			}
			.into(),
		);

		// Stage 5 — the sole active depositor. The depletion compounded its
		// deposit to zero; its collateral is realized through S on claim. The
		// double-floor (`floor(1501 * floor(collat * 1e18 / 1501) / 1e18)`)
		// strands 1 base unit in the unclaimed total, so it realizes one less
		// than the pool's recorded gain.
		assert_noop!(withdraw(1, DOT, PUSD, 1, 1), Error::<Test>::NoActiveDeposit);
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 7_865_547_022_726);
		assert!(deposit_row(DOT, PUSD, 1).is_none());

		// Every pUSD the pool held (1501 active + 350 pending) was burned.
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 0);
	});
}

#[test]
fn pending_backstop_rounds_down_at_the_minimum_balance_dead_zone() {
	build_and_execute(|| {
		register_branch(DOT, USDX, default_branch_config());
		mint_stable(USDX, 1, 60_000);
		assert_ok!(deposit(1, DOT, USDX, 50_000));

		let pool = Stability::pool_account(&DOT, &USDX);
		assert_eq!(stable_balance(USDX, pool), 50_000);

		// Burning 45_000 would strand 5_000 < 10_000 on the pool: the first
		// step rounds down to 40_000 and ends the walk because nothing
		// preservable remains.
		let (result, leftover) = simulate_pending_offset(DOT, USDX, 45_000, 45_000);
		assert_eq!(result.debt_offset, 40_000);
		assert_eq!(result.iterations_used, 1);
		assert_eq!(leftover, 5_000);

		// The pool sits exactly at the minimum and the row keeps the
		// unconsumed pending remainder.
		assert_eq!(stable_balance(USDX, pool), USDX_MIN_BALANCE);
		assert_eq!(pool_state(DOT, USDX).total_pending_deposits, 10_000);
		let row = deposit_row(DOT, USDX, 1).expect("row survives");
		assert_eq!(row.pending_deposit.expect("pending remainder").amount, 10_000);
		assert_eq!(row.claimable_collateral, 40_000);
	});
}
