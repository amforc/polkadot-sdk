use crate::{mock::*, tests::percent};

fn seed_long_rate_index() {
	for who in 1..=20 {
		mint_collateral(DOT, who, 1_000_000_000_000);
		assert_ok!(open(who, DOT, STABLE, 2_000, 500, FixedU128::from_rational(20 + who, 100)));
	}
}

// Every hint-taking entry point rolls back whole when its rate-index insertion needs more than the
// repair budget: a FinalRecovery exit leaves the vault in the FIFO, a failed open leaves no hold,
// mint or row and reports no weight refund, and a failed rate change leaves the old rate in place.
#[test]
fn hints_past_the_repair_budget_roll_back() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// Account 21 alone enters FinalRecovery after a price crash.
		mint_collateral(DOT, 21, 1_000_000_000_000);
		assert_ok!(open(21, DOT, STABLE, 1_000, 500, percent(1)));
		set_price(DOT, FixedU128::from_rational(1u128, 10u128));
		assert_ok!(enter_final_recovery(99, DOT, STABLE, 21));

		// Restore the price and seed a long index so a tail insertion at 1% needs more than the
		// repair budget.
		set_price(DOT, DEFAULT_PRICE);
		seed_long_rate_index();

		assert_noop!(
			exit_final_recovery(99, DOT, STABLE, 21),
			crate::Error::<Test>::InvalidPositionHints
		);

		mint_collateral(DOT, 22, 1_000_000_000_000);
		assert_noop!(
			crate::Pallet::<Test>::open_vault(
				RuntimeOrigin::signed(22),
				DOT,
				STABLE,
				1_000,
				500,
				percent(1),
				Position::endpoints_only()
			),
			crate::Error::<Test>::InvalidPositionHints
		);

		assert_noop!(
			change_rate(20, DOT, STABLE, percent(1)),
			crate::Error::<Test>::InvalidPositionHints
		);
	});
}
