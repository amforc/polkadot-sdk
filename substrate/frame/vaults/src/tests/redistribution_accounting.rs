//! Tests for the redistribution / aggregate-interest accounting identities and
//! the FinalRecovery exit and low-level liquidation accounting.
//!
//! Conventions:
//! - Eligible vaults use snapshot-corrected stake. Only `FinalRecovery` vaults have zero stake.
//! - "Recipient rate" means the recipient vault's `annual_rate`, not the liquidated vault's rate.
//! - Stake calculations are checked by `assert_accounting_identity_holds` (`stakes.total == Σ
//!   vault.redistribution_stake`) and the `try_state` identities.

use crate::{
	mock::*,
	pallet::Vaults,
	tests::{rate_pct, vault_status},
};

const ONE_YEAR_MS: Moment = pusd_primitives::MILLIS_PER_YEAR;

/// `floor(x * rate)` for the recipient-rate assertions.
fn weighted(x: Balance, rate: FixedU128) -> Balance {
	rate.saturating_mul_int(x)
}

/// Confirms that each debt and collateral unit has a vault or pending-pool owner.
fn assert_accounting_identity_holds() {
	let state = branch_state(DOT, PUSD).unwrap();
	let rows: Vec<_> = Vaults::<Test>::iter_prefix((DOT, PUSD))
		.map(|(owner, record)| (owner, record.vault))
		.collect();
	let sum_stake: Balance = rows.iter().map(|(_, v)| v.redistribution_stake).sum();
	let sum_principal: Balance = rows.iter().map(|(_, v)| v.debt.principal).sum();
	let sum_collateral: Balance = rows.iter().map(|(_, v)| v.collateral).sum();
	assert_eq!(
		state.stakes.total, sum_stake,
		"stakes.total must equal Σ vault.redistribution_stake of live recipients",
	);
	assert_eq!(state.debt.principal, sum_principal);
	assert_eq!(state.total_collateral, sum_collateral + state.pending_redistribution_collateral);
	assert_eq!(state.vault_count as usize, rows.len());
	assert_eq!(
		held(DOT, crate::Pallet::<Test>::redistribution_account(&DOT, &PUSD)),
		state.pending_redistribution_collateral,
	);
}

// Touch order must not change mixed-rate allocation or pending residue.
#[test]
fn later_touch_order_cannot_change_mixed_rate_liquidation_allocations() {
	let run = |first, second| {
		new_test_ext().execute_with(|| {
			register_market(DOT, PUSD);
			assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(10, 100)));
			assert_ok!(open(2, DOT, PUSD, 999, 500, rate_pct(90, 100)));
			assert_ok!(open(3, DOT, PUSD, 200, 200, rate_pct(5, 100)));
			let before_1 = vault(DOT, PUSD, 1).debt.principal;
			let before_2 = vault(DOT, PUSD, 2).debt.principal;
			let liquidated_debt = vault(DOT, PUSD, 3).debt.total();

			set_price(DOT, FixedU128::from_rational(1u128, 1u128));
			assert_ok!(liquidate(99, DOT, PUSD, 3, 0, 0));
			assert_eq!(vault(DOT, PUSD, 1).debt.principal, before_1);
			assert_eq!(vault(DOT, PUSD, 2).debt.principal, before_2);

			advance_time(ONE_YEAR_MS);
			assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(99), DOT, PUSD, first,));
			assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(99), DOT, PUSD, second,));
			let final_state = branch_state(DOT, PUSD).unwrap();
			let allocated_1 = vault(DOT, PUSD, 1).debt.principal - before_1;
			let allocated_2 = vault(DOT, PUSD, 2).debt.principal - before_2;
			// The nondividing stakes must leave one debt unit in the pending pool.
			assert_eq!(liquidated_debt, 202);
			assert_eq!(allocated_1, 101);
			assert_eq!(allocated_2, 100);
			assert_eq!(final_state.debt.pending_redistribution_principal, 1);
			(
				allocated_1,
				allocated_2,
				final_state.debt.weighted_principal,
				final_state.debt.outstanding(),
				final_state.debt.pending_redistribution_principal,
			)
		})
	};

	assert_eq!(run(1, 2), run(2, 1));
}

