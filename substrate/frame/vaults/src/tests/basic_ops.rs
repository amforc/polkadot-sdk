use crate::{
	mock::*,
	pallet::Vaults,
	tests::{assert_event, rate_pct, vault_events, vault_status, ONE_DAY_MS, ONE_YEAR_MS},
};
use pallet_linked_list::SortedListInterface;

#[test]
fn deposits_then_borrow_update_vault_and_emit_third_party_source() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// +200 collateral.
		assert_ok!(deposit_collateral(1, DOT, PUSD, 1, 200));
		assert_ok!(deposit_collateral(2, DOT, PUSD, 1, 100));
		assert_event(crate::Event::CollateralDeposited {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			from: 2,
			amount: 100,
		});
		// +300 debt (no rate change). `None` recipient defaults to the owner.
		assert_ok!(borrow(1, DOT, PUSD, 300, None));
		assert_eq!(held(DOT, 1), 1_300);
		let v = vault(DOT, PUSD, 1);
		assert_eq!(v.debt.principal, 800);
		// Each op charges a 1-unit upfront fee (open 500 & borrow 300 at 5%), both
		// recorded as debt: debt.interest = 2, total debt = 802.
		assert_eq!(v.debt.interest, 2);
		assert_eq!(v.debt.total(), 802);
		// The mock burns the Stability Pool's 75% share of each 1-unit fee, which Permill rounds
		// up to the whole unit, so nothing reaches FEE_DEST.
		assert_eq!(stable_balance(PUSD, FEE_DEST), 0);
		// Branch aggregate mirrors the vault principal.
		assert_eq!(branch_state(DOT, PUSD).unwrap().debt.principal, 800);
		// pUSD net to user: initial 500 + 300 borrowed. The upfront fee is recorded as
		// debt, not deducted from the minted pUSD the user receives.
		assert_eq!(stable_balance(PUSD, 1), 800);
	});
}

#[test]
fn borrow_with_recipient_mints_to_recipient_not_owner() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 2_000, 500, rate_pct(5, 100)));
		let owner_pre = stable_balance(PUSD, 1);
		let recipient_pre = stable_balance(PUSD, 4);

		assert_ok!(crate::Pallet::<Test>::borrow(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			300,
			None,
			Some(4),
			Position::endpoints_only()
		));

		assert_eq!(stable_balance(PUSD, 1), owner_pre);
		assert_eq!(stable_balance(PUSD, 4), recipient_pre + 300);
		let v = vault(DOT, PUSD, 1);
		assert_eq!(v.debt.principal, 800);
		assert_event(crate::Event::Borrowed {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			recipient: 4,
			amount: 300,
		});
	});
}

#[test]
fn withdraw_collateral_with_recipient_transfers_to_recipient() {
	for recipient in [1, 4] {
		build_and_execute(|| {
			register_market(DOT, PUSD);
			assert_ok!(open(1, DOT, PUSD, 3_000, 500, rate_pct(5, 100)));
			let recipient_pre = collateral_balance(DOT, recipient);

			assert_ok!(withdraw_collateral(1, DOT, PUSD, 250, Some(recipient)));

			assert_eq!(held(DOT, 1), 2_750);
			assert_eq!(collateral_balance(DOT, recipient), recipient_pre + 250);
			assert_event(crate::Event::CollateralWithdrawn {
				collateral_id: DOT,
				stable_id: PUSD,
				owner: 1,
				recipient,
				amount: 250,
			});
		});
	}
}

#[test]
fn repay_for_by_third_party_burns_payer_balance_and_updates_owner_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 2_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 2_000, 500, rate_pct(5, 100)));
		let payer_pre = stable_balance(PUSD, 2);
		let v_pre = vault(DOT, PUSD, 1);

		assert_ok!(repay(2, DOT, PUSD, 1, Some(100)));

		assert_eq!(stable_balance(PUSD, 2), payer_pre - 100);
		let v_post = vault(DOT, PUSD, 1);
		assert_eq!(v_post.debt.total(), v_pre.debt.total() - 100);
		// The event names the payer, not the owner, as the source of the stablecoin.
		assert_event(crate::Event::Repaid {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			from: 2,
			amount: 100,
		});
	});
}

