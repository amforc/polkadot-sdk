//! Realistic-scale market: 10-decimals collateral (`XBT`, ED 0.01 token)
//! against a 6-decimals stable (`USDX`, ED $0.01). The rest of the suite runs
//! unitless with min_balance 1, so it never exercises the ED interactions or
//! the decimals-offset price convention documented on `ProvidePrice` — this
//! module pins both. Anchor numbers: 1_000 XBT = 10^13 minor units, priced at
//! `10^-3` stable-per-collateral minor unit = $10_000; against 5_000 USDX
//! (5×10^9 minor units) of debt that is a 200% CR.

use crate::{mock::*, tests::rate_pct};
use frame::{
	arithmetic::Permill, prelude::TokenError, traits::fungibles::Mutate as FungiblesMutate,
};
use pusd_primitives::{collateralization_ratio, MILLIS_PER_YEAR};

fn vault(owner: AccountId) -> crate::types::Vault<Balance> {
	crate::pallet::Vaults::<Test>::get((XBT, USDX, owner)).expect("vault")
}

/// Register the realistic-scale `(XBT, USDX)` market: 10-decimals collateral
/// against a 6-decimals stable, both with a 0.01 minimum balance.
///
/// The price follows `ProvidePrice`'s unit contract: human $10/token scaled by
/// `10^(6 - 10)` → `10^-3` stable minor-units per collateral minor-unit.
fn register_realistic_market() {
	register_market_with(
		XBT,
		USDX,
		FixedU128::from_rational(1u128, 1_000u128),
		BranchConfig {
			debt_ceiling: 100_000_000 * USD,
			minimum_debt: 200 * USD,
			minimum_collateral: XBT_ED,
			..default_branch_config()
		},
	);
}

// The math is scale-free end to end: exact CR through the decimals-offset
// price, exact one-year interest, repay to husk, close.
#[test]
fn lifecycle_exact_at_realistic_scale() {
	build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		assert_eq!(stable_balance(USDX, 1), 5_000 * USD, "borrowed amount minted at scale");

		// CR = (10^13 × 10^-3) / (5×10^9 + upfront fee) — the human 200% less
		// the fee's dilution, exactly (recomputed from raw inputs through the
		// shared primitive, so storage + oracle plumbing is what's pinned).
		let fee = vault(1).debt.interest;
		assert!(fee > 0, "upfront fee recorded as interest");
		let cr = crate::Pallet::<Test>::vault_cr(XBT, USDX, 1).expect("cr");
		let expected = collateralization_ratio(
			&pusd_primitives::DebtCollateral {
				debt: 5_000 * USD + fee,
				collateral: 1_000 * XBT_UNIT,
			},
			FixedU128::from_rational(1u128, 1_000u128),
		)
		.expect("cr defined");
		assert_eq!(cr, expected);
		assert_eq!(expected.trunc(), FixedU128::from_u32(1), "human CR just under 200%");

		// One year at 5% on 5×10^9 minor units: exactly 250 USDX of interest.
		advance_time(MILLIS_PER_YEAR);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), XBT, USDX, 1));
		assert_eq!(vault(1).debt.interest, fee + 250 * USD);

		// Fund the interest, repay to a husk, close, and get the collateral back.
		assert_ok!(<Assets as FungiblesMutate<AccountId>>::mint_into(USDX, &1, fee + 250 * USD));
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(1),
			XBT,
			USDX,
			1,
			10_000 * USD
		));
		assert!(crate::Pallet::<Test>::vault_status(XBT, USDX, 1).expect("status").is_dormant());
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), XBT, USDX, None));
		assert_eq!(held(XBT, 1), 0);
		assert_eq!(collateral_balance(XBT, 1), 100_000_000 * XBT_UNIT, "genesis balance restored");
	});
}

// A config may set `minimum_debt` below the stable's ED (the guard floor is
// 100), but opening in `[minimum_debt, ED)` then fails at the mint: the owner
// has no stable account and the amount cannot create one. A sane realistic
// config keeps `minimum_debt >= stable ED`.
#[test]
fn open_below_stable_ed_reverts() {
	build_and_execute(|| {
		let config = BranchConfig { minimum_debt: 100, ..default_branch_config() };
		set_price(TOKEN_X, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(crate::Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			TOKEN_X,
			USDX,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			config,
		));
		assert_ok!(crate::Pallet::<Test>::set_global_debt_ceiling(
			RuntimeOrigin::root(),
			TOKEN_X,
			GLOBAL_CEILING
		));

		assert_eq!(stable_balance(USDX, 4), 0, "owner has no USDX account yet");
		assert_noop!(
			open(4, TOKEN_X, USDX, 10_000, 5_000, rate_pct(5, 100)),
			TokenError::BelowMinimum
		);
	});
}

// An incremental borrow below the stable's ED is recipient-state-dependent:
// it cannot create a fresh account but lands fine on an existing one.
#[test]
fn borrow_below_stable_ed_depends_on_recipient() {
	build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));

		let borrow = |recipient: AccountId| {
			crate::Pallet::<Test>::borrow(
				RuntimeOrigin::signed(1),
				XBT,
				USDX,
				5_000, // $0.005, below the 10_000-unit ED
				None,
				Some(recipient),
				Position::endpoints_only(),
			)
		};
		assert_eq!(stable_balance(USDX, 999), 0, "recipient is fresh");
		assert_noop!(borrow(999), TokenError::BelowMinimum);

		// The owner's account exists (funded by the open), so the same amount
		// lands as a sub-ED top-up.
		assert_ok!(borrow(1));
		assert_eq!(stable_balance(USDX, 1), 5_000 * USD + 5_000);
	});
}

