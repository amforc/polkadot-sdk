//! Adversarial boundaries: stale user intent, corrupted FIFO/storage
//! disagreement, and failed value movement after planning.

use crate::{mock::*, pending, Error};
use frame::traits::{
	fungibles::Balanced as FungiblesBalanced,
	tokens::{Fortitude, Precision, Preservation},
};
use pusd_primitives::{OnBranchYield, StabilityPoolOffsetApi};

fn seed_branch_with_debt() {
	register_branch(DOT, PUSD, default_branch_config());
	mint_collateral(DOT, 5, 2_000);
	assert_ok!(open_vault(5, DOT, PUSD, 1_000, 500));
	mint_stable(PUSD, 1, 1_000);
	assert_ok!(deposit(1, DOT, PUSD, 400));
	activate_all(&[1]);
}

fn enter_safety_mode() {
	set_price(DOT, FixedU128::from_rational(6u128, 10u128));
}

fn burn_stable(stable: StableId, who: AccountId, amount: Balance) {
	let credit = <Assets as FungiblesBalanced<AccountId>>::withdraw(
		stable,
		&who,
		amount,
		Precision::Exact,
		Preservation::Expendable,
		Fortitude::Polite,
	)
	.expect("pool stable balance covers the forced burn");
	drop(credit);
}

#[test]
fn yield_distribution_returns_credit_when_pool_account_cannot_hold_it() {
	build_and_execute(|| {
		register_branch(DOT, USDX, default_branch_config());
		mint_stable(USDX, 1, USDX_MIN_BALANCE);
		assert_ok!(deposit(1, DOT, USDX, USDX_MIN_BALANCE));
		advance_time(5_000);
		assert_ok!(activate(1, DOT, USDX));

		let pool = Stability::pool_account(&DOT, &USDX);
		let state_before = pool_state(DOT, USDX);
		let sums_before = crate::PoolSumsStore::<Test>::get((DOT, USDX, 0u32, 0u32));

		// USDX has a 10_000-unit minimum balance. Emptying the pool asset
		// account makes a sub-minimum yield credit unresolvable, so the
		// infallible hook must hand the credit back untouched.
		burn_stable(USDX, pool, USDX_MIN_BALANCE);
		assert_eq!(stable_balance(USDX, pool), 0);

		let leftover = distribute_yield(DOT, USDX, USDX_MIN_BALANCE - 1);
		assert_eq!(leftover.peek(), USDX_MIN_BALANCE - 1);
		drop(leftover);
		assert_eq!(stable_balance(USDX, pool), 0);
		assert_eq!(pool_state(DOT, USDX), state_before);
		assert_eq!(crate::PoolSumsStore::<Test>::get((DOT, USDX, 0u32, 0u32)), sums_before);

		// Restore the artificial corruption before the post-test try-state
		// identity check runs.
		mint_stable(USDX, pool, USDX_MIN_BALANCE);
	});
}

#[test]
fn yield_distribution_rejects_a_credit_for_another_stablecoin() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 400);
		assert_ok!(deposit(1, DOT, PUSD, 400));
		activate_all(&[1]);

		let pool = Stability::pool_account(&DOT, &PUSD);
		let state_before = pool_state(DOT, PUSD);
		let credit = <Assets as FungiblesBalanced<AccountId>>::issue(USDX, 20_000);
		let returned = Stability::distribute_yield(&DOT, &PUSD, credit);

		assert_eq!(returned.asset(), USDX);
		assert_eq!(returned.peek(), 20_000);
		assert_eq!(stable_balance(USDX, pool), 0);
		assert_eq!(pool_state(DOT, PUSD), state_before);
		drop(returned);
	});
}

