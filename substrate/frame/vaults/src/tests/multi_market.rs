//! Multi-stablecoin / multi-collateral market tests.
//!
//! A market is one stablecoin against one collateral. These exercise the
//! independence and shared-collateral properties the generalisation introduces:
//! one owner running several markets, markets sharing a collateral, and
//! in-market isolation of redemption, liquidation, redistribution, and yield.

use crate::{
	mock::*,
	pallet::StablecoinDebt,
	tests::{rate_pct, ONE_YEAR_MS},
};
use pusd_primitives::{CollateralRatio, VaultInterface};

// One owner can share a stablecoin or collateral across markets. Redemption and close
// change only the selected market and release only its share of the hold.
#[test]
fn owner_runs_markets_independently_through_redemption_and_close() {
	// Balances: (PUSD, other coin); holds: (DOT, other collateral); DOT hold after close.
	for (collateral, stable, amount, debt, pct, balances, holds, dot_hold_after_close) in [
		(ETH, EUSD, 500, 1_000, 5, (2_000, 1_000), (1_000, 500), 0),
		(ETH, PUSD, 1_000, 3_000, 7, (5_000, 5_000), (1_000, 1_000), 0),
		(DOT, EUSD, 600, 1_000, 5, (2_000, 1_000), (1_600, 1_600), 600),
	] {
		build_and_execute(|| {
			register_market(DOT, PUSD);
			register_market(collateral.clone(), stable);
			assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
			assert_ok!(open(1, collateral.clone(), stable, amount, debt, rate_pct(pct, 100)));
			assert_eq!((stable_balance(PUSD, 1), stable_balance(stable, 1)), balances);
			assert_eq!((held(DOT, 1), held(collateral.clone(), 1)), holds);
			let dot = vault(DOT, PUSD, 1);
			let other = vault(collateral.clone(), stable, 1);
			assert_eq!((dot.collateral, dot.debt.principal), (1_000, 2_000));
			assert_eq!((other.collateral, other.debt.principal), (amount, debt));
			assert_eq!(branch_state(DOT, PUSD).expect("market").debt.principal, 2_000);
			let other_state = branch_state(collateral.clone(), stable).expect("market");
			assert_eq!(other_state.debt.principal, debt);

			assert_eq!(redeem(DOT, PUSD, 9, 500), Ok(1));
			assert_eq!(vault(collateral.clone(), stable, 1), other);
			assert_eq!(branch_state(collateral.clone(), stable), Some(other_state.clone()));
			mint_stable(PUSD, 1, 10_000);
			assert_ok!(repay(1, DOT, PUSD, 1, None));
			assert_ok!(close_vault(1, DOT, PUSD, None));
			assert!(!vault_exists(DOT, PUSD, 1));
			assert_eq!(vault(collateral.clone(), stable, 1), other);
			assert_eq!(branch_state(collateral.clone(), stable), Some(other_state));
			assert_eq!(held(DOT, 1), dot_hold_after_close);
			assert_eq!(held(collateral.clone(), 1), amount);
		});
	}
}

// `StablecoinDebt` sums every collateral market issuing one coin, and stays
// blind to markets issuing another. Redemptions divides by it to price one
// stablecoin's dynamic fee across all of its collaterals at once.
#[test]
fn stablecoin_debt_sums_the_markets_issuing_that_coin() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(ETH, PUSD);
		register_market(ETH, EUSD);

		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		assert_ok!(open(1, ETH, PUSD, 1_000, 3_000, rate_pct(7, 100)));
		assert_ok!(open(2, ETH, EUSD, 1_000, 4_000, rate_pct(5, 100)));

		// Principal plus each market's own upfront fee.
		// ceil(principal × rate × 7 / 365.25): 2_000 × 5% → 2, 3_000 × 7% → 5,
		// and 4_000 × 5% → 4.
		assert_eq!(
			(
				branch_state(DOT, PUSD).expect("market").debt.outstanding(),
				branch_state(ETH, PUSD).expect("market").debt.outstanding(),
				branch_state(ETH, EUSD).expect("market").debt.outstanding(),
			),
			(2_002, 3_005, 4_004),
		);

		// Both PUSD markets, and only those, land in the PUSD total.
		assert_eq!(StablecoinDebt::<Test>::get(PUSD).outstanding, 5_007);
		assert_eq!(StablecoinDebt::<Test>::get(EUSD).outstanding, 4_004);

		// The aggregate tracks debt leaving as well as arriving: repaying 1_000
		// on one PUSD market drops the shared total by exactly that, and leaves
		// the other coin's total alone.
		assert_ok!(repay(1, ETH, PUSD, 1, Some(1_000)));
		assert_eq!(StablecoinDebt::<Test>::get(PUSD).outstanding, 4_007);
		assert_eq!(StablecoinDebt::<Test>::get(EUSD).outstanding, 4_004);
	});
}

