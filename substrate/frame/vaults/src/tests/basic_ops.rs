use crate::{
	mock::*,
	tests::{assert_event, percent, vault_events, vault_status, ONE_DAY_MS, ONE_YEAR_MS},
};
use pallet_linked_list::SortedListInterface;

// Each vault operation that names a payer or a recipient must move funds between those accounts,
// not the owner's, while the vault row stays the owner's. Every event names the account that paid
// or received.
#[test]
fn third_party_and_recipient_routing() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 3_000, 500, percent(5)));
		assert_ok!(open(2, DOT, STABLE, 2_000, 500, percent(5)));

		// A third-party deposit is credited to the owner's vault.
		assert_ok!(deposit_collateral(2, DOT, STABLE, 1, 100));

		assert_eq!(held(DOT, 1), 3_100);
		assert_event(crate::Event::CollateralDeposited {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			from: 2,
			amount: 100,
		});

		let owner_stable_before = stable_balance(STABLE, 1);
		let recipient_stable_before = stable_balance(STABLE, 4);
		assert_ok!(crate::Pallet::<Test>::borrow(
			RuntimeOrigin::signed(1),
			DOT,
			STABLE,
			300,
			None,
			Some(4),
			Position::endpoints_only()
		));

		assert_eq!(stable_balance(STABLE, 1), owner_stable_before);
		assert_eq!(stable_balance(STABLE, 4), recipient_stable_before + 300);
		assert_eq!(vault(DOT, STABLE, 1).debt.principal, 800);
		assert_event(crate::Event::Borrowed {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			recipient: 4,
			amount: 300,
		});

		let recipient_collateral_before = collateral_balance(DOT, 4);
		assert_ok!(withdraw_collateral(1, DOT, STABLE, 250, Some(4)));

		assert_eq!(held(DOT, 1), 2_850);
		assert_eq!(collateral_balance(DOT, 4), recipient_collateral_before + 250);
		assert_event(crate::Event::CollateralWithdrawn {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			recipient: 4,
			amount: 250,
		});

		let payer_before = stable_balance(STABLE, 2);
		let debt_before = vault(DOT, STABLE, 1).debt.total();
		assert_ok!(repay(2, DOT, STABLE, 1, Some(100)));

		assert_eq!(stable_balance(STABLE, 2), payer_before - 100);
		assert_eq!(vault(DOT, STABLE, 1).debt.total(), debt_before - 100);
		assert_event(crate::Event::Repaid {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			from: 2,
			// The open and borrow fees of 1 each are interest, so they are paid before principal.
			amount: crate::DebtBreakdown { principal: 98, interest: 2 },
		});
	});
}

/// Opens vaults 1 and 2, moves `funding` of vault 2's stablecoin to its owner, and returns vault
/// 1's total debt.
fn open_and_fund_repayer(funding: Balance) -> Balance {
	assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
	assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(5)));
	assert_ok!(<Stable as frame::traits::fungible::Mutate<AccountId>>::transfer(
		&2,
		&1,
		funding,
		frame::traits::tokens::Preservation::Expendable,
	));
	// 500 principal plus the open fee ceil(500 × 5% × 7 / 365.25) = 1.
	let total = vault(DOT, STABLE, 1).debt.total();
	assert_eq!(total, 501);
	total
}

/// Asserts that a full repayment burned exactly `total` and parked vault 1 as a debt-free husk
/// that only an explicit close releases.
fn assert_husk_until_explicit_close(balance_before: Balance, total: Balance) {
	assert_eq!(stable_balance(STABLE, 1), balance_before - total);
	assert_eq!(vault(DOT, STABLE, 1).debt.total(), 0);
	assert_eq!(held(DOT, 1), 1_000);
	assert!(vault_status(DOT, STABLE, 1).is_dormant());
	assert!(!<LinkedList as SortedListInterface<VaultList, AccountId>>::contains(
		&VaultList::Rate(DOT, STABLE),
		&1,
	));
	// Repaying to zero parks the vault as a husk; it does not close it.
	assert_eq!(
		vault_events(),
		vec![
			crate::Event::Repaid {
				collateral_id: DOT,
				stable_id: STABLE,
				owner: 1,
				from: 1,
				amount: crate::DebtBreakdown { principal: 500, interest: 1 },
			},
			crate::Event::VaultStatusChanged {
				collateral_id: DOT,
				stable_id: STABLE,
				owner: 1,
				old_status: crate::types::VaultStatus::Active,
				new_status: crate::types::VaultStatus::Dormant,
			},
		]
	);

	// Nothing is left to repay, and a zero `Repaid` must not pretend otherwise.
	assert_noop!(repay(1, DOT, STABLE, 1, None), crate::Error::<Test>::NoDebt);
	assert_noop!(repay(1, DOT, STABLE, 1, Some(1)), crate::Error::<Test>::NoDebt);

	let collateral_before = collateral_balance(DOT, 1);
	assert_ok!(close_vault(1, DOT, STABLE, None));

	assert!(!vault_exists(DOT, STABLE, 1));
	assert_eq!(held(DOT, 1), 0);
	assert_eq!(collateral_balance(DOT, 1), collateral_before + 1_000 + VAULT_DEPOSIT);
	assert_event(crate::Event::VaultClosed {
		collateral_id: DOT,
		stable_id: STABLE,
		owner: 1,
		recipient: 1,
		collateral: 1_000,
	});
}

