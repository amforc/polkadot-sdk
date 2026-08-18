//! Long sequences that drive many operations and then drain the pool to zero.
//!
//! Each one checks that every depositor is paid its exact share and that nothing is left
//! behind.

use crate::mock::*;

/// Deposit-and-activate `amount` for `who`, minting exactly `amount`.
fn seed_active(who: AccountId, amount: Balance) {
	seed_deposit(who, amount);
	activate_all(&[who]);
}

/// Claim `who`'s collateral to itself and assert the exact payout.
fn assert_claim_collateral(who: AccountId, expected: Balance) {
	let before = collateral_balance(DOT, who);
	assert_ok!(claim_collateral(who, DOT, PUSD, who));
	assert_eq!(collateral_balance(DOT, who) - before, expected);
}

/// The pool account holds nothing and the aggregates are all zero.
fn assert_pool_fully_drained() {
	let pool = Stability::pool_account(&DOT, &PUSD);
	assert_eq!(stable_balance(PUSD, pool), 0);
	assert_eq!(collateral_balance(DOT, pool), 0);
	let state = pool_state(DOT, PUSD);
	assert_eq!(state.total_active_deposits, 0);
	assert_eq!(state.total_pending_deposits, 0);
	assert_eq!(state.total_collateral_gains_unclaimed, 0);
	assert_eq!(state.total_yield_unclaimed, 0);
}

/// Three depositor cohorts across two yields and two offsets, then every one
/// of them realized and the pool drained to zero.
#[test]
fn multiple_depositor_cohorts_reconcile_to_zero() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 1_000);
		seed_active(2, 500);

		// Yield 1 over A = 1500: G = 150/1500 = 0.1.
		drop(distribute_yield(DOT, PUSD, 150));
		// Offset 1: A = 1500 → 900, P = 0.6, delta_S = floor(300 * 1e18 /
		// 1500) = 2e17, so S = 0.2.
		assert_eq!(simulate_offset(DOT, PUSD, 600, 300).0, 600);
		assert_eq!(pool_state(DOT, PUSD).coords.p, FixedU128::from_rational(3, 5));

		// A third depositor joins at P = 0.6, S = 0.2, G = 0.1: A = 900 + 900.
		seed_active(3, 900);

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

#[test]
fn full_depletion_epoch_boundary_stays_solvent() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 1_000);

		// Yield 1: G(0,0) = 100/1000 = 0.1.
		drop(distribute_yield(DOT, PUSD, 100));
		// A full offset bumps the epoch: delta_S = floor(800 * 1e18 / 1000) =
		// 8e17 on the epoch-0 row, then coords reset to (epoch 1, P = 1).
		assert_eq!(simulate_offset(DOT, PUSD, 1_000, 800).0, 1_000);
		assert_eq!(pool_state(DOT, PUSD).coords.epoch, 1);

		// A fresh epoch-1 depositor is untouched by epoch-0 history.
		seed_active(2, 1_000);
		// Yield 2 on epoch 1: G(1,0) = 50/1000 = 0.05.
		drop(distribute_yield(DOT, PUSD, 50));
		// Offset on epoch 1: A = 1000 → 600, P = 0.6, delta_S =
		// floor(200 * 1e18 / 1000) = 2e17, so S(1,0) = 0.2.
		assert_eq!(simulate_offset(DOT, PUSD, 400, 200).0, 400);

		// user1 (epoch 0): compounded is zero (an epoch behind), but its
		// epoch-0 gains stay claimable: collateral floor(1000 * 0.8) = 800,
		// yield floor(1000 * 0.1) = 100.
		assert_claim_collateral(1, 800);
		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		assert_eq!(stable_balance(PUSD, 1), 100);
		assert!(deposit_row(DOT, PUSD, 1).is_none());

		// user2 (epoch 1): collateral floor(1000 * 0.2) = 200, yield
		// floor(1000 * 0.05) = 50, compounded floor(1000 * 0.6) = 600.
		assert_claim_collateral(2, 200);
		assert_ok!(claim_yield(2, DOT, PUSD, 2));
		assert_ok!(withdraw(2, DOT, PUSD, 10_000, 2));
		assert_eq!(stable_balance(PUSD, 2), 650);

		// Collateral 800+200 = 1000, yield 100+50 = 150. Nothing stranded
		// across the epoch boundary.
		assert_pool_fully_drained();
	});
}
