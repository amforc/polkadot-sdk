use crate::{
	mock::*,
	pallet::Vaults,
	tests::{rate_pct, vault_status},
};
use frame::traits::{
	fungible::{Inspect as FungibleInspect, Mutate as FungibleMutate},
	tokens::Preservation,
};
use pallet_linked_list::SortedListInterface;

const ONE_DAY_MS: Moment = 24 * 3_600 * 1_000;

fn interest_time_at(asset: AssetId, now: Moment) -> Moment {
	crate::pallet::BranchStates::<Test>::get(asset, PUSD)
		.unwrap()
		.interest_time(now)
}

// Helper: top up `who`'s pUSD balance by `delta` so that subsequent
// repay_for / etc. doesn't trip on the upfront-fee residual.
fn top_up_pusd(who: AccountId, donor: AccountId, delta: Balance) {
	if delta == 0 {
		return;
	}
	assert_ok!(<Pusd as FungibleMutate<AccountId>>::transfer(
		&donor,
		&who,
		delta,
		Preservation::Expendable,
	));
}

#[test]
fn open_sets_annual_rate() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 2_000, rate_pct(37, 100)));
		assert_ok!(open(2, DOT, 1_000, 2_000, rate_pct(100, 100)));
		assert_eq!(Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().annual_rate, rate_pct(37, 100));
		assert_eq!(Vaults::<Test>::get((DOT, PUSD, 2)).unwrap().annual_rate, rate_pct(100, 100));
	});
}

#[test]
fn open_sets_last_interest_time_to_now() {
	build_and_execute(|| {
		register_default_branch();
		let t0 = pallet_timestamp::Pallet::<Test>::get();
		assert_ok!(open(1, DOT, 1_000, 2_000, rate_pct(5, 100)));
		assert_eq!(
			Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().last_interest_time,
			interest_time_at(DOT, t0)
		);
		advance_time(1_000);
		let t1 = pallet_timestamp::Pallet::<Test>::get();
		assert_ok!(open(2, DOT, 1_000, 2_000, rate_pct(5, 100)));
		assert_eq!(
			Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().last_interest_time,
			interest_time_at(DOT, t0)
		);
		assert_eq!(
			Vaults::<Test>::get((DOT, PUSD, 2)).unwrap().last_interest_time,
			interest_time_at(DOT, t1)
		);
		// Vault 1 was untouched by vault 2's open; poking it now settles it to the
		// current interest time (t1), confirming a poke advances the clock.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		assert_eq!(
			Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().last_interest_time,
			interest_time_at(DOT, t1)
		);
	});
}

// A vault is addressed by the `(collateral_id, caller)` storage key, so the
// caller can only ever reach their own vault; another account simply has no
// row to mutate. Access control falls out of the storage layout: changing a
// non-owner's rate fails with `VaultNotFound`, not a dedicated owner-check
// error.
#[test]
fn change_rate_from_non_owner_returns_vault_not_found() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 2_000, rate_pct(37, 100)));
		assert_noop!(
			crate::Pallet::<Test>::change_rate(
				RuntimeOrigin::signed(2),
				DOT,
				PUSD,
				rate_pct(50, 100),
				Position::endpoints_only()
			),
			crate::Error::<Test>::VaultNotFound
		);
	});
}

// This pins only that `change_rate` records the new rate. The interest folded on
// the touch and the upfront fee charged on a premature change are verified
// exactly by `change_rate_post_cooldown_full_state` and
// `change_rate_premature_increases_recorded_debt_by_fee` respectively.
#[test]
fn change_rate_sets_new_rate() {
	build_and_execute(|| {
		register_default_branch();
		// Open three vaults at 50%, then change each to a different rate
		// after the cooldown elapses (so no upfront fees intrude here).
		for who in 1u64..=3 {
			assert_ok!(open(who, DOT, 1_000, 2_000, rate_pct(50, 100)));
		}
		advance_time(2 * ONE_DAY_MS);
		assert_ok!(crate::Pallet::<Test>::change_rate(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			rate_pct(1, 200),
			Position::endpoints_only()
		));
		assert_ok!(crate::Pallet::<Test>::change_rate(
			RuntimeOrigin::signed(2),
			DOT,
			PUSD,
			rate_pct(60, 100),
			Position::endpoints_only()
		));
		assert_ok!(crate::Pallet::<Test>::change_rate(
			RuntimeOrigin::signed(3),
			DOT,
			PUSD,
			rate_pct(100, 100),
			Position::endpoints_only()
		));
		assert_eq!(Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().annual_rate, rate_pct(1, 200));
		assert_eq!(Vaults::<Test>::get((DOT, PUSD, 2)).unwrap().annual_rate, rate_pct(60, 100));
		assert_eq!(Vaults::<Test>::get((DOT, PUSD, 3)).unwrap().annual_rate, rate_pct(100, 100));
	});
}

