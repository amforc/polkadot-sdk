//! `deposit`: what leaves the wallet, what queues behind the entry delay, and what happens when a
//! second deposit arrives.

use crate::{mock::*, Error};

#[test]
fn deposit_moves_funds_and_queues_pending() {
	build_with_default_market(|| {
		mint_stable(PUSD, 1, 1_000);

		assert_ok!(deposit(1, DOT, PUSD, 400));

		assert_eq!(stable_balance(PUSD, 1), 600);
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 400);

		// The whole 400 queues in the first cohort at the fresh pending accumulators. Nothing is
		// active or claimable yet.
		let mut expected = Deposit::fresh(DepositSnapshot::fresh());
		expected.pending_deposit = Some(PendingDeposit {
			amount: 400,
			cohort: CohortId(0),
			snapshot: DepositSnapshot::fresh(),
		});
		assert_eq!(deposit_row(DOT, PUSD, 1), Some(expected));
		// Deposited at t = 1_000 with the 5_000 ms entry delay: 6_000, rounded up to the cohort
		// boundary at 10_000.
		assert_eq!(pending_deadline(DOT, PUSD, 1), Some(10_000));

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_pending_deposits, 400);
		assert_eq!(state.total_active_deposits, 0);

		System::assert_last_event(
			crate::Event::DepositReceived {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				amount: 400,
				used_for_recovery: 0,
				pending_amount: 400,
			}
			.into(),
		);
	});
}

#[test]
fn deposit_below_minimum_reverts_at_minimum_succeeds() {
	build_with_default_market(|| {
		mint_stable(PUSD, 1, 1_000);

		// The branch minimum is 100.
		assert_noop!(deposit(1, DOT, PUSD, 99), Error::<Test>::DepositTooSmall);
		assert_ok!(deposit(1, DOT, PUSD, 100));
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 100);
	});
}

#[test]
fn deposit_without_funds_reverts() {
	build_with_default_market(|| {
		// User 2 was never minted any PUSD, so the asset account itself is
		// missing (a funded-but-short wallet errors `BalanceLow` instead —
		// see `deposit_in_the_wallet_dead_zone_fails_instead_of_dusting`).
		assert_noop!(deposit(2, DOT, PUSD, 400), pallet_assets::Error::<Test>::NoAccount);
	});
}

#[test]
fn second_deposit_merges_and_resets_delay() {
	build_with_default_market(|| {
		mint_stable(PUSD, 1, 1_000);

		assert_ok!(deposit(1, DOT, PUSD, 400));
		advance_time(2_000);
		assert_ok!(deposit(1, DOT, PUSD, 300));

		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		let pending = row.pending_deposit.expect("still pending");
		assert_eq!(pending.amount, 700);
		// The merge restarts the whole amount's delay: topped up at t = 3_000, so the earliest
		// activation is 8_000, rounded up to the cohort boundary at 10_000.
		assert_eq!(pending_deadline(DOT, PUSD, 1), Some(10_000));
		assert_eq!(pool_state(DOT, PUSD).total_pending_deposits, 700);

		System::assert_last_event(
			crate::Event::DepositReceived {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				amount: 300,
				used_for_recovery: 0,
				pending_amount: 300,
			}
			.into(),
		);
	});
}

#[test]
fn deposit_in_the_wallet_dead_zone_fails_instead_of_dusting() {
	build_and_execute(|| {
		register_branch(DOT, USDX, branch_config_for(DOT, USDX));
		mint_stable(USDX, 1, 50_000);

		// 45_000 would leave 5_000 < the 10_000 USDX minimum in the wallet.
		// The funding withdrawal runs under `Preserve` (not a full drain), so
		// the asset pallet itself rejects it instead of folding the 5_000
		// into the debit. Depositing the whole wallet is the legitimate full
		// expend.
		assert_noop!(deposit(1, DOT, USDX, 45_000), pallet_assets::Error::<Test>::BalanceLow);
		assert_ok!(deposit(1, DOT, USDX, 50_000));
		assert_eq!(stable_balance(USDX, 1), 0);

		let pool = Stability::pool_account(&DOT, &USDX);
		assert_eq!(stable_balance(USDX, pool), 50_000);
	});
}
