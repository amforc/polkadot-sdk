use crate::{
	mock::*,
	tests::{assert_event, rate_pct, vault_events, vault_status, ONE_DAY_MS, ONE_YEAR_MS},
};
use frame::traits::{
	fungible::{Inspect as FungibleInspect, Mutate as FungibleMutate},
	tokens::Preservation,
};

fn interest_time_at(asset: AssetId, now: Moment) -> Moment {
	branch_state(asset, PUSD).unwrap().interest_time(now)
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
fn open_sets_last_interest_time_to_now() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		let t0 = pallet_timestamp::Pallet::<Test>::get();
		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		assert_eq!(vault(DOT, PUSD, 1).last_interest_time, interest_time_at(DOT, t0));
		advance_time(1_000);
		let t1 = pallet_timestamp::Pallet::<Test>::get();
		assert_ok!(open(2, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		assert_eq!(vault(DOT, PUSD, 1).last_interest_time, interest_time_at(DOT, t0));
		assert_eq!(vault(DOT, PUSD, 2).last_interest_time, interest_time_at(DOT, t1));
		// Vault 1 was untouched by vault 2's open; poking it now settles it to the
		// current interest time (t1), confirming a poke advances the clock.
		assert_ok!(poke(9, DOT, PUSD, 1));
		assert_eq!(vault(DOT, PUSD, 1).last_interest_time, interest_time_at(DOT, t1));
	});
}

#[test]
fn aggregate_interest_overflow_is_rejected_without_advancing_state() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(100, 100)));
		let mut state = branch_state(DOT, PUSD).unwrap();
		state.debt.minted_interest = Balance::MAX;
		advance_time(ONE_DAY_MS);
		let before = state.clone();

		assert_eq!(
			crate::Pallet::<Test>::accrue_aggregate_interest(
				&mut state,
				pallet_timestamp::Pallet::<Test>::get()
			),
			Err(crate::Error::<Test>::ArithmeticOverflow.into())
		);
		assert_eq!(state, before);
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
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(37, 100)));
		assert_noop!(
			change_rate(2, DOT, PUSD, rate_pct(50, 100)),
			crate::Error::<Test>::VaultNotFound
		);
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
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(50, 100)));
		let interest_at_open = vault(DOT, PUSD, 1).debt.interest;
		// Advance one full cooldown, then poke so the elapsed interest is settled
		// before the rate change (which then has nothing left to materialise).
		advance_time(ONE_DAY_MS);
		assert_ok!(poke(9, DOT, PUSD, 1));
		let v_pre = vault(DOT, PUSD, 1);
		assert!(
			v_pre.debt.interest > interest_at_open,
			"a day of interest was folded in by the poke"
		);

		let now_before_call = pallet_timestamp::Pallet::<Test>::get();
		assert_eq!(
			crate::Pallet::<Test>::predict_borrow_upfront_fee(
				DOT,
				PUSD,
				1,
				0,
				Some(rate_pct(75, 100))
			)
			.expect("registered market and vault"),
			0,
			"post-cooldown rate change should quote no upfront fee",
		);
		System::reset_events();
		assert_ok!(change_rate(1, DOT, PUSD, rate_pct(75, 100)));
		let v_post = vault(DOT, PUSD, 1);

		assert_eq!(v_post.last_interest_time, interest_time_at(DOT, now_before_call));
		assert_eq!(v_post.debt.principal, v_pre.debt.principal);
		// Fee-free and same interest-time as the poke: interest is exactly unchanged.
		assert_eq!(v_post.debt.interest, v_pre.debt.interest);
		assert_eq!(v_post.annual_rate, rate_pct(75, 100));
		// The defining side effect of a rate change: the cooldown clock is stamped
		// to the wall-clock moment of the call (`do_change_rate` in `dispatchable_impls.rs`).
		assert_eq!(v_post.last_rate_update, now_before_call);
		assert_event(crate::Event::BorrowRateChanged {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			old_rate: rate_pct(50, 100),
			new_rate: rate_pct(75, 100),
		});
		assert!(!vault_events()
			.iter()
			.any(|event| matches!(event, crate::Event::UpfrontFeeCharged { .. })));
	});
}