// Post-cooldown change_rate refreshes last_interest_time and folds the elapsed
// simple interest into `vault.debt.interest`. With no upfront fee charged
// (cooldown elapsed), the interest-bearing principal is unchanged.
//
// To pin the interest change *exactly* rather than with a `>=`, we settle the
// elapsed interest with an explicit poke first (asserting it was folded in), so
// the subsequent same-timestamp, fee-free change adds precisely nothing.
#[test]
fn change_rate_post_cooldown_full_state() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 2_000, rate_pct(50, 100)));
		let interest_at_open = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().debt.interest;
		// Advance one full cooldown, then poke so the elapsed interest is settled
		// before the rate change (which then has nothing left to materialise).
		advance_time(ONE_DAY_MS);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		let v_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		assert!(
			v_pre.debt.interest > interest_at_open,
			"a day of interest was folded in by the poke"
		);

		let now_before_call = pallet_timestamp::Pallet::<Test>::get();
		assert_eq!(
			crate::Pallet::<Test>::predict_rate_change_upfront_fee(DOT, PUSD, 1, rate_pct(75, 100)),
			0,
			"post-cooldown rate change should quote no upfront fee",
		);
		assert_ok!(crate::Pallet::<Test>::change_rate(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			rate_pct(75, 100),
			Position::endpoints_only()
		));
		let v_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();

		assert_eq!(v_post.last_interest_time, interest_time_at(DOT, now_before_call));
		assert_eq!(v_post.debt.principal, v_pre.debt.principal);
		// Fee-free and same interest-time as the poke: interest is exactly unchanged.
		assert_eq!(v_post.debt.interest, v_pre.debt.interest);
		assert_eq!(v_post.annual_rate, rate_pct(75, 100));
		// The defining side effect of a rate change: the cooldown clock is stamped
		// to the wall-clock moment of the call (`do_change_rate` in `dispatchable_impls.rs`).
		assert_eq!(v_post.last_rate_update, now_before_call);
	});
}

// A within-cooldown rate change charges an upfront fee that lands in
// `vault.debt.interest` and bumps recorded debt by exactly that fee.
#[test]
fn change_rate_premature_increases_recorded_debt_by_fee() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 2_000, rate_pct(50, 100)));
		advance_time(ONE_DAY_MS / 2);
		// Settle pending interest into accrued first so the change_rate
		// delta isolates the upfront-fee component.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(1), DOT, PUSD, 1));
		let v_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();

		let predicted =
			crate::Pallet::<Test>::predict_rate_change_upfront_fee(DOT, PUSD, 1, rate_pct(75, 100));
		assert!(predicted > 0, "premature change at debt=2000 must charge a fee");

		assert_ok!(crate::Pallet::<Test>::change_rate(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			rate_pct(75, 100),
			Position::endpoints_only()
		));
		let v_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		assert_eq!(v_post.debt.principal, v_pre.debt.principal);
		assert_eq!(v_post.debt.interest, v_pre.debt.interest + predicted);
	});
}

