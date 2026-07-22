//! Realization-on-touch: every value-moving operation settles gains into the
//! row's `claimable_*` fields — it never pays them to the wallet, and
//! never folds yield back into the active deposit. Gains accumulate
//! across successive touches and a single claim pays the total.
//!
//! Offsets here price collateral at the 1.25 registration price: cancelling
//! debt `D` seizes `D / 1.25 = 0.8 * D` collateral.

use crate::{mock::*, Error};

/// Deposit-and-activate `amount` for `who`, minting exactly `amount`.
fn seed_active(who: AccountId, amount: Balance) {
	seed_deposit(who, amount);
	activate_all(&[who]);
}

/// Mint `headroom` stablecoin for `who` and deposit `amount`, leaving the rest
/// in the wallet so later top-ups have funds; then activate.
fn seed_active_with_headroom(who: AccountId, amount: Balance, headroom: Balance) {
	mint_stable(PUSD, who, headroom);
	assert_ok!(deposit(who, DOT, PUSD, amount));
	activate_all(&[who]);
}

#[test]
fn top_up_realizes_offset_gain_into_claimable_not_wallet() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active_with_headroom(1, 1_000, 2_000);

		// Offset halves the pool: P = 500/1000 = 0.5, delta_S = C_sp * P/A =
		// 400 * (1/1000) = 0.4. The gain stays latent — offsets never touch
		// the row.
		assert_eq!(simulate_offset(DOT, PUSD, 500, 400).0, 500);
		assert_eq!(deposit_row(DOT, PUSD, 1).expect("row exists").claimable_collateral, 0);

		// A top-up is the first touch: it realizes the offset gain (§6.2)
		// gain = (D0/P0) * delta_S = (1000/1) * 0.4 = 400 into
		// `claimable_collateral` and compounds the deposit to
		// (D0/P0) * P = 1000 * 0.5 = 500 — but sends nothing to the wallet.
		let coll_before = collateral_balance(DOT, 1);
		assert_ok!(deposit(1, DOT, PUSD, 200));

		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.active_deposit, 500);
		assert_eq!(row.claimable_collateral, 400);
		assert_eq!(row.pending_deposit.expect("top-up queued").amount, 200);
		// No collateral moved to the wallet; only the 200 top-up left it.
		assert_eq!(collateral_balance(DOT, 1), coll_before);
		assert_eq!(stable_balance(PUSD, 1), 800);

		// The claim is what pays the realized gain out (against the same
		// pre-top-up balance, unchanged above).
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - coll_before, 400);
	});
}

#[test]
fn sequential_offsets_accumulate_claimable_across_touches() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		// Two depositors, so distributed gains actually split: only user 1
		// is touched below; user 2's share stays latent until the end.
		seed_active_with_headroom(1, 1_000, 2_000);
		seed_active(2, 1_000);

		// Offset 1 over A = 2000: P = 1 -> 0.5, delta_S = 800 * (1/2000) = 0.4.
		assert_eq!(simulate_offset(DOT, PUSD, 1_000, 800).0, 1_000);
		// A top-up touch realizes gain-1 = (1000/1) * 0.4 = 400 into
		// claimable and sets the snapshot to P = 0.5, S = 0.4.
		assert_ok!(deposit(1, DOT, PUSD, 400));
		assert_eq!(deposit_row(DOT, PUSD, 1).expect("row").claimable_collateral, 400);

		// Offset 2 over A = 1000: P = 0.5 -> 0.25,
		// delta_S = 400 * (0.5/1000) = 0.2, so S = 0.6.
		assert_eq!(simulate_offset(DOT, PUSD, 500, 400).0, 500);

		// A withdrawal touch realizes gain-2 = (500/0.5) * (0.6 - 0.4) = 200
		// against the reset snapshot and accumulates it: 400 + 200 = 600.
		// The withdrawal itself takes 100 off the compounded
		// (500/0.5) * 0.25 = 250.
		assert_ok!(withdraw(1, DOT, PUSD, 100, 1));
		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.claimable_collateral, 600);
		assert_eq!(row.active_deposit, 150);

		// One claim pays the whole accumulated gain.
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 600);
		assert_eq!(deposit_row(DOT, PUSD, 1).expect("row").claimable_collateral, 0);

		// User 2, untouched the whole time, realizes its complement:
		// gain = (1000/1) * (0.6 - 0) = 600 — together the full 800 + 400.
		assert_ok!(poke(7, 2, DOT, PUSD));
		let row = deposit_row(DOT, PUSD, 2).expect("row survives");
		assert_eq!(row.claimable_collateral, 600);
		assert_eq!(row.active_deposit, 250);
	});
}

