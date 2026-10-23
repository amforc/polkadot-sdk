use crate::{
	mock::*,
	tests::{percent, vault_status, ONE_DAY_MS, ONE_YEAR_MS},
	VaultStatus,
};
use pallet_linked_list::SortedListInterface;
use stablecoin_primitives::{RedemptionSettlement, VaultInterface};

// Behavior note: Dormant vaults can still be the target of `withdraw` and
// `repay` operations. The carve-outs are `change_rate` and collateral-only
// deposits that cannot revive the vault to `Debt >= MinimumDebt`.

// A redemption leaves the vault Active while the remaining debt is at least MinimumDebt (200).
// Below it the vault turns Dormant and parks in the redemption slot, unless it has no debt left
// to redeem. The status is read from index membership, so a Dormant vault is out of the rate
// index.
#[test]
fn redemption_parks_the_vault_below_minimum_debt() {
	use VaultStatus::{Active, Dormant};
	// Vault 1 owes 501: 500 plus the upfront fee of ceil(500 · 1% · 7d / 365.25d) = 1. A year
	// adds 500 · 1% = 5 of pending interest, which the redemption must settle before it cancels
	// debt. Each case is `(elapsed, redeemed, remaining debt, status, dormant slot)`.
	let cases: [(Moment, Balance, Balance, VaultStatus, Option<AccountId>); 7] = [
		(0, 200, 301, Active, None),
		(0, 301, 200, Active, None),
		(0, 302, 199, Dormant, Some(1)),
		(0, 350, 151, Dormant, Some(1)),
		(0, 501, 0, Dormant, None),
		(ONE_YEAR_MS, 306, 200, Active, None),
		(ONE_YEAR_MS, 506, 0, Dormant, None),
	];
	for (elapsed, redeemed, remaining, status, slot) in cases {
		ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
			// Distinct rates make vault 1, at the lower rate, the redemption target.
			assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
			assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
			advance_time(elapsed);

			assert_eq!(redeem(DOT, STABLE, 3, redeemed), Ok(1));

			let vault = vault(DOT, STABLE, 1);
			assert_eq!(vault.debt.total(), remaining, "redeeming {redeemed}");
			assert_eq!(vault_status(DOT, STABLE, 1), status, "redeeming {redeemed}");
			assert_eq!(
				market_state(DOT, STABLE).dormant_redemption_target,
				slot,
				"redeeming {redeemed}"
			);
			// Branch interest time starts at registration, so the touch stamps the elapsed time.
			assert_eq!(vault.last_interest_time, elapsed);
		});
	}
}

#[test]
fn redeem_step_rejects_frozen_branch_and_missing_vault() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_noop!(redeem_step(DOT, STABLE, 99, 3, 1, 0), crate::Error::<Test>::VaultNotFound);

		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		// A frozen branch must reject settlement, like every other price-dependent
		// path; the gate fires before any vault is touched or priced.
		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, true));

		assert_noop!(redeem_step(DOT, STABLE, 1, 3, 1, 0), crate::Error::<Test>::BranchFrozen);
	});
}

// The two-phase contract: sizing happens against the projection, application
// re-touches inside `redeem_step`. Both must see the same numbers within one
// dispatch, so a settlement sized exactly at the projected debt and collateral
// is accepted and cancels exactly those amounts.
#[test]
fn projected_redemption_snapshot_matches_execution_without_mutating_state() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(50)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(60)));
		advance_time(ONE_YEAR_MS + 1);

		let vault_before = vault(DOT, STABLE, 1);
		let held_before = held(DOT, 1);

		// Projection is a pure read.
		let projected;
		assert_storage_noop!(
			projected = crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &STABLE, &1)
				.expect("snapshot")
		);

		// The projection includes the year of pending interest the row lacks, rounded up for the
		// extra millisecond.
		assert_eq!(projected.debt, vault_before.debt.total() + 251);

		// A settlement filling the whole projected position is exact, proving
		// execution touched to the same values the projection reported.
		assert_ok!(redeem_step(DOT, STABLE, 1, 3, projected.debt, projected.collateral,));

		// The settlement drained the position completely, so the vault closed.
		assert!(!vault_exists(DOT, STABLE, 1));
		assert_eq!(held(DOT, 1), held_before - projected.collateral);
	});
}

#[test]
fn a_step_for_the_projected_debt_clears_it() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(10)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(20)));
		advance_time(1);
		let snapshot = crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &STABLE, &1)
			.expect("snapshot");

		assert_noop!(
			redeem_step(DOT, STABLE, 1, 3, snapshot.debt + 1, 0),
			crate::Error::<Test>::InvalidRedemptionSettlement
		);

		assert_ok!(redeem_step(DOT, STABLE, 1, 3, snapshot.debt, 0));

		let vault = vault(DOT, STABLE, 1);
		assert_eq!(vault.debt.total(), 0);
		assert_eq!(vault.interest_prepaid, 0);
	});
}