// Collateral/debt adjustments without a rate change keep the DLL ordering.
#[test]
fn collateral_or_debt_adjust_does_not_reorder_dll() {
	build_and_execute(|| {
		register_default_branch();
		for (who, pct) in [(1u64, 10), (2, 20), (3, 30), (4, 40), (5, 50)] {
			assert_ok!(open(who, DOT, 1_000, 500, rate_pct(pct, 100)));
		}
		let order_before = <LinkedList as SortedListInterface<VaultList, u64>>::iter_from_tail(
			&rate_list(DOT),
			10,
		);
		assert_ok!(crate::Pallet::<Test>::deposit_collateral_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			100
		));
		assert_ok!(crate::Pallet::<Test>::borrow(
			RuntimeOrigin::signed(2),
			DOT,
			PUSD,
			50,
			None,
			None,
			Position::endpoints_only()
		));
		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(3), DOT, PUSD, 3, 50));
		let order_after = <LinkedList as SortedListInterface<VaultList, u64>>::iter_from_tail(
			&rate_list(DOT),
			10,
		);
		assert_eq!(order_before, order_after);
	});
}

// Borrow refreshes last_interest_time, applies pending into accrued,
// charges the upfront fee, and grows recorded principal by exactly the
// borrowed amount.
//
// To isolate the upfront-fee delta from the materialised simple-interest
// accrual we poke the vault first (folding sim-pending into accrued).
#[test]
fn borrow_full_state_changes() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 3_000, 2_000, rate_pct(25, 100)));
		advance_time(ONE_DAY_MS);
		// Settle pending into accrued so the borrow delta isolates the
		// upfront fee.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(1), DOT, PUSD, 1));

		let v_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		let predicted_fee =
			crate::Pallet::<Test>::predict_borrow_upfront_fee(DOT, PUSD, 1, 500, None);
		let now_before_call = pallet_timestamp::Pallet::<Test>::get();

		assert_ok!(crate::Pallet::<Test>::borrow(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			500,
			None,
			None,
			Position::endpoints_only()
		));
		let v_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();

		assert_eq!(v_post.last_interest_time, interest_time_at(DOT, now_before_call));
		assert_eq!(v_post.debt.principal, v_pre.debt.principal + 500);
		assert_eq!(v_post.debt.interest, v_pre.debt.interest + predicted_fee);
	});
}

#[test]
fn borrow_with_new_rate_updates_rate_reorders_index_and_charges_predicted_fee() {
	build_and_execute(|| {
		register_default_branch();
		for (who, pct) in [(1u64, 20), (2, 10), (3, 30)] {
			assert_ok!(open(who, DOT, 5_000, 2_000, rate_pct(pct, 100)));
		}
		let v_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		// The upfront fee is charged at the branch *average* borrow rate (via
		// `simulate_borrow`), not the vault's own rate — confirmed by the exact
		// `assert_eq!(v_post.debt.interest, v_pre.debt.interest + predicted)` below.
		let predicted = crate::Pallet::<Test>::predict_borrow_upfront_fee(
			DOT,
			PUSD,
			1,
			500,
			Some(rate_pct(5, 100)),
		);
		assert!(predicted > 0);
		let now_before_call = pallet_timestamp::Pallet::<Test>::get();

		assert_ok!(crate::Pallet::<Test>::borrow(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			500,
			Some(rate_pct(5, 100)),
			None,
			Position::endpoints_only()
		));

		let v_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		assert_eq!(v_post.annual_rate, rate_pct(5, 100));
		assert_eq!(v_post.last_rate_update, now_before_call);
		assert_eq!(v_post.debt.principal, v_pre.debt.principal + 500);
		assert_eq!(v_post.debt.interest, v_pre.debt.interest + predicted);
		let order = <LinkedList as SortedListInterface<VaultList, u64>>::iter_from_tail(
			&rate_list(DOT),
			10,
		);
		assert_eq!(order, alloc::vec![1, 2, 3]);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::BorrowRateChanged {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			old_rate: rate_pct(20, 100),
			new_rate: rate_pct(5, 100),
		}));
	});
}

