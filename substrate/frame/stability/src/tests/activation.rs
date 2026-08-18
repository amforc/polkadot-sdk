//! What happens when the entry delay runs out.
//!
//! A matured pending deposit joins the active pool on the next write to its row. The owner does
//! not have to be the one who writes it, and the permissionless poke is what completes the move
//! when the owner stays away.

use crate::mock::*;

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
fn offset_at_exact_boundary_succeeds() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, DOT, PUSD, 400));

		// Exactly at t = 6_000 the deposit is mature: any touch folds it into
		// the active pool; the permissionless poke stands in for one.
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

		// The immediately following ordinary offset succeeds.
		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, 100, 80);
		assert_eq!(debt_offset, 100);
		assert_eq!(leftover, 0);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 300);
	});
}