#[test]
fn offset_apis_reject_a_credit_for_another_collateral() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 400);
		assert_ok!(deposit(1, DOT, PUSD, 400));
		activate_all(&[1]);
		mint_stable(PUSD, 2, 200);
		assert_ok!(deposit(2, DOT, PUSD, 200));

		let pool = Stability::pool_account(&DOT, &PUSD);
		let state_before = pool_state(DOT, PUSD);
		let (debt_offset, returned) =
			Stability::offset_liquidation(&DOT, &PUSD, 200, issue_collateral(TOKEN_X, 100));
		assert_eq!(debt_offset, 0);
		assert_eq!(returned.asset(), TOKEN_X);
		assert_eq!(returned.peek(), 100);
		drop(returned);

		let (result, returned) = Stability::offset_pending_liquidation(
			&DOT,
			&PUSD,
			200,
			5,
			issue_collateral(TOKEN_X, 100),
		);
		assert_eq!(result.debt_offset, 0);
		assert_eq!(returned.asset(), TOKEN_X);
		assert_eq!(returned.peek(), 100);
		drop(returned);

		assert_eq!(collateral_balance(TOKEN_X, pool), 0);
		assert_eq!(pool_state(DOT, PUSD), state_before);
		assert_eq!(deposit_row(DOT, PUSD, 2).unwrap().pending_deposit.unwrap().amount, 200);
	});
}

#[test]
fn stable_withdrawal_failure_returns_the_full_collateral_credit() {
	build_and_execute(|| {
		register_branch(TOKEN_X, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 400);
		assert_ok!(deposit(1, TOKEN_X, PUSD, 400));
		advance_time(5_000);
		assert_ok!(activate(1, TOKEN_X, PUSD));

		let pool = Stability::pool_account(&TOKEN_X, &PUSD);
		// Break the stable-balance identity so the stable withdrawal fails
		// before any part of the collateral credit is consumed.
		burn_stable(PUSD, pool, 400);

		let (debt_offset, returned) =
			Stability::do_offset_liquidation(&TOKEN_X, &PUSD, 200, issue_collateral(TOKEN_X, 100));
		assert_eq!(debt_offset, 0);
		assert_eq!(returned.asset(), TOKEN_X);
		assert_eq!(returned.peek(), 100);
		assert_eq!(collateral_balance(TOKEN_X, pool), 0);
		drop(returned);

		// Repair the deliberate corruption before the post-test invariant check.
		mint_stable(PUSD, pool, 400);
	});
}

#[test]
fn activation_rejects_pending_row_missing_fifo_slot() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, DOT, PUSD, 400));
		advance_time(5_000);

		let fifo = pending::list_id::<Test>(&DOT, &PUSD);
		assert_ok!(pending::remove::<Test>(&fifo, &1));
		assert!(!pending_contains(DOT, PUSD, 1));

		assert_noop!(activate(1, DOT, PUSD), Error::<Test>::PendingFifoInvariantBroken);
		let row = deposit_row(DOT, PUSD, 1).expect("row survives failed activation");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.pending_deposit.expect("pending remains").amount, 400);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.total_pending_deposits, 400);

		// Repair the deliberate FIFO corruption and prove the row can still
		// activate cleanly.
		assert_ok!(pending::append::<Test>(&fifo, 1));
		assert_ok!(activate(1, DOT, PUSD));
		let row = deposit_row(DOT, PUSD, 1).expect("activated row survives");
		assert_eq!(row.active_deposit, 400);
		assert!(row.pending_deposit.is_none());
	});
}

#[test]
fn safety_withdraw_after_offset_cannot_overdraw_stale_request() {
	build_and_execute(|| {
		seed_branch_with_debt();
		assert_ok!(request_withdraw(1, DOT, PUSD, 400));
		enter_safety_mode();

		// The request still says 400, but a liquidation offset shrinks the
		// live active deposit to 100 before the request matures.
		assert_eq!(simulate_offset(DOT, PUSD, 300, 120).0, 300);
		advance_time(600_000);

		assert_ok!(withdraw(1, DOT, PUSD, 400, 1));
		System::assert_has_event(
			crate::Event::WithdrawalExecuted {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 100,
			}
			.into(),
		);
		assert_eq!(stable_balance(PUSD, 1), 700);

		let row = deposit_row(DOT, PUSD, 1).expect("claimable keeps row alive");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.claimable_collateral, 120);
		assert_eq!(row.withdrawal_request.expect("request remainder stays bounded").amount, 300);

		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 120);
		assert!(deposit_row(DOT, PUSD, 1).is_none());
	});
}