#[test]
fn borrow_with_new_rate_rejects_rate_out_of_bounds_without_state_change() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 5_000, 2_000, rate_pct(20, 100)));
		let v_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		let balance_pre = pusd_balance(1);

		assert_noop!(
			crate::Pallet::<Test>::borrow(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				500,
				// Above the branch `maximum_borrow_rate` (400%).
				Some(rate_pct(401, 100)),
				None,
				Position::endpoints_only()
			),
			crate::Error::<Test>::RateOutOfBounds
		);

		assert_eq!(Vaults::<Test>::get((DOT, PUSD, 1)).unwrap(), v_pre);
		assert_eq!(pusd_balance(1), balance_pre);
	});
}

// Borrowing while passing the vault's *current* rate is a pure debt increase:
// it must not charge the full-principal rate-change fee nor reset the cooldown,
// mirroring `change_rate`'s equal-rate no-op.
#[test]
fn borrow_with_unchanged_rate_charges_no_rate_change_fee() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 5_000, 2_000, rate_pct(20, 100)));
		let opened_at = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().last_rate_update;

		// Advance only part-way into the rate-adjustment cooldown so a (buggy)
		// rate-change fee would still apply if the rate were treated as changed.
		advance_time(ONE_DAY_MS / 2);

		let fee_pure = crate::Pallet::<Test>::predict_borrow_upfront_fee(DOT, PUSD, 1, 500, None);
		let fee_same_rate = crate::Pallet::<Test>::predict_borrow_upfront_fee(
			DOT,
			PUSD,
			1,
			500,
			Some(rate_pct(20, 100)),
		);
		assert_eq!(fee_pure, fee_same_rate, "an unchanged rate must not add a rate-change fee");

		assert_ok!(crate::Pallet::<Test>::borrow(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			500,
			Some(rate_pct(20, 100)),
			None,
			Position::endpoints_only()
		));

		let v_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		assert_eq!(v_post.annual_rate, rate_pct(20, 100));
		assert_eq!(v_post.last_rate_update, opened_at, "no-op rate must not reset the cooldown");
		assert!(
			!System::events().iter().any(|e| matches!(
				e.event,
				RuntimeEvent::Vaults(crate::Event::BorrowRateChanged { .. })
			)),
			"no BorrowRateChanged event for an unchanged rate"
		);
	});
}

// Repay refreshes last_interest_time, settles pending interest, reduces
// entire debt by the repaid amount, and reduces recorded debt by the
// principal portion.
#[test]
fn repay_full_state_changes() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 3_000, 3_000, rate_pct(25, 100)));
		advance_time(ONE_DAY_MS);

		// Settle pending interest into a known-quantity accrued, then top up
		// the borrower's pUSD so they have enough to repay both principal
		// and accrued.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(1), DOT, PUSD, 1));
		let v_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		// Borrow more pUSD into a second account so we can shuttle some over.
		assert_ok!(open(2, DOT, 5_000, 3_000, rate_pct(25, 100)));
		top_up_pusd(1, 2, v_pre.debt.interest + 500);

		let now_before_call = pallet_timestamp::Pallet::<Test>::get();
		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, 500));
		let v_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();

		assert_eq!(v_post.last_interest_time, interest_time_at(DOT, now_before_call));

		// Entire debt reduces by the repaid amount (since `poke` already
		// folded prior pending interest into accrued, `repay_for(500)`
		// removes 500 cleanly from the entire-debt sum).
		let entire_pre = v_pre.debt.principal + v_pre.debt.interest;
		let entire_post = v_post.debt.principal + v_post.debt.interest;
		assert_eq!(entire_post, entire_pre - 500);

		// Recorded debt decreases by the principal portion. Since
		// accrued_interest > 0 and repay applies to accrued first, principal
		// reduction is `500 - min(500, accrued)`. Here we kept the accrued
		// small so the bulk of 500 hit principal.
		let pay_accrued = core::cmp::min(500, v_pre.debt.interest);
		let pay_principal = 500 - pay_accrued;
		assert_eq!(v_post.debt.principal, v_pre.debt.principal - pay_principal);
		// Interest is settled *before* principal: recorded interest drops by
		// exactly the accrued portion paid. Here 500 exceeds the small accrued, so
		// interest is fully cleared and the remainder hits principal.
		assert_eq!(v_post.debt.interest, v_pre.debt.interest - pay_accrued);
	});
}
// Poke is permissionless, refreshes last_interest_time, materialises
// sim-pending into accrued, and leaves principal unchanged. Other tests use
// `poke` only as a setup step; this one isolates the poke path itself.
//
// Storage exposes only `interest_bearing_debt + accrued_interest`, i.e. the
// recorded debt — which does not include the live sim-pending accrual. We pin
// the per-component changes instead of an entire-debt invariant.
#[test]
fn poke_full_state_changes() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 3_000, 2_000, rate_pct(25, 100)));
		advance_time(ONE_DAY_MS);

		let v_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		let now_before_call = pallet_timestamp::Pallet::<Test>::get();

		// Permissionless: any signed origin (here, account 2) can poke
		// account 1's vault.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(2), DOT, PUSD, 1));
		let v_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();

		assert_eq!(v_post.last_interest_time, interest_time_at(DOT, now_before_call));
		assert_eq!(v_post.debt.principal, v_pre.debt.principal);
		// One day at 25% on 2_000 principal materialises exactly
		// floor(2_000 * 0.25 * 1day / year) = 1 unit on top of the pending open fee.
		assert_eq!(v_post.debt.interest, v_pre.debt.interest + 1);
	});
}

