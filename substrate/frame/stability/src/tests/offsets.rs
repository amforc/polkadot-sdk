//! `offset_liquidation`: clamping, proportional `S` gains, `P` compounding,
//! and the interplay with yield (invariant 12).

use crate::{mock::*, Error};

#[test]
fn offset_burns_debt_and_distributes_gains_proportionally() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_deposit(1, 600);
		seed_deposit(2, 400);
		activate_all(&[1, 2]);

		// 500 debt seizes 500 / 1.25 = 400 collateral at the registration
		// price.
		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, 500, 400);
		assert_eq!(debt_offset, 500);
		assert_eq!(leftover, 0);

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 500);
		// P = 1 * (1000 - 500) / 1000 = 0.5.
		assert_eq!(state.coords.p, FixedU128::from_rational(1, 2));
		assert_eq!(state.total_collateral_gains_unclaimed, 400);
		// delta_S = 400 * (1/1000) = 0.4.
		let sums = crate::PoolSumsStore::<Test>::get((DOT, PUSD, 0u32, 0u32));
		assert_eq!(sums.s_collateral, FixedU128::from_inner(400_000_000_000_000_000));

		// 500 of the pool's 1000 stablecoin was burned.
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 500);
		assert_eq!(collateral_balance(DOT, pool), 400);

		System::assert_has_event(
			crate::Event::PoolOffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				debt_burned: 500,
				collateral_gain: 400,
				epoch: 0,
				scale: 0,
			}
			.into(),
		);

		// Compounded: floor(600 * 0.5) = 300; floor(400 * 0.5) = 200.
		// Gains: floor(600 * 0.4) = 240; floor(400 * 0.4) = 160.
		// (Deltas: DOT is native, and accounts hold genesis native balance.)
		let before_1 = collateral_balance(DOT, 1);
		let before_2 = collateral_balance(DOT, 2);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_ok!(claim_collateral(2, DOT, PUSD, 2));
		assert_eq!(collateral_balance(DOT, 1) - before_1, 240);
		assert_eq!(collateral_balance(DOT, 2) - before_2, 160);
		assert_ok!(withdraw(1, DOT, PUSD, 1_000, 1));
		assert_eq!(stable_balance(PUSD, 1), 300);
		assert_ok!(withdraw(2, DOT, PUSD, 1_000, 2));
		assert_eq!(stable_balance(PUSD, 2), 200);
	});
}

#[test]
fn offset_clamps_at_the_floor_then_only_depletion_passes() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_deposit(1, 1_000);
		activate_all(&[1]);

		// A first offset moves P off one, so the later equations exercise
		// P0 != 1: P = 800/1000 = 0.8, delta_S = 160 * (1/1000) = 0.16.
		assert_eq!(simulate_offset(DOT, PUSD, 200, 160).0, 200);
		assert_eq!(pool_state(DOT, PUSD).coords.p, FixedU128::from_rational(4, 5));

		// 750 of the remaining 800 would leave 50 < 100 (the floor): clamped
		// to 700, and the collateral share scales down with it:
		// floor(600 * 700 / 750) = 560, returning the unconsumed 40 with the
		// credit. P = 0.8 * (100/800) = 0.1,
		// delta_S = 560 * (0.8/800) = 0.56, so S = 0.72.
		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, 750, 600);
		assert_eq!(debt_offset, 700);
		assert_eq!(leftover, 40);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 100);

		// A = 100 = floor: any partial offset clamps to zero (no-op)...
		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, 50, 40);
		assert_eq!(debt_offset, 0);
		assert_eq!(leftover, 40);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 100);

		// ...while full depletion passes and starts epoch 1:
		// delta_S = 80 * (0.1/100) = 0.08, so S = 0.8.
		let (debt_offset, _) = simulate_offset(DOT, PUSD, 100, 80);
		assert_eq!(debt_offset, 100);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.coords.epoch, 1);
		assert_eq!(state.coords.scale, 0);
		assert_eq!(state.coords.p, FixedU128::one());

		// Fully depleted: nothing left to withdraw (the row still exists,
		// carrying the unclaimed gains).
		assert_noop!(withdraw(1, DOT, PUSD, 1, 1), Error::<Test>::NoActiveDeposit);
		// The depositor absorbed all three offsets:
		// gain = (D0/P0) * S = (1000/1) * 0.8 = 800 = 160 + 560 + 80.
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 800);
		// The final claim emptied the row entirely.
		assert!(deposit_row(DOT, PUSD, 1).is_none());
	});
}

