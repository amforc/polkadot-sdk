//! Where gains go when a row is written.
//!
//! Every operation that moves value settles the gains of the row into its claimable fields. It
//! never pays them to a wallet, and it never folds yield back into the active deposit. Gains add
//! up across writes, and one claim pays the total.
//!
//! Offsets here price collateral at the registration price of 1.25, so cancelling debt `D` seizes
//! `0.8 * D` collateral.

use crate::mock::*;

#[test]
fn sequential_offsets_accumulate_claimable_across_touches() {
	// Two depositors, so distributed gains actually split: only user 1
	// is touched below; user 2's share stays latent until the end.
	ExtBuilder::default()
		.default_branch()
		.stable_balance(1, 1_000)
		.matured_deposit(1, 1_000)
		.matured_deposit(2, 1_000)
		.build_and_execute(|| {
			// Offset 1 over A = 2000: P = 1 -> 0.5, delta_S = 800 * (1/2000) = 0.4.
			assert_eq!(simulate_offset(DOT, STABLE, 1_000, 800).debt(), 1_000);
			// A top-up touch realizes gain-1 = (1000/1) * 0.4 = 400 into
			// claimable and sets the snapshot to P = 0.5, S = 0.4.
			let collateral_before = collateral_balance(DOT, 1);
			assert_ok!(deposit(1, DOT, STABLE, 400));

			assert_eq!(deposit_row(DOT, STABLE, 1).expect("row").claimable_collateral, 400);
			// Realized, not paid: nothing reached the wallet.
			assert_eq!(collateral_balance(DOT, 1), collateral_before);

			// Offset 2 over A = 1000: P = 0.5 -> 0.25,
			// delta_S = 400 * (0.5/1000) = 0.2, so S = 0.6.
			assert_eq!(simulate_offset(DOT, STABLE, 500, 400).debt(), 500);

			// A withdrawal touch realizes gain-2 = (500/0.5) * (0.6 - 0.4) = 200
			// against the reset snapshot and accumulates it: 400 + 200 = 600.
			// The withdrawal itself takes 100 off the remaining principal
			// (500/0.5) * 0.25 = 250.
			assert_ok!(withdraw(1, DOT, STABLE, 100, 1));

			let row = deposit_row(DOT, STABLE, 1).expect("row survives");
			assert_eq!(row.claimable_collateral, 600);
			assert_eq!(row.active_deposit, 150);

			// One claim pays the whole accumulated gain.
			assert_claim_collateral(1, 600);
			assert_eq!(deposit_row(DOT, STABLE, 1).expect("row").claimable_collateral, 0);

			// User 2, untouched the whole time, realizes its complement:
			// gain = (1000/1) * (0.6 - 0) = 600 — together the full 800 + 400.
			assert_ok!(settle(7, 2, DOT, STABLE));
			let row = deposit_row(DOT, STABLE, 2).expect("row survives");
			assert_eq!(row.claimable_collateral, 600);
			assert_eq!(row.active_deposit, 250);
		});
}
