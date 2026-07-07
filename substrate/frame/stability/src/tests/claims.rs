//! `claim_collateral` / `claim_yield` / `poke_deposit`.
//!
//! Offsets and yield distribution do not exist yet, so claimables are seeded
//! by direct storage writes mirrored into the pool totals and backed by
//! minted pool-account balances — exactly the shape the offset and yield
//! engines will produce. End-to-end "earned" claims arrive with those
//! milestones.

use crate::{mock::*, Error};

/// Credit `who` with claimable gains the way an offset / yield distribution
/// would: row claimable + pool unclaimed total + backing pool balance.
fn seed_claimables(who: AccountId, collateral_gain: Balance, yield_gain: Balance) {
	let pool = Stability::pool_account(&DOT, &PUSD);
	crate::Deposits::<Test>::mutate((DOT, PUSD, who), |row| {
		let row = row.as_mut().expect("deposit row exists");
		row.claimable_collateral += collateral_gain;
		row.claimable_yield += yield_gain;
	});
	crate::PoolStates::<Test>::mutate(DOT, PUSD, |state| {
		let state = state.as_mut().expect("pool state exists");
		state.total_collateral_gains_unclaimed += collateral_gain;
		state.total_yield_unclaimed += yield_gain;
	});
	if collateral_gain > 0 {
		mint_collateral(DOT, pool, collateral_gain);
	}
	if yield_gain > 0 {
		mint_stable(PUSD, pool, yield_gain);
	}
}

#[test]
fn claim_collateral_pays_out_and_clears() {
	build_and_execute(|| {
		seed_active_deposit();
		seed_claimables(1, 70, 0);

		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));

		assert_eq!(collateral_balance(DOT, 1), before + 70);
		let row = deposit_row(DOT, PUSD, 1).expect("row survives with active deposit");
		assert_eq!(row.claimable_collateral, 0);
		assert_eq!(row.active_deposit, 400);
		assert_eq!(pool_state(DOT, PUSD).total_collateral_gains_unclaimed, 0);

		System::assert_last_event(
			crate::Event::CollateralClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 70,
			}
			.into(),
		);

		// Nothing left to claim.
		assert_noop!(claim_collateral(1, DOT, PUSD, 1), Error::<Test>::NoClaimableCollateral);
	});
}

#[test]
fn claim_yield_pays_out_to_recipient_and_clears() {
	build_and_execute(|| {
		seed_active_deposit();
		seed_claimables(1, 0, 55);

		assert_ok!(claim_yield(1, DOT, PUSD, 9));

		assert_eq!(stable_balance(PUSD, 9), 55);
		let row = deposit_row(DOT, PUSD, 1).expect("row survives with active deposit");
		assert_eq!(row.claimable_yield, 0);
		assert_eq!(pool_state(DOT, PUSD).total_yield_unclaimed, 0);

		System::assert_last_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 9,
				amount: 55,
			}
			.into(),
		);

		assert_noop!(claim_yield(1, DOT, PUSD, 9), Error::<Test>::NoClaimableYield);
	});
}

#[test]
fn claims_leave_active_and_pending_untouched() {
	build_and_execute(|| {
		seed_active_deposit();
		// A fresh immature pending amount on top of the 400 active.
		assert_ok!(deposit(1, DOT, PUSD, 300));
		seed_claimables(1, 70, 55);

		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_ok!(claim_yield(1, DOT, PUSD, 1));

		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 400);
		assert_eq!(row.pending_deposit.expect("still queued").amount, 300);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 400);
		assert_eq!(state.total_pending_deposits, 300);
	});
}

#[test]
fn final_claim_prunes_an_otherwise_empty_row() {
	build_and_execute(|| {
		seed_active_deposit();
		seed_claimables(1, 70, 55);
		// Drain the active deposit; only the two claimables keep the row.
		assert_ok!(withdraw(1, DOT, PUSD, 400, 1));
		assert!(deposit_row(DOT, PUSD, 1).is_some());

		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		// Yield is still claimable, so the row must survive the first claim.
		assert!(deposit_row(DOT, PUSD, 1).is_some());

		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		assert!(deposit_row(DOT, PUSD, 1).is_none());
	});
}

#[test]
fn claims_without_row_or_branch_revert() {
	build_and_execute(|| {
		assert_noop!(claim_collateral(1, DOT, PUSD, 1), Error::<Test>::BranchNotRegistered);
		assert_noop!(claim_yield(1, DOT, PUSD, 1), Error::<Test>::BranchNotRegistered);

		register_branch(DOT, PUSD, default_branch_config());
		assert_noop!(claim_collateral(1, DOT, PUSD, 1), Error::<Test>::DepositNotFound);
		assert_noop!(claim_yield(1, DOT, PUSD, 1), Error::<Test>::DepositNotFound);
	});
}

#[test]
fn claim_activates_matured_pending() {
	build_and_execute(|| {
		seed_active_deposit();
		assert_ok!(deposit(1, DOT, PUSD, 300));
		seed_claimables(1, 70, 0);

		// The claim is a caller-initiated operation, so it performs the
		// standard housekeeping: the matured 300 becomes active.
		run_to_block(11);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));

		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 700);
		assert!(row.pending_deposit.is_none());
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 700);
		assert_eq!(state.total_pending_deposits, 0);
	});
}

#[test]
fn poke_realizes_but_never_activates() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, DOT, PUSD, 400));

		// Long past maturity, a third party pokes the row: it stays pending.
		// Activating would expose the owner's capital to ordinary offsets,
		// and that choice belongs to the owner alone.
		run_to_block(50);
		assert_ok!(poke(7, 1, DOT, PUSD));

		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.pending_deposit.expect("still pending").amount, 400);
		assert!(pending_contains(DOT, PUSD, 1));
	});
}

#[test]
fn poke_without_row_or_branch_reverts() {
	build_and_execute(|| {
		assert_noop!(poke(7, 1, DOT, PUSD), Error::<Test>::BranchNotRegistered);
		register_branch(DOT, PUSD, default_branch_config());
		assert_noop!(poke(7, 1, DOT, PUSD), Error::<Test>::DepositNotFound);
	});
}