// A within-cooldown rate change charges an upfront fee that lands in
// `vault.debt.interest` and bumps recorded debt by exactly that fee.
#[test]
fn change_rate_premature_increases_recorded_debt_by_fee() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(50, 100)));
		advance_time(ONE_DAY_MS / 2);
		// Settle pending interest into accrued first so the change_rate
		// delta isolates the upfront-fee component.
		assert_ok!(poke(1, DOT, PUSD, 1));
		let v_pre = vault(DOT, PUSD, 1);

		let predicted = crate::Pallet::<Test>::predict_borrow_upfront_fee(
			DOT,
			PUSD,
			1,
			0,
			Some(rate_pct(75, 100)),
		)
		.expect("registered market and vault");
		// The sole vault's new 75% is the average: ceil(2_000 · 75% · 7d / 365.25d) = 29.
		assert_eq!(predicted, 29, "premature change at debt=2000 must charge a fee");

		assert_ok!(change_rate(1, DOT, PUSD, rate_pct(75, 100)));
		let v_post = vault(DOT, PUSD, 1);
		assert_eq!(v_post.debt.principal, v_pre.debt.principal);
		assert_eq!(v_post.debt.interest, v_pre.debt.interest + predicted);
		assert_event(crate::Event::UpfrontFeeCharged {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			amount: 29,
		});
		assert_event(crate::Event::BorrowRateChanged {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			old_rate: rate_pct(50, 100),
			new_rate: rate_pct(75, 100),
		});
	});
}

// Collateral/debt adjustments without a rate change keep the DLL ordering.
#[test]
fn collateral_or_debt_adjust_does_not_reorder_dll() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		for (who, pct) in [(1u64, 10), (2, 20), (3, 30), (4, 40), (5, 50)] {
			assert_ok!(open(who, DOT, PUSD, 1_000, 500, rate_pct(pct, 100)));
		}
		let order_before = LinkedList::iter_from_tail(rate_list(DOT, PUSD), 10);
		assert_ok!(deposit_collateral(1, DOT, PUSD, 1, 100));
		assert_ok!(borrow(2, DOT, PUSD, 50, None));
		assert_ok!(repay(3, DOT, PUSD, 3, Some(50)));
		let order_after = LinkedList::iter_from_tail(rate_list(DOT, PUSD), 10);
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
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 3_000, 2_000, rate_pct(25, 100)));
		advance_time(ONE_DAY_MS);
		// Settle pending into accrued so the borrow delta isolates the
		// upfront fee.
		assert_ok!(poke(1, DOT, PUSD, 1));

		let v_pre = vault(DOT, PUSD, 1);
		let predicted_fee =
			crate::Pallet::<Test>::predict_borrow_upfront_fee(DOT, PUSD, 1, 500, None)
				.expect("registered market and vault");
		// ceil(500 · 25% · 7d / 365.25d) = 3.
		assert_eq!(predicted_fee, 3);
		let now_before_call = pallet_timestamp::Pallet::<Test>::get();

		assert_ok!(borrow(1, DOT, PUSD, 500, None));
		let v_post = vault(DOT, PUSD, 1);

		assert_eq!(v_post.last_interest_time, interest_time_at(DOT, now_before_call));
		assert_eq!(v_post.debt.principal, v_pre.debt.principal + 500);
		assert_eq!(v_post.debt.interest, v_pre.debt.interest + predicted_fee);
		assert_event(crate::Event::Borrowed {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			recipient: 1,
			amount: 500,
		});
	});
}

