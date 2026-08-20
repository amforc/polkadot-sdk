//! `request_withdraw` and `withdraw`.
//!
//! The Normal-Mode path runs end to end, and the Safety-Mode path is checked at its amount and
//! timing boundaries. A request only exists in Safety Mode, so those tests enter it through the
//! debt fixture of the mock first. `mode` covers what a Normal-Mode request does instead.

use crate::{
	mock::*,
	types::{Deposit, DepositSnapshot, WithdrawalRequest},
	Error,
};
use pusd_primitives::BranchMode;

fn active_row(active: Balance, request: Option<WithdrawalRequest<Balance>>) -> Deposit<Balance> {
	let mut row = Deposit::fresh(DepositSnapshot::fresh());
	row.active_deposit = active;
	row.withdrawal_request = request;
	row
}

#[test]
fn withdraw_full_amount_prunes_row() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();

		assert_ok!(withdraw(1, DOT, PUSD, 400, 1));

		assert_eq!(stable_balance(PUSD, 1), 1_000);
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 0);
		assert!(deposit_row(DOT, PUSD, 1).is_none());
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.total_pending_deposits, 0);

		System::assert_last_event(
			crate::Event::WithdrawalExecuted {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 400,
			}
			.into(),
		);
	});
}

#[test]
fn withdraw_partial_keeps_row() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();

		assert_ok!(withdraw(1, DOT, PUSD, 150, 1));

		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 250);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 250);
		assert_eq!(stable_balance(PUSD, 1), 750);
	});
}

#[test]
fn withdraw_clamps_to_active_deposit() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();

		// Requesting more than the 400 active takes exactly the 400 — paid
		// to an empty-handed recipient, so the amount is visible on its own
		// rather than blending into user 1's original mint.
		assert_ok!(withdraw(1, DOT, PUSD, 1_000, 2));
		assert_eq!(stable_balance(PUSD, 2), 400);
		assert_eq!(stable_balance(PUSD, 1), 600);
		System::assert_last_event(
			crate::Event::WithdrawalExecuted {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 2,
				amount: 400,
			}
			.into(),
		);
	});
}

#[test]
fn withdraw_with_nothing_active_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		// Still pending (immature): nothing withdrawable.
		assert_ok!(deposit(1, DOT, PUSD, 400));
		assert_noop!(withdraw(1, DOT, PUSD, 400, 1), Error::<Test>::NoActiveDeposit);
	});
}

#[test]
fn withdraw_to_third_party_recipient() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();

		assert_ok!(withdraw(1, DOT, PUSD, 100, 9));
		assert_eq!(stable_balance(PUSD, 9), 100);
		assert_eq!(stable_balance(PUSD, 1), 600);
	});
}

#[test]
fn withdraw_leaves_pending_untouched() {
	build_and_execute(|| {
		seed_pool_with_matured_deposit();
		// A fresh pending amount on top of the 400 active.
		assert_ok!(deposit(1, DOT, PUSD, 300));

		assert_ok!(withdraw(1, DOT, PUSD, 200, 1));

		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 200);
		assert_eq!(row.pending_deposit.expect("still queued").amount, 300);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 200);
		assert_eq!(state.total_pending_deposits, 300);
	});
}

#[test]
fn withdraw_without_row_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		assert_noop!(withdraw(1, DOT, PUSD, 100, 1), Error::<Test>::DepositNotFound);
	});
}

#[test]
fn withdraw_on_unregistered_branch_reverts() {
	build_and_execute(|| {
		assert_noop!(withdraw(1, DOT, PUSD, 100, 1), Error::<Test>::PoolNotRegistered);
	});
}

