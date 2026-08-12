//! Multi-stablecoin / multi-collateral market tests.
//!
//! A market is one stablecoin against one collateral. These exercise the
//! independence and shared-collateral properties the generalisation introduces:
//! one owner running several markets, markets sharing a collateral, and
//! in-market isolation of redemption, liquidation, redistribution, and yield.

use crate::{
	mock::*,
	pallet::{StablecoinDebt, Vaults},
	tests::rate_pct,
};
use frame::traits::fungibles::Mutate;
use pusd_primitives::VaultInterface;

const ONE_YEAR_MS: Moment = pusd_primitives::MILLIS_PER_YEAR;

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
		let dot = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		let eth = Vaults::<Test>::get((ETH, EUSD, 1)).unwrap();
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

		// Independent rate lists (distinct list ids).
		assert_ne!(rate_list(DOT, PUSD), rate_list(ETH, PUSD));

		// Redeeming on the DOT market leaves the ETH market's vault untouched.
		let eth_before = Vaults::<Test>::get((ETH, PUSD, 1)).unwrap();
		assert_eq!(redeem(DOT, PUSD, 9, 500).unwrap(), 1);
		assert_eq!(Vaults::<Test>::get((ETH, PUSD, 1)).unwrap(), eth_before);
	});
}

// Two markets sharing a collateral (dotUSD/DOT, ethUSD/DOT) share the owner's
// DOT hold: `Σ_stablecoins vault.collateral == balance_on_hold(DOT, owner)`.
#[test]
fn markets_sharing_a_collateral_share_the_owner_hold() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(DOT, EUSD);

		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		assert_ok!(open(1, DOT, EUSD, 600, 1_000, rate_pct(5, 100)));

		// The owner's DOT hold aggregates both markets.
		assert_eq!(held(DOT, 1), 1_600);

		// Each row carries only its own market's share, and they sum to the hold.
		let pusd = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		let eusd = Vaults::<Test>::get((DOT, EUSD, 1)).unwrap();
		assert_eq!((pusd.collateral, eusd.collateral), (1_000, 600));
		assert_eq!(pusd.collateral + eusd.collateral, held(DOT, 1));

		// Distinct coins minted against the shared collateral.
		assert_eq!(stable_balance(PUSD, 1), 2_000);
		assert_eq!(stable_balance(EUSD, 1), 1_000);
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
		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), ETH, PUSD, 1, 1_000));
		assert_eq!(StablecoinDebt::<Test>::get(PUSD).outstanding, 4_007);
		assert_eq!(StablecoinDebt::<Test>::get(EUSD).outstanding, 4_004);
	});
}

#[test]
fn stablecoin_debt_projects_interest_across_untouched_markets() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(ETH, PUSD);

		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		assert_ok!(open(2, ETH, PUSD, 1_000, 3_000, rate_pct(7, 100)));
		let stored = StablecoinDebt::<Test>::get(PUSD);

		advance_time(ONE_YEAR_MS);
		let now = Timestamp::get();
		let expected = [DOT, ETH]
			.into_iter()
			.map(|collateral| {
				let branch = branch_state(collateral, PUSD).expect("registered branch");
				crate::Pallet::<Test>::accrued_branch_debt(&branch, now)
			})
			.sum();

		assert_eq!(<crate::Pallet<Test> as VaultInterface>::stablecoin_debt(&PUSD), expected);
		assert!(expected > stored.outstanding);
		assert_eq!(
			StablecoinDebt::<Test>::get(PUSD),
			stored,
			"read-only projection must not touch either market"
		);
	});
}

// Liquidating a vault in one market never touches another market's vaults,
// branch state, or holds.
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

		let other_vault = Vaults::<Test>::get((ETH, EUSD, 3)).unwrap();
		let other_state = branch_state(ETH, EUSD).unwrap();
		let other_hold = held(ETH, 3);

		// Drop DOT so owner 1 falls below MCR, then liquidate it.
		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(liquidate(DOT, PUSD, 1));

		// The ETH/EUSD market is byte-for-byte untouched.
		assert_eq!(Vaults::<Test>::get((ETH, EUSD, 3)).unwrap(), other_vault);
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
		let debt = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().debt.total();
		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, debt));
		// Repay-to-zero leaves a husk still holding the PUSD market's collateral;
		// close it to release only that market's share.
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), DOT, PUSD, None));

		// Only the PUSD market's 1_000 DOT was released; the EUSD market's 600
		// DOT remains held against its still-open vault.
		assert!(Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
		assert_eq!(held(DOT, 1), 600);
		assert_eq!(Vaults::<Test>::get((DOT, EUSD, 1)).unwrap().collateral, 600);
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
		let interest_before = Vaults::<Test>::get((ETH, EUSD, 1)).unwrap().debt.interest;
		let eusd_fee_before = stable_balance(EUSD, FEE_DEST);

		advance_time(ONE_YEAR_MS);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), ETH, EUSD, 1));

		let interest_after = Vaults::<Test>::get((ETH, EUSD, 1)).unwrap().debt.interest;
		// A full year at 50% on 5_000 principal accrues exactly 2_500 EUSD of vault
		// interest (interest is on principal, not the open fee).
		assert_eq!(interest_after - interest_before, 2_500);
		// Fee routing must use the market's stablecoin.
		let residual = 2_500u128 - SpFeeShare::get() * 2_500u128;
		assert_eq!(stable_balance(EUSD, FEE_DEST) - eusd_fee_before, residual);
		// The PUSD market was never involved, so its supply is unchanged.
		assert_eq!(total_stable(PUSD), pusd_before);
	});
}

// A redistribution liquidation in one market does not leak collateral or
// accounting into a market on a different collateral. (The per-market
// redistribution *account* derivation keys on `(collateral, stable)`; asserting
// the accounts are themselves distinct needs a wider `AccountId` than the mock's
// `u64`, so production exercises that — here the asset dimension keeps them
// separate.)
#[test]
fn redistribution_stays_inside_its_market() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(ETH, EUSD);

		// Two PUSD-market vaults: owner 1 is liquidated, owner 2 is the recipient.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// An untouched vault on the other collateral.
		assert_ok!(open(3, ETH, EUSD, 1_000, 500, rate_pct(5, 100)));

		let other_vault = Vaults::<Test>::get((ETH, EUSD, 3)).unwrap();
		let other_state = branch_state(ETH, EUSD).unwrap();
		let other_hold = held(ETH, 3);

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(liquidate_with(DOT, PUSD, 1, |_post_touch| LiquidationAllocation {
			offset: OffsetAllocation { collateral_recipient: 1, debt: 0, collateral: 0 },
			redistribution_collateral: 1_000,
			keeper: KeeperCompensation { recipient: 1, collateral: 0 },
		}));

		// The ETH/EUSD market is untouched: no ETH was parked, no state moved.
		assert_eq!(Vaults::<Test>::get((ETH, EUSD, 3)).unwrap(), other_vault);
		assert_eq!(branch_state(ETH, EUSD).unwrap(), other_state);
		assert_eq!(held(ETH, 3), other_hold);
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
		// Equal collateral and debt, but ETH is priced twice as high.
		assert!(cr_eth > cr_dot, "the higher-priced collateral has the higher CR");
	});
}
