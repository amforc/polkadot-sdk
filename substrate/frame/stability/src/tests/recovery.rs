//! Recovery offsets against a real vault in `FinalRecovery`, priced by the real redemptions
//! pallet, so an offset and a recovery redemption cannot be priced differently.
//!
//! The standing figures: the vault holds 1000 collateral against 500 debt, which is 499 borrowed
//! plus a 1 upfront fee. At the parked price of 0.52 its CR is 104%, so the recovery bonus is 3%
//! and cancelling debt `D` pays `floor(floor(D * 1.03) / 0.52)` collateral.
//!
//! The active pool is the 400 that account 1 deposits and matures while the branch is still
//! Normal, which also fixes the standing timestamp and accrued-interest figures.

use crate::{mock::*, types::RecoveryOffsetSource, Error};
use frame::prelude::ArithmeticError;

/// Open the standing vault (owner 5) and pin its exact debt.
fn open_standing_vault() {
	mint_collateral(DOT, 5, 2_000);
	assert_ok!(open_vault(5, DOT, STABLE, 1_000, 498));
	// 498 borrowed + 1 upfront fee, plus the unit of interest the next touch rounds up while the
	// pool matures: every literal below derives from 500.
	assert_eq!(vault_debt(DOT, STABLE, 5), 499);
}

/// The standing vault parked at 104% in `FinalRecovery`, with account 1's 400 active behind it.
fn standing_head_with_active_pool() {
	register_branch(DOT, STABLE, recovery_branch_config());
	open_standing_vault();
	seed_matured_deposit_from_balance(1, 1_000, 400);
	park_at_104_percent();
}

/// Asserts that this pallet reported a recovery offset paid by `source`.
fn assert_recovery_source(source: RecoveryOffsetSource) {
	System::assert_has_event(
		crate::Event::RecoveryOffsetApplied { collateral_id: DOT, stable_id: STABLE, source }
			.into(),
	);
}

/// Asserts that the standing vault settled `debt` for `collateral`, paid by `source`.
fn assert_recovery_offset(debt: Balance, collateral: Balance, source: RecoveryOffsetSource) {
	assert_vault_redeemed(5, debt, collateral);
	assert_recovery_source(source);
}

/// Drop to 0.52 (CR = 104%) and park the vault in `FinalRecovery`.
fn park_at_104_percent() {
	set_price(DOT, FixedU128::from_rational(52u128, 100u128));
	assert_ok!(enter_final_recovery(DOT, STABLE, 5));
}

#[test]
fn active_pool_recovery_offset_settles_the_head() {
	build_and_execute(|| {
		standing_head_with_active_pool();

		// 350 would leave 50 < the 100 floor: clamped to 300, and the settlement burns exactly
		// the clamped amount.
		assert_ok!(offset_recovery(DOT, STABLE, 350));

		// collateral_out = floor(floor(300 * 1.03) / 0.52) = floor(309/0.52)
		//                = 594.
		assert_recovery_offset(300, 594, RecoveryOffsetSource::ActivePool);

		// The vault settled 300 of its 500 debt and paid real collateral.
		assert_eq!(vault_debt(DOT, STABLE, 5), 200);
		assert_eq!(pool_stable_held(DOT, STABLE), 100);
		assert_eq!(pool_collateral_held(DOT, STABLE), 594);

		// The standard accumulator update: P = 100/400 = 0.25 and
		// delta_S = floor(594 * 1e18 / 400) = 1.485e18.
		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 100);
		assert_eq!(state.coords.p, FixedU128::from_rational(1, 4));
		assert_eq!(state.total_collateral_gains_unclaimed, 594);
		let sums = active_sums(0, 0);
		assert_eq!(sums.s_collateral, FixedU128::from_rational(1_485, 1_000));

		// The depositor realizes exactly the settled collateral:
		// gain = floor(400 * 1.485) = 594, compounded = floor(400 * 0.25).
		assert_claim_collateral(1, 594);
		let row = deposit_row(DOT, STABLE, 1).expect("row survives");
		assert_eq!(row.active_deposit, 100);
	});
}

