use crate::{
	mock::*,
	tests::{rate_pct, vault_status},
};
use pallet_linked_list::SortedListInterface;
use pusd_primitives::{RedemptionSettlement, VaultInterface};

const ONE_DAY_MS: Moment = 24 * 3_600 * 1_000;

// Behavior note: Dormant vaults can still be the target of `withdraw` and
// `repay` operations. The carve-outs are `change_rate` and collateral-only
// deposits that cannot revive the vault to `Debt >= MinimumDebt`.

#[test]
fn fully_redeemed_vault_becomes_dormant_and_leaves_rate_index() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// A and B at distinct rates so the redemption order is deterministic
		// (tail-first picks A first as it has the lower rate).
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));

		// Let a full year accrue so the redemption must poke the target's pending
		// interest before cancelling — redeeming at the genesis instant would only
		// exercise this against pre-touch stored debt.
		advance_time(pusd_primitives::MILLIS_PER_YEAR);
		let now = pallet_timestamp::Pallet::<Test>::get();
		// Redeem more than the fully-accrued debt (500 principal + 1 open fee + 5 year
		// interest = 506) so acct 1's debt is cancelled in full.
		let target = redeem(DOT, PUSD, 3, 1_000).expect("redeem ok");
		assert_eq!(target, 1);

		let v = vault(DOT, PUSD, 1);
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_eq!(v.debt.principal + v.debt.interest, 0);
		// The redemption poked the target: its interest clock advanced to now.
		assert_eq!(v.last_interest_time, branch_state(DOT, PUSD).unwrap().interest_time(now));
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.dormant_redemption_target, None);
		// Rate index no longer contains acct 1.
		assert!(!<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&rate_list(DOT, PUSD),
			&1
		));
	});
}

#[test]
fn redeemed_below_min_debt_becomes_dormant() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));

		// MinimumDebt = 200 (from default_branch_config). Redeem so acct 1
		// has < 200 left.
		assert_ok!(redeem(DOT, PUSD, 3, 350));
		let v = vault(DOT, PUSD, 1);
		let total = v.debt.principal + v.debt.interest;
		// Open fee 1 (500 @ 1%) → total 501; redeem 350 cancels interest-first (1) then
		// 349 principal, leaving exactly 151, below MinimumDebt 200.
		assert_eq!(total, 151);
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.dormant_redemption_target, Some(1));
		assert!(!<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&rate_list(DOT, PUSD),
			&1
		));
	});
}

#[test]
fn redeemed_above_min_debt_stays_active() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));

		// Redeem 200 — leaves acct 1 with ≈ 300 debt, well above MinimumDebt.
		assert_ok!(redeem(DOT, PUSD, 3, 200));
		assert!(vault_status(DOT, PUSD, 1).is_active());
		assert!(<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&rate_list(DOT, PUSD),
			&1
		));
	});
}

#[test]
fn redeem_step_rejects_frozen_branch_and_missing_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_noop!(redeem_step(DOT, PUSD, 99, 3, 1, 0), crate::Error::<Test>::VaultNotFound);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		// A frozen branch must reject settlement, like every other price-dependent
		// path; the gate fires before any vault is touched or priced.
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			true
		));
		assert_noop!(redeem_step(DOT, PUSD, 1, 3, 1, 0), crate::Error::<Test>::BranchFrozen);
	});
}

// The two-phase contract: sizing happens against the projection, application
// re-touches inside `redeem_step`. Both must see the same numbers within one
// dispatch, so a settlement sized exactly at the projected debt and collateral
// is accepted and cancels exactly those amounts.
#[test]
fn projected_redemption_snapshot_matches_execution_without_mutating_state() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(50, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(60, 100)));
		advance_time(pusd_primitives::MILLIS_PER_YEAR + 1);

		let branch_before = branch_state(DOT, PUSD).expect("branch stored");
		let vault_before = vault(DOT, PUSD, 1);
		let held_before = held(DOT, 1);
		let events_before = System::events();

		let projected =
			crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &PUSD, &1).expect("snapshot");
		// Projection is a pure read.
		assert_eq!(branch_state(DOT, PUSD), Some(branch_before));
		assert_eq!(try_vault(DOT, PUSD, 1), Some(vault_before.clone()));
		assert_eq!(held(DOT, 1), held_before);
		assert_eq!(System::events(), events_before);
		// The projection includes the year of pending interest the row lacks.
		assert_eq!(projected.debt, vault_before.debt.total() + 250);
		assert_eq!(projected.terminal_interest_charge, 1);

		// A settlement filling the whole projected position is exact, proving
		// execution touched to the same values the projection reported.
		assert_ok!(redeem_step(
			DOT,
			PUSD,
			1,
			3,
			projected.debt + projected.terminal_interest_charge,
			projected.collateral,
		));
		let v_post = vault(DOT, PUSD, 1);
		assert_eq!(v_post.debt.total(), 0);
		assert_eq!(held(DOT, 1), held_before - projected.collateral);
	});
}

