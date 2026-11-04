use crate::{
	mock::*,
	tests::{assert_event, percent, vault_events, vault_status, ONE_DAY_MS, ONE_YEAR_MS},
};

fn interest_time_at(asset: AssetId, now: Moment) -> Moment {
	market_state(asset, STABLE).interest_time(now)
}

#[test]
fn open_sets_last_interest_time_to_now() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		let t0 = Timestamp::get();

		assert_ok!(open(1, DOT, STABLE, 1_000, 2_000, percent(5)));

		assert_eq!(vault(DOT, STABLE, 1).last_interest_time, interest_time_at(DOT, t0));

		advance_time(1_000);
		let t1 = Timestamp::get();
		assert_ok!(open(2, DOT, STABLE, 1_000, 2_000, percent(5)));

		assert_eq!(vault(DOT, STABLE, 1).last_interest_time, interest_time_at(DOT, t0));
		assert_eq!(vault(DOT, STABLE, 2).last_interest_time, interest_time_at(DOT, t1));

		// Vault 1 was untouched by vault 2's open; poking it now settles it to the
		// current interest time (t1), confirming a poke advances the clock.
		assert_ok!(poke(9, DOT, STABLE, 1));

		assert_eq!(vault(DOT, STABLE, 1).last_interest_time, interest_time_at(DOT, t1));
	});
}

#[test]
fn aggregate_interest_overflow_is_rejected_without_advancing_state() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 2_000, percent(100)));
		let mut state = market_state(DOT, STABLE);
		state.debt.minted_interest = Balance::MAX;
		advance_time(ONE_DAY_MS);
		let before = state.clone();

		assert_eq!(
			crate::Pallet::<Test>::accrue_aggregate_interest(&mut state, Timestamp::get()),
			Err(crate::Error::<Test>::ArithmeticOverflow.into())
		);
		assert_eq!(state, before);
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
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 2_000, percent(50)));
		let interest_at_open = vault(DOT, STABLE, 1).debt.interest;
		// Advance one full cooldown, then poke so the elapsed interest is settled
		// before the rate change (which then has nothing left to materialise).
		advance_time(ONE_DAY_MS);
		assert_ok!(poke(9, DOT, STABLE, 1));
		let v_pre = vault(DOT, STABLE, 1);
		assert!(
			v_pre.debt.interest > interest_at_open,
			"a day of interest was folded in by the poke"
		);

		let now_before_call = Timestamp::get();
		assert_eq!(
			crate::Pallet::<Test>::predict_borrow_upfront_fee(DOT, STABLE, 1, 0, Some(percent(75)))
				.expect("registered branch and vault"),
			0,
			"post-cooldown rate change should quote no upfront fee",
		);

		System::reset_events();
		assert_ok!(change_rate(1, DOT, STABLE, percent(75)));

		let v_post = vault(DOT, STABLE, 1);

		assert_eq!(v_post.last_interest_time, interest_time_at(DOT, now_before_call));
		assert_eq!(v_post.debt.principal, v_pre.debt.principal);
		// Fee-free and same interest-time as the poke: interest is exactly unchanged.
		assert_eq!(v_post.debt.interest, v_pre.debt.interest);
		assert_eq!(v_post.annual_rate, percent(75));
		// The defining side effect of a rate change: the cooldown clock is stamped
		// to the wall-clock moment of the call (`do_change_rate` in `dispatchable_impls.rs`).
		assert_eq!(v_post.last_rate_update, now_before_call);
		// Past the cooldown the change charges no upfront fee.
		assert_eq!(
			vault_events(),
			vec![crate::Event::BorrowRateChanged {
				collateral_id: DOT,
				stable_id: STABLE,
				owner: 1,
				old_rate: percent(50),
				new_rate: percent(75),
			}]
		);
	});
}

