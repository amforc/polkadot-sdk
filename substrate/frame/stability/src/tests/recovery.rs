//! Recovery offsets against a real `FinalRecovery`
//! vault, priced by the real redemptions pallet — offset pricing and
//! recovery-redemption pricing share one code path by construction.
//!
//! Standing figures: the vault holds 1000 collateral against 500 debt (499
//! borrowed + 1 upfront fee, pinned below). At the parked price 0.52 its
//! CR is 520/500 = 104%, so the recovery bonus is
//! min(1.04 - 1 - buffer(0.01), penalty(5%)) = 3%, and cancelling debt D
//! pays floor(floor(D * 1.03) / 0.52) collateral.

use crate::{mock::*, types::Leg, Error};
use frame::prelude::ArithmeticError;

/// Open the standing vault (owner 5) and pin its exact debt.
fn open_standing_vault() {
	mint_collateral(DOT, 5, 2_000);
	assert_ok!(open_vault(5, DOT, PUSD, 1_000, 499));
	// 499 borrowed + 1 upfront fee: every literal below derives from 500.
	assert_eq!(vault_debt(DOT, PUSD, 5), 500);
}

/// Deposit-and-activate 400 for user 1 while the branch is still Normal.
fn seed_active_pool() {
	mint_stable(PUSD, 1, 1_000);
	assert_ok!(deposit(1, DOT, PUSD, 400));
	advance_time(5_000);
	assert_ok!(poke(1, 1, DOT, PUSD));
}

/// Drop to 0.52 (CR = 104%) and park the vault in `FinalRecovery`.
fn park_at_104_percent() {
	set_price(DOT, FixedU128::from_rational(52u128, 100u128));
	assert_ok!(enter_final_recovery(DOT, PUSD, 5));
}

#[test]
fn active_pool_recovery_offset_settles_the_head() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		open_standing_vault();
		seed_active_pool();
		park_at_104_percent();

		assert_ok!(offset_recovery(DOT, PUSD, 300));

		// collateral_out = floor(floor(300 * 1.03) / 0.52) = floor(309/0.52)
		//                = 594.
		System::assert_has_event(
			crate::Event::RecoveryOffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				debt_burned: 300,
				collateral_gain: 594,
				source: crate::types::RecoveryOffsetSource::ActivePool,
			}
			.into(),
		);

		// The vault settled 300 of its 500 debt and paid real collateral.
		assert_eq!(vault_debt(DOT, PUSD, 5), 200);
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 100);
		assert_eq!(collateral_balance(DOT, pool), 594);

		// The standard accumulator update: P = 100/400 = 0.25 and
		// delta_S = floor(594 * 1e18 / 400) = 1.485e18.
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 100);
		assert_eq!(state.coords.p, FixedU128::from_rational(1, 4));
		assert_eq!(state.total_collateral_gains_unclaimed, 594);
		let sums = crate::PoolSumsStore::<Test>::get((DOT, PUSD, Leg::Active, 0u32, 0u32));
		assert_eq!(sums.s_collateral, FixedU128::from_inner(1_485_000_000_000_000_000));

		// The depositor realizes exactly the settled collateral:
		// gain = floor(400 * 1.485) = 594, compounded = floor(400 * 0.25).
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 594);
		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 100);
	});
}

#[test]
fn recovery_offset_can_fully_deplete_the_pool() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		open_standing_vault();
		seed_active_pool();
		park_at_104_percent();

		// capacity = min(500, 1000) = 500, but the pool holds 400: full
		// depletion passes the post-offset floor clamp and bumps the epoch.
		assert_ok!(offset_recovery(DOT, PUSD, 1_000));

		// collateral_out = floor(floor(400 * 1.03) / 0.52) = floor(412/0.52)
		//                = 792.
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.coords.epoch, 1);
		assert_eq!(state.coords.p, FixedU128::one());
		assert_eq!(vault_debt(DOT, PUSD, 5), 100);

		// The old-epoch depositor realizes to zero active with the full
		// gain: floor(400 * floor(792e18/400) / 1e18) = 792.
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 792);
		assert!(deposit_row(DOT, PUSD, 1).is_none());
	});
}

#[test]
fn recovery_offset_clamps_at_the_pool_floor() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		open_standing_vault();
		seed_active_pool();
		park_at_104_percent();

		// 350 would leave 50 < the 100 floor: clamped to 300, and the
		// settlement burns exactly the clamped amount.
		assert_ok!(offset_recovery(DOT, PUSD, 350));
		System::assert_has_event(
			crate::Event::RecoveryOffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				debt_burned: 300,
				collateral_gain: 594,
				source: crate::types::RecoveryOffsetSource::ActivePool,
			}
			.into(),
		);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 100);
	});
}