#[test]
fn terminal_charge_is_rejected_on_a_base_debt_only_full_step() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(10, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(20, 100)));
		advance_time(1);
		let snapshot =
			crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &PUSD, &1).expect("snapshot");
		assert_eq!(snapshot.terminal_interest_charge, 1);
		assert_noop!(
			redeem_step(DOT, PUSD, 1, 3, snapshot.debt, 0),
			crate::Error::<Test>::InvalidRedemptionSettlement
		);
		assert_ok!(redeem_step(DOT, PUSD, 1, 3, snapshot.debt - 1, 0));
		let remaining = crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &PUSD, &1)
			.expect("remaining snapshot");
		assert_eq!(remaining.debt, 1);
		assert_eq!(remaining.terminal_interest_charge, 1);
	});
}

#[test]
fn redeem_step_rejects_invalid_settlements_without_state_change() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		let vault_pre = vault(DOT, PUSD, 1);
		let held_pre = held(DOT, 1);
		let snapshot =
			crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &PUSD, &1).expect("snapshot");

		// `assert_noop!` pins the whole storage root, so each rejection also
		// proves the rollback un-issues the credit synthesized for the
		// settlement — the rollback contract for settlement credits.
		//
		// Each case violates exactly one invariant so no check masks another.
		// Excess debt.
		assert_noop!(
			redeem_step(DOT, PUSD, 1, 3, snapshot.debt + 1, 0),
			crate::Error::<Test>::InvalidRedemptionSettlement
		);
		// Excess collateral, with valid nonzero payment.
		assert_noop!(
			redeem_step(DOT, PUSD, 1, 3, 1, snapshot.collateral + 1),
			crate::Error::<Test>::InvalidRedemptionSettlement
		);
		// Zero payment cannot release collateral.
		assert_noop!(
			redeem_step(DOT, PUSD, 1, 3, 0, 1),
			crate::Error::<Test>::InvalidRedemptionSettlement
		);
		// A payment in another market's coin cannot settle this market's debt.
		{
			use frame::deps::frame_support::storage::{with_transaction, TransactionOutcome};
			assert_noop!(
				with_transaction(|| {
					let result = <crate::Pallet<Test> as VaultInterface>::redeem_step(
						&DOT,
						&PUSD,
						&1,
						&3,
						settlement(USDX, 100, 0),
					);
					match result {
						Ok(()) => TransactionOutcome::Commit(Ok(())),
						Err(error) => TransactionOutcome::Rollback(Err(error)),
					}
				}),
				crate::Error::<Test>::InvalidRedemptionSettlement
			);
		}

		assert_eq!(vault(DOT, PUSD, 1), vault_pre);
		assert_eq!(held(DOT, 1), held_pre);
	});
}

// Stablecoin withdrawn from a funded redeemer becomes the Credit that `redeem_step` consumes.
// Overall the redeemer pays exactly the cancelled debt, total issuance falls by exactly that
// amount, and the ledger debt falls with it.
#[test]
fn redeem_step_burns_exactly_the_debt_payment() {
	use frame::traits::{
		fungibles::{Balanced, Mutate},
		tokens::{Fortitude, Precision, Preservation},
	};
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		<Assets as Mutate<AccountId>>::mint_into(PUSD, &3, 1_000).expect("mint redeemer pUSD");

		let issuance_pre = total_stable(PUSD);
		let redeemer_pre = stable_balance(PUSD, 3);
		let recipient_coll_pre = collateral_balance(DOT, 3);
		let v_pre = vault(DOT, PUSD, 1);

		let snapshot =
			crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &PUSD, &1).expect("snapshot");
		assert!(snapshot.debt >= 300);
		let debt_payment = <VaultStableAssets as Balanced<AccountId>>::withdraw(
			PUSD,
			&3,
			300,
			Precision::Exact,
			Preservation::Expendable,
			Fortitude::Polite,
		)
		.expect("withdraw payment");
		assert_ok!(<crate::Pallet<Test> as VaultInterface>::redeem_step(
			&DOT,
			&PUSD,
			&1,
			&3,
			RedemptionSettlement { debt_payment, collateral_to_recipient: 30 }
		));

		assert_eq!(total_stable(PUSD), issuance_pre - 300);
		assert_eq!(stable_balance(PUSD, 3), redeemer_pre - 300);
		let v_post = vault(DOT, PUSD, 1);
		let cancelled = (v_pre.debt.principal + v_pre.debt.interest) -
			(v_post.debt.principal + v_post.debt.interest);
		assert_eq!(cancelled, 300);
		assert_eq!(collateral_balance(DOT, 3), recipient_coll_pre + 30);
	});
}