// Each weight claim and the remaining pool must retain valid time anchors in both touch orders.
#[test]
fn nonzero_time_weight_residue_is_touch_order_independent() {
	let run = |first, second| {
		new_test_ext().execute_with(|| {
			register_market(DOT, PUSD);
			assert_ok!(open(1, DOT, PUSD, 1_001, 500, rate_pct(7, 100)));
			assert_ok!(open(2, DOT, PUSD, 997, 500, rate_pct(23, 100)));
			assert_ok!(open(3, DOT, PUSD, 200, 202, rate_pct(5, 100)));

			advance_time(1_000);
			set_price(DOT, FixedU128::from_rational(1u128, 1u128));
			assert_eq!(redistribute_for_test(DOT, PUSD, 3, 0).unwrap(), 204);

			assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, first));
			let after_first = branch_state(DOT, PUSD).unwrap();
			let tau = after_first.interest_time(Timestamp::get());
			// The pending residue must have zero accrued interest at the record time.
			assert!(!after_first.debt.pending_redistribution_weight.is_zero());
			assert_eq!(
				after_first.pending_redistribution_weight_time.to_wide(),
				after_first
					.debt
					.pending_redistribution_weight
					.raw()
					.checked_mul(tau.into())
					.unwrap()
			);

			assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, second));
			assert_accounting_identity_holds();
			(branch_state(DOT, PUSD).unwrap(), vault(DOT, PUSD, 1), vault(DOT, PUSD, 2))
		})
	};

	assert_eq!(run(1, 2), run(2, 1));
}

// After a redistribute-everything liquidation, the branch's
// `debt.weighted_principal` must reflect the economic debt at the recipient's
// actual rate — total economic debt × recipient rate — not the redistributed
// principal carried at rate=1.0.
#[test]
fn weighted_sum_after_redistribution_matches_avg_recipient_rate() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Liquidatee at 5%, recipient at 20% (distinct rates so the recipient-rate
		// weighting is genuinely exercised, not masked by equal rates). Both
		// stakes are 1000.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(20, 100)));

		// Drop price below MCR. Vault 1 is now liquidatable.
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));

		let coll_1 = held(DOT, 1);
		assert_ok!(redistribute_for_test(DOT, PUSD, 1, coll_1));
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(99), DOT, PUSD, 2));

		// Collateral conservation: the liquidatee's hold is released; the
		// recipient immediately receives the redistributed collateral.
		assert_eq!(held(DOT, 1), 0, "liquidatee collateral released");
		assert_eq!(held(DOT, 2), 2_000, "recipient owns redistributed collateral");

		let state = branch_state(DOT, PUSD).expect("branch state");
		let total_econ = state.debt.principal;
		// Vault 2 (20%) is the only recipient; ≤3 dust units of ceil/floor mismatch.
		let expected = weighted(total_econ, rate_pct(20, 100));
		let actual = state.debt.weighted_principal.whole;
		assert!(
			actual.abs_diff(expected) <= 3,
			"weighted_sum after redistribution out of bounds: actual={}, expected={} (20% of {})",
			actual,
			expected,
			total_econ,
		);
		// The stake identity (stakes.total == Σ vault.redistribution_stake) is
		// checked here too — it must survive the redistribution.
		assert_accounting_identity_holds();
	});
}

// Redistributed debt must accrue interest at the recipient's rate, not the liquidated vault's
// rate or the temporary accounting rate.
#[test]
fn aggregate_interest_post_redistribution_accrues_at_recipient_rates() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(20, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let coll_1 = held(DOT, 1);
		assert_ok!(redistribute_for_test(DOT, PUSD, 1, coll_1));

		let state_pre = branch_state(DOT, PUSD).unwrap();
		// Both the owned debt and the pending share use the recipient's rate.
		assert_eq!(state_pre.debt.pending_redistribution_principal, 501);
		assert_eq!(state_pre.debt.weighted_principal.whole, 200);

		advance_time(ONE_YEAR_MS);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(99), DOT, PUSD, 2));

		let post_minted = branch_state(DOT, PUSD).unwrap().debt.minted_interest;
		assert_eq!(post_minted - state_pre.debt.minted_interest, 200);
	});
}