// Exact repayment leaves a debt-free Dormant row with collateral held. Owners can repay to zero to
// raise the branch's collateral ratio in Safety mode. Only an explicit close releases the
// collateral and refunds its deposit.
#[test]
fn exact_repayment_leaves_a_husk_until_explicit_close() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		let total = open_and_fund_repayer(1);
		let balance_before = stable_balance(STABLE, 1);

		System::reset_events();
		assert_ok!(repay(1, DOT, STABLE, 1, Some(total)));

		assert_husk_until_explicit_close(balance_before, total);
	});
}

// Excess repayment takes only the debt and leaves the same husk as an exact one.
#[test]
fn excess_repayment_leaves_a_husk_until_explicit_close() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		let total = open_and_fund_repayer(400);
		let balance_before = stable_balance(STABLE, 1);

		System::reset_events();
		assert_ok!(repay(1, DOT, STABLE, 1, Some(balance_before)));

		assert_husk_until_explicit_close(balance_before, total);
	});
}

// A sub-minimum Dormant residual cannot be partially repaid (any non-zero
// remainder below MinimumDebt is `DebtWouldBecomeDust`), so the owner must clear
// it to exactly zero. The overpay cap turns that from an exact-amount guessing
// game into "send at least the dust"; the cleared vault is left as a husk that
// keeps its collateral and frees the branch's `dormant_redemption_target` slot.
#[test]
fn repay_overpay_rescues_subminimum_dormant_vault() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		// Redeem acct 1 (the rate-index tail) down to exactly MinimumDebt - 1 (199),
		// the largest sub-minimum residual, so it parks in the Dormant slot.
		let debt = vault(DOT, STABLE, 1).debt.total();
		assert_ok!(redeem(DOT, STABLE, 3, debt - 199));
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		let residual = vault(DOT, STABLE, 1).debt.total();
		assert_eq!(residual, 199, "residual is MinimumDebt - 1");
		assert_eq!(
			market_state(DOT, STABLE).dormant_redemption_target,
			Some(1),
			"sub-minimum redemption parked acct 1 in the dormant slot"
		);
		// The redemption cancelled 501 - 199 = 302 of debt at the price of 10 and released
		// floor(30.2) = 30 of the 1_000 collateral.
		assert_eq!(held(DOT, 1), 970);

		// Paying 100 would leave 99, which is neither zero nor the minimum.
		assert_noop!(
			repay(1, DOT, STABLE, 1, Some(100)),
			crate::Error::<Test>::DebtWouldBecomeDust
		);

		let balance_before = stable_balance(STABLE, 1);
		assert_ok!(repay(1, DOT, STABLE, 1, Some(balance_before)));

		assert_eq!(
			stable_balance(STABLE, 1),
			balance_before - residual,
			"only the dust residual burned"
		);
		let husk = vault(DOT, STABLE, 1);
		assert_eq!(husk.debt.total(), 0, "sub-minimum dust cleared to zero");
		assert_eq!(held(DOT, 1), 970, "collateral untouched by repay");
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		assert_eq!(
			market_state(DOT, STABLE).dormant_redemption_target,
			None,
			"slot released on repay-to-zero"
		);
	});
}