// The stablecoin debt view must include interest accrued on markets nobody has touched.
#[test]
fn stablecoin_debt_accrues_interest_across_untouched_markets() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(ETH, PUSD);

		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		assert_ok!(open(2, ETH, PUSD, 1_000, 3_000, rate_pct(7, 100)));
		let stored = StablecoinDebt::<Test>::get(PUSD);
		// Principal plus the upfront fee of each open: 2_000 + ceil(2_000 · 5% · 7d / 365.25d)
		// = 2_002 and 3_000 + ceil(3_000 · 7% · 7d / 365.25d) = 3_005.
		assert_eq!(stored.outstanding, 5_007);

		advance_time(ONE_YEAR_MS);

		// A year adds 2_000 · 5% = 100 and 3_000 · 7% = 210 that no market has minted yet.
		assert_eq!(<crate::Pallet<Test> as VaultInterface>::stablecoin_debt(&PUSD), 5_317);
		assert_eq!(
			StablecoinDebt::<Test>::get(PUSD),
			stored,
			"read-only projection must not touch either market"
		);
	});
}

// Liquidating a vault in one market never touches another market's vaults,
// branch state, or holds. The mock helper fully offsets the liquidated debt.
#[test]
fn liquidation_stays_inside_its_market() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(ETH, EUSD);

		// Two PUSD-market vaults so the liquidatee is not the last stake-bearer.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// An untouched vault on the other market.
		assert_ok!(open(3, ETH, EUSD, 1_000, 500, rate_pct(5, 100)));

		let other_vault = vault(ETH, EUSD, 3);
		let other_state = branch_state(ETH, EUSD).unwrap();
		let other_hold = held(ETH, 3);

		// Drop DOT so owner 1 falls below MCR, then liquidate it.
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(liquidate(DOT, PUSD, 1));

		// The ETH/EUSD market is byte-for-byte untouched.
		assert_eq!(vault(ETH, EUSD, 3), other_vault);
		assert_eq!(branch_state(ETH, EUSD).unwrap(), other_state);
		assert_eq!(held(ETH, 3), other_hold);
	});
}

// Yield (interest) on a market accrues in that market's own coin. The mock
// fee sink routes by the credit's own asset, so the balance assertions guard
// the accrual path end-to-end.
#[test]
fn yield_accrues_in_the_markets_own_coin() {
	build_and_execute(|| {
		register_market(ETH, EUSD);
		assert_ok!(open(1, ETH, EUSD, 100_000, 5_000, rate_pct(50, 100)));

		let pusd_before = total_stable(PUSD);
		let interest_before = vault(ETH, EUSD, 1).debt.interest;
		let eusd_fee_before = stable_balance(EUSD, FEE_DEST);

		advance_time(ONE_YEAR_MS);
		assert_ok!(poke(9, ETH, EUSD, 1));

		let interest_after = vault(ETH, EUSD, 1).debt.interest;
		// A full year at 50% on 5_000 principal accrues exactly 2_500 EUSD of vault
		// interest (interest is on principal, not the open fee).
		assert_eq!(interest_after - interest_before, 2_500);
		// Fee routing must use the market's stablecoin: the mock burns the Stability Pool's 75%
		// share and routes the other 625.
		assert_eq!(stable_balance(EUSD, FEE_DEST) - eusd_fee_before, 625);
		// The PUSD market was never involved, so its supply is unchanged.
		assert_eq!(total_stable(PUSD), pusd_before);
	});
}

// CR is computed per market: equal collateral and debt yield different ratios
// when the collateral prices differ.
#[test]
fn cr_differs_across_markets_when_prices_differ() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(ETH, EUSD);
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		set_price(ETH, FixedU128::from_rational(20u128, 1u128));

		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		assert_ok!(open(2, ETH, EUSD, 1_000, 2_000, rate_pct(5, 100)));

		let cr_dot = crate::Pallet::<Test>::vault_cr(DOT, PUSD, 1).unwrap();
		let cr_eth = crate::Pallet::<Test>::vault_cr(ETH, EUSD, 2).unwrap();
		// Both vaults owe 2_000 plus the upfront fee of ceil(2_000 · 5% · 7d / 365.25d) = 2. The
		// 1_000 of collateral is worth 10_000 on DOT and 20_000 on ETH, and the ratio rounds down:
		// 10_000 / 2_002 = 4.995004… and 20_000 / 2_002 = 9.990009….
		assert_eq!(
			cr_dot,
			CollateralRatio::Ratio(FixedU128::from_inner(4_995_004_995_004_995_004))
		);
		assert_eq!(
			cr_eth,
			CollateralRatio::Ratio(FixedU128::from_inner(9_990_009_990_009_990_009))
		);
	});
}