#[test]
fn dormant_pointer_clears_when_last_dormant_fully_redeemed() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		// Push acct 1 to Dormant via partial-below-MinDebt.
		assert_ok!(redeem(DOT, PUSD, 3, 350));
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.dormant_redemption_target, Some(1));
		// Now redeem acct 1's full residual. next_redemption_target prefers
		// dormant_redemption_target, so this hits acct 1 again.
		let v = vault(DOT, PUSD, 1);
		let residual = v.debt.principal + v.debt.interest;
		let target = redeem(DOT, PUSD, 3, residual).expect("redeem residual ok");
		assert_eq!(target, 1);
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.dormant_redemption_target, None);
	});
}

// `activate_dormant` is the permissionless revival path: touch never re-activates
// a Dormant vault, but once its fully-accrued debt is back at/above
// MinimumDebt a hint-bearing `activate_dormant` flips it to Active and clears the
// slot.
#[test]
fn activate_dormant_revives_when_accrued_debt_reaches_minimum() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Vault 1 carries the lower rate so it sits at the redemption tail; its
		// rate is still high enough that accrued interest lifts the dormant
		// remainder back over MinimumDebt (200) within a year.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(50, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(60, 100)));
		assert_ok!(redeem(DOT, PUSD, 3, 350));
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_eq!(branch_state(DOT, PUSD).unwrap().dormant_redemption_target, Some(1));

		advance_time(365 * ONE_DAY_MS);
		// Touch alone never re-activates a Dormant, even past MinimumDebt.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		assert!(vault_status(DOT, PUSD, 1).is_dormant(), "touch never re-activates a Dormant");

		assert_ok!(crate::Pallet::<Test>::activate_dormant(
			RuntimeOrigin::signed(9),
			DOT,
			PUSD,
			1,
			Position::endpoints_only()
		));
		assert!(vault_status(DOT, PUSD, 1).is_active());
		assert!(<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&rate_list(DOT, PUSD),
			&1
		));
		assert_eq!(branch_state(DOT, PUSD).unwrap().dormant_redemption_target, None);
	});
}

#[test]
fn activate_dormant_rejects_below_minimum_debt() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(redeem(DOT, PUSD, 3, 350)); // vault 1 → Dormant, debt ~150 < 200
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_noop!(
			crate::Pallet::<Test>::activate_dormant(
				RuntimeOrigin::signed(9),
				DOT,
				PUSD,
				1,
				Position::endpoints_only()
			),
			crate::Error::<Test>::DebtBelowMinimum
		);
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
	});
}

#[test]
fn activate_dormant_rejects_active_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_noop!(
			crate::Pallet::<Test>::activate_dormant(
				RuntimeOrigin::signed(9),
				DOT,
				PUSD,
				1,
				Position::endpoints_only()
			),
			crate::Error::<Test>::InvalidVaultStatus
		);
	});
}

#[test]
fn dormant_vault_with_residual_accrues_interest() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Distinct rates so the redemption deterministically targets vault 1 (the
		// lower-rate tail); at equal rates the LIFO tie-break would send it to vault 2
		// and this test would then read an untouched Active vault (green for the wrong
		// reason).
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(50, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(60, 100)));
		let target = redeem(DOT, PUSD, 3, 350).expect("redeem ok");
		assert_eq!(target, 1);
		// Vault 1 (open fee 5 → total 505) is redeemed by 350: interest-first cancels 5,
		// then 345 principal, leaving a 155 residual below MinimumDebt 200 → Dormant.
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_eq!(branch_state(DOT, PUSD).unwrap().dormant_redemption_target, Some(1));
		let v_pre = vault(DOT, PUSD, 1);
		assert_eq!(v_pre.debt.principal, 155);
		assert_eq!(v_pre.debt.interest, 0);

		advance_time(365 * ONE_DAY_MS); // ~1 year (365 days)
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(2), DOT, PUSD, 1));
		let v_post = vault(DOT, PUSD, 1);
		// The Dormant residual keeps accruing: floor(155 * 0.5 * 365days / year) = 77.
		assert_eq!(v_post.debt.interest, 77);
	});
}

