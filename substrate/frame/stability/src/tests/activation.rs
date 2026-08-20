//! What happens when the entry delay runs out, at the exact deadline boundary.
//!
//! A matured cohort activates on the next economic pool operation without settling its depositor
//! rows. `cohorts` covers the wider lifecycle; these tests pin the boundary millisecond.

use crate::mock::*;

#[test]
fn offset_with_only_immature_pending_fails() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, DOT, PUSD, 400));

		// Deposited at t = 1_000: the cohort boundary lands at 10_000, and t = 9_999 is one
		// millisecond short.
		advance_time(8_999);

		// With no active capital, an ordinary offset finds nothing to burn
		// and returns the credit whole.
		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, 100, 80);
		assert_eq!(debt_offset, 0);
		assert_eq!(leftover, 80);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 0);
		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.pending_deposit.expect("still pending").amount, 400);
	});
}

#[test]
fn offset_at_exact_boundary_succeeds() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, DOT, PUSD, 400));

		// Exactly at t = 10_000 the cohort is mature. The ordinary offset itself must advance it;
		// no depositor-row call runs first.
		advance_time(9_000);

		// Maturity itself moves no funds; the pool already holds the stablecoin.
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 400);
		assert_eq!(stable_balance(PUSD, 1), 600);

		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, 100, 80);
		assert_eq!(debt_offset, 100);
		assert_eq!(leftover, 0);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 300);
		assert_eq!(state.total_pending_deposits, 0);

		System::assert_has_event(
			crate::Event::CohortActivated {
				collateral_id: DOT,
				stable_id: PUSD,
				cohort: crate::types::CohortId(0),
				deadline: 10_000,
				amount: 400,
			}
			.into(),
		);

		// Aggregate activation did not touch the depositor row.
		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.pending_deposit.expect("not yet settled").amount, 400);
	});
}