// Recipient touches must preserve the market's rate-weighted projection.
#[test]
fn mixed_rate_recipients_materialize_at_their_own_rates() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100))); // A — recipient
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(50, 100))); // B — recipient
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(10, 100))); // C — liquidated

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let coll_3 = held(DOT, 3);
		assert_ok!(redistribute_for_test(DOT, PUSD, 3, coll_3));

		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(99), DOT, PUSD, 1));
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(99), DOT, PUSD, 2));

		let state = branch_state(DOT, PUSD).unwrap();
		let vault_a = vault(DOT, PUSD, 1);
		let vault_b = vault(DOT, PUSD, 2);
		let expected = weighted(vault_a.debt.principal, rate_pct(5, 100))
			.saturating_add(weighted(vault_b.debt.principal, rate_pct(50, 100)));
		let actual = state.debt.weighted_principal.whole;
		// One ceil (`average_branch_rate`) against two per-recipient floors.
		assert!(
			actual.abs_diff(expected) <= 2,
			"mixed-rate weighted sum drift too large: actual={}, expected={}",
			actual,
			expected,
		);
	});
}

// A rate change first materializes the assigned share. Past interest keeps the old rate, and the
// new rate applies to all principal after the change.
#[test]
fn recipient_rate_change_after_liquidation_reprices_the_absorbed_share() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100))); // A - reprices
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(50, 100))); // B - holds its share
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(10, 100))); // C - liquidated
		let vault_a_pre = vault(DOT, PUSD, 1);

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let redistributed = redistribute_for_test(DOT, PUSD, 3, held(DOT, 3)).expect("liquidated");

		// Wait past the cooldown to isolate interest from the rate-change fee, and restore the
		// price so the rate change passes the ratio checks.
		advance_time(ONE_YEAR_MS);
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(crate::Pallet::<Test>::change_rate(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			rate_pct(30, 100),
			Position::endpoints_only()
		));

		let vault_a = vault(DOT, PUSD, 1);
		assert_eq!(vault_a.annual_rate, rate_pct(30, 100));
		assert_eq!(vault_a.debt.principal, 500 + 251);
		assert_eq!(vault_a.collateral, 1_000 + 500);
		// Interest before the change uses the old rate for both principal sources.
		assert_eq!(vault_a.debt.interest, vault_a_pre.debt.interest + 37);

		// An untouched recipient keeps its share pending at its own rate.
		let vault_b = vault(DOT, PUSD, 2);
		assert_eq!(vault_b.debt.principal, 500);
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.debt.pending_redistribution_principal, redistributed - 251);
		assert_eq!(state.pending_redistribution_collateral, 500);
		assert_eq!(
			state.stakes.weighted.whole,
			weighted(1_000, rate_pct(30, 100)) + weighted(1_000, rate_pct(50, 100))
		);

		// Interest after the change uses the new rate for all principal.
		advance_time(ONE_YEAR_MS);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		let vault_a_post = vault(DOT, PUSD, 1);
		assert_eq!(vault_a_post.debt.interest, vault_a.debt.interest + 225);
		assert_accounting_identity_holds();
	});
}