#[test]
fn recovery_offset_error_paths() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active_pool();

		// No FinalRecovery vault queued.
		assert_noop!(offset_recovery(DOT, PUSD, 300), Error::<Test>::RecoveryVaultNotFound);

		open_standing_vault();
		park_at_104_percent();

		// A zero budget resolves to a zero burn.
		assert_noop!(offset_recovery(DOT, PUSD, 0), Error::<Test>::NoRecoveryOffsetPerformed);

		// Frozen halts recovery offsets like every other operation.
		MockOracleAvailable::set(false);
		assert_noop!(offset_recovery(DOT, PUSD, 300), Error::<Test>::BranchFrozen);
		MockOracleAvailable::set(true);
	});
}

#[test]
fn active_recovery_rolls_back_when_pool_accounting_fails_after_settlement() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		open_standing_vault();
		seed_active_pool();
		park_at_104_percent();

		crate::Pools::<Test>::mutate(DOT, PUSD, |pool| {
			pool.as_mut().unwrap().state.total_collateral_gains_unclaimed = Balance::MAX;
		});
		assert_noop!(offset_recovery(DOT, PUSD, 300), ArithmeticError::Overflow);

		// The whole cross-pallet settlement rolled back with the failed plan.
		assert_eq!(vault_debt(DOT, PUSD, 5), 500);
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 400);
		assert_eq!(collateral_balance(DOT, pool), 0);

		crate::Pools::<Test>::mutate(DOT, PUSD, |pool| {
			pool.as_mut().unwrap().state.total_collateral_gains_unclaimed = 0;
		});
	});
}

#[test]
fn incoming_recovery_rolls_back_when_deposit_accounting_fails_after_settlement() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		open_standing_vault();
		mint_stable(PUSD, 2, 300);
		assert_ok!(deposit(2, DOT, PUSD, 100));
		park_at_104_percent();

		crate::Deposits::<Test>::mutate((DOT, PUSD, 2), |row| {
			row.as_mut().unwrap().claimable_collateral = Balance::MAX;
		});
		let collateral_before = collateral_balance(DOT, 2);
		assert_noop!(deposit(2, DOT, PUSD, 200), ArithmeticError::Overflow);

		// The depositor burn and vault settlement rolled back with the failed
		// claimable-collateral update.
		assert_eq!(vault_debt(DOT, PUSD, 5), 500);
		assert_eq!(stable_balance(PUSD, 2), 200);
		assert_eq!(collateral_balance(DOT, 2), collateral_before);
		assert_eq!(deposit_row(DOT, PUSD, 2).unwrap().pending_deposit.unwrap().amount, 100);

		crate::Deposits::<Test>::mutate((DOT, PUSD, 2), |row| {
			row.as_mut().unwrap().claimable_collateral = 0;
		});
	});
}

#[test]
fn recovery_offset_with_empty_pool_is_rejected() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		open_standing_vault();
		park_at_104_percent();

		// A valid head, but no active capital to burn.
		assert_noop!(offset_recovery(DOT, PUSD, 300), Error::<Test>::NoRecoveryOffsetPerformed);
	});
}

#[test]
fn par_band_head_settles_at_face_value() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		open_standing_vault();
		seed_active_pool();

		// CR = 1000 * 0.5025 / 500 = 100.5%: at or above par, but inside the
		// 1% bonus buffer, where the raw excess CR - 100% - buffer would be
		// negative (-0.5%). The max(0, ·) clamp in `recovery_bonus` settles
		// at exactly face value instead of ever discounting.
		set_price(DOT, FixedU128::from_rational(5_025u128, 10_000u128));
		assert_ok!(enter_final_recovery(DOT, PUSD, 5));

		// collateral_out = floor(200 / 0.5025) = 398 — no bonus, no haircut.
		assert_ok!(offset_recovery(DOT, PUSD, 200));
		System::assert_has_event(
			crate::Event::RecoveryOffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				debt_burned: 200,
				collateral_gain: 398,
				source: crate::types::RecoveryOffsetSource::ActivePool,
			}
			.into(),
		);
		assert_eq!(vault_debt(DOT, PUSD, 5), 300);

		// Full settlement includes the terminal charge. Partial settlement does not.
		mint_stable(PUSD, 2, 301);
		assert_ok!(deposit(2, DOT, PUSD, 301));
		System::assert_has_event(
			crate::Event::RecoveryOffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				debt_burned: 301,
				collateral_gain: 599,
				source: crate::types::RecoveryOffsetSource::IncomingDeposit,
			}
			.into(),
		);
		assert_eq!(vault_debt(DOT, PUSD, 5), 0);
		let row = deposit_row(DOT, PUSD, 2).expect("row created");
		assert_eq!(row.claimable_collateral, 599);
		assert!(row.pending_deposit.is_none());
	});
}