#[test]
fn recovery_offset_can_fully_deplete_the_pool() {
	build_and_execute(|| {
		standing_head_with_active_pool();

		// capacity = min(500, 1000) = 500, but the pool holds 400: full
		// depletion passes the post-offset floor clamp and bumps the epoch.
		assert_ok!(offset_recovery(DOT, STABLE, 1_000));

		// collateral_out = floor(floor(400 * 1.03) / 0.52) = floor(412/0.52)
		//                = 792.
		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.coords, Accumulators { p: FixedU128::one(), epoch: 1, scale: 0 });
		assert_eq!(vault_debt(DOT, STABLE, 5), 100);

		// The old-epoch depositor realizes to zero active with the full
		// gain: floor(400 * floor(792e18/400) / 1e18) = 792.
		assert_claim_collateral(1, 792);
		assert!(deposit_row(DOT, STABLE, 1).is_none());
	});
}

#[test]
fn recovery_offset_error_paths() {
	build_and_execute(|| {
		register_branch(DOT, STABLE, recovery_branch_config());

		// No active capital to burn. The pool is sized before the head is looked up, so the empty
		// pool is what rejects the call: once funded, the same call fails on the missing head.
		assert_noop!(offset_recovery(DOT, STABLE, 300), Error::<Test>::NoRecoveryOffsetPerformed);
		seed_matured_deposit_from_balance(1, 1_000, 400);

		// No FinalRecovery vault queued.
		assert_noop!(offset_recovery(DOT, STABLE, 300), Error::<Test>::RecoveryVaultNotFound);

		open_standing_vault();
		park_at_104_percent();

		// A zero budget resolves to a zero burn.
		assert_noop!(offset_recovery(DOT, STABLE, 0), Error::<Test>::NoRecoveryOffsetPerformed);

		// Frozen halts recovery offsets like every other operation.
		MockOracleAvailable::set(false);
		assert_noop!(offset_recovery(DOT, STABLE, 300), Error::<Test>::BranchFrozen);
		MockOracleAvailable::set(true);
	});
}

#[test]
fn active_recovery_rolls_back_when_pool_accounting_fails_after_settlement() {
	build_and_execute(|| {
		standing_head_with_active_pool();

		hypothetically!({
			mutate_pool_state(DOT, STABLE, |state| {
				state.total_collateral_gains_unclaimed = Balance::MAX;
			});
			assert_noop!(offset_recovery(DOT, STABLE, 300), ArithmeticError::Overflow);

			// The whole cross-pallet settlement rolled back with the failed plan.
			assert_eq!(vault_debt(DOT, STABLE, 5), 500);
			assert_eq!(pool_stable_held(DOT, STABLE), 400);
			assert_eq!(pool_collateral_held(DOT, STABLE), 0);
		});
	});
}

#[test]
fn incoming_recovery_rolls_back_when_deposit_accounting_fails_after_settlement() {
	build_and_execute(|| {
		register_branch(DOT, STABLE, recovery_branch_config());
		open_standing_vault();
		seed_deposit_from_balance(2, 300, 100);
		park_at_104_percent();

		hypothetically!({
			crate::Deposits::<Test>::mutate((DOT, STABLE, 2), |row| {
				row.as_mut().expect("deposit row exists").claimable_collateral = Balance::MAX;
			});
			let collateral_before = collateral_balance(DOT, 2);
			assert_noop!(deposit(2, DOT, STABLE, 200), ArithmeticError::Overflow);

			// The depositor burn and vault settlement rolled back with the failed
			// claimable-collateral update.
			assert_eq!(vault_debt(DOT, STABLE, 5), 499);
			assert_eq!(stable_balance(STABLE, 2), 200);
			assert_eq!(collateral_balance(DOT, 2), collateral_before);
			assert_eq!(deposit_row(DOT, STABLE, 2).unwrap().pending_deposit.unwrap().amount, 100);
		});
	});
}

#[test]
fn par_band_head_settles_at_face_value() {
	build_and_execute(|| {
		register_branch(DOT, STABLE, recovery_branch_config());
		open_standing_vault();
		seed_matured_deposit_from_balance(1, 1_000, 400);

		// CR = 1000 * 0.5025 / 500 = 100.5%: at or above par, but inside the
		// 1% bonus buffer, where the raw excess CR - 100% - buffer would be
		// negative (-0.5%). The max(0, ·) clamp in `recovery_bonus` settles
		// at exactly face value instead of ever discounting.
		set_price(DOT, FixedU128::from_rational(5_025u128, 10_000u128));
		assert_ok!(enter_final_recovery(DOT, STABLE, 5));

		// collateral_out = floor(200 / 0.5025) = 398 — no bonus, no haircut.
		assert_ok!(offset_recovery(DOT, STABLE, 200));
		assert_recovery_offset(200, 398, RecoveryOffsetSource::ActivePool);
		assert_eq!(vault_debt(DOT, STABLE, 5), 300);

		// Full settlement cancels the remaining 300: floor(300 / 0.5025) = 597.
		seed_deposit(2, 300);
		assert_recovery_offset(300, 597, RecoveryOffsetSource::IncomingDeposit);
		assert_eq!(vault_debt(DOT, STABLE, 5), 0);
		let row = deposit_row(DOT, STABLE, 2).expect("row created");
		assert_eq!(row.claimable_collateral, 597);
		assert!(row.pending_deposit.is_none());
	});
}