#[test]
fn borrow_with_new_rate_updates_rate_reorders_index_and_charges_predicted_fee() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		for (who, pct) in [(1u64, 20), (2, 10), (3, 30)] {
			assert_ok!(open(who, DOT, PUSD, 5_000, 2_000, rate_pct(pct, 100)));
		}
		let v_pre = vault(DOT, PUSD, 1);
		// The rate change is premature, so the fee covers all 2_500 of the vault's principal, at
		// the post-borrow *average* rate (2_500 · 5% + 2_000 · 10% + 2_000 · 30%) / 6_500 ≈ 14.23%,
		// not the vault's own 5%: ceil(2_500 · 14.23% · 7d / 365.25d) = ceil(6.82) = 7, where the
		// own rate would give 3. The dispatch below must then charge exactly the quote.
		let predicted = crate::Pallet::<Test>::predict_borrow_upfront_fee(
			DOT,
			PUSD,
			1,
			500,
			Some(rate_pct(5, 100)),
		)
		.expect("registered market and vault");
		assert_eq!(predicted, 7);
		let now_before_call = pallet_timestamp::Pallet::<Test>::get();

		assert_ok!(borrow(1, DOT, PUSD, 500, Some(rate_pct(5, 100))));

		let v_post = vault(DOT, PUSD, 1);
		assert_eq!(v_post.annual_rate, rate_pct(5, 100));
		assert_eq!(v_post.last_rate_update, now_before_call);
		assert_eq!(v_post.debt.principal, v_pre.debt.principal + 500);
		assert_eq!(v_post.debt.interest, v_pre.debt.interest + predicted);
		let order = LinkedList::iter_from_tail(rate_list(DOT, PUSD), 10);
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
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 5_000, 2_000, rate_pct(20, 100)));

		// Above the branch `maximum_borrow_rate` (400%).
		assert_noop!(
			borrow(1, DOT, PUSD, 500, Some(rate_pct(401, 100))),
			crate::Error::<Test>::RateOutOfBounds
		);
	});
}