// A follow-on `borrow` against a recipient must keep the branch weighted_sum
// consistent with each vault's own-rate contribution: the borrow first touches
// the vault to fold in its redistribution share, then updates the weighted-sum
// bookkeeping so the aggregate still equals Σ (own ib_debt × own rate).
#[test]
fn borrow_after_redistribution_keeps_weighted_sum_consistent() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100))); // A — recipient + borrower
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(50, 100))); // B — recipient
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(10, 100))); // C — liquidated

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let coll_3 = held(DOT, 3);
		assert_ok!(redistribute_for_test(DOT, PUSD, 3, coll_3));
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));

		let interest_before = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().debt.interest;
		let predicted_fee =
			crate::Pallet::<Test>::predict_borrow_upfront_fee(DOT, PUSD, 1, 200, None)
				.expect("touch projection and fee calculation succeed");
		assert_ok!(crate::Pallet::<Test>::borrow(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			200,
			None,
			None,
			Position::endpoints_only()
		));
		assert_eq!(
			Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().debt.interest,
			interest_before + predicted_fee,
			"the prediction and execution paths share the pending-touch kernel",
		);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(99), DOT, PUSD, 2));

		let state = branch_state(DOT, PUSD).unwrap();
		let vault_a = vault(DOT, PUSD, 1);
		let vault_b = vault(DOT, PUSD, 2);
		let expected = weighted(vault_a.debt.principal, rate_pct(5, 100))
			.saturating_add(weighted(vault_b.debt.principal, rate_pct(50, 100)));
		let actual = state.debt.weighted_principal.whole;
		// Same ceil-vs-floor drift as above, plus the borrow's own reconciliation.
		assert!(
			actual.abs_diff(expected) <= 2,
			"weighted_sum drift after borrow: actual={}, expected={}",
			actual,
			expected,
		);
	});
}

// Push a vault into FinalRecovery, raise the price so the fully-accrued CR
// goes above MCR, and `poke` it. Exit from FinalRecovery requires an explicit
// hint and is NOT automatic on poke: poke leaves the vault in FinalRecovery,
// and a dedicated `exit_final_recovery` extrinsic does the index re-insert with
// caller-supplied hints.
#[test]
fn final_recovery_exit_requires_explicit_hint() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(crate::Pallet::<Test>::enter_final_recovery(
			RuntimeOrigin::signed(99),
			DOT,
			PUSD,
			1
		));
		assert!(matches!(vault_status(DOT, PUSD, 1), crate::types::VaultStatus::FinalRecovery));
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(99), DOT, PUSD, 1));
		assert!(
			matches!(vault_status(DOT, PUSD, 1), crate::types::VaultStatus::FinalRecovery),
			"poke must not auto-exit FinalRecovery; exit requires an explicit hint",
		);
		assert_ok!(crate::Pallet::<Test>::exit_final_recovery(
			RuntimeOrigin::signed(99),
			DOT,
			PUSD,
			1,
			Position::endpoints_only()
		));
		assert!(matches!(vault_status(DOT, PUSD, 1), crate::types::VaultStatus::Active));
	});
}

// The production path resolves active-pool collateral before returning the
// owner's surplus.
#[test]
fn liquidation_doesnt_leak_offset_collateral_to_liquidatee() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		ActiveSpCapacity::set(1_000);

		let pool_before = collateral_balance(DOT, SP_ACCOUNT);
		let owner_before = collateral_balance(DOT, 1);
		assert_ok!(liquidate(999, DOT, PUSD, 1, 0, 0));

		let outcome = System::events()
			.into_iter()
			.find_map(|record| match record.event {
				RuntimeEvent::Vaults(crate::Event::VaultLiquidated { outcome, .. }) => {
					Some(outcome)
				},
				_ => None,
			})
			.expect("liquidation event");
		assert_eq!(
			collateral_balance(DOT, SP_ACCOUNT) - pool_before,
			outcome.active_pool.collateral
		);
		assert_eq!(collateral_balance(DOT, 1) - owner_before, outcome.owner_surplus);
	});
}

#[test]
fn back_to_back_near_empty_redistributions_preserve_accounting_identity() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(3, DOT, PUSD, 5_000, 500, rate_pct(5, 100)));

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));

		for liquidatee in [1u64, 2u64] {
			let collateral = held(DOT, liquidatee);
			assert_ok!(redistribute_for_test(DOT, PUSD, liquidatee, collateral));
			assert_accounting_identity_holds();
		}
	});
}