// A full repayment leaves a live zero-debt Dormant husk (it no longer
// auto-closes), so the row survives and stays pokeable — poking it is a no-op on
// zero debt but must not error with `VaultNotFound`.
#[test]
fn poke_after_full_repayment_pokes_dormant_husk() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 3_000, 2_000, rate_pct(25, 100)));
		assert_ok!(open(2, DOT, 3_000, 2_000, rate_pct(25, 100)));
		// Repay all of vault 1's debt — first poke to settle accrued, then
		// transfer accrued from vault 2 to cover the residual.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(1), DOT, PUSD, 1));
		let v = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		let total = v.debt.principal + v.debt.interest;
		top_up_pusd(1, 2, v.debt.interest);
		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, total));
		// The husk survives as a Dormant zero-debt row and remains pokeable.
		assert!(vault_status(DOT, 1).is_dormant());
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(3), DOT, PUSD, 1));
		assert_eq!(Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().debt.total(), 0);
	});
}

// Redemption refreshes last_interest_time on the redeemed vault, applies
// pending interest, reduces entire debt by the redeemed amount, and reduces
// recorded debt accordingly. Tested through the `VaultInterface`
// trait (no `redeem` extrinsic exists yet).
#[test]
fn redemption_full_state_changes() {
	build_and_execute(|| {
		register_default_branch();
		// Six vaults across ascending rates so the rate index has a clear
		// "lowest rate" target at the tail.
		for (who, pct) in [(1u64, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6)] {
			assert_ok!(open(who, DOT, 1_000, 500, rate_pct(pct, 100)));
		}
		// Settle acct 1's recorded interest, then let a full year accrue so it
		// carries non-zero *pending* interest at redemption time. The redemption
		// must poke that pending interest before cancelling debt (otherwise the
		// entire-debt arithmetic below would not close), and we pin the exact
		// accrued amount rather than relying on a floor-to-zero coincidence.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		let v_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		advance_time(pusd_primitives::MILLIS_PER_YEAR);
		// Exact simple interest on acct 1 over the year: 500 principal * 1% = 5.
		let accrued_year: Balance = 5;

		let now_before_call = pallet_timestamp::Pallet::<Test>::get();
		// Collateral-leg baselines before the redemption.
		let redeemer_collateral_pre = collateral_balance(DOT, 5);
		let branch_collateral_pre =
			crate::pallet::BranchStates::<Test>::get(DOT, PUSD).unwrap().total_collateral;
		// Redeem 200 pUSD from acct 5 (the redeemer) — the helper uses the
		// rate-index tail, which is acct 1 (lowest rate).
		let target = redeem(DOT, 5, 200).expect("redeem ok");
		assert_eq!(target, 1);

		let v_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		// The redemption refreshed acct 1's interest clock — it poked the target.
		assert_eq!(v_post.last_interest_time, interest_time_at(DOT, now_before_call));

		// Entire debt = pre + the freshly-settled year interest − 200 redeemed.
		let entire_pre = v_pre.debt.principal + v_pre.debt.interest;
		let entire_post = v_post.debt.principal + v_post.debt.interest;
		assert_eq!(entire_post, entire_pre + accrued_year - 200);

		// Interest-first cancellation of the 200: the (recorded + freshly accrued)
		// interest is paid before principal.
		let interest_at_redeem = v_pre.debt.interest + accrued_year;
		let pay_accrued = core::cmp::min(200, interest_at_redeem);
		let pay_principal = 200 - pay_accrued;
		assert_eq!(v_post.debt.interest, interest_at_redeem - pay_accrued);
		assert_eq!(v_post.debt.principal, v_pre.debt.principal - pay_principal);

		// Collateral leg: 200 pUSD / price 10 = 20 collateral released from acct 1's
		// hold to the redeemer, who receives it free (not held).
		let collateral_released: Balance = 20;
		assert_eq!(v_post.collateral, v_pre.collateral - collateral_released); // 1_000 -> 980
		assert_eq!(held(DOT, 1), v_pre.collateral - collateral_released);
		assert_eq!(collateral_balance(DOT, 5), redeemer_collateral_pre + collateral_released);
		assert_eq!(
			crate::pallet::BranchStates::<Test>::get(DOT, PUSD).unwrap().total_collateral,
			branch_collateral_pre - collateral_released,
		);

		assert!(vault_status(DOT, 1).is_active());
	});
}