#[test]
fn close_vault_with_recipient_releases_collateral_to_recipient() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		let v = vault(DOT, PUSD, 1);
		let total = v.debt.total();
		assert_eq!(redeem(DOT, PUSD, 3, total).expect("redeem ok"), 1);
		assert!(vault_status(DOT, PUSD, 1).is_dormant());

		let residual = held(DOT, 1);
		let recipient_pre = collateral_balance(DOT, 4);
		assert_ok!(close_vault(1, DOT, PUSD, Some(4)));

		assert!(!vault_exists(DOT, PUSD, 1));
		assert_eq!(held(DOT, 1), 0);
		assert_eq!(collateral_balance(DOT, 4), recipient_pre + residual);
	});
}

// Exact and excess repayment both leave a debt-free Dormant row with collateral held.
// Owners can repay to zero to raise the branch's collateral ratio in Safety mode.
// Only an explicit close releases the collateral and refunds its deposit.
#[test]
fn exact_and_excess_repayment_leave_husks_until_explicit_close() {
	for (funding, overpay) in [(1, false), (400, true)] {
		build_and_execute(|| {
			register_market(DOT, PUSD);
			assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
			assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
			assert_ok!(<Pusd as frame::traits::fungible::Mutate<u64>>::transfer(
				&2,
				&1,
				funding,
				frame::traits::tokens::Preservation::Expendable,
			));
			// 500 principal plus the open fee ceil(500 × 5% × 7 / 365.25) = 1.
			let total = vault(DOT, PUSD, 1).debt.total();
			assert_eq!(total, 501);
			let balance_before = stable_balance(PUSD, 1);
			let requested = if overpay { balance_before } else { total };
			assert_ok!(repay(1, DOT, PUSD, 1, Some(requested)));

			assert_eq!(stable_balance(PUSD, 1), balance_before - total);
			assert_eq!(vault(DOT, PUSD, 1).debt.total(), 0);
			assert_eq!(held(DOT, 1), 1_000);
			assert!(vault_status(DOT, PUSD, 1).is_dormant());
			assert!(!<LinkedList as SortedListInterface<VaultList, u64>>::contains(
				&rate_list(DOT, PUSD),
				&1,
			));
			assert_event(crate::Event::Repaid {
				collateral_id: DOT,
				stable_id: PUSD,
				owner: 1,
				from: 1,
				amount: total,
			});
			assert!(!vault_events().iter().any(|e| matches!(e, crate::Event::VaultClosed { .. })));

			let collateral_before = collateral_balance(DOT, 1);
			assert_ok!(close_vault(1, DOT, PUSD, None));
			assert!(!vault_exists(DOT, PUSD, 1));
			assert_eq!(held(DOT, 1), 0);
			assert_eq!(collateral_balance(DOT, 1), collateral_before + 1_000 + VAULT_DEPOSIT);
			assert_event(crate::Event::VaultClosed {
				collateral_id: DOT,
				stable_id: PUSD,
				owner: 1,
				recipient: 1,
				collateral: 1_000,
			});
		});
	}
}

// Poking a nonexistent vault is an error, not a silent success — a typo'd
// owner must not look like a completed refresh.
#[test]
fn poke_missing_vault_errors() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_noop!(poke(1, DOT, PUSD, 99), crate::Error::<Test>::VaultNotFound);
	});
}

// A sub-minimum Dormant residual cannot be partially repaid (any non-zero
// remainder below MinimumDebt is `DebtWouldBecomeDust`), so the owner must clear
// it to exactly zero. The overpay cap turns that from an exact-amount guessing
// game into "send at least the dust"; the cleared vault is left as a husk that
// keeps its collateral and frees the branch's `dormant_redemption_target` slot.
#[test]
fn repay_overpay_rescues_subminimum_dormant_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		// Redeem acct 1 (the rate-index tail) down to exactly MinimumDebt - 1 (199),
		// the largest sub-minimum residual, so it parks in the Dormant slot.
		let debt = vault(DOT, PUSD, 1).debt.total();
		assert_ok!(redeem(DOT, PUSD, 3, debt - 199));
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		let residual = vault(DOT, PUSD, 1).debt.total();
		assert_eq!(residual, 199, "residual is MinimumDebt - 1");
		assert_eq!(
			branch_state(DOT, PUSD).expect("state").dormant_redemption_target,
			Some(1),
			"sub-minimum redemption parked acct 1 in the dormant slot"
		);
		// The redemption cancelled 501 - 199 = 302 of debt at the price of 10 and released
		// floor(30.2) = 30 of the 1_000 collateral.
		assert_eq!(held(DOT, 1), 970);

		// Paying 100 would leave 99, which is neither zero nor the minimum.
		assert_noop!(repay(1, DOT, PUSD, 1, Some(100)), crate::Error::<Test>::DebtWouldBecomeDust);

		let balance_before = stable_balance(PUSD, 1);
		assert_ok!(repay(1, DOT, PUSD, 1, Some(balance_before)));

		assert_eq!(
			stable_balance(PUSD, 1),
			balance_before - residual,
			"only the dust residual burned"
		);
		let husk = vault(DOT, PUSD, 1);
		assert_eq!(husk.debt.total(), 0, "sub-minimum dust cleared to zero");
		assert_eq!(held(DOT, 1), 970, "collateral untouched by repay");
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_eq!(
			branch_state(DOT, PUSD).expect("state").dormant_redemption_target,
			None,
			"slot released on repay-to-zero"
		);
	});
}