#[test]
fn below_par_head_rejects_offsets_and_deposits() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		open_standing_vault();
		seed_active_pool();
		// CR = 400/500 = 80%: below par. The (empty) Insurance Fund plays
		// no role — any split prices as BelowPar for the pool.
		set_price(DOT, FixedU128::from_rational(4u128, 10u128));
		assert_ok!(enter_final_recovery(DOT, PUSD, 5));

		assert_noop!(offset_recovery(DOT, PUSD, 300), Error::<Test>::RecoveryOffsetBelowPar);

		// Incoming deposits are rejected wholesale rather than settled at
		// a discount.
		mint_stable(PUSD, 2, 300);
		assert_noop!(deposit(2, DOT, PUSD, 300), Error::<Test>::RecoveryOffsetBelowPar);
		assert!(deposit_row(DOT, PUSD, 2).is_none());
		assert_eq!(stable_balance(PUSD, 2), 300);
	});
}

#[test]
fn incoming_deposit_recovers_first_and_queues_the_rest() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		open_standing_vault();
		seed_active_pool();
		park_at_104_percent();

		let state_before = pool_state(DOT, PUSD);
		let sums_before = crate::PoolSumsStore::<Test>::get((DOT, PUSD, Leg::Active, 0u32, 0u32));

		// Full settlement burns the depositor payment and collects the terminal charge.
		mint_stable(PUSD, 2, 800);
		assert_ok!(deposit(2, DOT, PUSD, 800));

		System::assert_has_event(
			crate::Event::RecoveryOffsetApplied {
				collateral_id: DOT,
				stable_id: PUSD,
				debt_burned: 501,
				collateral_gain: 992,
				source: crate::types::RecoveryOffsetSource::IncomingDeposit,
			}
			.into(),
		);
		System::assert_has_event(
			crate::Event::DepositReceived {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 2,
				amount: 800,
				used_for_recovery: 501,
				pending_amount: 299,
			}
			.into(),
		);

		// The head is fully drained (Dormant, out of the FIFO).
		assert_eq!(vault_debt(DOT, PUSD, 5), 0);

		let row = deposit_row(DOT, PUSD, 2).expect("row created");
		assert_eq!(row.claimable_collateral, 992);
		assert_eq!(row.pending_deposit.expect("leftover queued").amount, 299);
		assert_eq!(stable_balance(PUSD, 2), 0);

		// The used portion never entered the pool's stablecoin balance and
		// never touched the accumulators (invariant 7).
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 400);
		assert_eq!(state.total_pending_deposits, 299);
		assert_eq!(state.total_collateral_gains_unclaimed, 992);
		assert_eq!(state.coords.p, state_before.coords.p);
		assert_eq!(state.coords.epoch, state_before.coords.epoch);
		assert_eq!(state.coords.scale, state_before.coords.scale);
		assert_eq!(
			crate::PoolSumsStore::<Test>::get((DOT, PUSD, Leg::Active, 0u32, 0u32)),
			sums_before
		);
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 699);
		assert_eq!(collateral_balance(DOT, pool), 992);

		// With the head gone, a follow-up deposit queues normally.
		mint_stable(PUSD, 2, 100);
		assert_ok!(deposit(2, DOT, PUSD, 100));
		let row = deposit_row(DOT, PUSD, 2).expect("row kept");
		assert_eq!(row.pending_deposit.expect("merged").amount, 399);

		// The direct credit is claimable through the normal path.
		let before = collateral_balance(DOT, 2);
		assert_ok!(claim_collateral(2, DOT, PUSD, 2));
		assert_eq!(collateral_balance(DOT, 2) - before, 992);
	});
}

#[test]
fn incoming_deposit_fully_used_leaves_no_pending() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		open_standing_vault();
		seed_active_pool();
		park_at_104_percent();

		// used = min(200, capacity 500) = 200 — nothing left to queue;
		// collateral_out = floor(floor(200 * 1.03) / 0.52) = floor(206/0.52)
		//                = 396.
		mint_stable(PUSD, 2, 200);
		assert_ok!(deposit(2, DOT, PUSD, 200));

		System::assert_has_event(
			crate::Event::DepositReceived {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 2,
				amount: 200,
				used_for_recovery: 200,
				pending_amount: 0,
			}
			.into(),
		);
		let row = deposit_row(DOT, PUSD, 2).expect("row created");
		assert_eq!(row.claimable_collateral, 396);
		assert!(row.pending_deposit.is_none());
		assert_eq!(stable_balance(PUSD, 2), 0);

		// The head keeps its remaining 300 debt and stays in the FIFO.
		assert_eq!(vault_debt(DOT, PUSD, 5), 300);
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 0);
	});
}