// A within-cooldown rate change charges an upfront fee that lands in
// `vault.debt.interest` and bumps recorded debt by exactly that fee.
#[test]
fn change_rate_premature_increases_recorded_debt_by_fee() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 2_000, percent(50)));
		advance_time(ONE_DAY_MS / 2);
		// Settle pending interest into accrued first so the change_rate
		// delta isolates the upfront-fee component.
		assert_ok!(poke(1, DOT, STABLE, 1));
		let v_pre = vault(DOT, STABLE, 1);

		let predicted =
			crate::Pallet::<Test>::predict_borrow_upfront_fee(DOT, STABLE, 1, 0, Some(percent(75)))
				.expect("registered branch and vault");
		// The sole vault's new 75% is the average: ceil(2_000 · 75% · 7d / 365.25d) = 29.
		assert_eq!(predicted, 29, "premature change at debt=2000 must charge a fee");

		assert_ok!(change_rate(1, DOT, STABLE, percent(75)));

		let v_post = vault(DOT, STABLE, 1);
		assert_eq!(v_post.debt.principal, v_pre.debt.principal);
		assert_eq!(v_post.debt.interest, v_pre.debt.interest + predicted);
		assert_event(crate::Event::UpfrontFeeCharged {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			amount: 29,
		});
		assert_event(crate::Event::BorrowRateChanged {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			old_rate: percent(50),
			new_rate: percent(75),
		});
	});
}

#[test]
fn borrow_with_new_rate_updates_rate_reorders_index_and_charges_predicted_fee() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		for (who, pct) in [(1, 20), (2, 10), (3, 30)] {
			assert_ok!(open(who, DOT, STABLE, 5_000, 2_000, percent(pct)));
		}
		// Above the branch `maximum_borrow_rate` (400%).
		assert_noop!(
			borrow(1, DOT, STABLE, 500, Some(percent(401))),
			crate::Error::<Test>::RateOutOfBounds
		);
		let v_pre = vault(DOT, STABLE, 1);
		// The rate change is premature, so the fee covers all 2_500 of the vault's principal, at
		// the post-borrow *average* rate (2_500 · 5% + 2_000 · 10% + 2_000 · 30%) / 6_500 ≈ 14.23%,
		// not the vault's own 5%: ceil(2_500 · 14.23% · 7d / 365.25d) = ceil(6.82) = 7, where the
		// own rate would give 3. The dispatch below must then charge exactly the quote.
		let predicted = crate::Pallet::<Test>::predict_borrow_upfront_fee(
			DOT,
			STABLE,
			1,
			500,
			Some(percent(5)),
		)
		.expect("registered branch and vault");
		assert_eq!(predicted, 7);
		let now_before_call = Timestamp::get();

		assert_ok!(borrow(1, DOT, STABLE, 500, Some(percent(5))));

		let v_post = vault(DOT, STABLE, 1);
		assert_eq!(v_post.annual_rate, percent(5));
		assert_eq!(v_post.last_rate_update, now_before_call);
		assert_eq!(v_post.debt.principal, v_pre.debt.principal + 500);
		assert_eq!(v_post.debt.interest, v_pre.debt.interest + predicted);
		let order = LinkedList::iter_from_tail(VaultList::Rate(DOT, STABLE), 10);
		assert_eq!(order, alloc::vec![1, 2, 3]);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::BorrowRateChanged {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			old_rate: percent(20),
			new_rate: percent(5),
		}));
	});
}

