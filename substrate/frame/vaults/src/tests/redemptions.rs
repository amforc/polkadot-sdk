use crate::{
	mock::*,
	tests::{keeperless_branch, percent, vault_status, ONE_DAY_MS, ONE_YEAR_MS},
	VaultStatus,
};
use pallet_linked_list::SortedListInterface;
use stablecoin_primitives::VaultInterface;

// Behavior note: Dormant vaults can still be the target of `withdraw` and
// `repay` operations. The carve-outs are `change_rate` and collateral-only
// deposits that cannot revive the vault to `Debt >= MinimumDebt`.

// A redemption leaves the vault Active while the remaining debt is at least MinimumDebt (200).
// Below it the vault turns Dormant and parks in the redemption slot, unless it has no debt left
// to redeem. The status is read from index membership, so a Dormant vault is out of the rate
// index.
//
// Vault 1 owes 501: 500 plus the upfront fee of ceil(500 · 1% · 7d / 365.25d) = 1. A year adds
// 500 · 1% = 5 of pending interest, which the redemption must settle before it cancels debt.
fn redeem_from_vault_one_after(elapsed: Moment, redeemed: Balance) {
	// Distinct rates make vault 1, at the lower rate, the redemption target.
	assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
	assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
	advance_time(elapsed);

	assert_eq!(redeem(DOT, STABLE, 3, redeemed), Ok(1));

	// Branch interest time starts at registration, so the touch stamps the elapsed time.
	assert_eq!(vault(DOT, STABLE, 1).last_interest_time, elapsed);
}

#[test]
fn redemption_above_minimum_debt_keeps_the_vault_active() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		redeem_from_vault_one_after(0, 200);

		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 301);
		assert_eq!(vault_status(DOT, STABLE, 1), VaultStatus::Active);
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);
	});
}

#[test]
fn redemption_to_exactly_minimum_debt_keeps_the_vault_active() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		redeem_from_vault_one_after(0, 301);

		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 200);
		assert_eq!(vault_status(DOT, STABLE, 1), VaultStatus::Active);
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);
	});
}

#[test]
fn redemption_just_below_minimum_debt_parks_the_vault() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		redeem_from_vault_one_after(0, 302);

		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 199);
		assert_eq!(vault_status(DOT, STABLE, 1), VaultStatus::Dormant);
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, Some(1));
	});
}

#[test]
fn redemption_well_below_minimum_debt_parks_the_vault() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		redeem_from_vault_one_after(0, 350);

		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 151);
		assert_eq!(vault_status(DOT, STABLE, 1), VaultStatus::Dormant);
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, Some(1));
	});
}

// With no debt left to redeem, the vault is Dormant but stays out of the redemption slot.
#[test]
fn full_redemption_makes_the_vault_dormant_without_parking_it() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		redeem_from_vault_one_after(0, 501);

		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 0);
		assert_eq!(vault_status(DOT, STABLE, 1), VaultStatus::Dormant);
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);
	});
}

// The year's 5 of pending interest is settled first, so redeeming 306 leaves exactly the minimum.
#[test]
fn redemption_settles_pending_interest_before_the_minimum_debt_check() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		redeem_from_vault_one_after(ONE_YEAR_MS, 306);

		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 200);
		assert_eq!(vault_status(DOT, STABLE, 1), VaultStatus::Active);
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);
	});
}

// Redeeming the 501 owed plus the year's 5 of pending interest leaves nothing to park.
#[test]
fn full_redemption_settles_pending_interest_without_parking() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		redeem_from_vault_one_after(ONE_YEAR_MS, 506);

		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 0);
		assert_eq!(vault_status(DOT, STABLE, 1), VaultStatus::Dormant);
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);
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

// A step for exactly the projected debt clears it, including the unit that one millisecond of
// interest rounds up to. The vault survives as a husk, so its row shows that the redemption path
// forfeits the prepaid fraction as repayment does.
#[test]
fn redeem_step_clearing_debt_forfeits_prepaid_interest() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(10)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(20)));
		advance_time(1);
		let snapshot = crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &STABLE, &1)
			.expect("snapshot");

		assert_ok!(redeem_step(DOT, STABLE, 1, 3, snapshot.debt, 0));

		let vault = vault(DOT, STABLE, 1);
		assert_eq!(vault.debt.total(), 0);
		assert_eq!(vault.interest_prepaid, 0);
	});
}

