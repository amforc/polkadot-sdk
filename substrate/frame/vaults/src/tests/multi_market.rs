//! Multi-stablecoin / multi-collateral branch tests.
//!
//! A branch is one stablecoin against one collateral. These exercise the
//! independence and shared-collateral properties the generalisation introduces:
//! one owner running several branches, branches sharing a collateral or a
//! stablecoin, in-branch isolation of redemption and close, and per-branch
//! pricing.

use crate::{
	mock::*,
	pallet::StablecoinDebt,
	tests::{percent, ONE_YEAR_MS},
};
use stablecoin_primitives::{CollateralRatio, VaultInterface};

/// Redeems and closes owner 1's DOT/STABLE vault, checking that the owner's vault and branch on
/// `collateral`/`stable` stay untouched throughout.
fn redeem_and_close_dot_vault_sparing(collateral: AssetId, stable: StableId) {
	let dot = vault(DOT, STABLE, 1);
	assert_eq!((dot.collateral, dot.debt.principal), (1_000, 2_000));
	assert_eq!(market_state(DOT, STABLE).debt.principal, 2_000);
	let other = vault(collateral.clone(), stable, 1);
	let other_state = market_state(collateral.clone(), stable);

	assert_eq!(redeem(DOT, STABLE, 9, 500), Ok(1));

	assert_eq!(vault(collateral.clone(), stable, 1), other);
	assert_eq!(market_state(collateral.clone(), stable), other_state.clone());

	mint_stable(STABLE, 1, 10_000);
	assert_ok!(repay(1, DOT, STABLE, 1, None));
	assert_ok!(close_vault(1, DOT, STABLE, None));

	assert!(!vault_exists(DOT, STABLE, 1));
	assert_eq!(vault(collateral.clone(), stable, 1), other);
	assert_eq!(market_state(collateral, stable), other_state);
}

// One owner can run branches on distinct assets. Redemption and close change only the selected
// branch.
#[test]
fn owner_runs_markets_on_distinct_assets_independently() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		register_market(ETH, EUSD);
		assert_ok!(open(1, DOT, STABLE, 1_000, 2_000, percent(5)));
		assert_ok!(open(1, ETH, EUSD, 500, 1_000, percent(5)));
		assert_eq!((stable_balance(STABLE, 1), stable_balance(EUSD, 1)), (2_000, 1_000));
		assert_eq!((held(DOT, 1), held(ETH, 1)), (1_000, 500));
		let other = vault(ETH, EUSD, 1);
		assert_eq!((other.collateral, other.debt.principal), (500, 1_000));
		assert_eq!(market_state(ETH, EUSD).debt.principal, 1_000);

		redeem_and_close_dot_vault_sparing(ETH, EUSD);

		assert_eq!(held(DOT, 1), 0);
		assert_eq!(held(ETH, 1), 500);
	});
}

// One owner can share a stablecoin across branches. Redemption and close change only the
// selected branch, while both vaults mint into the same balance.
#[test]
fn owner_runs_markets_sharing_a_stablecoin_independently() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		register_market(ETH, STABLE);
		assert_ok!(open(1, DOT, STABLE, 1_000, 2_000, percent(5)));
		assert_ok!(open(1, ETH, STABLE, 1_000, 3_000, percent(7)));
		assert_eq!(stable_balance(STABLE, 1), 5_000);
		assert_eq!((held(DOT, 1), held(ETH, 1)), (1_000, 1_000));
		let other = vault(ETH, STABLE, 1);
		assert_eq!((other.collateral, other.debt.principal), (1_000, 3_000));
		assert_eq!(market_state(ETH, STABLE).debt.principal, 3_000);

		redeem_and_close_dot_vault_sparing(ETH, STABLE);

		assert_eq!(held(DOT, 1), 0);
		assert_eq!(held(ETH, 1), 1_000);
	});
}

// One owner can share a collateral across branches. Redemption and close change only the selected
// branch and release only its share of the hold.
#[test]
fn owner_runs_markets_sharing_a_collateral_independently() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		register_market(DOT, EUSD);
		assert_ok!(open(1, DOT, STABLE, 1_000, 2_000, percent(5)));
		assert_ok!(open(1, DOT, EUSD, 600, 1_000, percent(5)));
		assert_eq!((stable_balance(STABLE, 1), stable_balance(EUSD, 1)), (2_000, 1_000));
		assert_eq!(held(DOT, 1), 1_600);
		let other = vault(DOT, EUSD, 1);
		assert_eq!((other.collateral, other.debt.principal), (600, 1_000));
		assert_eq!(market_state(DOT, EUSD).debt.principal, 1_000);

		redeem_and_close_dot_vault_sparing(DOT, EUSD);

		// The other branch's 600 stays held.
		assert_eq!(held(DOT, 1), 600);
	});
}

// The stablecoin debt view must include interest accrued on branches nobody has touched.
#[test]
fn stablecoin_debt_accrues_interest_across_untouched_markets() {
	ExtBuilder::default()
		.branch(DOT, STABLE)
		.branch(ETH, STABLE)
		.build_and_execute(|| {
			assert_ok!(open(1, DOT, STABLE, 1_000, 2_000, percent(5)));
			assert_ok!(open(2, ETH, STABLE, 1_000, 3_000, percent(7)));
			let stored = StablecoinDebt::<Test>::get(STABLE);

			advance_time(ONE_YEAR_MS);

			// Principal plus the upfront fee of each open: 2_000 + ceil(2_000 · 5% · 7d / 365.25d)
			// = 2_002 and 3_000 + ceil(3_000 · 7% · 7d / 365.25d) = 3_005. A year adds
			// 2_000 · 5% = 100 and 3_000 · 7% = 210 that no branch has minted yet.
			assert_eq!(<crate::Pallet<Test> as VaultInterface>::stablecoin_debt(&STABLE), 5_317);
			// The view pairs that accrued debt with the limit borrowing is checked against.
			assert_eq!(
				crate::Pallet::<Test>::stablecoin_debt_and_ceiling(STABLE),
				(5_317, GLOBAL_CEILING)
			);
			assert_eq!(
				StablecoinDebt::<Test>::get(STABLE),
				stored,
				"read-only projection must not touch either branch"
			);
		});
}

// CR is computed per branch: equal collateral and debt yield different ratios
// when the collateral prices differ.
#[test]
fn cr_differs_across_markets_when_prices_differ() {
	ExtBuilder::default()
		.branch(DOT, STABLE)
		.branch(ETH, EUSD)
		.build_and_execute(|| {
			set_price(DOT, DEFAULT_PRICE);
			set_price(ETH, FixedU128::from_u32(20));

			assert_ok!(open(1, DOT, STABLE, 1_000, 2_000, percent(5)));
			assert_ok!(open(2, ETH, EUSD, 1_000, 2_000, percent(5)));

			let cr_dot = crate::Pallet::<Test>::vault_cr(DOT, STABLE, 1).unwrap();
			let cr_eth = crate::Pallet::<Test>::vault_cr(ETH, EUSD, 2).unwrap();
			// Both vaults owe 2_000 plus the upfront fee of ceil(2_000 · 5% · 7d / 365.25d) = 2.
			// The 1_000 of collateral is worth 10_000 on DOT and 20_000 on ETH, and the
			// ratio rounds down: 10_000 / 2_002 = 4.995004… and 20_000 / 2_002 =
			// 9.990009….
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