// Husks normally go through the ratio check because closing one is a TCR-worsening action that
// safety mode disallows. Once the market is liability-free there is no ratio left to protect,
// so the last husks close without it.
#[test]
fn liability_free_market_closes_husks_without_ratio_math() {
	use frame::traits::fungible::Mutate;
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 400, rate_pct(7, 100)));
		// Distinct timestamps create independent aggregate and vault residue.
		advance_time(30 * ONE_DAY_MS);
		assert_ok!(poke(9, DOT, PUSD, 1));
		advance_time(ONE_DAY_MS);
		assert_ok!(poke(9, DOT, PUSD, 2));
		// Top up both owners so overpay-repays can cover accrued interest.
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&1, 100));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&2, 100));

		assert_ok!(repay(2, DOT, PUSD, 2, Some(10_000)));
		assert_ok!(repay(1, DOT, PUSD, 1, Some(10_000)));
		assert!(vault_status(DOT, PUSD, 2).is_dormant(), "vault 2 is a husk");
		assert!(vault_status(DOT, PUSD, 1).is_dormant(), "vault 1 is a husk");

		// A maximal price proves that both closes bypass ratio math, not only the last close.
		set_price(DOT, FixedU128::from_inner(u128::MAX));
		assert_ok!(close_vault(2, DOT, PUSD, None));
		assert_ok!(close_vault(1, DOT, PUSD, None));
		assert!(!vault_exists(DOT, PUSD, 1), "last husk closed");

		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.debt.principal, 0);
		assert_eq!(state.stakes.total, 0);
		assert_eq!(state.debt.minted_interest, 0);
		assert_eq!(state.debt.pending_interest_attribution, 0);
		assert_eq!(state.debt.aggregate_interest_remainder, 0);
	});
}

// Frequent touches charge one rounded unit, and repayment forfeits the prepaid fraction.
#[test]
fn pokes_charge_once_and_full_repayment_forfeits_prepaid_interest() {
	for pokes in [1, 100] {
		build_and_execute(|| {
			register_market(DOT, PUSD);
			assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(10, 100)));
			mint_stable(PUSD, 1, 10);
			let opened = vault(DOT, PUSD, 1).debt.total();
			for _ in 0..pokes {
				advance_time(1);
				assert_ok!(poke(2, DOT, PUSD, 1));
			}
			let owed = vault(DOT, PUSD, 1).debt.total();
			assert_eq!(owed, opened + 1, "only the first poke rounds up");
			assert!(vault(DOT, PUSD, 1).interest_prepaid > 0);

			let balance_before = stable_balance(PUSD, 1);
			assert_ok!(repay(1, DOT, PUSD, 1, None));
			assert_eq!(vault(DOT, PUSD, 1).debt.total(), 0);
			assert_eq!(vault(DOT, PUSD, 1).interest_prepaid, 0);
			assert_eq!(stable_balance(PUSD, 1), balance_before - owed);
		});
	}
}

// Interest a touch mints ahead of the aggregate must not be minted again when the aggregate
// accrues it, or the market keeps an attribution no vault owes and cannot close.
#[test]
fn aggregate_accrual_does_not_remint_prepaid_interest() {
	use frame::traits::fungible::Mutate;
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// 500 at 10% accrues 50 a year: half a unit every hundredth of a year.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(10, 100)));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&1, 10));
		let opened = vault(DOT, PUSD, 1).debt.total();

		advance_time(ONE_YEAR_MS / 100);
		assert_ok!(poke(2, DOT, PUSD, 1));
		assert_eq!(vault(DOT, PUSD, 1).debt.total(), opened + 1, "half a unit rounds up");

		advance_time(ONE_YEAR_MS / 100);
		assert_ok!(poke(2, DOT, PUSD, 1));
		assert_eq!(vault(DOT, PUSD, 1).debt.total(), opened + 1, "the prepaid half covers it");
		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.debt.pending_interest_attribution, 0, "no second mint");
		assert_eq!(state.debt.minted_interest, vault(DOT, PUSD, 1).debt.interest);

		assert_ok!(repay(1, DOT, PUSD, 1, None));
		assert_ok!(close_vault(1, DOT, PUSD, None));
		let state = branch_state(DOT, PUSD).expect("branch state");
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
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 1_000)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(1, 1_000)));
		let minted_before = branch_state(DOT, PUSD).expect("branch state").debt.minted_interest;

		advance_time(ONE_YEAR_MS);
		assert_ok!(refresh_branch(9, DOT, PUSD));

		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.debt.minted_interest - minted_before, 1);
		assert_eq!(state.debt.pending_interest_attribution, 1);
	});
}

