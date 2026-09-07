//! Pending-deposit offsets: the last-resort backstop.
//!
//! The backstop takes from every pending deposit in proportion to its size, not from the oldest
//! first, and the pending accumulators track that in constant time. The active accumulators are
//! never touched.

use crate::{mock::*, Error};

#[test]
fn pending_offset_full_depletion_bumps_the_pending_epoch() {
	build_with_default_market(|| {
		for who in 1..=3 {
			seed_deposit(who, 100);
		}

		// The request exceeds the 300 pending total: full depletion, one
		// call, no per-depositor iteration cap. The collateral slice is
		// floor(150 * 300 / 1_000) = 45, so delta_S = 45/300 = 0.15.
		let (debt_offset, leftover) = simulate_pending_offset(DOT, PUSD, 1_000, 150);
		assert_eq!(debt_offset, 300);
		assert_eq!(leftover, 105);

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.pending_coords, Accumulators { p: FixedU128::one(), epoch: 1, scale: 0 });

		// Every row compounds to zero (epoch behind) but keeps its window
		// gain of floor(100 * 0.15) = 15.
		for who in 1..=3 {
			assert_eq!(realized_pending(DOT, PUSD, who), 0);
			assert_claim_collateral(who, 15);
			assert!(deposit_row(DOT, PUSD, who).is_none());
		}

		// A fresh deposit joins the new epoch cleanly.
		seed_deposit(1, 100);
		let row = deposit_row(DOT, PUSD, 1).expect("row created");
		assert_eq!(row.pending_deposit.expect("queued").snapshot.coords.epoch, 1);

		// A second depletion against a near-worthless credit: floor(1 * 100 / 1_000) = 0, so
		// the whole pending amount burns for a zero collateral credit. The flooring loss is
		// bounded by one collateral base unit per offset and only visible when the credit is
		// nearly worthless relative to the debt.
		assert_eq!(simulate_pending_offset(DOT, PUSD, 1_000, 1), (100, 1));
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.pending_coords.epoch, 2);
		assert_eq!(stable_balance(PUSD, Stability::pool_account(&DOT, &PUSD)), 0);

		// The row still holds the stale pending leg; realization is lazy. The next touch settles
		// it to nothing and prunes the empty row.
		assert!(deposit_row(DOT, PUSD, 1).is_some());
		assert_ok!(settle(7, 1, DOT, PUSD));
		assert!(deposit_row(DOT, PUSD, 1).is_none());
	});
}

#[test]
fn pending_offset_noop_cases_pass_remainders_through() {
	build_and_execute(|| {
		// Unregistered market: the credit comes back whole and nothing is written.
		assert_storage_noop!(assert_eq!(simulate_pending_offset(DOT, PUSD, 100, 50), (0, 50)));

		// Empty pending pool: same.
		register_branch(DOT, PUSD, default_branch_config());
		assert_storage_noop!(assert_eq!(simulate_pending_offset(DOT, PUSD, 100, 50), (0, 50)));

		// Zero remaining debt with a populated pending pool: same.
		seed_deposit(1, 200);
		assert_storage_noop!(assert_eq!(simulate_pending_offset(DOT, PUSD, 0, 50), (0, 50)));
	});
}

#[test]
fn pending_offset_ignores_active_deposits_and_accumulators() {
	build_with_default_market(|| {
		mint_stable(PUSD, 1, 600);
		assert_ok!(deposit_and_mature(1, DOT, PUSD, 600));
		seed_deposit(2, 400);
		drop(distribute_yield(DOT, PUSD, 60));

		let before = pool_state(DOT, PUSD);
		let sums_before = active_sums(0, 0);

		let (debt_offset, leftover) = simulate_pending_offset(DOT, PUSD, 200, 100);
		assert_eq!(debt_offset, 200);
		assert_eq!(leftover, 0);

		// Only pending capital moved, so the active side is unchanged down to the last
		// digit.
		let after = pool_state(DOT, PUSD);
		assert_eq!(after.coords, before.coords);
		assert_eq!(after.total_active_deposits, 600);
		assert_eq!(after.total_pending_deposits, 200);
		assert_eq!(active_sums(0, 0), sums_before);
		// The pending pair took the whole hit: P_pending = 200/400 = 0.5,
		// so the row realizes floor(400 * 0.5) = 200.
		assert_eq!(after.pending_coords.p, FixedU128::from_rational(1, 2));
		assert_eq!(realized_pending(DOT, PUSD, 2), 200);

		// The active depositor's yield claim is untouched.
		assert_claim_yield(1, 60);
	});
}