#[test]
fn redeem_step_rejects_invalid_settlements() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		let snapshot = crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &STABLE, &1)
			.expect("snapshot");

		// `assert_noop!` pins the whole storage root, so each rejection also
		// proves the rollback un-issues the credit synthesized for the
		// settlement — the rollback contract for settlement credits.
		//
		// Each case violates exactly one invariant so no check masks another.
		// Excess debt.
		assert_noop!(
			redeem_step(DOT, STABLE, 1, 3, snapshot.debt + 1, 0),
			crate::Error::<Test>::InvalidRedemptionSettlement
		);
		// Excess collateral, with valid nonzero payment.
		assert_noop!(
			redeem_step(DOT, STABLE, 1, 3, 1, snapshot.collateral + 1),
			crate::Error::<Test>::InvalidRedemptionSettlement
		);
		// Zero payment cannot release collateral.
		assert_noop!(
			redeem_step(DOT, STABLE, 1, 3, 0, 1),
			crate::Error::<Test>::InvalidRedemptionSettlement
		);
		// A payment in another branch's coin cannot settle this branch's debt.
		// Roll back the synthetic credit; this case checks the error, not atomicity.
		assert_err!(
			hypothetically!(<crate::Pallet<Test> as VaultInterface>::redeem_step(
				&DOT,
				&STABLE,
				&1,
				&3,
				settlement(USDX, 100, 0),
			)),
			crate::Error::<Test>::InvalidRedemptionSettlement
		);
	});
}

// Stablecoin withdrawn from a funded redeemer becomes the Credit that `redeem_step` consumes.
// Overall the redeemer pays exactly the cancelled debt, total issuance falls by exactly that
// amount, and the ledger debt falls with it.
#[test]
fn redeem_step_burns_exactly_the_debt_payment() {
	use frame::traits::{
		fungibles::Balanced,
		tokens::{Fortitude, Precision, Preservation},
	};
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		mint_stable(STABLE, 3, 1_000);

		let issuance_pre = total_stable(STABLE);
		let redeemer_pre = stable_balance(STABLE, 3);
		let recipient_coll_pre = collateral_balance(DOT, 3);
		let v_pre = vault(DOT, STABLE, 1);

		let snapshot = crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &STABLE, &1)
			.expect("snapshot");
		assert!(snapshot.debt >= 300);
		let debt_payment = <VaultStableAssets as Balanced<AccountId>>::withdraw(
			STABLE,
			&3,
			300,
			Precision::Exact,
			Preservation::Expendable,
			Fortitude::Polite,
		)
		.expect("withdraw payment");
		assert_ok!(<crate::Pallet<Test> as VaultInterface>::redeem_step(
			&DOT,
			&STABLE,
			&1,
			&3,
			RedemptionSettlement { debt_payment, collateral_to_recipient: 30 }
		));

		assert_eq!(total_stable(STABLE), issuance_pre - 300);
		assert_eq!(stable_balance(STABLE, 3), redeemer_pre - 300);
		let v_post = vault(DOT, STABLE, 1);
		let cancelled = (v_pre.debt.principal + v_pre.debt.interest) -
			(v_post.debt.principal + v_post.debt.interest);
		assert_eq!(cancelled, 300);
		assert_eq!(collateral_balance(DOT, 3), recipient_coll_pre + 30);
	});
}

#[test]
fn dormant_pointer_clears_when_last_dormant_fully_redeemed() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		// Push acct 1 to Dormant via partial-below-MinDebt.
		assert_ok!(redeem(DOT, STABLE, 3, 350));
		let state = market_state(DOT, STABLE);
		assert_eq!(state.dormant_redemption_target, Some(1));

		// Now redeem acct 1's full residual. next_redemption_target prefers
		// dormant_redemption_target, so this hits acct 1 again.
		let v = vault(DOT, STABLE, 1);
		let residual = v.debt.principal + v.debt.interest;
		let target = redeem(DOT, STABLE, 3, residual).expect("redeem residual ok");

		assert_eq!(target, 1);
		let state = market_state(DOT, STABLE);
		assert_eq!(state.dormant_redemption_target, None);
	});
}

