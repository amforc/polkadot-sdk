//! Worked examples with the numbers spelled out.
//!
//! Each test states the arithmetic it expects before it runs, so a reader can check the pallet
//! against the model by hand rather than by trusting the assertions.

use crate::{mock::*, types::Leg, Error};

/// These vaults sit at CR 120%, above the default 110% MCR, so
/// `FinalRecovery` would never admit them. This market's MCR is 130%; vaults
/// open at a higher price and the price is dropped to 1 DOT = 2 pUSD.
fn example_branch_config() -> pallet_vaults::BranchConfig<Balance> {
	let mut config = default_branch_config();
	config.minimum_collateralization_ratio = FixedU128::from_rational(130u128, 100u128);
	config.initial_collateralization_ratio = FixedU128::from_rational(140u128, 100u128);
	config.safety_collateralization_ratio = FixedU128::from_rational(150u128, 100u128);
	config.minimum_debt = 100;
	// No upfront fee, so drawn principal is the debt.
	config.upfront_fee_period = 0;
	// Caps the recovery bonus at 10%.
	config.redistribution_penalty = Permill::from_percent(10);
	config
}

/// Open `owner`'s vault at a price that clears the 140% ICR, then drop to
/// 1 DOT = 2 pUSD.
fn open_at_120_percent(owner: AccountId, collateral: Balance, debt: Balance) {
	set_price(DOT, FixedU128::from_rational(4, 1));
	mint_collateral(DOT, owner, collateral * 2);
	assert_ok!(open_vault(owner, DOT, PUSD, collateral, debt));
	assert_eq!(vault_debt(DOT, PUSD, owner), debt);
	set_price(DOT, FixedU128::from_rational(2, 1));
}

/// Deposit `amount` for `who` and fold it into the active pool.
fn seed_active(who: AccountId, amount: Balance) {
	seed_deposit(who, amount);
	activate_all(&[who]);
}

/// An incoming deposit meets a `FinalRecovery` head at CR >= 100%, so it is
/// spent on the recovery instead of queueing, and the depositor is paid in
/// collateral rather than a pool position.
#[test]
fn incoming_deposit_is_spent_on_a_recovery_head_above_par() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, example_branch_config());
		// 5_000 of debt against 3_000 DOT = 6_000 pUSD, a 120% CR.
		open_at_120_percent(5, 3_000, 5_000);
		assert_ok!(enter_final_recovery(DOT, PUSD, 5));

		mint_stable(PUSD, 1, 1_000);
		assert_ok!(deposit(1, DOT, PUSD, 1_000));

		// bonus = min(120% - 100% - 1%, 10%) = 10%, so the whole 1_000 is used
		// and pays 1_000 * 1.10 / 2 = 550 DOT.
		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.claimable_collateral, 550);
		assert!(row.pending_deposit.is_none());
		assert_eq!(row.active_deposit, 0);

		// P, S and G are untouched: this collateral never went through the
		// active pool's accumulators.
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.coords.p, FixedU128::one());
		assert_eq!(state.total_active_deposits, 0);
		let sums = crate::PoolSumsStore::<Test>::get((DOT, PUSD, Leg::Active, 0u32, 0u32));
		assert_eq!(sums.s_collateral, FixedU128::zero());
		assert_eq!(sums.g_yield, FixedU128::zero());

		// The vault settled 1_000 of its 5_000.
		assert_eq!(vault_debt(DOT, PUSD, 5), 4_000);
	});
}

/// The same deposit against a head below par is refused outright — no burn,
/// no pending position.
#[test]
fn incoming_deposit_is_rejected_against_a_head_below_par() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, example_branch_config());
		// 5_000 of debt against 2_000 DOT = 4_000 pUSD, an 80% CR.
		open_at_120_percent(5, 2_000, 2_000);
		// Draw the CR down to 80% by dropping the price.
		set_price(DOT, FixedU128::from_rational(8, 10));
		assert_ok!(enter_final_recovery(DOT, PUSD, 5));

		mint_stable(PUSD, 1, 1_000);
		assert_noop!(deposit(1, DOT, PUSD, 1_000), Error::<Test>::RecoveryOffsetBelowPar);

		assert_eq!(stable_balance(PUSD, 1), 1_000);
		assert!(deposit_row(DOT, PUSD, 1).is_none());
		assert_eq!(vault_debt(DOT, PUSD, 5), 2_000);
	});
}

/// A recovery offset served by the active pool, and the realization it
/// implies for a depositor holding a tenth of that pool.
#[test]
fn active_pool_recovery_offset_and_realization() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, example_branch_config());
		open_at_120_percent(5, 3_000, 5_000);

		// 10_000 of active deposits, of which this follows the 1_000 leg.
		seed_active(1, 1_000);
		seed_active(2, 9_000);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 10_000);
		assert_ok!(enter_final_recovery(DOT, PUSD, 5));

		assert_ok!(offset_recovery(DOT, PUSD, 2_000));

		// collateral_gain = 2_000 * 1.10 / 2 = 1_100 DOT. The pool drops to
		// 8_000 at P = 0.8, and S rises by 1_100 * 1.0 / 10_000 = 0.11.
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 8_000);
		assert_eq!(state.coords.p, FixedU128::from_rational(8, 10));
		let sums = crate::PoolSumsStore::<Test>::get((DOT, PUSD, Leg::Active, 0u32, 0u32));
		assert_eq!(sums.s_collateral, FixedU128::from_rational(11, 100));

		// The 1_000 depositor: compounded 1_000 * 0.8 = 800, collateral
		// 1_000 * 0.11 = 110.
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 110);
		assert_eq!(deposit_row(DOT, PUSD, 1).expect("row survives").active_deposit, 800);
	});
}

