//! `request_withdraw` and `withdraw`.
//!
//! The Normal-Mode path runs end to end, and the Safety-Mode request is checked at its amount
//! boundaries. A request only exists in Safety Mode, so those tests enter it through the debt
//! fixture of the mock first. `mode` covers the Safety delay and what a Normal-Mode request does
//! instead.

use crate::{mock::*, types::WithdrawalRequest, Error};

#[test]
fn withdraw_pays_at_most_the_active_deposit_and_prunes_the_row() {
	// The depositor holds 400 active and 600 in its wallet. The exact amount goes back to the
	// depositor. The over-ask goes to an empty-handed recipient, so the clamped amount is visible
	// on its own rather than blending into the original mint.
	let cases: [(Balance, AccountId, Balance, Balance); 2] =
		[(400, 1, 1_000, 1_000), (1_000, 2, 600, 400)];
	for (requested, recipient, depositor_balance, recipient_balance) in cases {
		build_and_execute(|| {
			seed_pool_with_matured_deposit();

			assert_ok!(withdraw(1, DOT, PUSD, requested, recipient));

			assert_eq!(stable_balance(PUSD, 1), depositor_balance);
			assert_eq!(stable_balance(PUSD, recipient), recipient_balance);
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
					recipient,
					amount: 400,
				}
				.into(),
			);
		});
	}
}

#[test]
fn withdraw_with_nothing_active_reverts() {
	build_with_default_market(|| {
		mint_stable(PUSD, 1, 1_000);
		// Still pending (immature): nothing withdrawable.
		assert_ok!(deposit(1, DOT, PUSD, 400));
		assert_noop!(withdraw(1, DOT, PUSD, 400, 1), Error::<Test>::NoActiveDeposit);
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
fn request_is_recorded_replaced_by_a_new_one_and_survives_deposits() {
	build_and_execute(|| {
		// Requests only record in Safety Mode: real branch debt plus the 0.6
		// price puts the TCR at 120%, under the 130% Safety threshold.
		seed_branch_with_debt();
		enter_safety_mode();

		let requested_at = Timestamp::get();
		assert_ok!(request_withdraw(1, DOT, PUSD, 250));
		let executable_at = requested_at + 600_000;
		let request = deposit_row(DOT, PUSD, 1)
			.expect("row exists")
			.withdrawal_request
			.expect("request recorded");
		assert_eq!(request, WithdrawalRequest { amount: 250, executable_at });
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

		// A new request replaces the old one, delay and all.
		advance_time(4_000);
		let requested_at = Timestamp::get();
		assert_ok!(request_withdraw(1, DOT, PUSD, 100));
		let expected = WithdrawalRequest { amount: 100, executable_at: requested_at + 600_000 };
		assert_eq!(
			deposit_row(DOT, PUSD, 1).expect("row exists").withdrawal_request,
			Some(expected.clone())
		);

		// A deposit leaves the request as it was.
		advance_time(1_000);
		assert_ok!(deposit(1, DOT, PUSD, 300));
		assert_eq!(
			deposit_row(DOT, PUSD, 1).expect("row exists").withdrawal_request,
			Some(expected)
		);
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