// Fee routing, end to end. The upfront fee is minted and handed whole to the
// mock's `DealWithFees`, which splits per `SpFeeShare`: the SP share is
// dropped (rescinding its mint) and the residual resolves to `FEE_DEST`. So
// `total_issuance` grows by the borrow amount plus that routed residual (the
// dropped SP share nets to zero), and we can pin the exact fee that reaches
// the destination in pUSD. The fee is also recorded as debt on
// `state.debt.minted_interest` and `vault.debt.interest`.
#[test]
fn open_mints_borrow_amount_and_routes_fee_residual_to_handler() {
	build_and_execute(|| {
		register_default_branch();
		let total_pre = <Pusd as FungibleInspect<AccountId>>::total_issuance();
		let predicted_fee =
			crate::Pallet::<Test>::predict_open_upfront_fee(DOT, PUSD, 2_000, rate_pct(10, 100));
		assert!(predicted_fee > 0);

		assert_ok!(open(1, DOT, 1_000, 2_000, rate_pct(10, 100)));

		// `DealWithFees` takes the `SpFeeShare` cut; the residual reaches FEE_DEST.
		let sp_share = SpFeeShare::get() * predicted_fee;
		let fee_residual = predicted_fee - sp_share;
		assert_eq!(fee_dest_balance(), fee_residual, "residual fee routed to FEE_DEST in pUSD");

		let total_post = <Pusd as FungibleInspect<AccountId>>::total_issuance();
		assert_eq!(total_post, total_pre + 2_000 + fee_residual);
		assert_eq!(<Pusd as FungibleInspect<AccountId>>::balance(&1), 2_000);
		let v = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		assert_eq!(v.debt.interest, predicted_fee);
		let state = crate::pallet::BranchStates::<Test>::get(DOT, PUSD).unwrap();
		assert_eq!(state.debt.minted_interest, predicted_fee);
	});
}

// Liquidate vault C, then permissionlessly poke A's vault — A absorbs C's debt
// and collateral via redistribution, so A's debt grows by its accrued interest
// plus its redistribution share. The redistribution interest is accrued
// per-stake with floor rounding, so rather than reconstruct the exact figure
// here we pin the invariant that matters: A's debt never decreases across a
// liquidation+redistribution cycle. (`redistribution_accounting.rs` pins the
// exact aggregate figures.)
#[test]
fn poke_after_liquidation_applies_redistribution_gains() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 3_000, 2_000, rate_pct(25, 100)));
		assert_ok!(open(3, DOT, 1_000, 2_000, rate_pct(25, 100)));
		set_price(DOT, FixedU128::from_rational(15u128, 10u128));

		let v_a_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		let entire_a_pre = v_a_pre.debt.principal + v_a_pre.debt.interest;

		assert_ok!(liquidate(DOT, 3));

		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(2), DOT, PUSD, 1));
		let v_a_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		let entire_a_post = v_a_post.debt.principal + v_a_post.debt.interest;
		assert!(
			entire_a_post >= entire_a_pre,
			"A's debt should not decrease across a liquidation cycle"
		);
	});
}