// Debt-bearing Dormant vaults keep stake and receive liquidation allocations.
#[test]
fn debt_bearing_dormant_vault_receives_redistribution_on_touch() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Distinct rates so the rate-index tail is deterministic — acct 1 at
		// the lower rate sits at the tail, where the redemption helper picks
		// it first.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(open(3, DOT, PUSD, 200, 200, rate_pct(5, 100)));
		assert_ok!(redeem(DOT, PUSD, 4, 350)); // leaves acct 1 Dormant with debt

		let vault_dormant_pre = vault(DOT, PUSD, 1);
		// At 1.0 vault 3 (200 collateral, ~200 debt) sits under MCR while 1 and 2 stay above it.
		set_price(DOT, FixedU128::from_rational(1u128, 1u128));
		// Redistribute vault 3's whole debt across the recipients (no offset).
		let coll_3 = held(DOT, 3);
		assert_ok!(liquidate_with(DOT, PUSD, 3, |_| LiquidationAllocation {
			offset: OffsetAllocation { collateral_recipient: 0, debt: 0, collateral: 0 },
			redistribution_collateral: coll_3,
			keeper: KeeperCompensation { recipient: 3, collateral: 0 },
		}));
		assert_eq!(
			vault(DOT, PUSD, 1).debt.principal,
			vault_dormant_pre.debt.principal,
			"the allocation stays lazy until this vault is touched",
		);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(2), DOT, PUSD, 1));
		let vault_dormant_post = vault(DOT, PUSD, 1);
		// Dormant debt must not remove a vault from redistribution.
		let gained = vault_dormant_post.debt.principal - vault_dormant_pre.debt.principal;
		assert_eq!(gained, 98);
		assert_eq!(branch_state(DOT, PUSD).unwrap().dormant_redemption_target, Some(1));
	});
}

// A debt-free Dormant vault remains eligible for redistribution, but it does not occupy the
// redemption slot. Redistribution can make it debt-bearing again.
#[test]
fn debt_free_dormant_husk_is_made_debt_bearing_by_redistribution() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(open(3, DOT, PUSD, 200, 200, rate_pct(5, 100)));
		assert_ok!(redeem(DOT, PUSD, 4, 700));

		let husk_before = vault(DOT, PUSD, 1);
		assert_eq!(husk_before.debt.total(), 0);
		assert_eq!(husk_before.collateral, 950);
		assert_eq!(husk_before.redistribution_stake, 950, "a debt-free husk stays eligible");
		assert_eq!(branch_state(DOT, PUSD).unwrap().dormant_redemption_target, None);

		set_price(DOT, FixedU128::from_rational(1u128, 1u128));
		assert_ok!(redistribute_for_test(DOT, PUSD, 3, held(DOT, 3)));
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(2), DOT, PUSD, 1));

		// The liquidated vault leaves the recipient set before allocation.
		let husk_after = vault(DOT, PUSD, 1);
		assert_eq!(husk_after.debt.principal, 97);
		assert_eq!(husk_after.collateral, 950 + 97);
		// Snapshot correction does not let new collateral increase this allocation weight.
		assert_eq!(husk_after.redistribution_stake, 949);
		// Redistribution does not put a Dormant vault in the redemption slot.
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_eq!(branch_state(DOT, PUSD).unwrap().dormant_redemption_target, None);
	});
}

// `borrow` requires `vault.debt.principal >= config.minimum_debt` after the
// operation. Borrowing on a Dormant vault that doesn't reach the threshold
// reverts.
#[test]
fn dormant_borrow_below_min_debt_reverts() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Distinct rates so the rate-index tail is deterministic — acct 1 at
		// the lower rate sits at the tail, where the redemption helper picks
		// it first.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(redeem(DOT, PUSD, 3, 480)); // pushes acct 1 to Dormant with tiny debt
										 // Borrow 1 — total debt would be far below MinimumDebt 200.
		assert_noop!(
			crate::Pallet::<Test>::borrow(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				1,
				None,
				None,
				Position::endpoints_only()
			),
			crate::Error::<Test>::DebtBelowMinimum
		);
	});
}

