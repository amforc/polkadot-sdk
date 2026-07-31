use crate::{
	mock::*,
	tests::{rate_pct, vault_status},
};

// `debt_in_front` sums the projected entire debt of vaults at rates strictly
// below a given rate, the amount a redemption eats before reaching that rate.
#[test]
fn debt_in_front_sums_lower_rate_vaults_only() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Eight vaults: 2 each at 0.5%, 0.6%, 0.7%, 0.8% with distinct debts.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 1000))); // 0.5%
		assert_ok!(open(2, DOT, PUSD, 1_000, 700, rate_pct(5, 1000)));
		assert_ok!(open(3, DOT, PUSD, 1_000, 600, rate_pct(6, 1000))); // 0.6%
		assert_ok!(open(4, DOT, PUSD, 1_000, 800, rate_pct(6, 1000)));
		assert_ok!(open(5, DOT, PUSD, 1_000, 900, rate_pct(7, 1000))); // 0.7%
		assert_ok!(open(6, DOT, PUSD, 1_000, 1_000, rate_pct(7, 1000)));
		assert_ok!(open(7, DOT, PUSD, 1_000, 400, rate_pct(8, 1000))); // 0.8%
		assert_ok!(open(8, DOT, PUSD, 1_000, 500, rate_pct(8, 1000)));

		// Every open charges an upfront fee of exactly 1: the fee is
		// ceil(principal × avg_rate × 7d/year) and every product here is
		// below 1. No time passes, so each vault's entire debt is
		// `principal + FEE`.
		const FEE: Balance = 1;
		for who in 1u64..=8 {
			let v = crate::pallet::Vaults::<Test>::get((DOT, PUSD, who)).expect("vault stored");
			assert_eq!(v.debt.interest, FEE, "upfront fee ceils to 1 at these magnitudes");
		}
		let debt_in_front =
			|rate, steps| crate::Pallet::<Test>::debt_in_front(DOT, PUSD, rate, steps);

		// Entire debt at rates strictly < 0.7%: vaults 1..=4.
		assert_eq!(debt_in_front(rate_pct(7, 1000), u32::MAX), 500 + 700 + 600 + 800 + 4 * FEE);

		// Entire debt at rates strictly < 0.6%: vaults 1..=2.
		assert_eq!(debt_in_front(rate_pct(6, 1000), u32::MAX), 500 + 700 + 2 * FEE);

		// Entire debt at rates strictly < 1% covers everything.
		let all = 500 + 700 + 600 + 800 + 900 + 1_000 + 400 + 500 + 8 * FEE;
		assert_eq!(debt_in_front(rate_pct(1, 100), u32::MAX), all);

		// The step cap stops the walk early.
		assert_eq!(
			debt_in_front(rate_pct(1, 100), 2),
			500 + 700 + 2 * FEE,
			"cap of 2 visits only the two tail vaults"
		);
		// A cap at least the list length matches the uncapped result.
		assert_eq!(debt_in_front(rate_pct(1, 100), 8), all);
	});
}

// The walk counts pending interest, not just what pokes have settled, so the
// total does not depend on who was poked when.
#[test]
fn debt_in_front_projects_pending_interest_poke_independent() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 5_000, 500, rate_pct(5, 1000))); // 0.5%
		assert_ok!(open(2, DOT, PUSD, 5_000, 700, rate_pct(6, 1000))); // 0.6%

		let debt_in_front =
			|| crate::Pallet::<Test>::debt_in_front(DOT, PUSD, rate_pct(1, 100), u32::MAX);

		// One year of unpoked accrual. Projected per-vault entire debt:
		//   vault 1: 500 + 1 (fee) + floor(500 × 0.5%) = 503
		//   vault 2: 700 + 1 (fee) + floor(700 × 0.6%) = 705
		advance_time(pusd_primitives::MILLIS_PER_YEAR);
		assert_eq!(debt_in_front(), 503 + 705);

		// Poking vault 1 moves its pending interest into recorded debt; the
		// total must not change.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		let v1 = crate::pallet::Vaults::<Test>::get((DOT, PUSD, 1)).expect("v1");
		assert_eq!(v1.debt.interest, 3, "fee 1 + year interest 2 settled by the poke");
		assert_eq!(debt_in_front(), 503 + 705, "projection unchanged by the poke");
	});
}