// `activate_dormant` is the permissionless revival path: touch never re-activates
// a Dormant vault, but once its fully-accrued debt is back at/above
// MinimumDebt a hint-bearing `activate_dormant` flips it to Active and clears the
// slot.
#[test]
fn activate_dormant_revives_when_accrued_debt_reaches_minimum() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// Vault 1 carries the lower rate so it sits at the redemption tail; its
		// rate is still high enough that accrued interest lifts the dormant
		// remainder back over MinimumDebt (200) within a year.
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(50)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(60)));
		assert_ok!(redeem(DOT, STABLE, 3, 350));
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, Some(1));

		assert_eq!(vault(DOT, STABLE, 1).debt.principal, 155);
		assert_eq!(vault(DOT, STABLE, 1).debt.interest, 0);

		advance_time(365 * ONE_DAY_MS);
		// Touch alone never re-activates a Dormant, even past MinimumDebt.
		assert_ok!(poke(9, DOT, STABLE, 1));

		// ceil(155 × 50% × 365 / 365.25) = 78.
		assert_eq!(vault(DOT, STABLE, 1).debt.interest, 78);
		assert!(vault_status(DOT, STABLE, 1).is_dormant(), "touch never re-activates a Dormant");
		assert_eq!(
			market_state(DOT, STABLE).dormant_redemption_target,
			Some(1),
			"the touch leaves the dormant slot in place"
		);

		assert_ok!(activate_dormant(9, DOT, STABLE, 1));

		assert!(vault_status(DOT, STABLE, 1).is_active());
		assert!(<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&VaultList::Rate(DOT, STABLE),
			&1
		));
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);
	});
}

#[test]
fn activate_dormant_rejects_below_minimum_debt() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		assert_ok!(redeem(DOT, STABLE, 3, 350)); // vault 1 → Dormant, debt ~150 < 200
		assert!(vault_status(DOT, STABLE, 1).is_dormant());

		assert_noop!(activate_dormant(9, DOT, STABLE, 1), crate::Error::<Test>::DebtBelowMinimum);
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
	});
}

#[test]
fn activate_dormant_rejects_active_vault() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));

		assert_noop!(activate_dormant(9, DOT, STABLE, 1), crate::Error::<Test>::InvalidVaultStatus);
	});
}

// Debt-bearing Dormant vaults keep stake and receive liquidation allocations.
#[test]
fn debt_bearing_dormant_vault_receives_redistribution_on_touch() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// Distinct rates so the rate-index tail is deterministic — acct 1 at
		// the lower rate sits at the tail, where the redemption helper picks
		// it first.
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		assert_ok!(open(3, DOT, STABLE, 200, 200, percent(5)));
		assert_ok!(redeem(DOT, STABLE, 4, 350)); // leaves acct 1 Dormant with debt

		let vault_dormant_pre = vault(DOT, STABLE, 1);
		// At 1.0 vault 3 (200 collateral, ~200 debt) sits under MCR while 1 and 2 stay above it.
		set_price(DOT, FixedU128::from_rational(1u128, 1u128));
		// Redistribute vault 3's whole debt across the recipients (no offset).
		let coll_3 = held(DOT, 3);
		assert_ok!(liquidate_with(DOT, STABLE, 3, |_| LiquidationAllocation {
			offset: OffsetAllocation { collateral_recipient: 0, debt: 0, collateral: 0 },
			redistribution_collateral: coll_3,
			keeper: KeeperCompensation { recipient: 3, collateral: 0 },
		}));
		assert_eq!(
			vault(DOT, STABLE, 1).debt.principal,
			vault_dormant_pre.debt.principal,
			"the allocation stays lazy until this vault is touched",
		);
		assert_ok!(poke(99, DOT, STABLE, 1));
		let vault_dormant_post = vault(DOT, STABLE, 1);
		// Dormant debt must not remove a vault from redistribution.
		let gained = vault_dormant_post.debt.principal - vault_dormant_pre.debt.principal;
		assert_eq!(gained, 98);
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, Some(1));
	});
}

// A debt-free Dormant vault remains eligible for redistribution, but it does not occupy the
// redemption slot. Redistribution can make it debt-bearing again.
#[test]
fn debt_free_dormant_husk_is_made_debt_bearing_by_redistribution() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		assert_ok!(open(3, DOT, STABLE, 200, 200, percent(5)));
		assert_ok!(redeem(DOT, STABLE, 4, 700));

		let husk_before = vault(DOT, STABLE, 1);
		assert_eq!(husk_before.debt.total(), 0);
		assert_eq!(husk_before.collateral, 950);
		assert_eq!(husk_before.redistribution_stake, 950, "a debt-free husk stays eligible");
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);

		set_price(DOT, FixedU128::from_u32(1));
		assert_ok!(redistribute_for_test(DOT, STABLE, 3, held(DOT, 3)));
		assert_ok!(poke(2, DOT, STABLE, 1));

		// The liquidated vault leaves the recipient set before allocation.
		let husk_after = vault(DOT, STABLE, 1);
		assert_eq!(husk_after.debt.principal, 97);
		assert_eq!(husk_after.collateral, 950 + 97);
		// Snapshot correction does not let new collateral increase this allocation share.
		assert_eq!(husk_after.redistribution_stake, 949);
		// Redistribution does not put a Dormant vault in the redemption slot.
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);
	});
}