// Borrowing while passing the vault's *current* rate is a pure debt increase:
// it must not charge the full-principal rate-change fee nor reset the cooldown,
// mirroring `change_rate`'s equal-rate no-op.
#[test]
fn borrow_with_unchanged_rate_charges_no_rate_change_fee() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 5_000, 2_000, rate_pct(20, 100)));
		let opened_at = vault(DOT, PUSD, 1).last_rate_update;
		let interest_at_open = vault(DOT, PUSD, 1).debt.interest;

		// Advance only part-way into the rate-adjustment cooldown so a (buggy)
		// rate-change fee would still apply if the rate were treated as changed.
		advance_time(ONE_DAY_MS / 2);

		let fee_pure = crate::Pallet::<Test>::predict_borrow_upfront_fee(DOT, PUSD, 1, 500, None)
			.expect("registered market and vault");
		let fee_same_rate = crate::Pallet::<Test>::predict_borrow_upfront_fee(
			DOT,
			PUSD,
			1,
			500,
			Some(rate_pct(20, 100)),
		)
		.expect("registered market and vault");
		assert_eq!(fee_pure, fee_same_rate, "an unchanged rate must not add a rate-change fee");

		assert_ok!(borrow(1, DOT, PUSD, 500, Some(rate_pct(20, 100))));

		// The fee covers the 500 increase alone: ceil(500 · 20% · 7d / 365.25d) = ceil(1.92) = 2.
		// A rate-change fee would cover all 2_500 of principal and charge ceil(9.58) = 10.
		assert_event(crate::Event::UpfrontFeeCharged {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			amount: 2,
		});
		let v_post = vault(DOT, PUSD, 1);
		// Half a day at 20% on 2_000 accrues ceil(2_000 · 20% · 0.5d / 365.25d) = ceil(0.55) = 1,
		// on top of the fee of 2.
		assert_eq!(v_post.debt.interest - interest_at_open, 3);
		assert_eq!(v_post.annual_rate, rate_pct(20, 100));
		assert_eq!(v_post.last_rate_update, opened_at, "no-op rate must not reset the cooldown");
		assert!(
			!vault_events()
				.iter()
				.any(|e| matches!(e, crate::Event::BorrowRateChanged { .. })),
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
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 3_000, 3_000, rate_pct(25, 100)));
		advance_time(ONE_DAY_MS);

		// Settle pending interest into a known-quantity accrued, then top up
		// the borrower's pUSD so they have enough to repay both principal
		// and accrued.
		assert_ok!(poke(1, DOT, PUSD, 1));
		let v_pre = vault(DOT, PUSD, 1);
		// Borrow more pUSD into a second account so we can shuttle some over.
		assert_ok!(open(2, DOT, PUSD, 5_000, 3_000, rate_pct(25, 100)));
		top_up_pusd(1, 2, v_pre.debt.interest + 500);

		let now_before_call = pallet_timestamp::Pallet::<Test>::get();
		assert_ok!(repay(1, DOT, PUSD, 1, Some(500)));
		let v_post = vault(DOT, PUSD, 1);

		assert_eq!(v_post.last_interest_time, interest_time_at(DOT, now_before_call));

		// Open fee ceil(3_000 × 25% × 7 / 365.25) = 15 plus one day's interest
		// ceil(3_000 × 25% / 365.25) = 3 are paid before the principal.
		assert_eq!(v_pre.debt.principal, 3_000);
		assert_eq!(v_pre.debt.interest, 18);
		assert_eq!(v_post.debt.principal, 2_518);
		assert_eq!(v_post.debt.interest, 0);
		assert_eq!(v_post.debt.total(), 2_518);
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
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 3_000, 2_000, rate_pct(25, 100)));
		advance_time(ONE_DAY_MS);

		let v_pre = vault(DOT, PUSD, 1);
		let now_before_call = pallet_timestamp::Pallet::<Test>::get();

		// Permissionless: any signed origin (here, account 2) can poke
		// account 1's vault.
		assert_ok!(poke(2, DOT, PUSD, 1));
		let v_post = vault(DOT, PUSD, 1);

		assert_eq!(v_post.last_interest_time, interest_time_at(DOT, now_before_call));
		assert_eq!(v_post.debt.principal, v_pre.debt.principal);
		// One day at 25% on 2_000 principal materialises
		// ceil(2_000 * 0.25 * 1day / year) = 2 units on top of the pending open fee.
		assert_eq!(v_post.debt.interest, v_pre.debt.interest + 2);
		assert_event(crate::Event::InterestAccrued {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			amount: 2,
		});
	});
}

// A full repayment leaves a live zero-debt Dormant husk (it no longer
// auto-closes), so the row survives and stays pokeable — poking it is a no-op on
// zero debt but must not error with `VaultNotFound`.
#[test]
fn poke_after_full_repayment_pokes_dormant_husk() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 3_000, 2_000, rate_pct(25, 100)));
		assert_ok!(open(2, DOT, PUSD, 3_000, 2_000, rate_pct(25, 100)));
		// Repay all of vault 1's debt — first poke to settle accrued, then
		// transfer accrued from vault 2 to cover the residual.
		assert_ok!(poke(1, DOT, PUSD, 1));
		let v = vault(DOT, PUSD, 1);
		let total = v.debt.principal + v.debt.interest;
		top_up_pusd(1, 2, v.debt.interest);
		assert_ok!(repay(1, DOT, PUSD, 1, Some(total)));
		// The husk survives as a Dormant zero-debt row and remains pokeable.
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_ok!(poke(3, DOT, PUSD, 1));
		assert_eq!(vault(DOT, PUSD, 1).debt.total(), 0);
	});
}