#[test]
fn pending_offset_clamps_to_the_minimum_pool_floor() {
	build_with_default_market(|| {
		seed_deposit(1, 200);

		// Burning 150 of 200 would leave 50 < the 100
		// `minimum_active_pool_balance` floor (the same rule as the
		// active side — it is what sizes a pool against `P`-precision
		// exhaustion, and the pending `P` runs on the same precision
		// parameters): the offset clamps to 100 and the collateral follows
		// pro-rata, floor(150 * 100 / 150) = 100.
		let (debt_offset, leftover) = simulate_pending_offset(DOT, PUSD, 150, 150);
		assert_eq!(debt_offset, 100);
		assert_eq!(leftover, 50);

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_pending_deposits, 100);
		assert_eq!(state.pending_coords.p, FixedU128::from_rational(1, 2));
		assert_eq!(realized_pending(DOT, PUSD, 1), 100);
	});
}

#[test]
fn pending_offset_accepts_sub_minimum_gain_after_registration_touch() {
	build_and_execute(|| {
		// Same registration invariant as the active offset: the pool account accepts every positive
		// gain even when the issued asset's minimum balance is larger.
		assert_ok!(Assets::force_create(RuntimeOrigin::root(), 77, 1, true, 1_000));
		let coll = AssetId::WithId(77);
		register_branch(coll.clone(), PUSD, branch_config_for(coll.clone(), PUSD));
		mint_stable(PUSD, 1, 200);
		assert_ok!(deposit(1, coll.clone(), PUSD, 200));

		// Gain 500 is below the 1_000 minimum but enters the pre-created account.
		let (debt_offset, leftover) = simulate_pending_offset(coll.clone(), PUSD, 200, 500);
		assert_eq!(debt_offset, 200);
		assert_eq!(leftover, 0);
		let row = deposit_row(coll.clone(), PUSD, 1).expect("kept");
		assert_eq!(row.pending_deposit.expect("lazy realization").amount, 200);
		let state = pool_state(coll.clone(), PUSD);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.pending_coords.p, FixedU128::one());
		assert_eq!(state.pending_coords.epoch, 1);
		let pool = Stability::pool_account(&coll, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 0);
		assert_eq!(collateral_balance(coll, pool), 500);
	});
}

#[test]
fn merged_top_up_shares_earlier_backstop_losses() {
	build_with_default_market(|| {
		seed_deposit(1, 200);

		// Backstop halves the pending pool: P_pending = 100/200 = 0.5.
		let (debt_offset, _) = simulate_pending_offset(DOT, PUSD, 100, 0);
		assert_eq!(debt_offset, 100);

		// The top-up realizes the loss first — floor(200 * 0.5) = 100 — and
		// merges at the current pending accumulators: 100 + 300 = 400.
		mint_stable(PUSD, 1, 300);
		assert_ok!(deposit(1, DOT, PUSD, 300));
		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.pending_deposit.expect("merged").amount, 400);
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 400);

		// A second backstop consumption prices the merged amount as one
		// stake: burning 200 of 400 halves it again to 200.
		let (debt_offset, _) = simulate_pending_offset(DOT, PUSD, 200, 0);
		assert_eq!(debt_offset, 200);
		assert_eq!(realized_pending(DOT, PUSD, 1), 200);

		// Activation folds the post-loss amount into the active pool.
		advance_time(10_000);
		assert_ok!(settle(1, 1, DOT, PUSD));
		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.active_deposit, 200);
		assert!(row.pending_deposit.is_none());
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 200);
		assert_eq!(state.total_pending_deposits, 0);
	});
}

