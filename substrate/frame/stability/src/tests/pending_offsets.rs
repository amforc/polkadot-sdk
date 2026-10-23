//! Pending-deposit offsets: the last-resort backstop.
//!
//! The backstop takes from every pending deposit in proportion to its size, not from the oldest
//! first, and the pending accumulators track that in constant time. The active accumulators are
//! never touched.

use crate::{mock::*, Error};

#[test]
fn pending_offset_ignores_active_deposits_and_accumulators() {
	// The active pool sits at its 100 floor, so a partial offset of it quotes nothing and the
	// whole debt falls to the pending leg: the only way the quote reaches pending capital
	// while active capital remains.
	ExtBuilder::default()
		.default_branch()
		.matured_deposit(1, 100)
		.deposit(2, 400)
		.build_and_execute(|| {
			distribute_all_yield(60);
			let before = pool_state(DOT, STABLE);
			let sums_before = active_sums(0, 0);

			let offset = simulate_offset(DOT, STABLE, 80, 40);

			assert_eq!(offset, SimulatedOffset { active: 0, pending: 80, leftover: 0 });

			// Only pending capital moved, so the active side is unchanged down to the last
			// digit.
			let after = pool_state(DOT, STABLE);
			assert_eq!(after.coords, before.coords);
			assert_eq!(after.total_active_deposits, 100);
			assert_eq!(after.total_pending_deposits, 320);
			assert_eq!(active_sums(0, 0), sums_before);
			// The pending pair took the whole hit: P_pending = 320/400 = 0.8,
			// so the row realizes floor(400 * 0.8) = 320.
			assert_eq!(after.pending_coords.p, FixedU128::from_rational(4, 5));
			assert_eq!(pending_after_settlement(DOT, STABLE, 2), 320);

			// The active depositor's yield claim is untouched.
			assert_claim_yield(1, 60);
		});
}

#[test]
fn pending_offset_clamps_to_the_minimum_pool_floor() {
	ExtBuilder::default().default_branch().deposit(1, 200).build_and_execute(|| {
		// Burning 150 of 200 would leave 50 < the 100
		// `minimum_active_pool_balance` floor (the same rule as the
		// active side — it is what sizes a pool against `P`-precision
		// exhaustion, and the pending `P` runs on the same precision
		// parameters): the offset clamps to 100 and the collateral follows
		// pro-rata, floor(150 * 100 / 150) = 100.
		let offset = simulate_offset(DOT, STABLE, 150, 150);

		assert_eq!(offset.debt(), 100);
		assert_eq!(offset.leftover, 50);

		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_pending_deposits, 100);
		assert_eq!(state.pending_coords.p, FixedU128::from_rational(1, 2));
		assert_eq!(pending_after_settlement(DOT, STABLE, 1), 100);
	});
}

#[test]
fn merged_top_up_shares_earlier_backstop_losses() {
	ExtBuilder::default().default_branch().deposit(1, 200).build_and_execute(|| {
		// Backstop halves the pending pool: P_pending = 100/200 = 0.5.
		let offset = simulate_offset(DOT, STABLE, 100, 0);
		assert_eq!(offset.debt(), 100);

		// The top-up realizes the loss first — floor(200 * 0.5) = 100 — and
		// merges at the current pending accumulators: 100 + 300 = 400.
		seed_deposit(1, 300);

		let row = deposit_row(DOT, STABLE, 1).expect("row exists");
		assert_eq!(row.pending_deposit.expect("merged").amount, 400);
		assert_eq!(pool_state(DOT, STABLE).total_pending_deposits, 400);

		// A second backstop consumption prices the merged amount as one
		// stake: burning 200 of 400 halves it again to 200.
		let offset = simulate_offset(DOT, STABLE, 200, 0);
		assert_eq!(offset.debt(), 200);
		assert_eq!(pending_after_settlement(DOT, STABLE, 1), 200);

		// Activation folds the post-loss amount into the active pool.
		advance_time(10_000);
		assert_ok!(settle(1, 1, DOT, STABLE));

		let row = deposit_row(DOT, STABLE, 1).expect("row exists");
		assert_eq!(row.active_deposit, 200);
		assert!(row.pending_deposit.is_none());
		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 200);
		assert_eq!(state.total_pending_deposits, 0);
	});
}