// A fee credit below the stable's ED resolved to a fresh fee account is
// silently dropped by `OnUnbalanced` — supply is rescinded while the fee stays
// recorded in `minted_interest`. Pre-funding the fee account (the documented
// runtime duty on `Config::FeeHandler`) makes the same credit land.
#[test]
fn sub_ed_fee_residual_is_dropped_without_prefund() {
	build_and_execute(|| {
		// Drop the whole upfront-fee credit at open so FEE_DEST stays fresh.
		SpFeeShare::set(Permill::from_percent(100));
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		assert_eq!(stable_balance(USDX, FEE_DEST), 0);

		// Route the full credit from here on.
		SpFeeShare::set(Permill::zero());

		// One minute at 5% on 5×10^9: ceil(2.5×10^8 × 60_000 / MILLIS_PER_YEAR)
		// = 476 minor units — well below the 10_000-unit ED.
		let minted_pre = branch_state(XBT, USDX).expect("state").debt.minted_interest;
		let issuance_pre = total_stable(USDX);
		advance_time(60_000);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), XBT, USDX, 1));
		let minted_delta =
			branch_state(XBT, USDX).expect("state").debt.minted_interest - minted_pre;
		assert_eq!(minted_delta, 476);
		assert_eq!(stable_balance(USDX, FEE_DEST), 0, "sub-ED credit dropped");
		assert_eq!(total_stable(USDX), issuance_pre, "mint rescinded: supply overstated by ledger");

		// Pre-fund the fee account; the next sub-ED credit now lands.
		assert_ok!(<Assets as FungiblesMutate<AccountId>>::mint_into(USDX, &FEE_DEST, USDX_ED));
		let minted_pre = branch_state(XBT, USDX).expect("state").debt.minted_interest;
		advance_time(60_000);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), XBT, USDX, 1));
		let minted_delta =
			branch_state(XBT, USDX).expect("state").debt.minted_interest - minted_pre;
		assert!(0 < minted_delta);
		assert!(minted_delta < USDX_ED);
		assert_eq!(stable_balance(USDX, FEE_DEST), USDX_ED + minted_delta);
	});
}

// A keeper leg below the collateral's ED to a fresh keeper reverts the whole
// transactional liquidation — the orchestrator must size non-zero legs to at
// least the collateral asset's minimum balance.
#[test]
fn liquidation_reverts_on_sub_ed_keeper_leg() {
	build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		assert_ok!(open(2, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		// 10^13 × 5×10^-5 = 5×10^8 stable minor units ≪ 110% of the debt.
		set_price(XBT, FixedU128::from_rational(5u128, 100_000u128));

		let keeper_leg = |collateral: Balance| {
			liquidate_with(XBT, USDX, 1, |_| LiquidationAllocation {
				offset: OffsetAllocation { collateral_recipient: 0, debt: 0, collateral: 0 },
				redistribution_collateral: 0,
				keeper: KeeperCompensation { recipient: 998, collateral },
			})
		};
		assert_eq!(collateral_balance(XBT, 998), 0, "keeper is fresh");
		assert_noop!(keeper_leg(XBT_ED - 1), TokenError::CannotCreate);
		assert!(
			crate::pallet::Vaults::<Test>::contains_key((XBT, USDX, 1)),
			"failed liquidation rolled back"
		);

		assert_ok!(keeper_leg(XBT_ED));
		assert_eq!(collateral_balance(XBT, 998), XBT_ED, "ED-sized keeper leg paid");
	});
}

// A recipient leg below the collateral's ED to a fresh recipient reverts the
// step; an ED-sized leg passes.
#[test]
fn redemption_reverts_on_sub_ed_recipient_leg() {
	build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));

		assert_eq!(collateral_balance(XBT, 997), 0, "recipient is fresh");
		assert_noop!(
			redeem_step(XBT, USDX, 1, 997, 100 * USD, XBT_ED - 1),
			TokenError::CannotCreate
		);

		assert_ok!(redeem_step(XBT, USDX, 1, 997, 100 * USD, XBT_ED));
		assert_eq!(collateral_balance(XBT, 997), XBT_ED, "ED-sized recipient leg paid");
	});
}

// Repaying with `Preservation::Expendable` reaps a payer left below the
// stable's ED: the sub-ED remainder is burned out of the supply.
#[test]
fn repay_dusts_sub_ed_payer_remainder() {
	build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(3, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));

		// Top the owner up to debt + a sub-ED remainder of 5_000 minor units.
		let debt = vault(3).debt.total();
		let top_up = debt - 5_000 * USD + 5_000;
		assert_ok!(<Assets as FungiblesMutate<AccountId>>::mint_into(USDX, &3, top_up));
		assert_eq!(stable_balance(USDX, 3), debt + 5_000);

		let issuance_pre = total_stable(USDX);
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(3),
			XBT,
			USDX,
			3,
			10_000 * USD
		));
		assert_eq!(stable_balance(USDX, 3), 0, "payer reaped, sub-ED remainder gone");
		assert_eq!(total_stable(USDX), issuance_pre - debt - 5_000, "remainder burned from supply");
	});
}
