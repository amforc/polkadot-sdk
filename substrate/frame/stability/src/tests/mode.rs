//! Branch-mode gating through the real vaults-derived mode:
//! oracle failure and governance freezes halt the pool, Safety Mode (low
//! TCR) turns withdrawals two-step, everything else keeps working.
//!
//! Fixtures come from the mock: [`seed_branch_with_debt`] opens a
//! 1000-collateral / 500-debt vault (TCR 250% at the 1.25 registration
//! price) and activates a 400 deposit for user 1; [`enter_safety_mode`]
//! drops DOT to 0.6 (TCR 120%, between the 110% MCR and the 130% Safety
//! threshold).

use crate::{mock::*, Error};

#[test]
fn frozen_branch_blocks_every_value_moving_operation() {
	build_and_execute(|| {
		seed_branch_with_debt();
		assert_ok!(deposit(1, DOT, PUSD, 300));
		drop(distribute_yield(DOT, PUSD, 60));
		// Let the 300 mature, so the freeze provably blocks its activation.
		advance_time(5_000);

		// A failing oracle reads as Frozen (fail closed).
		MockOracleAvailable::set(false);

		assert_noop!(deposit(1, DOT, PUSD, 100), Error::<Test>::BranchFrozen);
		assert_noop!(request_withdraw(1, DOT, PUSD, 100), Error::<Test>::BranchFrozen);
		assert_noop!(withdraw(1, DOT, PUSD, 100, 1), Error::<Test>::BranchFrozen);
		assert_noop!(claim_collateral(1, DOT, PUSD, 1), Error::<Test>::BranchFrozen);
		assert_noop!(claim_yield(1, DOT, PUSD, 1), Error::<Test>::BranchFrozen);
		assert_noop!(compound(1, DOT, PUSD, 10), Error::<Test>::BranchFrozen);
		// The infallible offset surfaces step aside on a frozen branch:
		// zeroed results, credits returned whole.
		let (debt_offset, leftover) = simulate_offset(DOT, PUSD, 100, 50);
		assert_eq!(debt_offset, 0);
		assert_eq!(leftover, 50);
		let (debt_offset, leftover) = simulate_pending_offset(DOT, PUSD, 100, 50);
		assert_eq!(debt_offset, 0);
		assert_eq!(leftover, 50);
		// Yield routing cannot fail: the frozen pool just takes nothing.
		let leftover = distribute_yield(DOT, PUSD, 40);
		assert_eq!(leftover.peek(), 40);
		drop(leftover);
		// Poke moves no value and stays available for housekeeping, but the
		// matured pending amount stays put: activation changes offsettable
		// risk, which the freeze halts.
		assert_ok!(poke(7, 1, DOT, PUSD));
		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 400);
		assert_eq!(row.pending_deposit.expect("still pending").amount, 300);

		// Oracle recovery reopens the pool; the next touch folds the 300 in.
		MockOracleAvailable::set(true);
		assert_ok!(deposit(1, DOT, PUSD, 100));
		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 700);
		assert_eq!(row.pending_deposit.expect("new amount queued").amount, 100);
	});
}

#[test]
fn governance_freeze_gates_and_admin_clear_reopens() {
	build_and_execute(|| {
		seed_branch_with_debt();

		assert_ok!(Vaults::set_governance_frozen(RuntimeOrigin::root(), DOT, PUSD, true));
		assert_noop!(deposit(1, DOT, PUSD, 100), Error::<Test>::BranchFrozen);
		assert_noop!(withdraw(1, DOT, PUSD, 100, 1), Error::<Test>::BranchFrozen);

		assert_ok!(Vaults::set_governance_frozen(RuntimeOrigin::signed(ADMIN), DOT, PUSD, false));
		assert_ok!(deposit(1, DOT, PUSD, 100));
	});
}

#[test]
fn safety_mode_enforces_two_step_withdrawals() {
	build_and_execute(|| {
		seed_branch_with_debt();
		enter_safety_mode();

		// No immediate exit: Safety needs a matured request.
		assert_noop!(withdraw(1, DOT, PUSD, 100, 1), Error::<Test>::WithdrawalRequestMissing);
		assert_ok!(request_withdraw(1, DOT, PUSD, 250));
		assert_noop!(withdraw(1, DOT, PUSD, 100, 1), Error::<Test>::SafetyWithdrawalDelayActive);

		// Wait out the full 600_000 ms Safety delay.
		advance_time(600_000);
		assert_ok!(withdraw(1, DOT, PUSD, 300, 1));
		// take = min(requested-remaining 250, active 400): the request
		// bounds the exit and is consumed by it.
		System::assert_last_event(
			crate::Event::WithdrawalExecuted {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 250,
			}
			.into(),
		);
		assert_noop!(withdraw(1, DOT, PUSD, 100, 1), Error::<Test>::WithdrawalRequestMissing);
	});
}

#[test]
fn safety_mode_keeps_deposits_claims_and_offsets_working() {
	build_and_execute(|| {
		seed_branch_with_debt();
		enter_safety_mode();

		// New capital may still enter (queued behind the entry delay).
		assert_ok!(deposit(1, DOT, PUSD, 100));

		// Liquidation offsets keep operating in Safety Mode. At the 0.6
		// price, 48 debt seizes 48 / 0.6 = 80 collateral:
		// delta_S = 80 * (1/400) = 0.2, P = 352/400 = 0.88;
		// gain = (400/1) * 0.2 = 80, compounded = (400/1) * 0.88 = 352.
		assert_eq!(simulate_offset(DOT, PUSD, 48, 80).0, 48);
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 80);

		drop(distribute_yield(DOT, PUSD, 70));
		// A = 352 after the offset: delta_G = 70 * (0.88/352) = 0.175;
		// the claim realizes against the post-claim snapshot (P0 = 0.88):
		// yield = floor(352 * 0.175 / 0.88) = 70.
		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		System::assert_has_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 70,
			}
			.into(),
		);
	});
}

#[test]
fn normal_mode_request_forwards_to_withdraw() {
	build_and_execute(|| {
		seed_branch_with_debt();

		// In Normal Mode a request has no purpose — the exit is immediate —
		// so the call executes as a direct withdrawal to the caller and
		// records nothing.
		assert_ok!(request_withdraw(1, DOT, PUSD, 250));
		System::assert_last_event(
			crate::Event::WithdrawalExecuted {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 250,
			}
			.into(),
		);
		assert_eq!(stable_balance(PUSD, 1), 850);
		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 150);
		assert!(row.withdrawal_request.is_none());
	});
}
