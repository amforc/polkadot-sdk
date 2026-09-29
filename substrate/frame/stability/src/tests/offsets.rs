//! Active-pool offsets: how an offset is capped, how the collateral is shared out through `S`,
//! how `P` shrinks, and why none of it disturbs the yield already recorded in `G`.

use crate::{mock::*, Error};
use pusd_primitives::{
	BranchMode, BranchSnapshot, OffsetLegs, StabilityPoolInspect, StabilityPoolOffset,
};

#[test]
fn offset_burns_debt_and_distributes_gains_proportionally() {
	build_with_default_market(|| {
		seed_matured_deposit(1, 600);
		seed_matured_deposit(2, 400);

		// 500 debt seizes 500 / 1.25 = 400 collateral at the registration
		// price.
		let offset = simulate_offset(DOT, PUSD, 500, 400);
		assert_eq!(offset.debt(), 500);
		assert_eq!(offset.leftover, 0);

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 500);
		// P = 1 * (1000 - 500) / 1000 = 0.5.
		assert_eq!(state.coords.p, FixedU128::from_rational(1, 2));
		assert_eq!(state.total_collateral_gains_unclaimed, 400);
		// delta_S = 400 * (1/1000) = 0.4.
		let sums = active_sums(0, 0);
		assert_eq!(sums.s_collateral, FixedU128::from_inner(400_000_000_000_000_000));

		// 500 of the pool's 1000 stablecoin was burned.
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 500);
		assert_eq!(collateral_balance(DOT, pool), 400);

		System::assert_has_event(
			crate::Event::OffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				active: Some(crate::types::LegCoords { epoch: 0, scale: 0 }),
				pending: None,
			}
			.into(),
		);

		// Compounded: floor(600 * 0.5) = 300; floor(400 * 0.5) = 200.
		// Gains: floor(600 * 0.4) = 240; floor(400 * 0.4) = 160.
		assert_claim_collateral(1, 240);
		assert_claim_collateral(2, 160);
		assert_ok!(withdraw(1, DOT, PUSD, 1_000, 1));
		assert_eq!(stable_balance(PUSD, 1), 300);
		assert_ok!(withdraw(2, DOT, PUSD, 1_000, 2));
		assert_eq!(stable_balance(PUSD, 2), 200);
	});
}

#[test]
fn offset_clamps_at_the_floor_then_only_depletion_passes() {
	build_with_default_market(|| {
		seed_matured_deposit(1, 1_000);

		// A first offset moves P off one, so the later equations exercise
		// P0 != 1: P = 800/1000 = 0.8, delta_S = 160 * (1/1000) = 0.16.
		assert_eq!(simulate_offset(DOT, PUSD, 200, 160).debt(), 200);
		assert_eq!(pool_state(DOT, PUSD).coords.p, FixedU128::from_rational(4, 5));

		// 750 of the remaining 800 would leave 50 < 100 (the floor): clamped
		// to 700, and the collateral share scales down with it:
		// floor(600 * 700 / 750) = 560, returning the unconsumed 40 with the
		// credit. P = 0.8 * (100/800) = 0.1,
		// delta_S = 560 * (0.8/800) = 0.56, so S = 0.72.
		let offset = simulate_offset(DOT, PUSD, 750, 600);
		assert_eq!(offset.debt(), 700);
		assert_eq!(offset.leftover, 40);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 100);

		// A = 100 = floor: any partial offset clamps to zero (no-op)...
		let offset = simulate_offset(DOT, PUSD, 50, 40);
		assert_eq!(offset.debt(), 0);
		assert_eq!(offset.leftover, 40);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 100);

		// ...while full depletion passes and starts epoch 1:
		// delta_S = 80 * (0.1/100) = 0.08, so S = 0.8.
		let offset = simulate_offset(DOT, PUSD, 100, 80);
		assert_eq!(offset.debt(), 100);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.coords, Accumulators { p: FixedU128::one(), epoch: 1, scale: 0 });

		// Fully depleted: nothing left to withdraw (the row still exists,
		// carrying the unclaimed gains).
		assert_noop!(withdraw(1, DOT, PUSD, 1, 1), Error::<Test>::NoActiveDeposit);
		// The depositor absorbed all three offsets:
		// gain = (D0/P0) * S = (1000/1) * 0.8 = 800 = 160 + 560 + 80.
		assert_claim_collateral(1, 800);
		// The final claim emptied the row entirely.
		assert!(deposit_row(DOT, PUSD, 1).is_none());
	});
}

