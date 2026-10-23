//! `request_withdraw` and `withdraw`.
//!
//! The Normal-Mode path runs end to end, and the Safety-Mode request is checked at its amount
//! boundaries. A request only exists in Safety Mode, so those tests enter it through the debt
//! fixture of the mock first. `mode` covers the Safety delay and what a Normal-Mode request does
//! instead.

use crate::{mock::*, types::WithdrawalRequest, Error};

/// Withdraws `requested` from the single depositor's 400 active to `recipient`, then asserts that
/// exactly the 400 left the pool and that the emptied row was pruned.
fn withdraw_and_assert_the_active_deposit_is_paid_out(requested: Balance, recipient: AccountId) {
	assert_ok!(withdraw(1, DOT, STABLE, requested, recipient));

	assert_eq!(pool_stable_held(DOT, STABLE), 0);
	assert!(deposit_row(DOT, STABLE, 1).is_none());
	let state = pool_state(DOT, STABLE);
	assert_eq!(state.total_active_deposits, 0);
	assert_eq!(state.total_pending_deposits, 0);

	System::assert_last_event(market_event!(WithdrawalExecuted {
		depositor: 1,
		recipient,
		amount: 400,
	}));
}

#[test]
fn withdraw_of_the_whole_active_deposit_pays_it_and_prunes_the_row() {
	ExtBuilder::single_depositor().build_and_execute(|| {
		withdraw_and_assert_the_active_deposit_is_paid_out(400, 1);

		// The depositor's 600 in its wallet plus the 400 it held active.
		assert_eq!(stable_balance(STABLE, 1), 1_000);

		// Storing the emptied row instead of pruning it is what the invariant check rejects.
		hypothetically!({
			let snapshot =
				DepositSnapshot { coords: pool_state(DOT, STABLE).coords, sums: active_sums(0, 0) };
			crate::Deposits::<Test>::insert((DOT, STABLE, 1), Deposit::fresh(snapshot));

			assert_err!(
				crate::try_state::do_try_state::<Test>(),
				"empty deposit row was not removed"
			);
		});
	});
}

#[test]
fn withdraw_over_the_active_deposit_pays_at_most_the_active_deposit() {
	ExtBuilder::single_depositor().build_and_execute(|| {
		withdraw_and_assert_the_active_deposit_is_paid_out(1_000, 2);

		// The over-ask goes to an empty-handed recipient, so the clamped amount is visible on its
		// own rather than blending into the depositor's original mint.
		assert_eq!(stable_balance(STABLE, 1), 600);
		assert_eq!(stable_balance(STABLE, 2), 400);
	});
}

#[test]
fn withdraw_leaves_pending_untouched() {
	ExtBuilder::single_depositor().build_and_execute(|| {
		// A fresh pending amount on top of the 400 active.
		assert_ok!(deposit(1, DOT, STABLE, 300));

		assert_ok!(withdraw(1, DOT, STABLE, 200, 1));

		let row = deposit_row(DOT, STABLE, 1).expect("row survives");
		assert_eq!(row.active_deposit, 200);
		assert_eq!(row.pending_deposit.expect("still queued").amount, 300);
		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 200);
		assert_eq!(state.total_pending_deposits, 300);
	});
}

#[test]
fn request_is_recorded_replaced_by_a_new_one_and_survives_deposits() {
	ExtBuilder::branch_with_debt().build_and_execute(|| {
		// Requests only record in Safety Mode: real branch debt plus the 0.6
		// price puts the TCR at 120%, under the 130% Safety threshold.
		enter_safety_mode();
		let requested_at = Timestamp::get();

		assert_ok!(request_withdraw(1, DOT, STABLE, 250));

		let executable_at = requested_at + 600_000;
		let request = deposit_row(DOT, STABLE, 1)
			.expect("row exists")
			.withdrawal_request
			.expect("request recorded");
		assert_eq!(request, WithdrawalRequest { amount: 250, executable_at });
		System::assert_last_event(market_event!(WithdrawalRequested {
			depositor: 1,
			amount: 250,
			executable_at,
		}));

		// A new request replaces the old one, delay and all.
		advance_time(4_000);
		let requested_at = Timestamp::get();
		assert_ok!(request_withdraw(1, DOT, STABLE, 100));

		let expected = WithdrawalRequest { amount: 100, executable_at: requested_at + 600_000 };
		assert_eq!(
			deposit_row(DOT, STABLE, 1).expect("row exists").withdrawal_request,
			Some(expected.clone())
		);

		// A deposit leaves the request as it was.
		advance_time(1_000);
		assert_ok!(deposit(1, DOT, STABLE, 300));

		assert_eq!(
			deposit_row(DOT, STABLE, 1).expect("row exists").withdrawal_request,
			Some(expected)
		);
	});
}

#[test]
fn withdraw_and_request_reject_zero_and_pending_only() {
	ExtBuilder::branch_with_debt().stable_balance(2, 400).build_and_execute(|| {
		assert_noop!(withdraw(1, DOT, STABLE, 0, 1), Error::<Test>::ZeroAmount);
		// The request rejects the zero before deciding whether to forward.
		assert_noop!(request_withdraw(1, DOT, STABLE, 0), Error::<Test>::ZeroAmount);

		// Still pending (immature): nothing withdrawable, so the entry delay cannot be bypassed.
		assert_ok!(deposit(2, DOT, STABLE, 400));
		assert_noop!(withdraw(2, DOT, STABLE, 400, 2), Error::<Test>::NoActiveDeposit);

		enter_safety_mode();

		assert_noop!(request_withdraw(1, DOT, STABLE, 0), Error::<Test>::ZeroAmount);
	});
}

#[test]
fn normal_withdraw_ignores_request_and_prunes_it_with_the_row() {
	ExtBuilder::branch_with_debt().build_and_execute(|| {
		// A request recorded in Safety Mode lingers if the branch recovers
		// before execution.
		enter_safety_mode();
		assert_ok!(request_withdraw(1, DOT, STABLE, 100));
		exit_safety_mode();

		// Normal Mode is not bounded by the 100-unit request.
		assert_ok!(withdraw(1, DOT, STABLE, 400, 1));

		assert_eq!(stable_balance(STABLE, 1), 1_000);
		// The emptied row takes the leftover request with it.
		assert!(deposit_row(DOT, STABLE, 1).is_none());
	});
}
