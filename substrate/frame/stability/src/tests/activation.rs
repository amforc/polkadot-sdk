//! Entry-delay maturity: a matured pending deposit folds into the active pool
//! on the next touch of its row or through bounded idle maintenance, with no
//! owner-gated step. The permissionless poke is the deterministic path when
//! blocks expose no idle weight. The delay exists to block MEV-style
//! liquidation-only participation, not to give the owner a second decision
//! point.

use crate::mock::*;
use frame::prelude::Weight;

fn run_idle(remaining: Weight) -> Weight {
	Stability::on_idle_activation_walk_with_weight(remaining, Weight::from_parts(1, 0))
}

#[test]
fn offset_with_only_immature_pending_fails() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, DOT, PUSD, 400));

		// Deposited at t = 1_000, activatable at 6_000; t = 5_999 is one
		// millisecond short. Even a poke cannot fold it in.
		advance_time(4_999);
		assert_ok!(poke(7, 1, DOT, PUSD));
		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.pending_deposit.expect("still pending").amount, 400);

		// With no active capital, an ordinary offset finds nothing to burn
		// and returns the credit whole.
		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, 100, 80);
		assert_eq!(debt_offset, 0);
		assert_eq!(leftover, 80);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 0);
	});
}

#[test]
fn poke_at_exact_boundary_activates_and_offset_succeeds() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, DOT, PUSD, 400));

		// Exactly at t = 6_000 the deposit is mature; any touch folds it in —
		// here a third-party poke, no owner action involved.
		advance_time(5_000);
		assert_ok!(poke(7, 1, DOT, PUSD));

		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.active_deposit, 400);
		assert!(row.pending_deposit.is_none());
		// Activation joins at the current accumulators.
		assert_eq!(row.snapshot.coords.p, FixedU128::one());
		assert_eq!(row.snapshot.coords.epoch, 0);
		assert_eq!(row.snapshot.coords.scale, 0);

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 400);
		assert_eq!(state.total_pending_deposits, 0);
		assert_eq!(pending_count(DOT, PUSD), 0);
		assert!(!pending_contains(DOT, PUSD, 1));

		// Activation moves no funds; the pool already held the stablecoin.
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 400);
		assert_eq!(stable_balance(PUSD, 1), 600);

		System::assert_has_event(
			crate::Event::PendingDepositActivated {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				amount: 400,
			}
			.into(),
		);

		// The now-active capital absorbs an ordinary offset.
		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, 100, 80);
		assert_eq!(debt_offset, 100);
		assert_eq!(leftover, 0);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 300);
	});
}

#[test]
fn on_idle_activates_matured_rows_with_weight_metered_cursor() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// One more row than two two-row idle budgets.
		for owner in 1..=5 {
			seed_deposit(owner, 100);
		}
		advance_time(5_000);

		let used = run_idle(Weight::from_parts(2, 0));
		assert_eq!(used, Weight::from_parts(2, 0));
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 200);
		assert!(crate::ActivationCursor::<Test>::get().is_some());

		let used = run_idle(Weight::from_parts(2, 0));
		assert_eq!(used, Weight::from_parts(2, 0));
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 400);
		assert!(crate::ActivationCursor::<Test>::get().is_some());

		let used = run_idle(Weight::from_parts(1, 0));
		assert_eq!(used, Weight::from_parts(1, 0));
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 500);
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 0);
		assert!(crate::ActivationCursor::<Test>::get().is_some());
		for owner in 1..=5 {
			let row = deposit_row(DOT, PUSD, owner).expect("row exists");
			assert_eq!(row.active_deposit, 100);
			assert!(row.pending_deposit.is_none());
		}
		// A final metered iterator probe observes the drain and clears the cursor
		// without revisiting a row.
		assert_eq!(run_idle(Weight::from_parts(1, 0)), Weight::from_parts(1, 0));
		assert!(crate::ActivationCursor::<Test>::get().is_none());
	});
}

#[test]
fn on_idle_with_no_remaining_weight_is_a_noop() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_deposit(1, 100);
		advance_time(5_000);

		assert_eq!(run_idle(Weight::zero()), Weight::zero());
		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.pending_deposit.expect("still pending").amount, 100);
		assert!(crate::ActivationCursor::<Test>::get().is_none());
	});
}

#[test]
fn on_idle_skips_frozen_rows_and_activates_them_after_recovery() {
	build_and_execute(|| {
		seed_branch_with_debt();
		assert_ok!(deposit(1, DOT, PUSD, 300));
		advance_time(5_000);
		MockOracleAvailable::set(false);

		assert_eq!(run_idle(Weight::MAX), Weight::from_parts(2, 0));
		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.active_deposit, 400);
		assert_eq!(row.pending_deposit.expect("still pending").amount, 300);

		MockOracleAvailable::set(true);
		assert_eq!(run_idle(Weight::MAX), Weight::from_parts(2, 0));
		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.active_deposit, 700);
		assert!(row.pending_deposit.is_none());
	});
}

#[test]
fn on_idle_rolls_back_a_broken_fifo_row_and_retries_after_repair() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_deposit(1, 100);
		advance_time(5_000);
		assert_ok!(pending_remove(DOT, PUSD, 1));

		assert_eq!(run_idle(Weight::MAX), Weight::from_parts(2, 0));
		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.pending_deposit.expect("still pending").amount, 100);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 0);
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 100);

		assert_ok!(pending_append(DOT, PUSD, 1));
		assert_eq!(run_idle(Weight::MAX), Weight::from_parts(2, 0));
		let row = deposit_row(DOT, PUSD, 1).expect("row activates after repair");
		assert_eq!(row.active_deposit, 100);
		assert!(row.pending_deposit.is_none());
	});
}