#[test]
fn redeem_step_rejects_invalid_requests() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));

		assert_noop!(redeem_step(DOT, STABLE, 99, 3, 1, 0), crate::Error::<Test>::VaultNotFound);

		let snapshot = crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &STABLE, &1)
			.expect("snapshot");

		// `assert_noop!` pins the whole storage root, so each rejection also
		// proves the rollback un-issues the credit synthesized for the
		// settlement — the rollback contract for settlement credits.
		//
		// Each rejection violates exactly one invariant so no check masks another.
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
		// Roll back the synthetic credit; this check pins the error, not atomicity.
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

		// A frozen branch must reject settlement, like every other price-dependent path. The
		// freeze comes last so that it masks none of the settlement checks above.
		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, true));

		assert_noop!(redeem_step(DOT, STABLE, 1, 3, 1, 0), crate::Error::<Test>::BranchFrozen);
	});
}

// `activate_dormant` is the permissionless revival path: touch never re-activates
// a Dormant vault, but once its fully-accrued debt is back at/above
// MinimumDebt a hint-bearing `activate_dormant` flips it to Active and clears the
// slot. It refuses a Dormant vault below the minimum and a vault that is not Dormant.
#[test]
fn activate_dormant_requires_dormant_vault_at_minimum_debt() {
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

		assert_noop!(activate_dormant(9, DOT, STABLE, 1), crate::Error::<Test>::DebtBelowMinimum);
		assert!(vault_status(DOT, STABLE, 2).is_active());
		assert_noop!(activate_dormant(9, DOT, STABLE, 2), crate::Error::<Test>::InvalidVaultStatus);

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
		assert!(<LinkedList as SortedListInterface<VaultList, AccountId>>::contains(
			&VaultList::Rate(DOT, STABLE),
			&1
		));
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);
	});
}

// Debt-bearing Dormant vaults keep stake and receive liquidation allocations.
#[test]
fn debt_bearing_dormant_vault_receives_redistribution_on_touch() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// Distinct rates make account 1 the first redemption target.
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		assert_ok!(open(3, DOT, STABLE, 200, 200, percent(5)));
		assert_ok!(redeem(DOT, STABLE, 4, 350)); // leaves acct 1 Dormant with debt

		let vault_dormant_pre = vault(DOT, STABLE, 1);

		// At 1.0 vault 3 (200 collateral, ~200 debt) sits under MCR while 1 and 2 stay above it.
		set_price(DOT, FixedU128::from_u32(1));
		// An empty pool sends the residual to both eligible vaults.
		assert_ok!(liquidate(99, DOT, STABLE, 3, 0, 0));

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
	keeperless_branch().build_and_execute(|| {
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
		assert_ok!(liquidate(99, DOT, STABLE, 3, 0, 0));
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

// A Dormant vault accepts neither a rate change nor a collateral-only deposit. It revives only
// through a borrow that lifts its principal to at least MinimumDebt; the upfront fee does not
// count toward the minimum. A batch must therefore borrow before it deposits, because each call
// validates the state left by the prior call.
#[test]
fn dormant_vault_accepts_only_a_reviving_borrow() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		let minimum_debt = market_config(DOT, STABLE).minimum_debt;
		// Distinct rates make account 1 the first redemption target.
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		assert_ok!(redeem(DOT, STABLE, 3, 350));
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		// While Dormant it is out of the rate index and parked as the redemption target.
		assert!(!<LinkedList as SortedListInterface<VaultList, AccountId>>::contains(
			&VaultList::Rate(DOT, STABLE),
			&1
		));
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, Some(1));

		assert_noop!(
			change_rate(1, DOT, STABLE, percent(7)),
			crate::Error::<Test>::InvalidVaultStatus
		);
		// A deposit alone cannot revive a Dormant vault, so it must not lead a batch.
		assert_noop!(
			deposit_collateral(1, DOT, STABLE, 1, 100),
			crate::Error::<Test>::InvalidVaultStatus
		);

		let shortfall = minimum_debt - vault(DOT, STABLE, 1).debt.principal;
		assert!(shortfall > 1);
		assert_noop!(
			borrow(1, DOT, STABLE, shortfall - 1, None),
			crate::Error::<Test>::DebtBelowMinimum
		);

		assert_ok!(borrow(1, DOT, STABLE, shortfall, None));

		assert_eq!(vault(DOT, STABLE, 1).debt.principal, minimum_debt);
		assert!(vault_status(DOT, STABLE, 1).is_active());
		// Revival re-inserts the vault into the rate index and clears the dormant slot.
		assert!(<LinkedList as SortedListInterface<VaultList, AccountId>>::contains(
			&VaultList::Rate(DOT, STABLE),
			&1
		));
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);

		// The deposit leg of the batch is now accepted.
		let held_before = held(DOT, 1);
		assert_ok!(deposit_collateral(1, DOT, STABLE, 1, 100));

		assert_eq!(held(DOT, 1), held_before + 100);
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