/// The guards of the offset interface, read through the trait rather than through the mock's
/// simulation, which only offers what a quote sized and so never reaches a refusal.
///
/// Every refusal consumes the credit it was handed, so each probe issues its collateral inside a
/// hypothetical that rolls the issuance back with it.
#[test]
fn offset_without_capacity_quotes_nothing_and_refuses_settlement() {
	build_and_execute(|| {
		let offset_active = |branch: BranchSnapshot, debt: Balance, collateral: Balance| {
			hypothetically!(Stability::offset(
				&DOT,
				&PUSD,
				branch,
				OffsetLegs { active: debt, pending: 0 },
				OffsetLegs {
					active: issue_collateral(DOT, collateral),
					pending: issue_collateral(DOT, 0),
				},
			))
		};

		// Unregistered market: no pool row exists, so even a healthy mode has nothing to quote
		// and nothing to settle against.
		let healthy = BranchSnapshot { mode: BranchMode::Normal, now: Timestamp::get() };
		assert!(Stability::quote(&DOT, &PUSD, healthy).is_none());
		assert_err!(offset_active(healthy, 100, 50), Error::<Test>::OffsetSettlementFailed);

		// Registered but empty pool: the market quotes, for nothing, and a caller that settles
		// past its quote is refused.
		register_branch(DOT, PUSD, default_branch_config());
		assert_eq!(Stability::quote_active(&market_quote(&DOT, &PUSD), 100), 0);
		assert_err!(
			offset_active(branch_snapshot(&DOT, &PUSD), 100, 50),
			Error::<Test>::OffsetSettlementFailed
		);

		// A funded pool whose cohort is due and not yet advanced in storage. The quote counts
		// the matured 1_000 as active, so capacity is not what refuses the calls below.
		seed_matured_deposit(1, 1_000);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 0);
		let branch = branch_snapshot(&DOT, &PUSD);
		assert_eq!(branch.mode, BranchMode::Normal);
		let quote = market_quote(&DOT, &PUSD);
		assert_eq!(Stability::quote_active(&quote, 100), 100);

		// A zero request quotes nothing. An offset with no debt on either leg settles nothing
		// and returns before it can advance the due cohort, so it writes nothing.
		assert_eq!(Stability::quote_active(&quote, 0), 0);
		let no_collateral =
			OffsetLegs { active: issue_collateral(DOT, 0), pending: issue_collateral(DOT, 0) };
		let burned = storage_noop(|| {
			Stability::offset(
				&DOT,
				&PUSD,
				branch,
				OffsetLegs { active: 0, pending: 0 },
				no_collateral,
			)
		});
		assert!(matches!(burned, Ok(None)));

		// A leg that cancels no debt may carry no collateral: the pool would receive it for
		// free.
		assert_err!(offset_active(branch, 0, 50), Error::<Test>::OffsetSettlementFailed);

		// A frozen market has no capacity, however well funded its pool is.
		let frozen = BranchSnapshot { mode: BranchMode::Frozen, now: branch.now };
		assert!(Stability::quote(&DOT, &PUSD, frozen).is_none());
		assert_err!(offset_active(frozen, 100, 50), Error::<Test>::OffsetSettlementFailed);
	});
}

#[test]
fn combined_offset_settles_active_then_pending() {
	build_and_execute(|| {
		register_branch(DOT, USDX, branch_config_for(DOT, USDX));
		mint_stable(USDX, 1, 60_000);
		assert_ok!(deposit_and_mature(1, DOT, USDX, 60_000));
		mint_stable(USDX, 2, 40_000);
		assert_ok!(deposit(2, DOT, USDX, 40_000));

		let quote = market_quote(&DOT, &USDX);
		assert_eq!(Stability::quote_active(&quote, 60_000), 60_000);
		assert_eq!(Stability::quote_pending(&quote, 40_000, 60_000), 40_000);
		// USDX has a 10_000-unit minimum. Active first preserves the shared
		// account at 40_000; pending then performs the full expendable drain.
		// Reversing the order would leave active unable to drain under the
		// `Preserve` decision made by its sizing pass.
		hypothetically!({
			assert_ok!(Stability::offset(
				&DOT,
				&USDX,
				branch_snapshot(&DOT, &USDX),
				OffsetLegs { active: 60_000, pending: 40_000 },
				OffsetLegs {
					active: issue_collateral(DOT, 240),
					pending: issue_collateral(DOT, 160),
				},
			));

			let pool_account = Stability::pool_account(&DOT, &USDX);
			let state = pool_state(DOT, USDX);
			assert_eq!(state.total_active_deposits, 0);
			assert_eq!(state.total_pending_deposits, 0);
			assert_eq!(state.total_collateral_gains_unclaimed, 400);
			assert_eq!(state.coords, Accumulators { p: FixedU128::one(), epoch: 1, scale: 0 });
			assert_eq!(
				state.pending_coords,
				Accumulators { p: FixedU128::one(), epoch: 1, scale: 0 }
			);
			assert_eq!(stable_balance(USDX, pool_account), 0);
			assert_eq!(collateral_balance(DOT, pool_account), 400);
			assert_eq!(
				crate::PoolSumsStore::<Test>::get((DOT, USDX, Leg::Active, 0u32, 0u32))
					.s_collateral,
				FixedU128::from_rational(1, 250)
			);
			assert_eq!(
				crate::PoolSumsStore::<Test>::get((DOT, USDX, Leg::Pending, 0u32, 0u32))
					.s_collateral,
				FixedU128::from_rational(1, 250)
			);
			// Both legs report in one event.
			System::assert_has_event(
				crate::Event::OffsetApplied {
					collateral_id: DOT,
					stable_id: USDX,
					active: Some(crate::types::LegCoords { epoch: 1, scale: 0 }),
					pending: Some(crate::types::LegCoords { epoch: 1, scale: 0 }),
				}
				.into(),
			);
		});
	});
}