/// One liquidation cascading through the full waterfall —
/// active-pool offset, keeper JIT, the pending-deposit backstop, and the
/// redistribution residual. This Stability test isolates the two pool stages;
/// keeper JIT and final redistribution are modelled as the arithmetic Vaults
/// performs around those calls.
///
/// Every stage prices at the same credit-wide ratio 1152.845 / 2200
/// = 0.52402045… DOT per pUSD — each split is pro-rata against the debt
/// still standing, so stages differ only by flooring.
#[test]
fn full_liquidation_waterfall_active_jit_pending_and_residual() {
	build_and_execute(|| {
		const DOT_E10: Balance = 10_000_000_000;

		register_branch(DOT, PUSD, default_branch_config());
		seed_matured_deposit(1, 1_501); // active pool = 1501 pUSD, P = 1, epoch 0.
		seed_deposit(2, 250); // pending pool, alongside ...
		seed_deposit(3, 100); // ... user 3 — consumed pro-rata, not in order.

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
		assert_eq!(state.coords, Accumulators { p: FixedU128::one(), epoch: 1, scale: 0 });
		let sums = active_sums(0, 0);
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

		// Stage 2 — keeper JIT (performed by Vaults, not a Stability call). It
		// burns 300 pUSD with keeper liquidity and takes the matching collateral
		// share off the remainder before the pending backstop runs.
		let jit_debt = 300;
		let jit_collateral = pro_rata_floor(leftover1, jit_debt, 699);
		// 157.2061 / 300 = 0.52402 DOT per pUSD — the same stage price.
		assert_eq!(jit_collateral, 1_572_061_363_636); // 157.2061363636 DOT.
		let after_jit_debt = 699 - jit_debt; // 399 pUSD.
		let after_jit_collat = leftover1 - jit_collateral; // 2_090_841_613_637 = 209.0841613637 DOT.
		assert_eq!(after_jit_collat, 2_090_841_613_637);

		// Stage 3 — pending-deposit backstop, one pro-rata consumption. The
		// 350 pending total is fully depleted against the 399 still standing:
		//   collateral slice = floor(2_090_841_613_637 * 350 / 399)
		//                    = 1_834_071_590_909  (183.4071590909 DOT),
		//   delta_S = floor(1_834_071_590_909e18 / 350)
		//           = 5_240_204_545_454_285_714_285_714_285e-18,
		//             i.e. 0.5240204545 DOT per pUSD — the same stage price.
		let (debt_offset2, leftover2) =
			simulate_pending_offset(DOT, PUSD, after_jit_debt, after_jit_collat);
		assert_eq!(debt_offset2, 350);
		// The residual — the 49 pUSD debt still standing + this collateral —
		// is what Vaults records for redistribution.
		assert_eq!(leftover2, 256_770_022_728); // 25.6770022728 DOT.

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(state.pending_coords.epoch, 1);

		System::assert_has_event(
			crate::Event::PendingDepositOffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				debt_burned: 350,
				collateral_gain: 1_834_071_590_909,
				epoch: 1,
				scale: 0,
			}
			.into(),
		);

		// Stage 4 — the consumed pending depositors realize their pro-rata
		// gains through the pending `S` on claim:
		//   user 2: floor(250 * delta_S) = 1_310_051_136_363,
		//   user 3: floor(100 * delta_S) =   524_020_454_545.
		// The double-floor strands 1 base unit of the recorded 183.4071 DOT
		// gain in the unclaimed total.
		assert_claim_collateral(2, 1_310_051_136_363);
		assert!(deposit_row(DOT, PUSD, 2).is_none());
		assert_claim_collateral(3, 524_020_454_545);
		assert!(deposit_row(DOT, PUSD, 3).is_none());

		// Stage 5 — the sole active depositor. The depletion compounded its
		// deposit to zero; its collateral is realized through S on claim. The
		// double-floor (`floor(1501 * floor(collat * 1e18 / 1501) / 1e18)`)
		// strands 1 base unit in the unclaimed total, so it realizes one less
		// than the pool's recorded gain.
		assert_noop!(withdraw(1, DOT, PUSD, 1, 1), Error::<Test>::NoActiveDeposit);
		assert_claim_collateral(1, 7_865_547_022_726);
		assert!(deposit_row(DOT, PUSD, 1).is_none());

		// Every pUSD the pool held (1501 active + 350 pending) was burned.
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 0);
	});
}