// Borrowing while passing the vault's *current* rate is a pure debt increase:
// it must not charge the full-principal rate-change fee nor reset the cooldown,
// mirroring `change_rate`'s equal-rate no-op.
#[test]
fn borrow_with_unchanged_rate_charges_no_rate_change_fee() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 5_000, 2_000, percent(20)));
		let opened_at = vault(DOT, STABLE, 1).last_rate_update;
		let interest_at_open = vault(DOT, STABLE, 1).debt.interest;

		// Advance only part-way into the rate-adjustment cooldown so a (buggy)
		// rate-change fee would still apply if the rate were treated as changed.
		advance_time(ONE_DAY_MS / 2);

		let fee_pure = crate::Pallet::<Test>::predict_borrow_upfront_fee(DOT, STABLE, 1, 500, None)
			.expect("registered branch and vault");
		let fee_same_rate = crate::Pallet::<Test>::predict_borrow_upfront_fee(
			DOT,
			STABLE,
			1,
			500,
			Some(percent(20)),
		)
		.expect("registered branch and vault");
		assert_eq!(fee_pure, fee_same_rate, "an unchanged rate must not add a rate-change fee");

		System::reset_events();
		assert_ok!(borrow(1, DOT, STABLE, 500, Some(percent(20))));

		// The fee covers the 500 increase alone: ceil(500 · 20% · 7d / 365.25d) = ceil(1.92) = 2.
		// A rate-change fee would cover all 2_500 of principal and charge ceil(9.58) = 10.
		// An unchanged rate emits no `BorrowRateChanged`.
		assert_eq!(
			vault_events(),
			vec![
				crate::Event::InterestAccrued {
					collateral_id: DOT,
					stable_id: STABLE,
					owner: 1,
					amount: 1,
				},
				crate::Event::UpfrontFeeCharged {
					collateral_id: DOT,
					stable_id: STABLE,
					owner: 1,
					amount: 2,
				},
				crate::Event::Borrowed {
					collateral_id: DOT,
					stable_id: STABLE,
					owner: 1,
					recipient: 1,
					amount: 500,
				},
				crate::Event::InterestIssued { collateral_id: DOT, stable_id: STABLE, amount: 1 },
			]
		);
		let v_post = vault(DOT, STABLE, 1);
		// Half a day at 20% on 2_000 accrues ceil(2_000 · 20% · 0.5d / 365.25d) = ceil(0.55) = 1,
		// on top of the fee of 2.
		assert_eq!(v_post.debt.interest - interest_at_open, 3);
		assert_eq!(v_post.annual_rate, percent(20));
		assert_eq!(v_post.last_rate_update, opened_at, "no-op rate must not reset the cooldown");
	});
}

// Redemption settles the target's pending interest before cancelling debt, so interest is paid
// before principal, and releases the redeemed collateral to the recipient. The clock stamp and the
// hold are owned by the redemption tests and `try_state`.
#[test]
fn redemption_full_state_changes() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// Six vaults across ascending rates so the rate index has a clear
		// "lowest rate" target at the tail.
		for (who, pct) in [(1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6)] {
			assert_ok!(open(who, DOT, STABLE, 1_000, 500, percent(pct)));
		}
		// Settle acct 1's recorded interest, then let a full year accrue so it
		// carries non-zero *pending* interest at redemption time. The redemption
		// must poke that pending interest before cancelling debt (otherwise the
		// entire-debt arithmetic below would not close), and we pin the exact
		// accrued amount rather than relying on a floor-to-zero coincidence.
		assert_ok!(poke(9, DOT, STABLE, 1));
		let v_pre = vault(DOT, STABLE, 1);
		advance_time(ONE_YEAR_MS);
		let recipient_collateral_pre = collateral_balance(DOT, 5);

		// Redeem 200 STABLE to acct 5 (the recipient) — the helper uses the
		// rate-index tail, which is acct 1 (lowest rate).
		let target = redeem(DOT, STABLE, 5, 200).expect("redeem ok");
		assert_eq!(target, 1);

		// Fee 1 plus a year's interest 5 are paid before 194 principal.
		let v_post = vault(DOT, STABLE, 1);
		assert_eq!(v_post.debt.interest, 0);
		assert_eq!(v_post.debt.principal, 306);
		assert_eq!(v_post.debt.total(), 306);

		// Collateral leg: 200 STABLE / price 10 = 20 collateral released from acct 1's
		// hold to the recipient, who receives it free (not held).
		let collateral_released: Balance = 20;
		assert_eq!(v_post.collateral, v_pre.collateral - collateral_released); // 1_000 -> 980
		assert_eq!(collateral_balance(DOT, 5), recipient_collateral_pre + collateral_released);

		assert!(vault_status(DOT, STABLE, 1).is_active());
		assert_event(crate::Event::VaultRedeemed {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			recipient: 5,
			debt_cancelled: crate::DebtBreakdown { principal: 194, interest: 6 },
			collateral_to_recipient: 20,
			vault_annual_rate: percent(1),
		});
	});
}