#[test]
fn below_par_head_rejects_offsets_and_deposits() {
	build_and_execute(|| {
		register_branch(DOT, STABLE, recovery_branch_config());
		open_standing_vault();
		seed_matured_deposit_from_balance(1, 1_000, 400);
		// CR = 400/500 = 80%: below par. The (empty) Insurance Fund plays
		// no role — any split prices as BelowPar for the pool.
		set_price(DOT, FixedU128::from_rational(4u128, 10u128));
		assert_ok!(enter_final_recovery(DOT, STABLE, 5));

		assert_noop!(offset_recovery(DOT, STABLE, 300), Error::<Test>::RecoveryOffsetBelowPar);

		// Incoming deposits are rejected wholesale rather than settled at
		// a discount.
		mint_stable(STABLE, 2, 300);
		assert_noop!(deposit(2, DOT, STABLE, 300), Error::<Test>::RecoveryOffsetBelowPar);
		assert!(deposit_row(DOT, STABLE, 2).is_none());
		assert_eq!(stable_balance(STABLE, 2), 300);
	});
}

#[test]
fn incoming_deposit_recovers_first_and_queues_the_rest() {
	build_and_execute(|| {
		standing_head_with_active_pool();

		let state_before = pool_state(DOT, STABLE);
		let sums_before = active_sums(0, 0);

		// Full settlement burns 500 of the depositor payment:
		// collateral_out = floor(floor(500 * 1.03) / 0.52) = floor(515/0.52) = 990.
		seed_deposit(2, 800);

		assert_recovery_offset(500, 990, RecoveryOffsetSource::IncomingDeposit);
		System::assert_has_event(market_event!(DepositReceived {
			depositor: 2,
			amount: 800,
			pending_amount: 300,
		}));

		// The head is fully drained (Dormant, out of the FIFO).
		assert_eq!(vault_debt(DOT, STABLE, 5), 0);

		let row = deposit_row(DOT, STABLE, 2).expect("row created");
		assert_eq!(row.claimable_collateral, 990);
		assert_eq!(row.pending_deposit.expect("leftover queued").amount, 300);
		assert_eq!(stable_balance(STABLE, 2), 0);

		// The part spent on the recovery never entered the pool balance and never touched
		// the accumulators.
		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 400);
		assert_eq!(state.total_pending_deposits, 300);
		assert_eq!(state.total_collateral_gains_unclaimed, 990);
		assert_eq!(state.coords, state_before.coords);
		assert_eq!(active_sums(0, 0), sums_before);
		assert_eq!(pool_stable_held(DOT, STABLE), 700);
		assert_eq!(pool_collateral_held(DOT, STABLE), 990);

		// With the head gone, a follow-up deposit queues normally.
		seed_deposit(2, 100);
		let row = deposit_row(DOT, STABLE, 2).expect("row kept");
		assert_eq!(row.pending_deposit.expect("merged").amount, 400);

		// The direct credit is claimable through the normal path.
		assert_claim_collateral(2, 990);
	});
}

#[test]
fn incoming_deposit_fully_used_leaves_no_pending() {
	build_and_execute(|| {
		standing_head_with_active_pool();

		// used = min(200, capacity 500) = 200 — nothing left to queue;
		// collateral_out = floor(floor(200 * 1.03) / 0.52) = floor(206/0.52)
		//                = 396.
		seed_deposit(2, 200);

		System::assert_has_event(market_event!(DepositReceived {
			depositor: 2,
			amount: 200,
			pending_amount: 0,
		}));
		let row = deposit_row(DOT, STABLE, 2).expect("row created");
		assert_eq!(row.claimable_collateral, 396);
		assert!(row.pending_deposit.is_none());
		assert_eq!(stable_balance(STABLE, 2), 0);

		// The head keeps its remaining 300 debt and stays in the FIFO.
		assert_eq!(vault_debt(DOT, STABLE, 5), 300);
		assert_eq!(pool_state(DOT, STABLE).total_pending_deposits, 0);
	});
}

