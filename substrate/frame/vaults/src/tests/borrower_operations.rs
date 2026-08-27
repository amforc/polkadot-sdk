use crate::{mock::*, tests::rate_pct};

// `close_vault` requires zero debt; with debt outstanding it returns
// `DebtOutstanding`. The separate "system needs at least one vault" guard
// lives on the liquidation path — see `last_vault.rs`.
#[test]
fn close_last_vault_with_debt_reverts() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_noop!(
			crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), DOT, PUSD, None),
			crate::Error::<Test>::DebtOutstanding
		);
	});
}

// `repay_for` returns `DebtWouldBecomeDust` when the post-state would land
// strictly between zero and `MinimumDebt`. (Full repayment to zero is allowed.)
#[test]
fn repay_into_dust_window_reverts() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// borrow=1000, min_debt=200. Repay 850 would leave 150 < 200.
		assert_ok!(open(1, DOT, PUSD, 1_000, 1_000, rate_pct(5, 100)));
		assert_noop!(
			crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, Some(850)),
			crate::Error::<Test>::DebtWouldBecomeDust
		);
	});
}

// Withdrawing more collateral than is held returns `InsufficientCollateral`
// (the held-balance check fires before the CR check).
#[test]
fn withdraw_more_than_held_reverts() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_noop!(
			crate::Pallet::<Test>::withdraw_collateral(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				2_000,
				None
			),
			crate::Error::<Test>::InsufficientCollateral
		);
	});
}

#[test]
fn withdraw_breaking_cr_reverts() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// open another vault so that we don't hit the last-vault rule.
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// 1000 DOT @ $10 backs 500 pUSD — withdrawing 950 leaves
		// 50 DOT × $10 = $500, CR == 100% < ICR 120%.
		assert_noop!(
			crate::Pallet::<Test>::withdraw_collateral(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				950,
				None
			),
			crate::Error::<Test>::UnsafeCollateralizationRatio
		);
	});
}

#[test]
fn zero_amount_repay_is_rejected_without_touching_the_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 5_000, rate_pct(50, 100)));
		let before = vault(DOT, PUSD, 1);
		advance_time(86_400_000);

		assert_noop!(
			crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, Some(0)),
			crate::Error::<Test>::ZeroAmount
		);
		assert_eq!(try_vault(DOT, PUSD, 1), Some(before));
		assert_eq!(held(DOT, 1), 1_000);
	});
}

#[test]
fn zero_amount_withdrawal_is_rejected_without_touching_the_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 5_000, rate_pct(50, 100)));
		let before = vault(DOT, PUSD, 1);
		advance_time(86_400_000);
		MockOracleAvailable::set(false);

		assert_noop!(
			crate::Pallet::<Test>::withdraw_collateral(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				0,
				None
			),
			crate::Error::<Test>::ZeroAmount
		);
		MockOracleAvailable::set(true);
		assert_eq!(try_vault(DOT, PUSD, 1), Some(before));
		assert_eq!(held(DOT, 1), 1_000);
	});
}

#[test]
fn zero_amount_deposit_is_rejected_without_touching_the_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 5_000, rate_pct(50, 100)));
		let before = vault(DOT, PUSD, 1);
		advance_time(86_400_000);

		assert_noop!(
			crate::Pallet::<Test>::deposit_collateral_for(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				1,
				0
			),
			crate::Error::<Test>::ZeroAmount
		);
		assert_eq!(try_vault(DOT, PUSD, 1), Some(before));
		assert_eq!(held(DOT, 1), 1_000);
	});
}

#[test]
fn zero_amount_borrow_is_rejected_without_touching_the_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 5_000, rate_pct(50, 100)));
		let before = vault(DOT, PUSD, 1);
		advance_time(86_400_000);

		assert_noop!(
			crate::Pallet::<Test>::borrow(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				0,
				Some(rate_pct(10, 100)),
				None,
				Position::endpoints_only()
			),
			crate::Error::<Test>::ZeroAmount
		);
		assert_eq!(
			crate::Pallet::<Test>::predict_borrow_upfront_fee(
				DOT,
				PUSD,
				1,
				0,
				Some(rate_pct(10, 100)),
			)
			.expect("registered market and vault"),
			0,
			"the quote must reflect that a zero borrow is not executable"
		);
		assert_eq!(try_vault(DOT, PUSD, 1), Some(before));
	});
}

// `repay_for` is exempt from the Safety-mode TCR gate: repaying always improves
// branch TCR, so it must succeed even while the branch sits in Safety mode
// (this is why repay-to-zero leaves a husk rather than auto-closing — the close
// would release collateral and could worsen TCR).
#[test]
fn repay_for_allowed_in_safety_mode() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 5_000, rate_pct(5, 100)));
		// Drop price into Safety (TCR ≈ 126%, between ICR 120% and Safety 130%).
		set_price(DOT, FixedU128::from_rational(63u128, 10u128));
		let tcr_before = crate::Pallet::<Test>::branch_tcr(DOT, PUSD).expect("tcr");
		assert!(tcr_before < rate_pct(130, 100), "setup must leave the branch in Safety mode");
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			Some(2_000)
		));
		let tcr_after = crate::Pallet::<Test>::branch_tcr(DOT, PUSD).expect("tcr");
		assert!(tcr_after > tcr_before, "repay improves branch TCR even in Safety mode");
	});
}

// `change_rate` to the current rate returns early without re-inserting in the rate
// index or charging a fee — but it still touches first (`do_change_rate`), so it
// settles pending interest and advances the interest clock. It is a no-op on the
// *rate*, not on the whole storage row.
#[test]
fn change_rate_to_same_rate_is_no_op() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 5_000, 10_000, rate_pct(5, 100)));
		let pre = vault(DOT, PUSD, 1);
		advance_time(86_400_000); // one day of pending interest
		let now = pallet_timestamp::Pallet::<Test>::get();
		assert_eq!(
			crate::Pallet::<Test>::predict_rate_change_upfront_fee(DOT, PUSD, 1, rate_pct(5, 100),)
				.expect("registered market and vault"),
			0,
			"the quote must reflect that an unchanged rate is a no-op"
		);
		assert_ok!(crate::Pallet::<Test>::change_rate(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			rate_pct(5, 100),
			Position::endpoints_only()
		));
		let post = vault(DOT, PUSD, 1);
		// Rate, principal and cooldown clock are untouched (no real rate change).
		assert_eq!(post.annual_rate, pre.annual_rate);
		assert_eq!(post.debt.principal, pre.debt.principal);
		assert_eq!(post.last_rate_update, pre.last_rate_update);
		// But interest is settled: exactly floor(10_000 * 0.05 * 1day / year) = 1, and no
		// upfront fee is added (which would have pushed debt.interest higher).
		assert_eq!(post.debt.interest, pre.debt.interest + 1);
		assert_eq!(post.last_interest_time, branch_state(DOT, PUSD).unwrap().interest_time(now));
	});
}