// Open records the quoted fee as debt on top of the principal and mints the borrowed amount in
// full. Fee rounding is owned by `math.rs` and the fee's routing by
// `refresh_branch_issues_pending_aggregate_interest`.
#[test]
fn open_records_debt_mints_borrow_and_emits_events() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_event(crate::Event::BranchRegistered { collateral_id: DOT, stable_id: STABLE });
		let rate = percent(37);
		// ceil(2_000 × 37% × 7 / 365.25) = 15.
		let fee = 15;
		assert_eq!(
			crate::Pallet::<Test>::predict_open_upfront_fee(DOT, STABLE, 2_000, rate)
				.expect("registered branch"),
			fee
		);

		assert_ok!(open(1, DOT, STABLE, 1_000, 2_000, rate));

		let v = vault(DOT, STABLE, 1);
		assert_eq!(v.annual_rate, rate);
		assert_eq!(v.debt.principal, 2_000);
		assert_eq!(v.debt.interest, fee);
		assert!(vault_status(DOT, STABLE, 1).is_active());
		assert_eq!(LinkedList::iter_from_tail(VaultList::Rate(DOT, STABLE), 1), vec![1]);
		assert_eq!(stable_balance(STABLE, 1), 2_000);
		assert_event(crate::Event::VaultOpened {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			collateral: 1_000,
			debt: 2_000,
			annual_rate: rate,
		});
		assert_event(crate::Event::UpfrontFeeCharged {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			amount: fee,
		});
	});
}

#[test]
fn long_idle_exact_interest_is_not_rounded() {
	use stablecoin_primitives::VaultInterface;
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(20)));

		advance_time(10 * ONE_YEAR_MS);
		let snapshot = crate::Pallet::<Test>::project_redemption_snapshot(&DOT, &STABLE, &1)
			.expect("snapshot");

		assert_eq!(snapshot.debt, 1_502);
	});
}

// The open fee is priced by the same checked-borrow path every borrow uses: the new debt over
// the upfront-fee period at the post-open average rate, not the opener's own rate.
#[test]
fn open_fee_is_charged_at_the_post_open_average_rate() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// Pre-existing debt at 5% so the average is a genuine blend.
		assert_ok!(open(1, DOT, STABLE, 20_000, 100_000, percent(5)));

		// The average after the open is (100_000 · 5% + 100_000 · 15%) / 200_000 = 10%, so the
		// fee is ceil(100_000 · 10% · 7d / 365.25d) = ceil(191.65) = 192. The opener's own 15%
		// would give ceil(287.47) = 288.
		assert_eq!(
			crate::Pallet::<Test>::predict_open_upfront_fee(DOT, STABLE, 100_000, percent(15))
				.expect("registered branch"),
			192
		);

		assert_ok!(open(2, DOT, STABLE, 20_000, 100_000, percent(15)));

		assert_eq!(vault(DOT, STABLE, 2).debt.interest, 192, "charged fee matches the quote");
	});
}