// A liquidation below accumulator resolution must remain an explicit branch liability.
#[test]
fn sub_resolution_liquidation_remains_explicitly_pending() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		mint_collateral(DOT, 1, 2_000_000_000_000_000_000_000_000);
		mint_collateral(DOT, 2, 3_000_000_000_000_000_000_000_000);
		// Large collateral creates a stake total that exceeds accumulator resolution.
		assert_ok!(open(
			1,
			DOT,
			PUSD,
			1_000_000_000_000_000_000_000_000,
			1_000_000,
			rate_pct(5, 100)
		));
		assert_ok!(open(
			2,
			DOT,
			PUSD,
			2_000_000_000_000_000_000_000_000,
			1_000_000,
			rate_pct(5, 100)
		));
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let debt_3 = vault(DOT, PUSD, 3).debt.total();
		let coll_3 = held(DOT, 3);
		let p1_before = vault(DOT, PUSD, 1).debt.principal;
		let p2_before = vault(DOT, PUSD, 2).debt.principal;
		assert_ok!(redistribute_for_test(DOT, PUSD, 3, coll_3));
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 2));

		assert!(debt_3 < 3_000_000, "the event must sit below the index resolution");
		let p1_after = vault(DOT, PUSD, 1).debt.principal;
		let p2_after = vault(DOT, PUSD, 2).debt.principal;
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(
			(p1_after - p1_before) +
				(p2_after - p2_before) +
				state.debt.pending_redistribution_principal,
			debt_3,
		);
		assert_eq!(
			held(DOT, crate::Pallet::<Test>::redistribution_account(&DOT, &PUSD)),
			state.pending_redistribution_collateral,
		);

		// Stake consolidation must give the remaining bearer the exact residue.
		mint_stable(PUSD, 2, 10_000);
		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(2), DOT, PUSD, 2, None));
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(2), DOT, PUSD, None));
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		let drained = branch_state(DOT, PUSD).unwrap();
		assert_eq!(drained.debt.pending_redistribution_principal, 0);
		assert_eq!(drained.pending_redistribution_collateral, 0);
		assert_eq!(held(DOT, crate::Pallet::<Test>::redistribution_account(&DOT, &PUSD)), 0,);
		assert_accounting_identity_holds();
	});
}

// Pending redistribution belongs to the branch, not to its recipients at liquidation time. It
// survives recipient changes and moves to a later sole recipient without loss.
#[test]
fn pending_residue_outlives_its_recipients_and_lands_on_a_later_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		mint_collateral(DOT, 1, 2_000_000_000_000_000_000_000_000);
		mint_collateral(DOT, 2, 3_000_000_000_000_000_000_000_000);
		// Large stakes keep the allocation below accumulator resolution.
		assert_ok!(open(
			1,
			DOT,
			PUSD,
			1_000_000_000_000_000_000_000_000,
			1_000_000,
			rate_pct(5, 100)
		));
		assert_ok!(open(
			2,
			DOT,
			PUSD,
			2_000_000_000_000_000_000_000_000,
			1_000_000,
			rate_pct(5, 100)
		));
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(redistribute_for_test(DOT, PUSD, 3, held(DOT, 3)));
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 2));
		let seeded = branch_state(DOT, PUSD).unwrap();
		let residue = seeded.debt.pending_redistribution_principal;
		let residue_collateral = seeded.pending_redistribution_collateral;
		// Neither recipient can claim an amount at this accumulator resolution.
		assert_eq!(residue, 501);
		assert_eq!(residue_collateral, 1_000);

		// The new recipient must exist before the last old one closes: a close touches the vault,
		// and a sole stake bearer would absorb the residue itself instead of leaving it pending.
		mint_stable(PUSD, 2, 10_000_000);
		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(2), DOT, PUSD, 2, None));
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(2), DOT, PUSD, None));
		assert_ok!(open(4, DOT, PUSD, 40_000, 500, rate_pct(5, 100)));
		mint_stable(PUSD, 1, 10_000_000);
		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, None));
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), DOT, PUSD, None));

		let stranded = branch_state(DOT, PUSD).unwrap();
		assert_eq!(stranded.debt.pending_redistribution_principal, residue);
		assert_eq!(stranded.pending_redistribution_collateral, residue_collateral);

		let fresh_before = vault(DOT, PUSD, 4);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 4));
		let fresh_after = vault(DOT, PUSD, 4);
		assert_eq!(fresh_after.debt.principal - fresh_before.debt.principal, residue);
		assert_eq!(fresh_after.collateral - fresh_before.collateral, residue_collateral);
		let drained = branch_state(DOT, PUSD).unwrap();
		assert_eq!(drained.debt.pending_redistribution_principal, 0);
		assert_eq!(drained.pending_redistribution_collateral, 0);
		assert_eq!(held(DOT, crate::Pallet::<Test>::redistribution_account(&DOT, &PUSD)), 0);
		assert_accounting_identity_holds();
	});
}