// Redemption refreshes last_interest_time on the redeemed vault, applies
// pending interest, reduces entire debt by the redeemed amount, and reduces
// recorded debt accordingly. Tested through the `VaultInterface`
// trait (no `redeem` extrinsic exists yet).
#[test]
fn redemption_full_state_changes() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Six vaults across ascending rates so the rate index has a clear
		// "lowest rate" target at the tail.
		for (who, pct) in [(1u64, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6)] {
			assert_ok!(open(who, DOT, PUSD, 1_000, 500, rate_pct(pct, 100)));
		}
		// Settle acct 1's recorded interest, then let a full year accrue so it
		// carries non-zero *pending* interest at redemption time. The redemption
		// must poke that pending interest before cancelling debt (otherwise the
		// entire-debt arithmetic below would not close), and we pin the exact
		// accrued amount rather than relying on a floor-to-zero coincidence.
		assert_ok!(poke(9, DOT, PUSD, 1));
		let v_pre = vault(DOT, PUSD, 1);
		advance_time(ONE_YEAR_MS);

		let now_before_call = pallet_timestamp::Pallet::<Test>::get();
		// Collateral-leg baselines before the redemption.
		let recipient_collateral_pre = collateral_balance(DOT, 5);
		let branch_collateral_pre = branch_state(DOT, PUSD).unwrap().total_collateral;
		// Redeem 200 pUSD to acct 5 (the recipient) — the helper uses the
		// rate-index tail, which is acct 1 (lowest rate).
		let target = redeem(DOT, PUSD, 5, 200).expect("redeem ok");
		assert_eq!(target, 1);

		let v_post = vault(DOT, PUSD, 1);
		// The redemption refreshed acct 1's interest clock — it poked the target.
		assert_eq!(v_post.last_interest_time, interest_time_at(DOT, now_before_call));

		// Fee 1 plus a year's interest 5 are paid before 194 principal.
		assert_eq!(v_pre.debt.principal, 500);
		assert_eq!(v_pre.debt.interest, 1);
		assert_eq!(v_post.debt.interest, 0);
		assert_eq!(v_post.debt.principal, 306);
		assert_eq!(v_post.debt.total(), 306);

		// Collateral leg: 200 pUSD / price 10 = 20 collateral released from acct 1's
		// hold to the recipient, who receives it free (not held).
		let collateral_released: Balance = 20;
		assert_eq!(v_post.collateral, v_pre.collateral - collateral_released); // 1_000 -> 980
		assert_eq!(held(DOT, 1), v_pre.collateral - collateral_released);
		assert_eq!(collateral_balance(DOT, 5), recipient_collateral_pre + collateral_released);
		assert_eq!(
			branch_state(DOT, PUSD).unwrap().total_collateral,
			branch_collateral_pre - collateral_released,
		);

		assert!(vault_status(DOT, PUSD, 1).is_active());
		assert_event(crate::Event::VaultRedeemed {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			recipient: 5,
			debt_cancelled: 200,
			collateral_to_recipient: 20,
			vault_annual_rate: rate_pct(1, 100),
		});
	});
}

