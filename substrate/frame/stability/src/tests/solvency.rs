//! Long sequences that drive many operations and then drain the pool to zero.
//!
//! Each one checks that every depositor is paid its exact share and that nothing is left
//! behind.

use crate::mock::*;

/// Three depositor cohorts across two yields and two offsets, then every one
/// of them realized and the pool drained to zero.
#[test]
fn multiple_depositor_cohorts_reconcile_to_zero() {
	build_with_default_market(|| {
		seed_matured_deposit(1, 1_000);
		seed_matured_deposit(2, 500);

		// Yield 1 over A = 1500: G = 150/1500 = 0.1.
		drop(distribute_yield(DOT, PUSD, 150));
		// Offset 1: A = 1500 → 900, P = 0.6, delta_S = floor(300 * 1e18 /
		// 1500) = 2e17, so S = 0.2.
		assert_eq!(simulate_offset(DOT, PUSD, 600, 300).0, 600);
		assert_eq!(pool_state(DOT, PUSD).coords.p, FixedU128::from_rational(3, 5));

		// A third depositor joins at P = 0.6, S = 0.2, G = 0.1: A = 900 + 900.
		seed_matured_deposit(3, 900);

		// Yield 2 over A = 1800: delta_G = floor(180 * 0.6 / 1800) = 6e16,
		// so G = 0.16.
		drop(distribute_yield(DOT, PUSD, 180));
		// Offset 2: A = 1800 → 900, P = 0.6 → floor(0.6 * 900 / 1800) = 0.3,
		// delta_S = floor(450 * 0.6 / 1800) = 1.5e17, so S = 0.35.
		assert_eq!(simulate_offset(DOT, PUSD, 900, 450).0, 900);
		assert_eq!(pool_state(DOT, PUSD).coords.p, FixedU128::from_rational(3, 10));

		// Collateral: user1 floor(1000 * 0.35) = 350, user2 floor(500 * 0.35)
		// = 175, user3 floor(900 * (0.35 - 0.2) / 0.6) = 225.
		assert_claim_collateral(1, 350);
		assert_claim_collateral(2, 175);
		assert_claim_collateral(3, 225);

		// Yield: user1 floor(1000 * 0.16) = 160, user2 floor(500 * 0.16) = 80,
		// user3 floor(900 * (0.16 - 0.1) / 0.6) = 90.
		// Compounded: floor(1000 * 0.3) = 300, floor(500 * 0.3) = 150,
		// floor(900 * 0.3 / 0.6) = 450.
		for (who, yield_gain, compounded) in [(1, 160, 300), (2, 80, 150), (3, 90, 450)] {
			assert_ok!(claim_yield(who, DOT, PUSD, who));
			assert_ok!(withdraw(who, DOT, PUSD, 10_000, who));
			assert_eq!(stable_balance(PUSD, who), yield_gain + compounded);
			assert!(deposit_row(DOT, PUSD, who).is_none());
		}

		// The reconciliation: collateral 350+175+225 = 750 (= 300+450),
		// yield 160+80+90 = 330 (= 150+180), compounded 300+150+450 = 900. The
		// pool holds nothing.
		assert_pool_fully_drained();
	});
}