#[test]
fn withdraw_realizes_yield_into_claimable_never_compounds() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 600);

		// delta_G = Y * (P/A) = 60 * (1/600) = 0.1.
		drop(distribute_yield(DOT, PUSD, 60));

		// The withdrawal realizes yield = (600/1) * 0.1 = 60 into
		// `claimable_yield`; the active deposit is only reduced by the 100
		// withdrawn — the 60 is NOT added to it.
		assert_ok!(withdraw(1, DOT, PUSD, 100, 1));
		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.claimable_yield, 60);
		assert_eq!(row.active_deposit, 500);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 500);
		// Only the withdrawn 100 reached the wallet.
		assert_eq!(stable_balance(PUSD, 1), 100);

		// The yield is paid out separately, on its own claim.
		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		assert_eq!(stable_balance(PUSD, 1), 160);
	});
}

#[test]
fn claim_after_full_withdrawal_pays_earned_gains() {
	// A fully withdrawn row survives solely to carry its realized gains, and a later
	// claim pays them and prunes the row. Distinct from
	// `final_claim_prunes_an_otherwise_empty_row`, which seeds the claimable
	// directly, here it is earned through an offset.
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 1_000);

		// Offset: P = 500/1000 = 0.5, delta_S = 400 * (1/1000) = 0.4;
		// gain = (D0/P0) * delta_S = (1000/1) * 0.4 = 400.
		assert_eq!(simulate_offset(DOT, PUSD, 500, 400).0, 500);

		// A full withdrawal realizes the 400 gain and drains the compounded
		// 500 active to zero. The row stays alive on the claimable alone.
		assert_ok!(withdraw(1, DOT, PUSD, 10_000, 1));
		let row = deposit_row(DOT, PUSD, 1).expect("row kept by its claimable");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.claimable_collateral, 400);
		// With nothing active, a further withdrawal has nothing to take.
		assert_noop!(withdraw(1, DOT, PUSD, 1, 1), Error::<Test>::NoActiveDeposit);

		// The claim pays the earned gain and prunes the now-empty row.
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 400);
		assert!(deposit_row(DOT, PUSD, 1).is_none());
		assert_eq!(pool_state(DOT, PUSD).total_collateral_gains_unclaimed, 0);
	});
}

#[test]
fn permissionless_poke_realizes_gain_into_owner_claimable() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 1_000);

		// P = 0.5, delta_S = 400 * (1/1000) = 0.4.
		assert_eq!(simulate_offset(DOT, PUSD, 500, 400).0, 500);

		// A third party pokes: realization is permissionless, so gain =
		// (1000/1) * 0.4 = 400 lands in the owner's claimable and the
		// deposit compounds to 500 — without the owner lifting a finger.
		assert_ok!(poke(7, 1, DOT, PUSD));
		let row = deposit_row(DOT, PUSD, 1).expect("row survives");
		assert_eq!(row.claimable_collateral, 400);
		assert_eq!(row.active_deposit, 500);

		// The owner still owns the payout: the poker has no row to claim from.
		assert_noop!(claim_collateral(7, DOT, PUSD, 7), Error::<Test>::DepositNotFound);
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 400);
	});
}