// A vault redeemed below `MinimumDebt` parks as the dormant target. The next
// redemption drains it first, so its residual counts as debt in front even
// though it left the rate index — and it consumes one walk step.
#[test]
fn debt_in_front_includes_dormant_redemption_target() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 5_000, 500, rate_pct(5, 1000))); // 0.5%, tail
		assert_ok!(open(2, DOT, PUSD, 5_000, 700, rate_pct(6, 1000))); // 0.6%
		let debt_in_front =
			|steps| crate::Pallet::<Test>::debt_in_front(DOT, PUSD, rate_pct(1, 100), steps);
		assert_eq!(debt_in_front(u32::MAX), 501 + 701); // principal + fee 1 each, no elapsed time

		// Redeem vault 1 (entire debt 501) down to 199, one below `MinimumDebt`.
		assert_ok!(redeem(DOT, PUSD, 3, 302));
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_eq!(
			branch_state(DOT, PUSD).expect("state").dormant_redemption_target,
			Some(1),
			"sub-minimum residual parks vault 1 as the dormant target"
		);
		assert_eq!(
			debt_in_front(u32::MAX),
			199 + 701,
			"the dormant target's residual (199) is consumed first and counted"
		);
		// The dormant target consumes one step of the walk budget, mirroring the
		// per-touch step accounting a real redemption pays.
		assert_eq!(debt_in_front(1), 199, "one step reaches only the dormant target");
		assert_eq!(debt_in_front(0), 0, "a zero budget counts nothing");
	});
}

// Final recovery vaults gate every redemption, so their entire debt counts in
// front of any rate — before the dormant target and the rate index.
#[test]
fn debt_in_front_counts_final_recovery_queue_first() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// The only vault: 70 collateral backing 501 of entire debt (fee 1).
		assert_ok!(open(1, DOT, PUSD, 70, 500, rate_pct(5, 1000)));
		// At price 7 its CR is 490/501 < 1.10, and as the last eligible vault it
		// enters final recovery instead of liquidation.
		set_price(DOT, FixedU128::from_rational(7, 1));
		assert_ok!(Vaults::enter_final_recovery(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		set_price(DOT, FixedU128::from_rational(10, 1));

		// Two active vaults join the rate index afterwards.
		assert_ok!(open(2, DOT, PUSD, 1_000, 700, rate_pct(6, 1000))); // entire debt 701
		assert_ok!(open(3, DOT, PUSD, 1_000, 900, rate_pct(7, 1000))); // entire debt 901

		let debt_in_front =
			|rate, steps| crate::Pallet::<Test>::debt_in_front(DOT, PUSD, rate, steps);

		// The recovery vault counts at every rate; vault 3 at exactly 0.7% does not.
		assert_eq!(debt_in_front(rate_pct(7, 1000), u32::MAX), 501 + 701);
		assert_eq!(debt_in_front(rate_pct(1, 100), u32::MAX), 501 + 701 + 901);
		// The recovery vault consumes the first walk step.
		assert_eq!(debt_in_front(rate_pct(1, 100), 1), 501);
		assert_eq!(debt_in_front(rate_pct(1, 100), 0), 0);

		// Redeeming vault 2 below `MinimumDebt` parks it as the dormant target:
		// the walk orders recovery (501), then the dormant residual (199), then
		// the rate index (901).
		assert_ok!(redeem_from(DOT, PUSD, 2, 4, 502));
		assert!(vault_status(DOT, PUSD, 2).is_dormant());
		assert_eq!(debt_in_front(rate_pct(1, 100), u32::MAX), 501 + 199 + 901);
		assert_eq!(debt_in_front(rate_pct(1, 100), 2), 501 + 199);
	});
}

// A zero step budget and an empty rate index both return zero.
#[test]
fn debt_in_front_zero_for_no_steps_or_empty_index() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Empty rate index → nothing in front.
		assert_eq!(crate::Pallet::<Test>::debt_in_front(DOT, PUSD, rate_pct(1, 100), u32::MAX), 0);
		assert_ok!(open(1, DOT, PUSD, 5_000, 500, rate_pct(5, 1000)));
		// A zero step budget visits no vaults.
		assert_eq!(crate::Pallet::<Test>::debt_in_front(DOT, PUSD, rate_pct(1, 100), 0), 0);
	});
}