#[test]
fn offset_zero_request_or_empty_pool_noops() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());

		// Empty pool: nothing to offset, the credit comes back whole.
		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, 100, 50);
		assert_eq!(debt_offset, 0);
		assert_eq!(leftover, 50);

		// Zero request against a funded pool: same.
		seed_deposit(1, 1_000);
		activate_all(&[1]);
		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, 0, 50);
		assert_eq!(debt_offset, 0);
		assert_eq!(leftover, 50);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords.p, FixedU128::one());
		assert_eq!(state.total_active_deposits, 1_000);
	});
}

#[test]
fn offset_on_unregistered_branch_noops_and_returns_the_credit() {
	build_and_execute(|| {
		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, 100, 50);
		assert_eq!(debt_offset, 0);
		assert_eq!(leftover, 50);
	});
}

#[test]
fn sequential_offsets_compound_p() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_deposit(1, 1_000);
		activate_all(&[1]);

		// Two collateral-free offsets with distinct ratios:
		// P = 1 * (500/1000) = 0.5, then 0.5 * (300/500) = 0.3.
		assert_eq!(simulate_offset(DOT, PUSD, 500, 0).0, 500);
		assert_eq!(simulate_offset(DOT, PUSD, 200, 0).0, 200);
		assert_eq!(pool_state(DOT, PUSD).coords.p, FixedU128::from_rational(3, 10));

		// Compounded: floor(1000 * 0.3) = 300.
		assert_ok!(withdraw(1, DOT, PUSD, 1_000, 1));
		assert_eq!(stable_balance(PUSD, 1), 300);
	});
}

#[test]
fn offset_preserves_claimable_yield() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_deposit(1, 600);
		activate_all(&[1]);
		drop(distribute_yield(DOT, PUSD, 60));

		// The offset halves the deposit but must not touch the yield
		// already recorded in G (invariant 12): the claim still pays the
		// full floor(600 * 0.1) = 60. The 300 debt seizes 300 / 1.25 = 240
		// collateral.
		assert_eq!(simulate_offset(DOT, PUSD, 300, 240).0, 300);
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
		// While the deposit itself was halved: floor(600 * 0.5) = 300,
		// and the collateral gain is delta_S = 240 * (1/600) = 0.4 over the
		// 600 stake: floor(600 * 0.4) = 240.
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 240);
		assert_ok!(withdraw(1, DOT, PUSD, 1_000, 1));
		// 60 claimed yield + 300 compounded deposit.
		assert_eq!(stable_balance(PUSD, 1), 360);
	});
}

#[test]
fn offset_api_trait_surface_matches_the_engine() {
	build_and_execute(|| {
		use pusd_primitives::StabilityPoolOffsetApi;
		type Api = Stability;

		register_branch(DOT, PUSD, default_branch_config());
		seed_deposit(1, 1_000);
		activate_all(&[1]);

		// The trait methods are thin wrappers over the engine functions.
		let (debt_offset, remainder) =
			<Api as StabilityPoolOffsetApi<_, _, _, _>>::offset_liquidation(
				&DOT,
				&PUSD,
				500,
				issue_collateral(DOT, 400),
			);
		assert_eq!(debt_offset, 500);
		assert_eq!(remainder.peek(), 0);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 500);

		// An empty pending queue passes the debt and the credit through.
		let (result, remainder) =
			<Api as StabilityPoolOffsetApi<_, _, _, _>>::offset_pending_liquidation(
				&DOT,
				&PUSD,
				100,
				5,
				issue_collateral(DOT, 50),
			);
		assert_eq!(result.debt_offset, 0);
		assert_eq!(remainder.peek(), 50);
	});
}