/// Routes every minted unit to the pool, so the unit of interest the head's touch rounds up inside
/// the recovery settlement reaches the pool as yield instead of the fee account.
fn route_all_yield_to_pool() {
	let config =
		StabilityPoolConfig { yield_share: Permill::from_percent(100), ..default_pool_config() };
	assert_ok!(Stability::set_stability_pool_config(RuntimeOrigin::root(), DOT, STABLE, config));
}

/// Parks the head and lets interest run past the unit its last touch charged ahead, so the touch
/// inside the settlement rounds up a unit the aggregate accrual did not cover.
fn park_with_unminted_vault_interest() {
	park_at_104_percent();
	advance_time(30 * 24 * 3_600 * 1_000);
	System::reset_events();
}

/// The settlement re-enters this pallet through the vault's yield hook. The yield it distributes
/// must survive the commit of the pool row the operation held in memory while it ran: the
/// post-test `try_state` then proves the pool balance still matches its tracked totals.
fn assert_settlement_yield_survives(yield_before: Balance) {
	let distributed: Balance = System::events()
		.into_iter()
		.filter_map(|record| match record.event {
			RuntimeEvent::Stability(crate::Event::YieldDistributed { amount, .. }) => Some(amount),
			_ => None,
		})
		.sum();
	assert!(distributed > 0, "the operation must route yield to the pool");
	assert_eq!(pool_state(DOT, STABLE).total_yield_unclaimed, yield_before + distributed);
}

#[test]
fn incoming_deposit_keeps_yield_distributed_during_settlement() {
	build_and_execute(|| {
		register_branch(DOT, STABLE, recovery_branch_config());
		route_all_yield_to_pool();
		open_standing_vault();
		seed_matured_deposit_from_balance(1, 1_000, 400);
		park_with_unminted_vault_interest();
		let yield_before = pool_state(DOT, STABLE).total_yield_unclaimed;

		seed_deposit(2, 800);

		assert_recovery_source(RecoveryOffsetSource::IncomingDeposit);
		assert_settlement_yield_survives(yield_before);
	});
}

#[test]
fn active_pool_offset_keeps_yield_distributed_during_settlement() {
	build_and_execute(|| {
		register_branch(DOT, STABLE, recovery_branch_config());
		route_all_yield_to_pool();
		open_standing_vault();
		seed_matured_deposit_from_balance(1, 1_000, 400);
		park_with_unminted_vault_interest();
		let yield_before = pool_state(DOT, STABLE).total_yield_unclaimed;

		assert_ok!(offset_recovery(DOT, STABLE, 300));

		assert_recovery_source(RecoveryOffsetSource::ActivePool);
		assert_settlement_yield_survives(yield_before);
	});
}

// The deposit is charged for a settlement that closes the head, so only a deposit that found no
// head may report the smaller weight.
#[test]
fn deposit_refunds_to_the_no_recovery_weight_only_without_a_head() {
	build_and_execute(|| {
		standing_head_with_active_pool();
		mint_stable(PUSD, 2, 900);

		let settled = Stability::deposit(RuntimeOrigin::signed(2), DOT, PUSD, 800)
			.expect("the deposit settles the head");
		assert_vault_redeemed(5, 500, 990);
		assert_eq!(settled.actual_weight, None);

		let queued = Stability::deposit(RuntimeOrigin::signed(2), DOT, PUSD, 100)
			.expect("the deposit queues");
		assert_eq!(queued.actual_weight, Some(<() as crate::WeightInfo>::deposit_no_recovery()));
	});
}

// A head that cannot be offset is still read and priced, so the deposit that meets it keeps the
// full charge rather than the weight of an empty queue.
#[test]
fn deposit_keeps_the_full_weight_for_a_head_it_cannot_offset() {
	build_and_execute(|| {
		standing_head_with_active_pool();
		mint_stable(PUSD, 2, 900);
		// The registration price lifts the head above the exit threshold.
		set_price(DOT, REGISTRATION_PRICE);

		let queued = Stability::deposit(RuntimeOrigin::signed(2), DOT, PUSD, 100)
			.expect("the deposit queues past the head");
		assert_eq!(queued.actual_weight, None);
		assert!(!System::events().into_iter().any(|record| matches!(
			record.event,
			RuntimeEvent::Stability(crate::Event::RecoveryOffsetApplied { .. })
		)));
	});
}