// The sole survivor must receive the complete pending-pool complement.
#[test]
fn sole_survivor_receives_the_exact_remainder() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 10_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let debt_2 = vault(DOT, PUSD, 2).debt.total();
		let coll_2 = held(DOT, 2);
		let principal_before = vault(DOT, PUSD, 1).debt.principal;
		assert_ok!(redistribute_for_test(DOT, PUSD, 2, coll_2));
		assert_eq!(vault(DOT, PUSD, 1).debt.principal, principal_before);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		let principal_after = vault(DOT, PUSD, 1).debt.principal;
		assert_eq!(principal_after - principal_before, debt_2);
		assert_eq!(held(DOT, 1), 11_000);
		assert_accounting_identity_holds();
	});
}

// A one-unit stake floor keeps a debt-bearing vault eligible for redistribution and liquidation.
#[test]
fn dust_ratio_stake_floors_to_one_unit_and_stays_liquidatable() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// The fixture makes the corrected stake floor to zero before the minimum applies.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 2_000_000, 500, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(1u128, 10_000u128));
		let coll_2 = held(DOT, 2);
		assert_ok!(redistribute_for_test(DOT, PUSD, 2, coll_2));

		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// The liquidation snapshot is total stake 1_000 over collateral 2_001_000, so the new
		// vault's stake floor(1_000 × 1_000 / 2_001_000) = 0 is lifted to the one-unit minimum.
		assert_eq!(vault(DOT, PUSD, 3).redistribution_stake, 1);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 3));

		set_price(DOT, FixedU128::from_rational(50u128, 100u128));
		assert_ok!(liquidate(99, DOT, PUSD, 3, 0, 0));
		assert!(!vault_exists(DOT, PUSD, 3));
		assert_accounting_identity_holds();
	});
}

#[test]
fn vault_cr_projects_lazy_redistribution_before_materialization() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let coll_3 = held(DOT, 3);
		assert_ok!(redistribute_for_test(DOT, PUSD, 3, coll_3));
		// Restore price so the view's CR is defined for vault 1.
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));

		let view_pre = crate::Pallet::<Test>::vault_cr(DOT, PUSD, 1).expect("cr");
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(99), DOT, PUSD, 1));
		let view_post = crate::Pallet::<Test>::vault_cr(DOT, PUSD, 1).expect("cr");
		// Projection must match execution before materialization.
		assert_eq!(view_pre, view_post);
	});
}

#[test]
fn touch_does_not_revive_dormant_when_interest_lifts_above_min_debt() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 100_000, 500, rate_pct(50, 100))); // co-recipient
		assert_ok!(open(2, DOT, PUSD, 10_000, 500, rate_pct(50, 100))); // target

		// Reduce vault 2 to a small dust principal (well under MinimumDebt=200)
		// via a redemption cancel so the vault becomes Dormant with non-zero
		// residual debt.
		assert_ok!(redeem_step(DOT, PUSD, 2, 99, 450, 9_000));
		assert!(crate::Pallet::<Test>::vault_status(DOT, PUSD, 2).unwrap().is_dormant());

		// Advance time so that simple interest at 50% APR pushes the residual
		// principal back over MinimumDebt=200.
		advance_time(ONE_YEAR_MS * 10);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(99), DOT, PUSD, 2));

		// Dormant status is sticky: passive accrual never re-indexes a vault.
		// Even though the debt has crossed MinimumDebt again, the vault stays
		// Dormant until an explicit, hint-bearing activation (`borrow` /
		// `activate_dormant`).
		assert!(
			vault(DOT, PUSD, 2).debt.total() >= 200,
			"sanity: accrual should have lifted residual debt back over MinimumDebt",
		);
		assert!(
			crate::Pallet::<Test>::vault_status(DOT, PUSD, 2).unwrap().is_dormant(),
			"poke must NOT auto-revive a Dormant vault; re-entry requires an explicit hint",
		);
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(
			state.dormant_redemption_target,
			Some(2),
			"the dormant slot is retained; nothing revived the vault",
		);
	});
}