// Husks normally go through the ratio check because closing one is a TCR-worsening action that
// safety mode disallows. Once the branch is liability-free there is no ratio left to protect,
// so the last husks close without it.
#[test]
fn liability_free_market_closes_husks_without_ratio_math() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 400, percent(7)));
		// Distinct timestamps create independent aggregate and vault residue.
		advance_time(30 * ONE_DAY_MS);
		assert_ok!(poke(9, DOT, STABLE, 1));
		advance_time(ONE_DAY_MS);
		assert_ok!(poke(9, DOT, STABLE, 2));
		// Top up both owners so overpay-repays can cover accrued interest.
		mint_stable(STABLE, 1, 100);
		mint_stable(STABLE, 2, 100);

		assert_ok!(repay(2, DOT, STABLE, 2, Some(10_000)));
		assert_ok!(repay(1, DOT, STABLE, 1, Some(10_000)));

		assert!(vault_status(DOT, STABLE, 2).is_dormant(), "vault 2 is a husk");
		assert!(vault_status(DOT, STABLE, 1).is_dormant(), "vault 1 is a husk");

		// A maximal price proves that both closes bypass ratio math, not only the last close.
		set_price(DOT, FixedU128::from_inner(u128::MAX));
		assert_ok!(close_vault(2, DOT, STABLE, None));
		assert_ok!(close_vault(1, DOT, STABLE, None));

		assert!(!vault_exists(DOT, STABLE, 1), "last husk closed");

		let state = market_state(DOT, STABLE);
		assert_eq!(state.debt.principal, 0);
		assert_eq!(state.stakes.total, 0);
		assert_eq!(state.debt.minted_interest, 0);
		assert_eq!(state.debt.pending_interest_attribution, 0);
		assert_eq!(state.debt.aggregate_interest_remainder, 0);
	});
}

/// Pokes vault 1 `pokes` times, one millisecond apart, then repays it in full. Only the first poke
/// rounds up, and the repayment forfeits the prepaid fraction.
fn assert_pokes_charge_once_and_repayment_forfeits_prepaid(pokes: u32) {
	assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(10)));
	mint_stable(STABLE, 1, 10);
	let opened = vault(DOT, STABLE, 1).debt.total();

	for _ in 0..pokes {
		advance_time(1);
		assert_ok!(poke(2, DOT, STABLE, 1));
	}

	let owed = vault(DOT, STABLE, 1).debt.total();
	assert_eq!(owed, opened + 1, "only the first poke rounds up");
	assert!(vault(DOT, STABLE, 1).interest_prepaid > 0);

	let balance_before = stable_balance(STABLE, 1);
	assert_ok!(repay(1, DOT, STABLE, 1, None));

	assert_eq!(vault(DOT, STABLE, 1).debt.total(), 0);
	assert_eq!(vault(DOT, STABLE, 1).interest_prepaid, 0);
	assert_eq!(stable_balance(STABLE, 1), balance_before - owed);
}

// A touch charges one rounded unit, and repayment forfeits the prepaid fraction.
#[test]
fn single_poke_charges_one_unit_and_full_repayment_forfeits_prepaid_interest() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_pokes_charge_once_and_repayment_forfeits_prepaid(1);
	});
}

// Frequent touches still charge one rounded unit in total: the prepaid fraction covers the later
// ones.
#[test]
fn frequent_pokes_charge_once_and_full_repayment_forfeits_prepaid_interest() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_pokes_charge_once_and_repayment_forfeits_prepaid(100);
	});
}

// Interest a touch mints ahead of the aggregate must not be minted again when the aggregate
// accrues it, or the branch keeps an attribution no vault owes and cannot close.
#[test]
fn aggregate_accrual_does_not_remint_prepaid_interest() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// 500 at 10% accrues 50 a year: half a unit every hundredth of a year.

		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(10)));
		mint_stable(STABLE, 1, 10);
		let opened = vault(DOT, STABLE, 1).debt.total();

		advance_time(ONE_YEAR_MS / 100);
		assert_ok!(poke(2, DOT, STABLE, 1));

		assert_eq!(vault(DOT, STABLE, 1).debt.total(), opened + 1, "half a unit rounds up");

		advance_time(ONE_YEAR_MS / 100);
		assert_ok!(poke(2, DOT, STABLE, 1));

		assert_eq!(vault(DOT, STABLE, 1).debt.total(), opened + 1, "the prepaid half covers it");
		let state = market_state(DOT, STABLE);
		assert_eq!(state.debt.pending_interest_attribution, 0, "no second mint");
		assert_eq!(state.debt.minted_interest, vault(DOT, STABLE, 1).debt.interest);

		assert_ok!(repay(1, DOT, STABLE, 1, None));
		assert_ok!(close_vault(1, DOT, STABLE, None));

		let state = market_state(DOT, STABLE);
		assert_eq!(state.debt.minted_interest, 0);
		assert_eq!(state.debt.pending_interest_attribution, 0);
		assert!(state.is_removable());
	});
}