/// One offset that depletes the active pool and the pending backstop together.
///
/// Both legs price at the same credit-wide ratio 1152.845 / 2200 = 0.52402045… DOT per STABLE,
/// because each slice is pro rata against the whole debt. The legs therefore differ only by
/// flooring, and each floor strands at most one base unit.
#[test]
fn active_then_pending_depletion_strands_bounded_dust() {
	const DOT_E10: Balance = 10_000_000_000;

	ExtBuilder::default()
		.default_branch()
		.matured_deposit(1, 1_501) // active pool = 1501 STABLE, P = 1, epoch 0.
		.deposit(2, 250) // pending pool, alongside ...
		.deposit(3, 100) // ... user 3 — consumed pro-rata, not in order.
		.build_and_execute(|| {
			// 2200 STABLE of debt is offered against 1152.845 DOT. The active leg depletes the 1501
			// pool, and the pending leg quoted behind it depletes the 350 backstop; the 349 STABLE
			// the pool cannot take stay with the caller, with their share:
			//   active slice  = floor(11_528_450_000_000 * 1501 / 2200) = 7_865_547_022_727,
			//   pending slice = floor(11_528_450_000_000 *  350 / 2200) = 1_834_071_590_909,
			// i.e. 0.52402 DOT per STABLE burned on both legs.
			let c0 = 1_152_845 * (DOT_E10 / 1_000); // 11_528_450_000_000
			let offset = simulate_offset(DOT, STABLE, 2_200, c0);

			assert_eq!(
				offset,
				SimulatedOffset { active: 1_501, pending: 350, leftover: 1_828_831_386_364 }
			);

			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_active_deposits, 0);
			assert_eq!(state.coords, Accumulators { p: FixedU128::one(), epoch: 1, scale: 0 });
			let sums = active_sums(0, 0);
			assert_eq!(sums.s_collateral, FixedU128::from_rational(7_865_547_022_727, 1_501));
			// Pending: delta_S = floor(1_834_071_590_909e18 / 350), 0.5240204545 DOT per STABLE.
			assert_eq!(state.total_pending_deposits, 0);
			assert_eq!(state.pending_coords.epoch, 1);

			System::assert_has_event(market_event!(OffsetApplied {
				active: Some(crate::types::LegCoords { epoch: 1, scale: 0 }),
				pending: Some(crate::types::LegCoords { epoch: 1, scale: 0 }),
			}));

			// The consumed pending depositors realize their pro-rata
			// gains through the pending `S` on claim:
			//   user 2: floor(250 * delta_S) = 1_310_051_136_363,
			//   user 3: floor(100 * delta_S) =   524_020_454_545.
			// The double-floor strands 1 base unit of the recorded 183.4071 DOT
			// gain in the unclaimed total.
			assert_claim_collateral(2, 1_310_051_136_363);
			assert!(deposit_row(DOT, STABLE, 2).is_none());
			assert_claim_collateral(3, 524_020_454_545);
			assert!(deposit_row(DOT, STABLE, 3).is_none());

			// The sole active depositor. The depletion compounded its
			// deposit to zero; its collateral is realized through S on claim. The
			// double-floor (`floor(1501 * floor(collat * 1e18 / 1501) / 1e18)`)
			// strands 1 base unit in the unclaimed total, so it realizes one less
			// than the pool's recorded gain.
			assert_noop!(withdraw(1, DOT, STABLE, 1, 1), Error::<Test>::NoActiveDeposit);
			assert_claim_collateral(1, 7_865_547_022_726);
			assert!(deposit_row(DOT, STABLE, 1).is_none());

			// Every STABLE the pool held (1501 active + 350 pending) was burned, and the two
			// stranded base units are all the collateral the pool still holds.
			assert_eq!(pool_stable_held(DOT, STABLE), 0);
			assert_eq!(pool_state(DOT, STABLE).total_collateral_gains_unclaimed, 2);
		});
}

/// The accepted pending-deposit portion of a liquidation allocation, shared
/// pro-rata across every pending deposit: each of the three depositors takes
/// its share of both the burn and the collateral, whatever its age. The pool
/// keeps 400 of the 1_400 pending total.
#[test]
fn pending_deposit_offset_is_shared_pro_rata() {
	ExtBuilder::default()
		.default_branch()
		.deposit(1, 300) // Alice.
		.deposit(2, 600) // Bob.
		.deposit(3, 500) // Cara.
		.build_and_execute(|| {
			// One O(1) accumulator update covers every row. Burning 1_000 of 1_400 leaves
			// P_pending = 400/1_400 = 2/7 (inner floor(400e18/1_400) = 285_714_285_714_285_714)
			// and distributes the 500 collateral at delta_S = 500/1_400 = 5/14
			// (inner 357_142_857_142_857_142).
			let offset = simulate_offset(DOT, STABLE, 1_000, 500);

			assert_eq!(offset, SimulatedOffset { active: 0, pending: 1_000, leftover: 0 });
			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_pending_deposits, 400);
			assert_eq!(state.total_collateral_gains_unclaimed, 500);
			assert_eq!(
				state.pending_coords,
				Accumulators {
					p: FixedU128::from_inner(285_714_285_714_285_714),
					epoch: 0,
					scale: 0
				}
			);
			assert_eq!(pool_stable_held(DOT, STABLE), 400);
			System::assert_has_event(market_event!(OffsetApplied {
				active: None,
				pending: Some(crate::types::LegCoords { epoch: 0, scale: 0 }),
			}));

			// Rows realize lazily. Remainders are floor(stake * 2/7): 85 / 171 / 142, with the
			// 2-unit flooring residue staying inside `total_pending_deposits`. Settlement realizes
			// the loss and the direct credit onto the row, and the gain is claimable through the
			// normal path: floor(stake * 5/14) = 107 / 214 / 178, with 1 unit of the 500 stranded
			// in the unclaimed total.
			for (who, remaining, gain) in [(1, 85, 107), (2, 171, 214), (3, 142, 178)] {
				assert_eq!(pending_after_settlement(DOT, STABLE, who), remaining);
				assert_ok!(settle(who, who, DOT, STABLE));
				let row =
					deposit_row(DOT, STABLE, who).expect("kept: pending remainder + claimable");
				assert_eq!(
					row.pending_deposit.as_ref().expect("partially consumed").amount,
					remaining
				);
				assert_eq!(row.claimable_collateral, gain);
				assert_claim_collateral(who, gain);
			}
			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_pending_deposits, 400);
			assert_eq!(state.total_collateral_gains_unclaimed, 1);
		});
}