// A collateral deposit cannot reactivate a Dormant vault. A batch must borrow past MinimumDebt
// before it deposits collateral because each call validates the state left by the prior call.
#[test]
fn dormant_revived_by_borrow_then_accepts_deposit() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(redeem(DOT, PUSD, 3, 350));
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		// While Dormant it is out of the rate index and parked as the redemption target.
		assert!(!<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&rate_list(DOT, PUSD),
			&1
		));
		assert_eq!(branch_state(DOT, PUSD).unwrap().dormant_redemption_target, Some(1));

		// A deposit alone cannot revive a Dormant vault → rejected (so it must not
		// lead a batch).
		assert_noop!(
			crate::Pallet::<Test>::deposit_collateral_for(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				1,
				100
			),
			crate::Error::<Test>::InvalidVaultStatus
		);

		// Borrow across MinimumDebt revives it to Active...
		assert_ok!(crate::Pallet::<Test>::borrow(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			500,
			None,
			None,
			Position::endpoints_only()
		));
		assert!(vault_status(DOT, PUSD, 1).is_active());
		// ...re-inserted into the rate index and the dormant slot cleared.
		assert!(<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&rate_list(DOT, PUSD),
			&1
		));
		assert_eq!(branch_state(DOT, PUSD).unwrap().dormant_redemption_target, None);

		// ...after which the deposit leg of the batch is accepted.
		let held_before = held(DOT, 1);
		assert_ok!(crate::Pallet::<Test>::deposit_collateral_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			100
		));
		assert_eq!(held(DOT, 1), held_before + 100);
	});
}

#[test]
fn dormant_vault_cannot_change_rate() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Distinct rates so the rate-index tail is deterministic — acct 1 at
		// the lower rate sits at the tail, where the redemption helper picks
		// it first.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(redeem(DOT, PUSD, 3, 350));
		assert_noop!(
			crate::Pallet::<Test>::change_rate(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				rate_pct(7, 100),
				Position::endpoints_only()
			),
			crate::Error::<Test>::InvalidVaultStatus
		);
	});
}

#[test]
fn redemption_is_path_independent_across_chunks() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(TOKEN_X, PUSD);
		// Identical vaults on the two markets (same owner, collateral, debt, rate).
		assert_ok!(open(1, DOT, PUSD, 1_000, 1_000, rate_pct(5, 100)));
		assert_ok!(open(1, TOKEN_X, PUSD, 1_000, 1_000, rate_pct(5, 100)));
		let dot_pre = vault(DOT, PUSD, 1);
		let tokenx_pre = vault(TOKEN_X, PUSD, 1);
		assert_eq!(dot_pre.debt.total(), tokenx_pre.debt.total());
		let dot_held_pre = held(DOT, 1);
		let tokenx_held_pre = held(TOKEN_X, 1);
		let recipient_dot_pre = collateral_balance(DOT, 3);
		let recipient_tokenx_pre = collateral_balance(TOKEN_X, 3);

		// Many: three 100-unit redemptions against the DOT vault (stays Active > 200).
		for _ in 0..3 {
			assert_eq!(redeem(DOT, PUSD, 3, 100).expect("redeem ok"), 1);
		}
		// One: a single 300-unit redemption against the identical TOKEN_X vault.
		assert_eq!(redeem(TOKEN_X, PUSD, 3, 300).expect("redeem ok"), 1);

		let dot_post = vault(DOT, PUSD, 1);
		let tokenx_post = vault(TOKEN_X, PUSD, 1);
		// Exactly 300 debt cancelled on each path, leaving identical residual debt
		// (including the interest-first split of principal vs interest).
		assert_eq!(dot_pre.debt.total() - dot_post.debt.total(), 300);
		assert_eq!(dot_post.debt.principal, tokenx_post.debt.principal);
		assert_eq!(dot_post.debt.interest, tokenx_post.debt.interest);
		// Exactly 30 collateral released on each path (3 * floor(100/10) == floor(300/10)).
		assert_eq!(dot_held_pre - held(DOT, 1), 30);
		assert_eq!(tokenx_held_pre - held(TOKEN_X, 1), 30);
		assert_eq!(collateral_balance(DOT, 3) - recipient_dot_pre, 30);
		assert_eq!(collateral_balance(TOKEN_X, 3) - recipient_tokenx_pre, 30);
	});
}
