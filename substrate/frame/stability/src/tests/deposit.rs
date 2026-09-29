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
		let fresh = DepositSnapshot {
			coords: Accumulators { p: FixedU128::one(), epoch: 0, scale: 0 },
			sums: PoolSums::default(),
		};
		let mut expected = Deposit::fresh(fresh);
		expected.pending_deposit =
			Some(PendingDeposit { amount: 400, cohort: CohortId(0), snapshot: fresh });
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
		mint_stable(PUSD, 2, 200);

		// Both join cohort 0 at t = 1_000: 6_000, rounded up to the boundary at 10_000.
		assert_ok!(deposit(1, DOT, PUSD, 400));
		assert_ok!(deposit(2, DOT, PUSD, 200));
		assert_eq!(pending_deadline(DOT, PUSD, 1), Some(10_000));

		// The top-up at t = 6_000 lands in the next window, with cohort 0 still open: the
		// earliest activation is 11_000, rounded up to the boundary at 15_000.
		advance_time(5_000);
		assert_ok!(deposit(1, DOT, PUSD, 300));

		// The merge restarts the delay of the whole amount, so the first 400 leaves cohort 0 and
		// waits out the later deadline with the top-up.
		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		let pending = row.pending_deposit.expect("still pending");
		assert_eq!(pending.amount, 700);
		assert_eq!(pending.cohort, CohortId(1));
		assert_eq!(pending_deadline(DOT, PUSD, 1), Some(15_000));

		// The member that stayed behind keeps its cohort, which now carries its 200 alone.
		assert_eq!(pending_deadline(DOT, PUSD, 2), Some(10_000));
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.open_cohorts.len(), 2);
		let older = &state.open_cohorts[0];
		let newer = &state.open_cohorts[1];
		assert_eq!((older.id.0, older.deadline, older.members, older.amount), (0, 10_000, 1, 200));
		assert_eq!((newer.id.0, newer.deadline, newer.members, newer.amount), (1, 15_000, 1, 700));
		assert_eq!(state.total_pending_deposits, 900);

		System::assert_last_event(
			crate::Event::DepositReceived {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				amount: 300,
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