// `borrow` requires `vault.debt.principal >= config.minimum_debt` after the
// operation, so a Dormant vault revives only by borrowing at least the shortfall.
#[test]
fn dormant_borrow_below_min_debt_reverts() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		let minimum_debt = market_config(DOT, STABLE).minimum_debt;
		// Distinct rates make account 1 the first redemption target.
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		// Leaves a tenth of the minimum, which parks the vault as Dormant.
		let redeemed = vault(DOT, STABLE, 1).debt.total() - minimum_debt / 10;
		assert_eq!(redeem(DOT, STABLE, 3, redeemed), Ok(1));
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		let shortfall = minimum_debt - vault(DOT, STABLE, 1).debt.principal;
		assert!(shortfall > 1);

		assert_noop!(
			borrow(1, DOT, STABLE, shortfall - 1, None),
			crate::Error::<Test>::DebtBelowMinimum
		);
		hypothetically!({
			assert_ok!(borrow(1, DOT, STABLE, shortfall, None));
			assert_eq!(vault(DOT, STABLE, 1).debt.principal, minimum_debt);
			assert!(vault_status(DOT, STABLE, 1).is_active());
		});
	});
}

// A collateral deposit cannot reactivate a Dormant vault. A batch must borrow past MinimumDebt
// before it deposits collateral because each call validates the state left by the prior call.
#[test]
fn dormant_revived_by_borrow_then_accepts_deposit() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		assert_ok!(redeem(DOT, STABLE, 3, 350));
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		// While Dormant it is out of the rate index and parked as the redemption target.
		assert!(!<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&VaultList::Rate(DOT, STABLE),
			&1
		));
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, Some(1));

		// A deposit alone cannot revive a Dormant vault → rejected (so it must not
		// lead a batch).
		assert_noop!(
			deposit_collateral(1, DOT, STABLE, 1, 100),
			crate::Error::<Test>::InvalidVaultStatus
		);

		// Borrow across MinimumDebt revives it to Active...
		assert_ok!(borrow(1, DOT, STABLE, 500, None));

		assert!(vault_status(DOT, STABLE, 1).is_active());
		// ...re-inserted into the rate index and the dormant slot cleared.
		assert!(<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&VaultList::Rate(DOT, STABLE),
			&1
		));
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);

		// ...after which the deposit leg of the batch is accepted.
		let held_before = held(DOT, 1);
		assert_ok!(deposit_collateral(1, DOT, STABLE, 1, 100));
		assert_eq!(held(DOT, 1), held_before + 100);
	});
}

#[test]
fn dormant_vault_cannot_change_rate() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// Distinct rates so the rate-index tail is deterministic — acct 1 at
		// the lower rate sits at the tail, where the redemption helper picks
		// it first.
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		assert_ok!(redeem(DOT, STABLE, 3, 350));
		assert_noop!(
			change_rate(1, DOT, STABLE, percent(7)),
			crate::Error::<Test>::InvalidVaultStatus
		);
	});
}

#[test]
fn redemption_is_path_independent_across_chunks() {
	ExtBuilder::default()
		.branch(DOT, STABLE)
		.branch(TOKEN_X, STABLE)
		.build_and_execute(|| {
			// Identical vaults on the two branches (same owner, collateral, debt, rate).
			assert_ok!(open(1, DOT, STABLE, 1_000, 1_000, percent(5)));
			assert_ok!(open(1, TOKEN_X, STABLE, 1_000, 1_000, percent(5)));
			let dot_pre = vault(DOT, STABLE, 1);
			let tokenx_pre = vault(TOKEN_X, STABLE, 1);
			assert_eq!(dot_pre.debt.total(), tokenx_pre.debt.total());
			let dot_held_pre = held(DOT, 1);
			let tokenx_held_pre = held(TOKEN_X, 1);
			let recipient_dot_pre = collateral_balance(DOT, 3);
			let recipient_tokenx_pre = collateral_balance(TOKEN_X, 3);

			// Many: three 100-unit redemptions against the DOT vault (stays Active > 200).
			for _ in 0..3 {
				assert_eq!(redeem(DOT, STABLE, 3, 100).expect("redeem ok"), 1);
			}
			// One: a single 300-unit redemption against the identical TOKEN_X vault.
			assert_eq!(redeem(TOKEN_X, STABLE, 3, 300).expect("redeem ok"), 1);

			let dot_post = vault(DOT, STABLE, 1);
			let tokenx_post = vault(TOKEN_X, STABLE, 1);
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