// Touch frequency must not change vault debt, branch debt, or total issuance.
#[test]
fn poke_cadence_cannot_change_accrued_state() {
	let run = |poke_gaps_ms: &[Moment]| {
		ExtBuilder::default().branch(DOT, STABLE).build().execute_with(|| {
			assert_ok!(open(1, DOT, STABLE, 1_000_000, 1_000_000, percent(50)));
			assert_ok!(open(2, DOT, STABLE, 1_000_000, 1_000_000, percent(50)));
			let base1 = vault(DOT, STABLE, 1).debt.interest;
			let base2 = vault(DOT, STABLE, 2).debt.interest;

			for gap in poke_gaps_ms {
				advance_time(*gap);
				assert_ok!(poke(9, DOT, STABLE, 1));
			}
			assert_ok!(poke(9, DOT, STABLE, 2));

			let vault_1 = vault(DOT, STABLE, 1);
			let vault_2 = vault(DOT, STABLE, 2);
			// Both schedules cover the same ten-day simple-interest period.
			assert_eq!(vault_1.debt.interest - base1, 13_690);
			assert_eq!(vault_2.debt.interest - base2, 13_690);
			let state = market_state(DOT, STABLE);
			assert_eq!(state.debt.pending_interest_attribution, 0);
			assert_eq!(state.debt.minted_interest, vault_1.debt.interest + vault_2.debt.interest);
			check_invariants();
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
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000_000, 1_000_000, percent(50)));
		assert_ok!(open(2, DOT, STABLE, 1_000_000, 1_000_000, percent(50)));
		let state_pre = market_state(DOT, STABLE);
		let vault_1_pre = vault(DOT, STABLE, 1);
		let vault_2_pre = vault(DOT, STABLE, 2);
		let fee_pre = stable_balance(STABLE, FEE_DEST);
		let total_pre = total_stable(STABLE);
		advance_time(ONE_YEAR_MS);
		let projected = crate::Pallet::<Test>::accrued_stablecoin_debt(&STABLE);

		System::reset_events();
		assert_ok!(refresh_branch(99, DOT, STABLE));

		let expected: Balance = 1_000_000;
		let state = market_state(DOT, STABLE);
		assert_eq!(state.debt.minted_interest - state_pre.debt.minted_interest, expected);
		// An unfrozen branch's refresh changes no mode.
		assert_eq!(
			vault_events(),
			vec![crate::Event::InterestIssued {
				collateral_id: DOT,
				stable_id: STABLE,
				amount: expected
			}]
		);
		assert_eq!(state.debt.pending_interest_attribution, expected);
		assert_eq!(state.debt.aggregate_interest_remainder, 0);
		assert_eq!(state.debt.last_interest_time, state.interest_time(Timestamp::get()));
		assert!(!state.is_frozen());
		assert_eq!(vault(DOT, STABLE, 1), vault_1_pre, "the refresh writes no vault row");
		assert_eq!(vault(DOT, STABLE, 2), vault_2_pre, "the refresh writes no vault row");

		// The mock pool burns its 75% share (750_000), so only the residual 250_000 is issued.
		let residual: Balance = 250_000;
		assert_eq!(stable_balance(STABLE, FEE_DEST) - fee_pre, residual);
		assert_eq!(total_stable(STABLE) - total_pre, residual);

		// The stablecoin-wide projection before the refresh equals the realized aggregate after.
		let stablecoin_debt = crate::pallet::StablecoinDebt::<Test>::get(STABLE);
		assert_eq!(stablecoin_debt.outstanding, state.debt.outstanding());
		assert_eq!(stablecoin_debt.outstanding, projected);
		assert!(stablecoin_debt.pending_interest.is_zero());

		assert_storage_noop!(assert_ok!(refresh_branch(99, DOT, STABLE)));

		let total = total_stable(STABLE);
		assert_ok!(poke(9, DOT, STABLE, 1));
		assert_ok!(poke(9, DOT, STABLE, 2));

		let state = market_state(DOT, STABLE);
		assert_eq!(state.debt.pending_interest_attribution, 0);
		assert_eq!(
			state.debt.minted_interest - state_pre.debt.minted_interest,
			expected,
			"pokes attribute, they do not mint"
		);
		assert_eq!(vault(DOT, STABLE, 1).debt.interest - vault_1_pre.debt.interest, 500_000);
		assert_eq!(vault(DOT, STABLE, 2).debt.interest - vault_2_pre.debt.interest, 500_000);
		assert_eq!(total_stable(STABLE), total, "issuance unchanged by the pokes");
	});
}
