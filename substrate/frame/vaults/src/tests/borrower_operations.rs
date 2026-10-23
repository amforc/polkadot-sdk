use crate::{
	mock::*,
	tests::{percent, ONE_DAY_MS},
};

// `close_vault` requires zero debt; with debt outstanding it returns
// `DebtOutstanding`.
#[test]
fn close_with_outstanding_debt_reverts() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));

		assert_noop!(close_vault(1, DOT, STABLE, None), crate::Error::<Test>::DebtOutstanding);
	});
}

// `repay_for` returns `DebtWouldBecomeDust` when the post-state would land
// strictly between zero and `MinimumDebt`. (Full repayment to zero is allowed.)
#[test]
fn repay_into_dust_window_reverts() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// borrow=1000, min_debt=200. Repay 850 would leave 150 < 200.
		assert_ok!(open(1, DOT, STABLE, 1_000, 1_000, percent(5)));

		assert_noop!(
			repay(1, DOT, STABLE, 1, Some(850)),
			crate::Error::<Test>::DebtWouldBecomeDust
		);
	});
}

// Withdrawing more collateral than is held returns `InsufficientCollateral`
// (the held-balance check fires before the CR check).
#[test]
fn withdraw_more_than_held_reverts() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));

		assert_noop!(
			withdraw_collateral(1, DOT, STABLE, 2_000, None),
			crate::Error::<Test>::InsufficientCollateral
		);
	});
}

#[test]
fn withdraw_breaking_cr_reverts() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));

		// 1000 DOT @ 10 STABLE backs 500 STABLE — withdrawing 950 leaves
		// 50 DOT × 10 = 500 STABLE, CR == 100% < ICR 120%.
		assert_noop!(
			withdraw_collateral(1, DOT, STABLE, 950, None),
			crate::Error::<Test>::UnsafeCollateralizationRatio
		);
	});
}

// Each amount-taking call rejects zero before it loads a price, so the rejection holds with the
// oracle down and leaves no trace.
#[test]
fn zero_amounts_are_rejected_without_touching_the_vault() {
	let calls: [fn() -> DispatchResult; 4] = [
		|| repay(1, DOT, STABLE, 1, Some(0)),
		|| withdraw_collateral(1, DOT, STABLE, 0, None),
		|| deposit_collateral(1, DOT, STABLE, 1, 0),
		|| borrow(1, DOT, STABLE, 0, Some(percent(10))),
	];
	for call in calls {
		ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
			assert_ok!(open(1, DOT, STABLE, 1_000, 5_000, percent(50)));
			advance_time(ONE_DAY_MS);
			MockOracleAvailable::set(false);

			assert_noop!(call(), crate::Error::<Test>::ZeroAmount);
			MockOracleAvailable::set(true);
		});
	}
}

// `repay_for` is exempt from the Safety-mode TCR gate: repaying always improves
// branch TCR, so it must succeed even while the branch sits in Safety mode
// (this is why repay-to-zero leaves a husk rather than auto-closing — the close
// would release collateral and could worsen TCR).
#[test]
fn repay_for_allowed_in_safety_mode() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 5_000, percent(5)));
		// Drop price into Safety (TCR ≈ 126%, between ICR 120% and Safety 130%).
		set_price(DOT, FixedU128::from_rational(63u128, 10u128));
		let tcr_before = crate::Pallet::<Test>::branch_tcr(DOT, STABLE).expect("tcr");
		assert!(tcr_before < percent(130), "setup must leave the branch in Safety mode");

		assert_ok!(repay(1, DOT, STABLE, 1, Some(2_000)));

		let tcr_after = crate::Pallet::<Test>::branch_tcr(DOT, STABLE).expect("tcr");
		assert!(tcr_after > tcr_before, "repay improves branch TCR even in Safety mode");
	});
}

// `change_rate` to the current rate returns early without re-inserting in the rate
// index or charging a fee — but it still touches first (`do_change_rate`), so it
// settles pending interest and advances the interest clock. It is a no-op on the
// *rate*, not on the whole storage row.
#[test]
fn change_rate_to_same_rate_is_no_op() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 5_000, 10_000, percent(5)));
		let pre = vault(DOT, STABLE, 1);
		advance_time(ONE_DAY_MS); // one day of pending interest
		let now = Timestamp::get();
		assert_eq!(
			crate::Pallet::<Test>::predict_borrow_upfront_fee(DOT, STABLE, 1, 0, Some(percent(5)))
				.expect("registered branch and vault"),
			0,
			"the quote must reflect that an unchanged rate is a no-op"
		);

		assert_ok!(change_rate(1, DOT, STABLE, percent(5)));

		let post = vault(DOT, STABLE, 1);
		// Rate, principal and cooldown clock are untouched (no real rate change).
		assert_eq!(post.annual_rate, pre.annual_rate);
		assert_eq!(post.debt.principal, pre.debt.principal);
		assert_eq!(post.last_rate_update, pre.last_rate_update);
		// But interest is settled: ceil(10_000 * 0.05 * 1day / year) = 2, and no upfront fee is
		// added (which would have pushed debt.interest higher).
		assert_eq!(post.debt.interest, pre.debt.interest + 2);
		assert_eq!(post.last_interest_time, market_state(DOT, STABLE).interest_time(now));
	});
}