// The routed fee must match vault debt, market debt, and the net issuance increase.
#[test]
fn open_mints_borrow_amount_and_routes_fee_residual_to_handler() {
	// Fee is ceil(2_000 × rate × 7 / 365.25): 10% → 4, 37% → 15, 100% → 39. The mock pool
	// burns 75% of it, so the residual is ceil(fee / 4): 1, 4, 10.
	for (pct, fee, fee_residual) in [(10, 4, 1), (37, 15, 4), (100, 39, 10)] {
		build_and_execute(|| {
			register_market(DOT, PUSD);
			assert_event(crate::Event::BranchRegistered { collateral_id: DOT, stable_id: PUSD });
			let total_pre = <Pusd as FungibleInspect<AccountId>>::total_issuance();
			let rate = rate_pct(pct, 100);
			assert_eq!(
				crate::Pallet::<Test>::predict_open_upfront_fee(DOT, PUSD, 2_000, rate)
					.expect("registered market"),
				fee
			);
			assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate));

			let v = vault(DOT, PUSD, 1);
			assert_eq!(v.annual_rate, rate);
			assert_eq!(v.debt.principal, 2_000);
			assert_eq!(v.debt.interest, fee);
			assert_eq!(held(DOT, 1), 1_000);
			assert!(vault_status(DOT, PUSD, 1).is_active());
			assert_eq!(LinkedList::iter_from_tail(rate_list(DOT, PUSD), 1), vec![1]);
			assert_eq!(<Pusd as FungibleInspect<AccountId>>::balance(&1), 2_000);
			assert_eq!(branch_state(DOT, PUSD).expect("branch").debt.minted_interest, fee);
			// The mock burns its 75% share; only the residual is issued.
			assert_eq!(stable_balance(PUSD, FEE_DEST), fee_residual);
			assert_eq!(
				<Pusd as FungibleInspect<AccountId>>::total_issuance(),
				total_pre + 2_000 + fee_residual
			);
			assert_event(crate::Event::VaultOpened {
				collateral_id: DOT,
				stable_id: PUSD,
				owner: 1,
				collateral: 1_000,
				debt: 2_000,
				annual_rate: rate,
			});
			assert_event(crate::Event::UpfrontFeeCharged {
				collateral_id: DOT,
				stable_id: PUSD,
				owner: 1,
				amount: fee,
			});
		});
	}
}

#[test]
fn long_idle_exact_interest_is_not_rounded() {
	use pusd_primitives::VaultInterface;
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(20, 100)));
		advance_time(10 * ONE_YEAR_MS);
		let snapshot =
			crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &PUSD, &1).expect("snapshot");
		assert_eq!(snapshot.debt, 1_502);
	});
}

// The open fee is priced by the same checked-borrow path every borrow uses: the new debt over
// the upfront-fee period at the post-open average rate, not the opener's own rate.
#[test]
fn open_fee_is_charged_at_the_post_open_average_rate() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Pre-existing debt at 5% so the average is a genuine blend.
		assert_ok!(open(1, DOT, PUSD, 20_000, 100_000, rate_pct(5, 100)));

		// The average after the open is (100_000 · 5% + 100_000 · 15%) / 200_000 = 10%, so the
		// fee is ceil(100_000 · 10% · 7d / 365.25d) = ceil(191.65) = 192. The opener's own 15%
		// would give ceil(287.47) = 288.
		assert_eq!(
			crate::Pallet::<Test>::predict_open_upfront_fee(DOT, PUSD, 100_000, rate_pct(15, 100))
				.expect("registered market"),
			192
		);
		assert_ok!(open(2, DOT, PUSD, 20_000, 100_000, rate_pct(15, 100)));
		assert_eq!(vault(DOT, PUSD, 2).debt.interest, 192, "charged fee matches the quote");
	});
}

// Touch frequency must not change vault debt, market debt, or total issuance.
#[test]
fn poke_cadence_cannot_change_accrued_state() {
	let run = |poke_gaps_ms: &[Moment]| {
		new_test_ext().execute_with(|| {
			register_market(DOT, PUSD);
			assert_ok!(open(1, DOT, PUSD, 1_000_000, 1_000_000, rate_pct(50, 100)));
			assert_ok!(open(2, DOT, PUSD, 1_000_000, 1_000_000, rate_pct(50, 100)));
			let base1 = vault(DOT, PUSD, 1).debt.interest;
			let base2 = vault(DOT, PUSD, 2).debt.interest;
			for gap in poke_gaps_ms {
				advance_time(*gap);
				assert_ok!(poke(9, DOT, PUSD, 1));
			}
			assert_ok!(poke(9, DOT, PUSD, 2));

			let vault_1 = vault(DOT, PUSD, 1);
			let vault_2 = vault(DOT, PUSD, 2);
			// Both schedules cover the same ten-day simple-interest period.
			assert_eq!(vault_1.debt.interest - base1, 13_690);
			assert_eq!(vault_2.debt.interest - base2, 13_690);
			let state = branch_state(DOT, PUSD).unwrap();
			assert_eq!(state.debt.pending_interest_attribution, 0);
			assert_eq!(state.debt.minted_interest, vault_1.debt.interest + vault_2.debt.interest);
			crate::try_state::do_try_state::<Test>().expect("post-test invariants hold");
			(state, vault_1, vault_2)
		})
	};
	assert_eq!(run(&[ONE_DAY_MS; 10]), run(&[10 * ONE_DAY_MS]));
}