// Accrued simple interest is path-independent up to per-poke floor dust: poking
// a vault repeatedly over a window accrues the same interest as a single poke at
// the end, minus at most one base unit of floor rounding per intermediate poke.
// (Interest accrues on a fixed principal — accrued interest is not folded back
// into principal — so it never compounds.)
#[test]
fn accrued_interest_is_path_independent_across_pokes() {
	build_and_execute(|| {
		register_default_branch();
		// Two identical vaults with a large principal so the daily floor dust is a
		// negligible fraction. Acct 1 is poked daily; acct 2 only once at the end.
		assert_ok!(open(1, DOT, 1_000_000, 1_000_000, rate_pct(50, 100)));
		assert_ok!(open(2, DOT, 1_000_000, 1_000_000, rate_pct(50, 100)));
		let base1 = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().debt.interest;
		let base2 = Vaults::<Test>::get((DOT, PUSD, 2)).unwrap().debt.interest;

		const DAYS: u64 = 10;
		for _ in 0..DAYS {
			advance_time(ONE_DAY_MS);
			assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		}
		// One poke over the whole window.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 2));

		let many = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().debt.interest - base1;
		let once = Vaults::<Test>::get((DOT, PUSD, 2)).unwrap().debt.interest - base2;
		// Exact: each daily poke accrues floor(1e6 * 0.5 * 1day / year) = 1_368, so ten
		// pokes give 13_680; a single poke over ten days accrues
		// floor(1e6 * 0.5 * 10days / year) = 13_689. Frequent poking loses exactly 9
		// units to per-poke flooring (bounded by one unit per intermediate poke).
		assert_eq!(many, 13_680, "ten daily pokes: 10 * floor(1e6 * 0.5 * 1day / year)");
		assert_eq!(once, 13_689, "one poke over ten days: floor(1e6 * 0.5 * 10days / year)");
		assert!(once - many <= DAYS as u128, "floor dust bounded by one unit per poke");
	});
}

// The open fee is priced by the same `apply_borrow` path every borrow uses.
// Pin it against the closed form it must equal: the post-open debt-weighted
// average rate applied to the new debt over the upfront-fee period.
#[test]
fn open_fee_matches_post_open_average_rate_closed_form() {
	build_and_execute(|| {
		register_default_branch();
		// Pre-existing debt at 5% so the average is a genuine blend.
		assert_ok!(open(1, DOT, 10_000, 500, rate_pct(5, 100)));

		let state = crate::pallet::BranchStates::<Test>::get(DOT, PUSD).unwrap();
		let config = crate::pallet::BranchConfigs::<Test>::get((DOT, PUSD)).unwrap();
		let new_debt: Balance = 1_000;
		let new_rate = rate_pct(10, 100);
		let total_ib =
			state.debt.principal + state.debt.pending_redistribution_principal + new_debt;
		let weighted = state.debt.weighted_principal_sum + new_rate.saturating_mul_int(new_debt);
		let avg = crate::math::average_branch_rate(weighted, total_ib);
		let expected = crate::math::simple_interest_ceil(new_debt, avg, config.upfront_fee_period);
		assert!(expected > 0);

		assert_eq!(
			crate::Pallet::<Test>::predict_open_upfront_fee(DOT, PUSD, new_debt, new_rate),
			expected
		);
		assert_ok!(open(2, DOT, 20_000, new_debt, new_rate));
		let vault = Vaults::<Test>::get((DOT, PUSD, 2)).unwrap();
		assert_eq!(vault.debt.interest, expected, "charged fee matches the quote");
	});
}
