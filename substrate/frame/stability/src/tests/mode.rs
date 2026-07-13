//! Branch-mode gating (SPEC.md §8.1) through the real vaults-derived mode:
//! oracle failure and governance freezes halt the pool, Safety Mode (low
//! TCR) turns withdrawals two-step, everything else keeps working.

use crate::{mock::*, Error};

/// Register the market, give it real branch debt (a 1000-collateral /
/// 500-debt vault: TCR 250% at the 1.25 registration price), and activate a
/// 400 deposit for user 1 — all still in Normal Mode.
fn seed_branch_with_debt() {
	register_branch(DOT, PUSD, default_branch_config());
	mint_collateral(DOT, 5, 2_000);
	assert_ok!(open_vault(5, DOT, PUSD, 1_000, 500));
	mint_stable(PUSD, 1, 1_000);
	assert_ok!(deposit(1, DOT, PUSD, 400));
	advance_time(5_000);
	assert_ok!(activate(1, DOT, PUSD));
}

/// TCR = 1000 * 0.6 / 500 = 120%: below the 130% Safety threshold, above
/// the 110% MCR.
fn enter_safety_mode() {
	set_price(DOT, FixedU128::from_rational(6u128, 10u128));
}

#[test]
fn frozen_branch_blocks_every_value_moving_operation() {
	build_and_execute(|| {
		seed_branch_with_debt();
		assert_ok!(deposit(1, DOT, PUSD, 300));
		drop(distribute_yield(DOT, PUSD, 60));

		// A failing oracle reads as Frozen (fail closed).
		MockOracleAvailable::set(false);

		assert_noop!(deposit(1, DOT, PUSD, 100), Error::<Test>::BranchFrozen);
		assert_noop!(activate(1, DOT, PUSD), Error::<Test>::BranchFrozen);
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
		let (result, leftover) = simulate_pending_offset(DOT, PUSD, 100, 50, 5);
		assert_eq!(result.debt_offset, 0);
		assert_eq!(leftover, 50);
		// Yield routing cannot fail: the frozen pool just takes nothing.
		let leftover = distribute_yield(DOT, PUSD, 40);
		assert_eq!(leftover.peek(), 40);
		drop(leftover);
		// Poke moves no value and stays available for housekeeping.
		assert_ok!(poke(7, 1, DOT, PUSD));

		// Oracle recovery reopens the pool.
		MockOracleAvailable::set(true);
		assert_ok!(deposit(1, DOT, PUSD, 100));
	});
}

#[test]
fn governance_freeze_gates_and_admin_clear_reopens() {
	build_and_execute(|| {
		seed_branch_with_debt();

		assert_ok!(Vaults::enable_frozen_mode(RuntimeOrigin::root(), DOT, PUSD));
		assert_noop!(deposit(1, DOT, PUSD, 100), Error::<Test>::BranchFrozen);
		assert_noop!(withdraw(1, DOT, PUSD, 100, 1), Error::<Test>::BranchFrozen);

		assert_ok!(Vaults::clear_governance_frozen_mode(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
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

		// Liquidation offsets keep operating in Safety Mode: A = 400,
		// delta_S = floor(20 * 1e18 / 400) = 5e16,
		// gain = floor(400 * 0.05) = 20, compounded = floor(400 * 0.875).
		assert_eq!(simulate_offset(DOT, PUSD, 50, 20).0, 50);
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 20);

		drop(distribute_yield(DOT, PUSD, 70));
		// A = 350 after the offset: delta_G = floor(70 * P / 350) with
		// P = 0.875 → 175e15; yield = floor(350 * 0.175 / 0.875) = 70.
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
fn normal_request_carries_its_delay_into_safety() {
	build_and_execute(|| {
		seed_branch_with_debt();

		// Requested in Normal Mode at t = 6_000: executable_at = 606_000.
		assert_ok!(request_withdraw(1, DOT, PUSD, 250));
		enter_safety_mode();

		// The branch turned, but the request already carries its delay.
		assert_noop!(withdraw(1, DOT, PUSD, 250, 1), Error::<Test>::SafetyWithdrawalDelayActive);
		advance_time(600_000);
		assert_ok!(withdraw(1, DOT, PUSD, 250, 1));
		assert_eq!(stable_balance(PUSD, 1), 850);
	});
}