// Full-lifecycle identity soak: open → liquidate with a redistribution split
// → recipient touches → partial repay → redemption → overpay-close. The
// `try_state` identities (Σ principal exact, Σ floor(rate·stake) exact,
// weighted-principal bounds) must hold at every stage, not just at the end.
#[test]
fn full_lifecycle_holds_branch_identities() {
	fn assert_identities() {
		#[cfg(feature = "try-runtime")]
		crate::try_state::do_try_state::<Test>().expect("branch identities hold");
	}
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 2_000, 800, rate_pct(25, 100)));
		assert_ok!(open(3, DOT, PUSD, 3_000, 1_000, rate_pct(50, 100)));
		assert_identities();

		// A month of accrual so touches materialise real interest.
		advance_time(30 * 24 * 3_600 * 1_000);

		// Liquidate vault 1 through the production three-way path: active
		// Stability Pool, keeper JIT, then redistribution.
		set_price(DOT, FixedU128::from_rational(55u128, 100u128));
		let keeper_8_pre = collateral_balance(DOT, 8);
		let pool_pre = collateral_balance(DOT, SP_ACCOUNT);
		ActiveSpCapacity::set(200);
		mint_stable(PUSD, 8, 200);
		assert_ok!(liquidate(8, DOT, PUSD, 1, 200, 0));
		assert_identities();
		let outcome = System::events()
			.into_iter()
			.find_map(|record| match record.event {
				RuntimeEvent::Vaults(crate::Event::VaultLiquidated { outcome, .. }) => {
					Some(outcome)
				},
				_ => None,
			})
			.expect("liquidation event");
		assert_ne!(outcome.active_pool.debt, 0);
		assert_ne!(outcome.keeper_jit.debt, 0);
		assert_ne!(outcome.redistribution.debt, 0);
		assert_eq!(collateral_balance(DOT, SP_ACCOUNT) - pool_pre, outcome.active_pool.collateral);
		assert_eq!(
			collateral_balance(DOT, 8) - keeper_8_pre,
			outcome.keeper_reward + outcome.keeper_jit.collateral
		);

		// The recipient must accrue interest from the redistribution time.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 2));
		assert_identities();

		// Partial repay exercises the full-contribution weighted-sum swap.
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(2),
			DOT,
			PUSD,
			2,
			Some(300)
		));
		assert_identities();

		// Redemption against the cheapest vault at a healthy price.
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		let recipient_7_pre = collateral_balance(DOT, 7);
		assert_ok!(redeem(DOT, PUSD, 7, 400));
		// At price 10 the redemption releases floor(debt_cancelled / 10) collateral free
		// to the recipient.
		let released = collateral_balance(DOT, 7) - recipient_7_pre;
		assert_eq!(released, 40, "redeemed 400 debt at price 10 releases 40 collateral");
		assert_identities();

		// Touch the remaining whale, then close it by overpaying.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 3));
		assert_identities();
		assert_ok!(<Pusd as frame::traits::fungible::Mutate<u64>>::transfer(
			&1,
			&3,
			stable_balance(PUSD, 1),
			frame::traits::tokens::Preservation::Expendable,
		));
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(3),
			DOT,
			PUSD,
			3,
			Some(stable_balance(PUSD, 3))
		));
		// Repay-to-zero leaves a husk; close it to release the collateral and end
		// the lifecycle with the row gone.
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(3), DOT, PUSD, None));
		assert!(!vault_exists(DOT, PUSD, 3), "vault 3 closed");
		assert_identities();
	});
}

