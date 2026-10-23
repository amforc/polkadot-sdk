use crate::{
	mock::*,
	tests::{percent, vault_status, ONE_YEAR_MS},
};

// `debt_in_front` sums the projected entire debt of vaults at rates strictly
// below a given rate, the amount a redemption eats before reaching that rate.
#[test]
fn debt_in_front_sums_lower_rate_vaults_only() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// Eight vaults: 2 each at 0.5%, 0.6%, 0.7%, 0.8% with distinct debts.
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, FixedU128::from_rational(5, 1000))); // 0.5%
		assert_ok!(open(2, DOT, STABLE, 1_000, 700, FixedU128::from_rational(5, 1000)));
		assert_ok!(open(3, DOT, STABLE, 1_000, 600, FixedU128::from_rational(6, 1000))); // 0.6%
		assert_ok!(open(4, DOT, STABLE, 1_000, 800, FixedU128::from_rational(6, 1000)));
		assert_ok!(open(5, DOT, STABLE, 1_000, 900, FixedU128::from_rational(7, 1000))); // 0.7%
		assert_ok!(open(6, DOT, STABLE, 1_000, 1_000, FixedU128::from_rational(7, 1000)));
		assert_ok!(open(7, DOT, STABLE, 1_000, 400, FixedU128::from_rational(8, 1000))); // 0.8%
		assert_ok!(open(8, DOT, STABLE, 1_000, 500, FixedU128::from_rational(8, 1000)));

		// Every open charges an upfront fee of exactly 1: the fee is
		// ceil(principal × avg_rate × 7d/year) and every product here is
		// below 1. No time passes, so each vault's entire debt is
		// `principal + FEE`.
		const FEE: Balance = 1;
		for who in 1u64..=8 {
			let v = vault(DOT, STABLE, who);
			assert_eq!(v.debt.interest, FEE, "upfront fee ceils to 1 at these magnitudes");
		}
		let debt_in_front =
			|rate, steps| crate::Pallet::<Test>::debt_in_front(DOT, STABLE, rate, steps);

		// Entire debt at rates strictly < 0.7%: vaults 1..=4.
		assert_eq!(
			debt_in_front(FixedU128::from_rational(7, 1000), u32::MAX),
			Ok((500 + 700 + 600 + 800 + 4 * FEE, false))
		);

		// Entire debt at rates strictly < 0.6%: vaults 1..=2.
		assert_eq!(
			debt_in_front(FixedU128::from_rational(6, 1000), u32::MAX),
			Ok((500 + 700 + 2 * FEE, false))
		);

		// Entire debt at rates strictly < 1% covers everything.
		let all = 500 + 700 + 600 + 800 + 900 + 1_000 + 400 + 500 + 8 * FEE;
		assert_eq!(debt_in_front(percent(1), u32::MAX), Ok((all, false)));

		// The step cap stops the walk early.
		assert_eq!(
			debt_in_front(percent(1), 2),
			Ok((500 + 700 + 2 * FEE, true)),
			"cap of 2 visits only the two tail vaults"
		);
		// A cap at least the list length matches the uncapped result.
		assert_eq!(debt_in_front(percent(1), 8), Ok((all, false)));
	});
}

// The walk counts pending interest, not just what pokes have settled, so the
// total does not depend on who was poked when.
#[test]
fn debt_in_front_projects_pending_interest_poke_independent() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 5_000, 500, FixedU128::from_rational(5, 1000))); // 0.5%
		assert_ok!(open(2, DOT, STABLE, 5_000, 700, FixedU128::from_rational(6, 1000))); // 0.6%

		let debt_in_front =
			|| crate::Pallet::<Test>::debt_in_front(DOT, STABLE, percent(1), u32::MAX);

		// One year of unpoked accrual. Projected per-vault entire debt:
		//   vault 1: 500 + 1 (fee) + ceil(500 × 0.5%) = 504
		//   vault 2: 700 + 1 (fee) + ceil(700 × 0.6%) = 706
		advance_time(ONE_YEAR_MS);

		assert_eq!(debt_in_front(), Ok((504 + 706, false)));

		// Poking vault 1 moves its pending interest into recorded debt; the
		// total must not change.
		assert_ok!(poke(9, DOT, STABLE, 1));

		let v1 = vault(DOT, STABLE, 1);
		assert_eq!(v1.debt.interest, 4, "fee 1 + year interest 3 settled by the poke");
		assert_eq!(debt_in_front(), Ok((504 + 706, false)), "projection unchanged by the poke");
	});
}

