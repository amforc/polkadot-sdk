//! What each operating mode allows, driven through the real vault pallet rather than a stub.
//!
//! A lost oracle price halts every value-moving operation. Safety Mode makes withdrawals two-step,
//! and in Normal Mode a withdrawal request executes at once.
//!
//! The fixtures come from the mock. [`ExtBuilder::branch_with_debt`] opens one vault and activates
//! a deposit at a TCR of 250%. [`enter_safety_mode`] then drops the price until the TCR reaches
//! 120%, between the liquidation ratio and the Safety threshold.

use crate::{mock::*, Error};

#[test]
fn frozen_branch_blocks_every_value_moving_operation() {
	ExtBuilder::branch_with_debt().build_and_execute(|| {
		assert_ok!(deposit(1, DOT, STABLE, 300));
		distribute_all_yield(60);
		// Let the 300's cohort mature, so the freeze provably blocks its advancement.
		advance_time(9_000);

		// A failing oracle reads as Frozen (fail closed).
		MockOracleAvailable::set(&false);

		assert_noop!(deposit(1, DOT, STABLE, 100), Error::<Test>::BranchFrozen);
		assert_noop!(request_withdraw(1, DOT, STABLE, 100), Error::<Test>::BranchFrozen);
		assert_noop!(withdraw(1, DOT, STABLE, 100, 1), Error::<Test>::BranchFrozen);
		assert_noop!(claim_collateral(1, DOT, STABLE, 1), Error::<Test>::BranchFrozen);
		assert_noop!(claim_yield(1, DOT, STABLE, 1), Error::<Test>::BranchFrozen);
		assert_noop!(compound(1, DOT, STABLE, 10), Error::<Test>::BranchFrozen);
		// A frozen branch quotes nothing, so no leg is sized and no offset is
		// made: the credit stays whole and nothing is written.
		assert_storage_noop!(assert_eq!(
			simulate_offset(DOT, STABLE, 100, 50),
			SimulatedOffset { active: 0, pending: 0, leftover: 50 }
		));
		// Yield routing cannot fail: the frozen pool just takes nothing, and not even the
		// matured cohort advances.
		assert_yield_declined(DOT, STABLE, 40);
	});
}

#[test]
fn safety_mode_enforces_two_step_withdrawals() {
	ExtBuilder::branch_with_debt().build_and_execute(|| {
		enter_safety_mode();

		// Safety Mode still takes and pays yield: only a freeze declines it. 40 over the 400 active
		// gives delta_G = 0.1.
		distribute_all_yield(40);
		assert_claim_yield(1, 40);

		// No immediate exit: Safety needs a matured request.
		assert_noop!(withdraw(1, DOT, STABLE, 100, 1), Error::<Test>::WithdrawalRequestMissing);
		assert_ok!(request_withdraw(1, DOT, STABLE, 250));
		assert_noop!(withdraw(1, DOT, STABLE, 100, 1), Error::<Test>::SafetyWithdrawalDelayActive);

		// The 600_000 ms Safety delay holds until its last millisecond.
		advance_time(599_999);
		assert_noop!(withdraw(1, DOT, STABLE, 100, 1), Error::<Test>::SafetyWithdrawalDelayActive);

		// At exactly `executable_at` a partial exit draws the request down.
		advance_time(1);
		assert_ok!(withdraw(1, DOT, STABLE, 100, 1));
		let request = deposit_row(DOT, STABLE, 1)
			.expect("row survives")
			.withdrawal_request
			.expect("request still open");
		assert_eq!(request.amount, 150);

		assert_ok!(withdraw(1, DOT, STABLE, 300, 1));
		// take = min(requested-remaining 150, active 300): the request
		// bounds the exit and is consumed by it.
		System::assert_last_event(market_event!(WithdrawalExecuted {
			depositor: 1,
			recipient: 1,
			amount: 150,
		}));
		assert_noop!(withdraw(1, DOT, STABLE, 100, 1), Error::<Test>::WithdrawalRequestMissing);
	});
}

#[test]
fn normal_mode_request_forwards_to_withdraw() {
	ExtBuilder::branch_with_debt().build_and_execute(|| {
		// In Normal Mode a request has no purpose — the exit is immediate —
		// so the call executes as a direct withdrawal to the caller and
		// records nothing.
		assert_ok!(request_withdraw(1, DOT, STABLE, 250));

		System::assert_last_event(market_event!(WithdrawalExecuted {
			depositor: 1,
			recipient: 1,
			amount: 250,
		}));
		assert_eq!(stable_balance(STABLE, 1), 850);
		let row = deposit_row(DOT, STABLE, 1).expect("row survives");
		assert_eq!(row.active_deposit, 150);
		assert!(row.withdrawal_request.is_none());
	});
}
