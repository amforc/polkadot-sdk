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
			)
			.map_err(|error| {
				assert_eq!(error.post_info.actual_weight, None);
				error.error
			}),
			crate::Error::<Test>::InvalidPositionHints
		);

		assert_noop!(
			change_rate(20, DOT, STABLE, percent(1)),
			crate::Error::<Test>::InvalidPositionHints
		);
	});
}

/// Opens vault 21 with a hint `steps` positions above its slot and checks that the dispatch
/// pre-charges the full repair budget and refunds the steps it did not walk.
fn assert_open_vault_refunds_unused_steps(steps: u32) {
	use crate::WeightInfo;
	use frame::prelude::GetDispatchInfo;
	use linked_list_interface::SortedListInterface;

	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		seed_long_rate_index();
		mint_collateral(DOT, 21, 1_000_000_000_000);
		let hint = Position::between(20 - AccountId::from(steps), 19 - AccountId::from(steps));
		let rate = FixedU128::from_rational(395, 1000);
		let call = crate::Call::<Test>::open_vault {
			collateral_id: DOT,
			stable_id: STABLE,
			initial_collateral: 1_000,
			initial_debt: 500,
			annual_rate: rate,
			hint: hint.clone(),
		};
		assert_eq!(
			call.get_dispatch_info().call_weight,
			<() as WeightInfo>::open_vault(MaxHintRepairSteps::get()),
		);

		let post = crate::Pallet::<Test>::open_vault(
			RuntimeOrigin::signed(21),
			DOT,
			STABLE,
			1_000,
			500,
			rate,
			hint,
		)
		.expect("valid hint within the repair budget");

		assert_eq!(post.actual_weight, Some(<() as WeightInfo>::open_vault(steps)));
		assert_eq!(
			LinkedList::neighbors(&VaultList::Rate(DOT, STABLE), &21),
			Some(Position::between(20, 19)),
		);
	});
}

#[test]
fn open_vault_with_exact_hint_refunds_the_whole_repair_budget() {
	assert_open_vault_refunds_unused_steps(0);
}

#[test]
fn open_vault_refunds_all_but_the_one_repair_step_walked() {
	assert_open_vault_refunds_unused_steps(1);
}

#[test]
fn open_vault_using_the_whole_repair_budget_refunds_nothing() {
	assert_open_vault_refunds_unused_steps(MaxHintRepairSteps::get());
}