// Interest-time debt-time accounting: interest on a redistributed share
// accrues from the liquidation moment t1, not from the branch interest-time
// origin or any absolute origin. Liquidate at t1, touch the recipient at t2, and the
// redistribution part of the accrued interest must equal
// `share · rate · (t2 - t1) / year` to within fixed-point flooring.
#[test]
fn redistributed_principal_accrues_interest_from_liquidation_moment() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 2_000, 800, rate_pct(50, 100)));

		// The 501 debt remains pending until the recipient is touched.
		set_price(DOT, FixedU128::from_rational(55u128, 100u128));
		let v_pre = vault(DOT, PUSD, 2);
		let redistributed = redistribute_for_test(DOT, PUSD, 1, 0).unwrap();
		assert_eq!(redistributed, 501);
		let v_at_record = vault(DOT, PUSD, 2);
		assert_eq!(v_at_record.debt.principal, v_pre.debt.principal);
		let minted_pre = branch_state(DOT, PUSD).unwrap().debt.minted_interest;

		// The expected interest includes own and redistributed principal for two years.
		advance_time(2 * ONE_YEAR_MS);
		let projected =
			<crate::Pallet<Test> as pusd_primitives::VaultInterface>::stablecoin_debt(&PUSD);
		assert_eq!(branch_state(DOT, PUSD).unwrap().debt.minted_interest, minted_pre);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 2));
		let v_post = vault(DOT, PUSD, 2);
		assert_eq!(v_post.debt.principal, v_at_record.debt.principal + 501);
		assert_eq!(v_post.debt.interest - v_at_record.debt.interest, 1_301);
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.debt.minted_interest - minted_pre, 1_301);
		assert_eq!(state.debt.pending_interest_attribution, 0);
		assert_eq!(projected, state.debt.outstanding());
	});
}

// Seeds pending redistribution debt of 502 with rate weight 50.2.
fn seed_redistributed_recipient() {
	register_market(DOT, PUSD);
	assert_ok!(open(1, DOT, PUSD, 10_000, 500, rate_pct(10, 100)));
	assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(20, 100)));
	// Only vault 2 must be liquidatable in this fixture.
	set_price(DOT, FixedU128::from_rational(50u128, 100u128));
	let coll_2 = held(DOT, 2);
	assert_ok!(redistribute_for_test(DOT, PUSD, 2, coll_2));
}

// A recipient touch moves pending interest to the vault and preserves projected branch debt.
#[test]
fn recipient_owned_redistribution_interest_stays_in_branch_projection() {
	build_and_execute(|| {
		seed_redistributed_recipient();

		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.debt.principal, 500);
		assert_eq!(state.debt.pending_redistribution_principal, 502);
		let accrued_at_record =
			crate::Pallet::<Test>::accrued_branch_debt(&state, Timestamp::get());
		assert_eq!(accrued_at_record, 1_003);

		advance_time(ONE_YEAR_MS);
		let accrued_after_idle_year = crate::Pallet::<Test>::accrued_branch_debt(
			&branch_state(DOT, PUSD).unwrap(),
			Timestamp::get(),
		);
		assert_eq!(accrued_after_idle_year, 1_104);

		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(99), DOT, PUSD, 1));

		let state = branch_state(DOT, PUSD).unwrap();
		let vault = vault(DOT, PUSD, 1);
		assert_eq!(vault.debt.interest, 101);
		assert!(vault.interest_remainder != 0);
		assert_eq!(
			crate::Pallet::<Test>::accrued_branch_debt(&state, Timestamp::get()),
			accrued_after_idle_year,
		);
		assert_accounting_identity_holds();
	});
}

// Branch refresh frequency must not change projected debt at the same time.
#[test]
fn branch_debt_projection_is_refresh_cadence_independent() {
	let run = |refreshes: u64| {
		new_test_ext().execute_with(|| {
			seed_redistributed_recipient();
			assert_ok!(open(9, DOT, PUSD, 1_000, 300, rate_pct(5, 100)));

			for step in 0..10u64 {
				advance_time(ONE_YEAR_MS / 10);
				if step < refreshes {
					assert_ok!(crate::Pallet::<Test>::poke(
						RuntimeOrigin::signed(99),
						DOT,
						PUSD,
						9
					));
				}
			}
			crate::Pallet::<Test>::accrued_branch_debt(
				&branch_state(DOT, PUSD).unwrap(),
				Timestamp::get(),
			)
		})
	};
	assert_eq!(run(9), run(0));
}
