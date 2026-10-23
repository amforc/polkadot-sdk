use crate::{
	mock::*,
	tests::{percent, ONE_DAY_MS},
};

// Each request fails on the guard it names and leaves no trace. Closing needs zero debt. A
// withdrawal beyond the held collateral fails before the ratio check, and one within it that
// drops the vault below ICR fails on the ratio. A poke of a missing vault is an error, so a
// mistyped owner cannot pass for a completed refresh.
#[test]
fn vault_ops_reject_invalid_requests() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));

		assert_noop!(close_vault(1, DOT, STABLE, None), crate::Error::<Test>::DebtOutstanding);
		assert_noop!(
			withdraw_collateral(1, DOT, STABLE, 2_000, None),
			crate::Error::<Test>::InsufficientCollateral
		);
		// 1_000 DOT at 10 backs about 500 of debt, so withdrawing 950 leaves CR ≈ 100% < ICR 120%.
		assert_noop!(
			withdraw_collateral(1, DOT, STABLE, 950, None),
			crate::Error::<Test>::UnsafeCollateralizationRatio
		);
		assert_noop!(poke(1, DOT, STABLE, 99), crate::Error::<Test>::VaultNotFound);
	});
}

// Each amount-taking call rejects zero before it loads a price, so the rejection holds with the
// oracle down and leaves no trace.
#[test]
fn zero_amounts_are_rejected_without_touching_the_vault() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 5_000, percent(50)));
		advance_time(ONE_DAY_MS);
		MockOracleAvailable::set(&false);

		assert_noop!(repay(1, DOT, STABLE, 1, Some(0)), crate::Error::<Test>::ZeroAmount);
		assert_noop!(
			withdraw_collateral(1, DOT, STABLE, 0, None),
			crate::Error::<Test>::ZeroAmount
		);
		assert_noop!(deposit_collateral(1, DOT, STABLE, 1, 0), crate::Error::<Test>::ZeroAmount);
		assert_noop!(
			borrow(1, DOT, STABLE, 0, Some(percent(10))),
			crate::Error::<Test>::ZeroAmount
		);
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
