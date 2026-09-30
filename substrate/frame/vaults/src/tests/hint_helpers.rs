use crate::{
	mock::*,
	tests::{rate_pct, vault_status},
};
use frame::traits::fungible::Mutate as FungibleMutate;

fn fund_account(who: AccountId) {
	assert_ok!(<Balances as FungibleMutate<AccountId>>::mint_into(&who, 1_000_000_000_000));
}

fn seed_long_rate_index() {
	for who in 1u64..=20 {
		fund_account(who);
		assert_ok!(open(who, DOT, PUSD, 2_000, 500, rate_pct(20 + u128::from(who), 100)));
	}
}

// Exiting FinalRecovery back into the rate index with an unrepairable hint rolls
// the whole operation back: the vault stays in the FIFO and storage is unchanged.
#[test]
fn exit_final_recovery_invalid_hint_rolls_back() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Account 21 alone enters FinalRecovery after a price crash.
		fund_account(21);
		assert_ok!(open(21, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		set_price(DOT, FixedU128::from_rational(1u128, 10u128));
		assert_ok!(enter_final_recovery(99, DOT, PUSD, 21));

		// Restore the price and seed a long index so the tail re-insertion at 1%
		// needs more than the repair budget.
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		seed_long_rate_index();

		assert_noop!(
			exit_final_recovery(99, DOT, PUSD, 21),
			crate::Error::<Test>::InvalidPositionHints
		);
		assert!(vault_status(DOT, PUSD, 21).is_final_recovery());
		assert_eq!(
			LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, PUSD), 10),
			alloc::vec![21]
		);
	});
}

#[test]
fn open_vault_invalid_hint_rolls_back_hold_mint_and_storage() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		seed_long_rate_index();
		fund_account(21);

		assert_noop!(
			crate::Pallet::<Test>::open_vault(
				RuntimeOrigin::signed(21),
				DOT,
				PUSD,
				1_000,
				500,
				rate_pct(1, 100),
				Position::endpoints_only()
			),
			crate::Error::<Test>::InvalidPositionHints
		);

		assert!(!vault_exists(DOT, PUSD, 21));
		assert_eq!(held(DOT, 21), 0);
	});
}

#[test]
fn change_rate_invalid_hint_rolls_back_rate_fee_and_index() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		seed_long_rate_index();

		assert_noop!(
			change_rate(20, DOT, PUSD, rate_pct(1, 100)),
			crate::Error::<Test>::InvalidPositionHints
		);
	});
}