#[test]
fn request_withdraw_records_exact_executable_at() {
	build_and_execute(|| {
		// Requests only record in Safety Mode: real branch debt plus the 0.6
		// price puts the TCR at 120%, under the 130% Safety threshold.
		seed_branch_with_debt();
		enter_safety_mode();

		let requested_at = Timestamp::get();
		assert_ok!(request_withdraw(1, DOT, PUSD, 250));
		let executable_at = requested_at + 600_000;

		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		let request = row.withdrawal_request.expect("request recorded");
		assert_eq!(request.amount, 250);
		assert_eq!(request.executable_at, executable_at);

		System::assert_last_event(
			crate::Event::WithdrawalRequested {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				amount: 250,
				executable_at,
			}
			.into(),
		);
	});
}

#[test]
fn new_request_replaces_old() {
	build_and_execute(|| {
		seed_branch_with_debt();
		enter_safety_mode();
		assert_ok!(request_withdraw(1, DOT, PUSD, 250));

		advance_time(4_000);
		let requested_at = Timestamp::get();
		assert_ok!(request_withdraw(1, DOT, PUSD, 100));

		let request = deposit_row(DOT, PUSD, 1)
			.expect("row exists")
			.withdrawal_request
			.expect("request recorded");
		assert_eq!(request.amount, 100);
		assert_eq!(request.executable_at, requested_at + 600_000);
	});
}

#[test]
fn zero_amount_requests_and_withdrawals_revert() {
	build_and_execute(|| {
		seed_branch_with_debt();
		assert_noop!(withdraw(1, DOT, PUSD, 0, 1), Error::<Test>::ZeroAmount);
		// The request rejects the zero before deciding whether to forward.
		assert_noop!(request_withdraw(1, DOT, PUSD, 0), Error::<Test>::ZeroAmount);
		enter_safety_mode();
		assert_noop!(request_withdraw(1, DOT, PUSD, 0), Error::<Test>::ZeroAmount);
	});
}

#[test]
fn deposit_leaves_request_unchanged() {
	build_and_execute(|| {
		seed_branch_with_debt();
		enter_safety_mode();
		let requested_at = Timestamp::get();
		assert_ok!(request_withdraw(1, DOT, PUSD, 250));

		// A new deposit leaves the request as it was.
		advance_time(1_000);
		assert_ok!(deposit(1, DOT, PUSD, 300));

		let request = deposit_row(DOT, PUSD, 1)
			.expect("row exists")
			.withdrawal_request
			.expect("request survives the deposit");
		assert_eq!(request.amount, 250);
		assert_eq!(request.executable_at, requested_at + 600_000);
	});
}

#[test]
fn normal_withdraw_ignores_request_and_prunes_it_with_the_row() {
	build_and_execute(|| {
		// A request recorded in Safety Mode lingers if the branch recovers
		// before execution.
		seed_branch_with_debt();
		enter_safety_mode();
		assert_ok!(request_withdraw(1, DOT, PUSD, 100));
		exit_safety_mode();

		// Normal Mode is not bounded by the 100-unit request.
		assert_ok!(withdraw(1, DOT, PUSD, 400, 1));
		assert_eq!(stable_balance(PUSD, 1), 1_000);
		// The emptied row takes the leftover request with it.
		assert!(deposit_row(DOT, PUSD, 1).is_none());
	});
}

// Pin the partial-consumption boundary directly; `tests/mode.rs` covers the
// broader Safety flow through the live vaults-derived mode.

#[test]
fn safety_withdrawal_respects_delay_boundary() {
	let request = WithdrawalRequest { amount: 250, executable_at: 606_000 };
	let mut row = active_row(400, Some(request.clone()));
	// One millisecond early: rejected, request untouched.
	let got = Stability::resolve_withdrawal(BranchMode::Safety, 605_999, 100, &mut row);
	assert_eq!(got, Err(Error::<Test>::SafetyWithdrawalDelayActive.into()));
	assert_eq!(row.withdrawal_request, Some(request));

	// At exactly `executable_at`: allowed.
	let got = Stability::resolve_withdrawal(BranchMode::Safety, 606_000, 100, &mut row);
	assert_eq!(got, Ok(100));
	assert_eq!(row.withdrawal_request.as_ref().expect("still open").amount, 150);
}