#[test]
fn offset_with_sub_minimum_collateral_gain_steps_aside() {
	build_and_execute(|| {
		// A collateral whose pallet-assets minimum balance exceeds the gain:
		// resolving the first-ever gain into the empty pool account fails.
		assert_ok!(Assets::force_create(RuntimeOrigin::root(), 77, 1, true, 1_000));
		let coll = AssetId::WithId(77);
		register_branch(coll.clone(), PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, coll.clone(), PUSD, 1_000));
		advance_time(5_000);
		assert_ok!(poke(1, 1, coll.clone(), PUSD));

		// 500 collateral for 500 debt: gain 500 < the 1_000 minimum on an
		// empty account — the whole offset steps aside, nothing moves.
		let (debt_offset, leftover) = simulate_offset(coll.clone(), PUSD, 500, 500);
		assert_eq!(debt_offset, 0);
		assert_eq!(leftover, 500);
		let state = pool_state(coll.clone(), PUSD);
		assert_eq!(state.total_active_deposits, 1_000);
		assert_eq!(state.coords.p, FixedU128::one());
		let pool = Stability::pool_account(&coll, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 1_000);

		// A gain clearing the minimum lands normally.
		let (debt_offset, leftover) = simulate_offset(coll.clone(), PUSD, 500, 1_500);
		assert_eq!(debt_offset, 500);
		assert_eq!(leftover, 0);
		assert_eq!(collateral_balance(coll, pool), 1_500);
	});
}

#[test]
fn compounded_yield_absorbs_offsets() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		set_min_active_pool(20);
		seed_deposit(1, 600);
		activate_all(&[1]);
		drop(distribute_yield(DOT, PUSD, 60));
		assert_ok!(compound(1, DOT, PUSD, 60));

		// The compounded 60 is offsettable: A = 660, and the 627 offset
		// exceeds the original 600 deposit — only possible because the
		// compounded yield absorbs too. P = 33/660 = 0.05,
		// compounded = floor(660 * 0.05) = 33.
		assert_eq!(simulate_offset(DOT, PUSD, 627, 0).0, 627);
		assert_ok!(withdraw(1, DOT, PUSD, 1_000, 1));
		assert_eq!(stable_balance(PUSD, 1), 33);
	});
}

#[test]
fn offset_rounds_down_at_the_pool_minimum_balance_dead_zone() {
	build_and_execute(|| {
		register_branch(DOT, USDX, default_branch_config());
		// One active depositor plus 6_000 raw units of pending deposit, so
		// the pool balance exceeds the active total by less than the
		// 10_000-unit USDX minimum.
		mint_stable(USDX, 1, 100_000);
		assert_ok!(deposit(1, DOT, USDX, 100_000));
		advance_time(5_000);
		assert_ok!(poke(1, 1, DOT, USDX));
		mint_stable(USDX, 2, 16_000);
		assert_ok!(deposit(2, DOT, USDX, 6_000));

		let pool = Stability::pool_account(&DOT, &USDX);
		assert_eq!(stable_balance(USDX, pool), 106_000);

		// The accounting cap allows the full 100_000 active total, but
		// burning it would strand 6_000 < 10_000 on the pool account: the
		// plan rounds the offset down to the preserving limit 96_000, and
		// the collateral share scales with it: floor(80_000 * 96_000 /
		// 100_000) = 76_800.
		let (debt_offset, leftover) = simulate_offset(DOT, USDX, 100_000, 80_000);
		assert_eq!(debt_offset, 96_000);
		assert_eq!(leftover, 80_000 - 76_800);

		// The pool sits exactly at the minimum: alive, no dust burned, and
		// the balance still backs active 4_000 + pending 6_000.
		assert_eq!(stable_balance(USDX, pool), USDX_MIN_BALANCE);
		let state = pool_state(DOT, USDX);
		assert_eq!(state.total_active_deposits, 4_000);
		assert_eq!(state.total_pending_deposits, 6_000);
	});
}