// A liability-free close can discard only unowned rounding residue. These forged states verify
// that issued or attributed interest still prevents removal.
#[test]
fn liability_free_close_fails_closed_on_unattributed_liability() {
	use frame::traits::fungible::Mutate;
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(10, 100)));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&1, 10));
		assert_ok!(repay(1, DOT, PUSD, 1, Some(1_000)));
		// Isolate each residue so that each guard has independent coverage.

		// Minted interest prevents branch removal.
		mutate_branch_state(DOT, PUSD, |state| state.debt.minted_interest = 1);
		assert_noop!(close_vault(1, DOT, PUSD, None), DispatchError::Corruption);
		mutate_branch_state(DOT, PUSD, |state| state.debt.minted_interest = 0);

		// Unattributed issuance prevents branch removal independently.
		mutate_branch_state(DOT, PUSD, |state| state.debt.pending_interest_attribution = 1);
		assert_noop!(close_vault(1, DOT, PUSD, None), DispatchError::Corruption);
		assert!(Vaults::<Test>::contains_key((DOT, PUSD, 1)));
		mutate_branch_state(DOT, PUSD, |state| state.debt.pending_interest_attribution = 0);

		assert_ok!(close_vault(1, DOT, PUSD, None));
	});
}

#[test]
fn redemption_slot_rejects_second_owner() {
	fn park(owner: AccountId) -> DispatchResult {
		let snapshot =
			<crate::Pallet<Test> as pusd_primitives::VaultInterface>::project_redemption_snapshot(
				&DOT, &PUSD, &owner,
			)?;
		redeem_step(DOT, PUSD, owner, 7, snapshot.debt - 150, (snapshot.debt - 150) / 10)
	}
	fn parked() -> Option<AccountId> {
		branch_state(DOT, PUSD).expect("branch state").dormant_redemption_target
	}
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(3, 100)));

		assert_ok!(park(1));
		assert_eq!(parked(), Some(1));
		assert_eq!(park(2).unwrap_err(), crate::Error::<Test>::DormantTargetOccupied.into());
		assert_eq!(parked(), Some(1), "slot still points at the first owner");
		assert!(vault_status(DOT, PUSD, 1).is_dormant(), "first dormant intact");
		assert!(
			vault_status(DOT, PUSD, 2).is_active(),
			"second vault stays Active (step rolled back)"
		);
	});
}

// Full repayments must empty the market's interest ledger whatever order they settle in. Each vault
// rounds its own interest up, so together they cover the aggregate the market minted.
#[test]
fn full_repayments_empty_the_interest_ledger_in_either_order() {
	use frame::traits::fungible::Mutate;
	let run = |first: u64, second: u64| {
		new_test_ext().execute_with(|| {
			register_market(DOT, PUSD);
			assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(10, 100)));
			assert_ok!(open(2, DOT, PUSD, 1_400, 700, rate_pct(37, 100)));
			assert_ok!(<Pusd as Mutate<u64>>::mint_into(&1, 1_000));
			assert_ok!(<Pusd as Mutate<u64>>::mint_into(&2, 1_000));
			advance_time(10 * ONE_DAY_MS);

			for owner in [first, second] {
				assert_ok!(repay(owner, DOT, PUSD, owner, None));
				assert_ok!(close_vault(owner, DOT, PUSD, None));
			}

			let state = branch_state(DOT, PUSD).expect("branch persists after closes");
			assert_eq!(state.debt.minted_interest, 0);
			assert_eq!(state.debt.pending_interest_attribution, 0);
			assert_eq!(state.debt.aggregate_interest_remainder, 0);
			assert!(state.is_removable());
			crate::try_state::do_try_state::<Test>().expect("post-test invariants hold");
			total_stable(PUSD)
		})
	};
	assert_eq!(run(1, 2), run(2, 1));
}