#[test]
fn offset_refuses_stale_sizing_reads() {
	build_with_default_market(|| {
		set_min_active_pool(100);
		seed_matured_deposit(1, 1_000);

		// 950 would strand 50 below the 100 minimum: the read clamps to 900,
		// and demanding the unclamped 950 anyway fails exactly. The probe
		// credit is issued inside the rolled-back hypothetical.
		let quote = market_quote(&DOT, &PUSD);
		assert_eq!(Stability::quote_active(&quote, 950), 900);
		// Everything is activated: the pending leg sizes to zero even behind the active
		// reservation.
		assert_eq!(Stability::quote_pending(&quote, 100, 900), 0);
		assert_err!(
			hypothetically!(Stability::offset(
				&DOT,
				&PUSD,
				branch_snapshot(&DOT, &PUSD),
				OffsetLegs { active: 950, pending: 0 },
				OffsetLegs {
					active: issue_collateral(DOT, 400),
					pending: issue_collateral(DOT, 0)
				},
			)),
			Error::<Test>::OffsetSettlementFailed,
		);

		// The clamped amount itself executes exactly.
		hypothetically!({
			assert_ok!(Stability::offset(
				&DOT,
				&PUSD,
				branch_snapshot(&DOT, &PUSD),
				OffsetLegs { active: 900, pending: 0 },
				OffsetLegs {
					active: issue_collateral(DOT, 400),
					pending: issue_collateral(DOT, 0)
				},
			));
			assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 100);
		});
	});
}

#[test]
fn compounded_yield_absorbs_offsets() {
	build_with_default_market(|| {
		set_min_active_pool(20);
		seed_matured_deposit(1, 600);
		drop(distribute_yield(DOT, PUSD, 60));
		assert_ok!(compound(1, DOT, PUSD, 60));

		// The compounded 60 is offsettable: A = 660, and the 601 offset
		// exceeds the original 600 deposit — only possible because the
		// compounded yield absorbs too. The survival ratio 59/660 is
		// 0.089393..., so the 18-decimal P floors and the withdrawal pays
		// floor(660 * P) = 58; the odd unit strands as pool-owned dust.
		assert_eq!(simulate_offset(DOT, PUSD, 601, 0).debt(), 601);
		assert_ok!(withdraw(1, DOT, PUSD, 1_000, 1));
		assert_eq!(stable_balance(PUSD, 1), 58);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 1);
	});
}

#[test]
fn offset_rounds_down_at_the_pool_minimum_balance_dead_zone() {
	build_and_execute(|| {
		register_branch(DOT, USDX, branch_config_for(DOT, USDX));
		// One active depositor plus 6_000 raw units of pending deposit, so
		// the pool balance exceeds the active total by less than the
		// 10_000-unit USDX minimum.
		mint_stable(USDX, 1, 100_000);
		assert_ok!(deposit_and_mature(1, DOT, USDX, 100_000));
		mint_stable(USDX, 2, 16_000);
		assert_ok!(deposit(2, DOT, USDX, 6_000));

		let pool = Stability::pool_account(&DOT, &USDX);
		assert_eq!(stable_balance(USDX, pool), 106_000);

		// The accounting cap allows the full 100_000 active total, but
		// burning it would strand 6_000 < 10_000 on the pool account: the
		// plan rounds the offset down to the preserving limit 96_000, and
		// the collateral share scales with it: floor(80_000 * 96_000 /
		// 100_000) = 76_800.
		let offset = simulate_offset(DOT, USDX, 100_000, 80_000);
		assert_eq!(offset.debt(), 96_000);
		assert_eq!(offset.leftover, 80_000 - 76_800);

		// The pool sits exactly at the minimum: alive, no dust burned, and
		// the balance still backs active 4_000 + pending 6_000.
		assert_eq!(stable_balance(USDX, pool), USDX_MIN_BALANCE);
		let state = pool_state(DOT, USDX);
		assert_eq!(state.total_active_deposits, 4_000);
		assert_eq!(state.total_pending_deposits, 6_000);
	});
}