// Two vaults at 50% on 1_000_000 each accrue exactly 1_000_000 of aggregate
// interest over one year. The refresh issues it to the yield route without
// writing either vault row, a repeat refresh changes nothing, and the later
// vault touches attribute from the issued pool instead of minting again.
#[test]
fn refresh_branch_issues_pending_aggregate_interest() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000_000, 1_000_000, rate_pct(50, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000_000, 1_000_000, rate_pct(50, 100)));
		let state_pre = branch_state(DOT, PUSD).unwrap();
		let vault_1_pre = vault(DOT, PUSD, 1);
		let vault_2_pre = vault(DOT, PUSD, 2);
		let fee_pre = stable_balance(PUSD, FEE_DEST);
		let total_pre = total_stable(PUSD);
		advance_time(ONE_YEAR_MS);
		let projected = crate::Pallet::<Test>::accrued_stablecoin_debt(&PUSD);

		assert_ok!(refresh_branch(99, DOT, PUSD));

		let expected: Balance = 1_000_000;
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.debt.minted_interest - state_pre.debt.minted_interest, expected);
		assert_event(crate::Event::InterestIssued {
			collateral_id: DOT,
			stable_id: PUSD,
			amount: expected,
		});
		assert_eq!(state.debt.pending_interest_attribution, expected);
		assert_eq!(state.debt.aggregate_interest_remainder, 0);
		assert_eq!(state.debt.last_interest_time, state.interest_time(Timestamp::get()));
		assert!(!state.is_frozen());
		assert_eq!(vault(DOT, PUSD, 1), vault_1_pre, "the refresh writes no vault row");
		assert_eq!(vault(DOT, PUSD, 2), vault_2_pre, "the refresh writes no vault row");

		// The mock pool burns its 75% share (750_000), so only the residual 250_000 is issued.
		let residual: Balance = 250_000;
		assert_eq!(stable_balance(PUSD, FEE_DEST) - fee_pre, residual);
		assert_eq!(total_stable(PUSD) - total_pre, residual);

		// The stablecoin-wide projection before the refresh equals the realized aggregate after.
		let stablecoin_debt = crate::pallet::StablecoinDebt::<Test>::get(PUSD);
		assert_eq!(stablecoin_debt.outstanding, state.debt.outstanding());
		assert_eq!(stablecoin_debt.outstanding, projected);
		assert!(stablecoin_debt.pending_interest.is_zero());
		assert!(
			!vault_events()
				.iter()
				.any(|event| matches!(event, crate::Event::ModeChanged { .. })),
			"an unfrozen market's refresh changes no mode"
		);

		assert_storage_noop!(assert_ok!(refresh_branch(99, DOT, PUSD)));

		let total = total_stable(PUSD);
		assert_ok!(poke(9, DOT, PUSD, 1));
		assert_ok!(poke(9, DOT, PUSD, 2));
		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.debt.pending_interest_attribution, 0);
		assert_eq!(
			state.debt.minted_interest - state_pre.debt.minted_interest,
			expected,
			"pokes attribute, they do not mint"
		);
		assert_eq!(vault(DOT, PUSD, 1).debt.interest - vault_1_pre.debt.interest, 500_000);
		assert_eq!(vault(DOT, PUSD, 2).debt.interest - vault_2_pre.debt.interest, 500_000);
		assert_eq!(total_stable(PUSD), total, "issuance unchanged by the pokes");
	});
}