/// A liquidation offset, then yield routed over the shrunken pool, and both
/// depositors realizing all three quantities.
#[test]
fn offset_then_yield_then_realization() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 1_000);
		seed_active(2, 9_000);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 10_000);

		// Offset 2_000 of debt for 1_200 DOT: S rises by 1_200/10_000 = 0.12,
		// the pool drops to 8_000 and P to 0.8.
		assert_eq!(simulate_offset(DOT, PUSD, 2_000, 1_200).0, 2_000);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 8_000);
		assert_eq!(state.coords.p, FixedU128::from_rational(8, 10));

		// Yield 400 over A = 8_000 at P = 0.8: G rises by 400 * 0.8 / 8_000 = 0.04.
		drop(distribute_yield(DOT, PUSD, 400));
		let sums = crate::PoolSumsStore::<Test>::get((DOT, PUSD, Leg::Active, 0u32, 0u32));
		assert_eq!(sums.s_collateral, FixedU128::from_rational(12, 100));
		assert_eq!(sums.g_yield, FixedU128::from_rational(4, 100));

		// The 1_000 depositor: compounded 800, collateral 1_000 * 0.12 = 120,
		// yield 1_000 * 0.04 = 40.
		let before = collateral_balance(DOT, 1);
		assert_ok!(claim_collateral(1, DOT, PUSD, 1));
		assert_eq!(collateral_balance(DOT, 1) - before, 120);
		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		assert_eq!(stable_balance(PUSD, 1), 40);
		assert_eq!(deposit_row(DOT, PUSD, 1).expect("row survives").active_deposit, 800);

		// The 9_000 depositor moves with it: compounded 9_000 * 0.8 = 7_200,
		// collateral 9_000 * 0.12 = 1_080, yield 9_000 * 0.04 = 360.
		let before = collateral_balance(DOT, 2);
		assert_ok!(claim_collateral(2, DOT, PUSD, 2));
		assert_eq!(collateral_balance(DOT, 2) - before, 1_080);
		assert_ok!(claim_yield(2, DOT, PUSD, 2));
		assert_eq!(stable_balance(PUSD, 2), 360);
		assert_eq!(deposit_row(DOT, PUSD, 2).expect("row survives").active_deposit, 7_200);
	});
}

/// A full depletion at a compounded `P`: the epoch closes and the
/// accumulators reset, while the closing epoch's collateral stays claimable.
///
/// `P = 0.42` is the interesting starting point, and it is reached here with
/// two collateral-free offsets. The epoch and scale indices are labels — the
/// arithmetic depends only on `P` and the pool total — so this asserts the
/// epoch increments rather than that it reaches any particular value.
#[test]
fn full_depletion_and_epoch_transition() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 1_000);

		// Two offsets that take no collateral, purely to compound P: 1_000 →
		// 600 gives P = 0.6, then 600 → 420 gives P = 0.6 * 0.7 = 0.42.
		assert_eq!(simulate_offset(DOT, PUSD, 400, 0).0, 400);
		assert_eq!(simulate_offset(DOT, PUSD, 180, 0).0, 180);
		assert_eq!(pool_state(DOT, PUSD).coords.p, FixedU128::from_rational(42, 100));

		// The depositor this follows joins at P = 0.42 with 600, and a third
		// tops the pool up to 1_500.
		seed_active(2, 600);
		seed_active(3, 480);
		assert_eq!(pool_state(DOT, PUSD).total_active_deposits, 1_500);
		let epoch_before = pool_state(DOT, PUSD).coords.epoch;

		// Deplete: S rises by 900 * 0.42 / 1_500 = 0.252 on the closing epoch,
		// which is where the gains stay claimable from.
		assert_eq!(simulate_offset(DOT, PUSD, 1_500, 900).0, 1_500);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.coords.epoch, epoch_before + 1);
		assert_eq!(state.coords.scale, 0);
		assert_eq!(state.coords.p, FixedU128::one());
		let closing =
			crate::PoolSumsStore::<Test>::get((DOT, PUSD, Leg::Active, epoch_before, 0u32));
		assert_eq!(closing.s_collateral, FixedU128::from_rational(252, 1_000));
		// The new epoch starts from zeroed sums.
		let opening =
			crate::PoolSumsStore::<Test>::get((DOT, PUSD, Leg::Active, epoch_before + 1, 0u32));
		assert_eq!(opening.s_collateral, FixedU128::zero());

		// The 600 depositor: compounded is zero an epoch behind, but its
		// collateral 600 * 0.252 / 0.42 = 360 stays claimable.
		let before = collateral_balance(DOT, 2);
		assert_ok!(claim_collateral(2, DOT, PUSD, 2));
		assert_eq!(collateral_balance(DOT, 2) - before, 360);
	});
}
