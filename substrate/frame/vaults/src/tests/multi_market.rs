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
use frame::traits::fungibles::Mutate;
use pusd_primitives::{CollateralRatio, VaultInterface};

// One owner runs dotUSD/DOT and ethUSD/ETH independently: each market mints
// only its own coin and locks only its own collateral.
#[test]
fn owner_runs_two_markets_independently() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(ETH, EUSD);

		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		assert_ok!(open(1, ETH, EUSD, 500, 1_000, rate_pct(5, 100)));

		// Each market minted only its own coin.
		assert_eq!(stable_balance(PUSD, 1), 2_000);
		assert_eq!(stable_balance(EUSD, 1), 1_000);

		// Each market locked only its own collateral asset.
		assert_eq!(held(DOT, 1), 1_000);
		assert_eq!(held(ETH, 1), 500);

		// Distinct rows, each carrying its own market's collateral and principal.
		let dot = vault(DOT, PUSD, 1);
		let eth = vault(ETH, EUSD, 1);
		assert_eq!((dot.collateral, dot.debt.principal), (1_000, 2_000));
		assert_eq!((eth.collateral, eth.debt.principal), (500, 1_000));
	});
}

// The same stablecoin against two collaterals (dotUSD/DOT, dotUSD/ETH) are
// independent markets with independent debt and rate lists.
#[test]
fn same_stable_two_collaterals_are_independent() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(ETH, PUSD);

		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		assert_ok!(open(1, ETH, PUSD, 1_000, 3_000, rate_pct(7, 100)));

		// The same coin is minted from both markets into one balance.
		assert_eq!(stable_balance(PUSD, 1), 5_000);

		// Independent per-market debt ledgers.
		assert_eq!(branch_state(DOT, PUSD).unwrap().debt.principal, 2_000);
		assert_eq!(branch_state(ETH, PUSD).unwrap().debt.principal, 3_000);

		// Redeeming on the DOT market leaves the ETH market's vault untouched.
		let eth_before = vault(ETH, PUSD, 1);
		assert_eq!(redeem(DOT, PUSD, 9, 500).unwrap(), 1);
		assert_eq!(vault(ETH, PUSD, 1), eth_before);
	});
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

		let dot_pusd = branch_state(DOT, PUSD).unwrap().debt.outstanding();
		let eth_pusd = branch_state(ETH, PUSD).unwrap().debt.outstanding();
		let eth_eusd = branch_state(ETH, EUSD).unwrap().debt.outstanding();

		// Principal plus the upfront fee the open charges into `minted_interest`,
		// `ceil(drawn * rate * 7 days / year)`: ceil(2_000 * 5% * 7/365) = 2,
		// ceil(3_000 * 7% * 7/365) = 5, ceil(4_000 * 5% * 7/365) = 4.
		assert_eq!((dot_pusd, eth_pusd, eth_eusd), (2_002, 3_005, 4_004));

		// Both PUSD markets, and only those, land in the PUSD total.
		assert_eq!(StablecoinDebt::<Test>::get(PUSD).outstanding, 5_007);
		assert_eq!(StablecoinDebt::<Test>::get(PUSD).outstanding, dot_pusd + eth_pusd);
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
// branch state, or holds. The mock pool has no capacity and the keeper offers no
// JIT, so the whole vault redistributes inside its own market.
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

// Closing one market on a shared collateral releases only that market's share of
// the owner's hold; the sibling market's collateral stays locked.
#[test]
fn closing_one_market_leaves_shared_collateral_held() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(DOT, EUSD);

		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		assert_ok!(open(1, DOT, EUSD, 600, 1_000, rate_pct(5, 100)));
		assert_eq!(held(DOT, 1), 1_600);

		// Fund acct 1 to cover the principal plus the upfront fee, then close.
		<VaultStableAssets as Mutate<AccountId>>::mint_into(PUSD, &1, 10_000)
			.expect("mint pUSD to repay");
		let debt = vault(DOT, PUSD, 1).debt.total();
		assert_ok!(repay(1, DOT, PUSD, 1, Some(debt)));
		// Repay-to-zero leaves a husk still holding the PUSD market's collateral;
		// close it to release only that market's share.
		assert_ok!(close_vault(1, DOT, PUSD, None));

		// Only the PUSD market's 1_000 DOT was released; the EUSD market's 600
		// DOT remains held against its still-open vault.
		assert!(!vault_exists(DOT, PUSD, 1));
		assert_eq!(held(DOT, 1), 600);
		assert_eq!(vault(DOT, EUSD, 1).collateral, 600);
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