// A vault redeemed below `MinimumDebt` parks as the dormant target. The next
// redemption drains it first, so its residual counts as debt in front even
// though it left the rate index — and it consumes one walk step.
#[test]
fn debt_in_front_includes_dormant_redemption_target() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 5_000, 500, FixedU128::from_rational(5, 1000))); // 0.5%, tail
		assert_ok!(open(2, DOT, STABLE, 5_000, 700, FixedU128::from_rational(6, 1000))); // 0.6%
		let debt_in_front =
			|steps| crate::Pallet::<Test>::debt_in_front(DOT, STABLE, percent(1), steps);
		// Each vault owes principal plus a fee of 1; no interest has elapsed.
		assert_eq!(debt_in_front(u32::MAX), Ok((501 + 701, false)));

		// Redeem vault 1 (entire debt 501) down to 199, one below `MinimumDebt`.
		assert_ok!(redeem(DOT, STABLE, 3, 302));

		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		assert_eq!(
			market_state(DOT, STABLE).dormant_redemption_target,
			Some(1),
			"sub-minimum residual parks vault 1 as the dormant target"
		);
		assert_eq!(
			debt_in_front(u32::MAX),
			Ok((199 + 701, false)),
			"the dormant target's residual (199) is consumed first and counted"
		);
		// The dormant target consumes one step of the walk budget, mirroring the
		// per-touch step accounting a real redemption pays.
		assert_eq!(debt_in_front(1), Ok((199, true)), "one step reaches only the dormant target");
		assert_eq!(debt_in_front(0), Ok((0, true)), "a zero budget counts nothing");
	});
}

// Final recovery vaults gate every redemption, so their entire debt counts in
// front of any rate — before the dormant target and the rate index.
#[test]
fn debt_in_front_counts_final_recovery_queue_first() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// The only vault: 70 collateral backing 501 of entire debt (fee 1).
		assert_ok!(open(1, DOT, STABLE, 70, 500, FixedU128::from_rational(5, 1000)));
		// At price 7 its CR is 490/501 < 1.10, and as the last eligible vault it
		// enters final recovery instead of liquidation.
		set_price(DOT, FixedU128::from_u32(7));
		assert_ok!(enter_final_recovery(9, DOT, STABLE, 1));
		set_price(DOT, DEFAULT_PRICE);

		// Two active vaults join the rate index afterwards.
		assert_ok!(open(2, DOT, STABLE, 1_000, 700, FixedU128::from_rational(6, 1000))); // entire debt 701
		assert_ok!(open(3, DOT, STABLE, 1_000, 900, FixedU128::from_rational(7, 1000))); // entire debt 901

		let debt_in_front =
			|rate, steps| crate::Pallet::<Test>::debt_in_front(DOT, STABLE, rate, steps);

		// The recovery vault counts at every rate; vault 3 at exactly 0.7% does not.
		assert_eq!(
			debt_in_front(FixedU128::from_rational(7, 1000), u32::MAX),
			Ok((501 + 701, false))
		);
		assert_eq!(debt_in_front(percent(1), u32::MAX), Ok((501 + 701 + 901, false)));
		// The recovery vault consumes the first walk step.
		assert_eq!(debt_in_front(percent(1), 1), Ok((501, true)));
		assert_eq!(debt_in_front(percent(1), 0), Ok((0, true)));

		// Redeeming vault 2 below `MinimumDebt` parks it as the dormant target:
		// the walk orders recovery (501), then the dormant residual (199), then
		// the rate index (901).
		assert_ok!(redeem_from(DOT, STABLE, 2, 4, 502));

		assert!(vault_status(DOT, STABLE, 2).is_dormant());
		assert_eq!(debt_in_front(percent(1), u32::MAX), Ok((501 + 199 + 901, false)));
		assert_eq!(debt_in_front(percent(1), 2), Ok((501 + 199, true)));
	});
}

// A branch without vaults has nothing in front of any rate, and the walk reports no truncation.
#[test]
fn debt_in_front_is_zero_for_an_empty_index() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_eq!(
			crate::Pallet::<Test>::debt_in_front(DOT, STABLE, percent(1), u32::MAX),
			Ok((0, false))
		);
	});
}