#[test]
fn pending_backstop_rounds_down_at_the_minimum_balance_dead_zone() {
	build_and_execute(|| {
		register_branch(DOT, USDX, branch_config_for(DOT, USDX));
		mint_stable(USDX, 1, 60_000);
		assert_ok!(deposit(1, DOT, USDX, 50_000));

		let pool = Stability::pool_account(&DOT, &USDX);
		assert_eq!(stable_balance(USDX, pool), 50_000);

		// Burning 45_000 would strand 5_000 < 10_000 on the pool account:
		// the offset rounds down to 40_000. The collateral follows pro-rata
		// (floor(45_000 * 40_000 / 45_000) = 40_000) and
		// P_pending = 10_000/50_000 = 0.2.
		let (debt_offset, leftover) = simulate_pending_offset(DOT, USDX, 45_000, 45_000);
		assert_eq!(debt_offset, 40_000);
		assert_eq!(leftover, 5_000);
		System::assert_has_event(
			crate::Event::PendingDepositOffsetApplied {
				collateral_id: DOT,
				stable_id: USDX,
				debt_burned: 40_000,
				collateral_gain: 40_000,
				epoch: 0,
				scale: 0,
			}
			.into(),
		);

		// The pool sits exactly at the minimum; the row realizes the
		// unconsumed remainder floor(50_000 * 0.2) = 10_000 and the direct
		// gain floor(50_000 * 40_000/50_000) = 40_000.
		assert_eq!(stable_balance(USDX, pool), USDX_MIN_BALANCE);
		assert_eq!(pool_state(DOT, USDX).total_pending_deposits, 10_000);
		assert_ok!(settle(1, 1, DOT, USDX));
		let row = deposit_row(DOT, USDX, 1).expect("row survives");
		assert_eq!(row.pending_deposit.expect("pending remainder").amount, 10_000);
		assert_eq!(row.claimable_collateral, 40_000);
	});
}

/// The accepted pending-deposit portion of a liquidation allocation, shared
/// pro-rata across every pending deposit: each of the three depositors takes
/// its share of both the burn and the collateral, whatever its age. The pool
/// keeps 400 of the 1_400 pending total.
#[test]
fn pending_deposit_offset_is_shared_pro_rata() {
	build_with_default_market(|| {
		seed_deposit(1, 300); // Alice.
		seed_deposit(2, 600); // Bob.
		seed_deposit(3, 500); // Cara.

		// One O(1) accumulator update covers every row. Burning 1_000 of 1_400 leaves
		// P_pending = 400/1_400 = 2/7 (inner floor(400e18/1_400) = 285_714_285_714_285_714)
		// and distributes the 500 collateral at delta_S = 500/1_400 = 5/14
		// (inner 357_142_857_142_857_142).
		assert_eq!(simulate_pending_offset(DOT, PUSD, 1_000, 500), (1_000, 0));
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_pending_deposits, 400);
		assert_eq!(state.total_collateral_gains_unclaimed, 500);
		assert_eq!(
			state.pending_coords,
			Accumulators { p: FixedU128::from_inner(285_714_285_714_285_714), epoch: 0, scale: 0 }
		);
		assert_eq!(stable_balance(PUSD, Stability::pool_account(&DOT, &PUSD)), 400);
		System::assert_has_event(
			crate::Event::PendingDepositOffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				debt_burned: 1_000,
				collateral_gain: 500,
				epoch: 0,
				scale: 0,
			}
			.into(),
		);

		// Rows realize lazily. Remainders are floor(stake * 2/7): 85 / 171 / 142, with the
		// 2-unit flooring residue staying inside `total_pending_deposits`. Settlement realizes
		// the loss and the direct credit onto the row, and the gain is claimable through the
		// normal path: floor(stake * 5/14) = 107 / 214 / 178, with 1 unit of the 500 stranded in
		// the unclaimed total.
		for (who, remaining, gain) in [(1, 85, 107), (2, 171, 214), (3, 142, 178)] {
			assert_eq!(realized_pending(DOT, PUSD, who), remaining);
			assert_ok!(settle(who, who, DOT, PUSD));
			let row = deposit_row(DOT, PUSD, who).expect("kept: pending remainder + claimable");
			assert_eq!(row.pending_deposit.as_ref().expect("partially consumed").amount, remaining);
			assert_eq!(row.claimable_collateral, gain);
			assert_claim_collateral(who, gain);
		}
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_pending_deposits, 400);
		assert_eq!(state.total_collateral_gains_unclaimed, 1);
	});
}