// Stability cohorts activate without touching vaults, so a branch refresh alone must issue all the
// yield accrued so far. Two vaults of 500 at 0.1% each accrue half a unit a year, which whole-unit
// accrual rates would drop entirely.
#[test]
fn branch_refresh_issues_sub_unit_rates_without_vault_touches() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, FixedU128::from_rational(1, 1_000)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, FixedU128::from_rational(1, 1_000)));
		let minted_before = market_state(DOT, STABLE).debt.minted_interest;

		advance_time(ONE_YEAR_MS);
		assert_ok!(refresh_branch(9, DOT, STABLE));

		let state = market_state(DOT, STABLE);
		assert_eq!(state.debt.minted_interest - minted_before, 1);
		assert_eq!(state.debt.pending_interest_attribution, 1);
	});
}

// A liability-free close can discard only unowned rounding residue. These forged states verify
// that issued or attributed interest still prevents removal.
#[test]
fn liability_free_close_fails_closed_on_unattributed_liability() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(10)));
		mint_stable(STABLE, 1, 10);
		assert_ok!(repay(1, DOT, STABLE, 1, Some(1_000)));
		// Isolate each residue so that each guard has independent coverage.

		// Minted interest prevents branch removal.
		mutate_branch_state(DOT, STABLE, |state| state.debt.minted_interest = 1);
		assert_noop!(close_vault(1, DOT, STABLE, None), DispatchError::Corruption);
		mutate_branch_state(DOT, STABLE, |state| state.debt.minted_interest = 0);

		// Unattributed issuance prevents branch removal independently.
		mutate_branch_state(DOT, STABLE, |state| state.debt.pending_interest_attribution = 1);
		assert_noop!(close_vault(1, DOT, STABLE, None), DispatchError::Corruption);
		mutate_branch_state(DOT, STABLE, |state| state.debt.pending_interest_attribution = 0);

		assert_ok!(close_vault(1, DOT, STABLE, None));
	});
}

#[test]
fn redemption_slot_rejects_second_owner() {
	fn park(owner: AccountId) -> DispatchResult {
		let snapshot =
			<crate::Pallet<Test> as stablecoin_primitives::VaultInterface>::project_redemption_snapshot(
				&DOT, &STABLE, &owner,
			)?;
		redeem_step(DOT, STABLE, owner, 7, snapshot.debt - 150, (snapshot.debt - 150) / 10)
	}
	fn parked() -> Option<AccountId> {
		market_state(DOT, STABLE).dormant_redemption_target
	}
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		assert_ok!(open(3, DOT, STABLE, 1_000, 500, percent(3)));

		assert_ok!(park(1));

		assert_eq!(parked(), Some(1));

		assert_noop!(park(2), crate::Error::<Test>::DormantTargetOccupied);
	});
}

// Full repayments must empty the branch's interest ledger whatever order they settle in. Each vault
// rounds its own interest up, so together they cover the aggregate the branch minted.
#[test]
fn full_repayments_empty_the_interest_ledger_in_either_order() {
	let run = |first: AccountId, second: AccountId| {
		ExtBuilder::default().branch(DOT, STABLE).build().execute_with(|| {
			assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(10)));
			assert_ok!(open(2, DOT, STABLE, 1_400, 700, percent(37)));
			mint_stable(STABLE, 1, 1_000);
			mint_stable(STABLE, 2, 1_000);
			advance_time(10 * ONE_DAY_MS);

			for owner in [first, second] {
				assert_ok!(repay(owner, DOT, STABLE, owner, None));
				assert_ok!(close_vault(owner, DOT, STABLE, None));
			}

			// The branch persists after its vaults close.
			let state = market_state(DOT, STABLE);
			assert_eq!(state.debt.minted_interest, 0);
			assert_eq!(state.debt.pending_interest_attribution, 0);
			assert_eq!(state.debt.aggregate_interest_remainder, 0);
			assert!(state.is_removable());
			check_invariants();
			total_stable(STABLE)
		})
	};
	assert_eq!(run(1, 2), run(2, 1));
}
