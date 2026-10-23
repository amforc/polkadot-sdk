//! Realistic-scale branch: 10-decimals collateral (`XBT`, ED 0.01 token)
//! against a 6-decimals stable (`USDX`, ED $0.01). The rest of the suite runs
//! unitless with min_balance 1, so it never exercises the ED interactions or
//! the decimals-offset price convention documented on `ProvidePrice` — this
//! module pins both. Anchor numbers: 1_000 XBT = 10^13 minor units, priced at
//! `10^-3` stable-per-collateral minor unit = $10_000; against 5_000 USDX
//! (5×10^9 minor units) of debt that is a 200% CR.

use crate::{
	mock::*,
	tests::{percent, vault_status, ONE_YEAR_MS},
	types::BranchConfigUpdate,
	Error,
};
use frame::{
	arithmetic::Permill,
	prelude::TokenError,
	traits::{
		fungible::Inspect as FungibleInspect,
		tokens::{Fortitude, Preservation},
		AccountTouch,
	},
};
use stablecoin_primitives::CollateralRatio;

fn xbt_vault(owner: AccountId) -> crate::types::Vault<Balance> {
	crate::mock::vault(XBT, USDX, owner)
}

/// Settings denominated for `USDX`: the debt floor is a scale-appropriate 200
/// coins, which is also what clears the stablecoin's own minimum balance.
fn usdx_branch_config() -> BranchConfig<Balance> {
	BranchConfig { minimum_debt: 200 * USD, ..default_branch_config() }
}

/// Register the realistic-scale `(XBT, USDX)` branch: 10-decimals collateral
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
			minimum_collateral: XBT_ED,
			..usdx_branch_config()
		},
	);
}

#[test]
fn fee_account_pays_its_own_asset_account_deposit() {
	ExtBuilder::default().price(TOKEN_X, DEFAULT_PRICE).build_and_execute(|| {
		let deposit = <Assets as AccountTouch<StableId, AccountId>>::deposit_required(USDX);
		let spendable = |who| {
			<Balances as FungibleInspect<AccountId>>::reducible_balance(
				&who,
				Preservation::Expendable,
				Fortitude::Polite,
			)
		};
		let creator_spendable = spendable(STABLECOIN_OWNER);
		let fee_spendable = spendable(FEE_DEST);

		assert_ok!(crate::Pallet::<Test>::create_branch(
			RuntimeOrigin::signed(STABLECOIN_OWNER),
			TOKEN_X,
			USDX,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			usdx_branch_config(),
			(),
		));

		assert_eq!(
			spendable(STABLECOIN_OWNER),
			creator_spendable -
				MarketDepositBase::get() -
				<Balances as FungibleInspect<AccountId>>::minimum_balance(),
			"the creator pays only the branch's own refundable deposit",
		);
		assert_eq!(
			spendable(FEE_DEST),
			fee_spendable - deposit - <Balances as FungibleInspect<AccountId>>::minimum_balance(),
			"the stablecoin-wide fee account owns its refundable deposit",
		);
	});
}

// The math is scale-free end to end: exact CR through the decimals-offset
// price, exact one-year interest, repay to husk, close.
#[test]
fn lifecycle_exact_at_realistic_scale() {
	ExtBuilder::default().build_and_execute(|| {
		register_realistic_market();

		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, percent(5)));

		assert_eq!(stable_balance(USDX, 1), 5_000 * USD, "borrowed amount minted at scale");

		// ceil(5×10^9 · 5% · 7d / 365.25d) = ceil(4_791_238.88) minor units.
		let fee = xbt_vault(1).debt.interest;
		assert_eq!(fee, 4_791_239, "upfront fee recorded as interest");
		// 10^13 minor units at 10^-3 are worth 10^10 against 5_004_791_239 of debt. The ratio
		// rounds down to ≈ 1.998, just under 200%.
		let expected = FixedU128::from_rational_with_rounding(
			10_000_000_000,
			5_004_791_239,
			frame::arithmetic::Rounding::Down,
		);
		assert_eq!(
			crate::Pallet::<Test>::vault_cr(XBT, USDX, 1).expect("cr"),
			CollateralRatio::Ratio(expected)
		);

		// One year at 5% on 5×10^9 minor units: exactly 250 USDX of interest.
		advance_time(ONE_YEAR_MS);
		assert_ok!(poke(9, XBT, USDX, 1));

		assert_eq!(xbt_vault(1).debt.interest, fee + 250 * USD);

		// Fund the interest, repay to a husk, close, and get the collateral back.
		mint_stable(USDX, 1, fee + 250 * USD);
		assert_ok!(repay(1, XBT, USDX, 1, Some(10_000 * USD)));
		assert!(vault_status(XBT, USDX, 1).is_dormant());
		assert_ok!(close_vault(1, XBT, USDX, None));

		assert_eq!(held(XBT, 1), 0);
		assert_eq!(collateral_balance(XBT, 1), 100_000_000 * XBT_UNIT, "genesis balance restored");
	});
}

// The debt floor has to clear the stablecoin's minimum balance, or the smallest
// vault the branch admits could never be paid what it borrows. The floor is
// judged against the coin, not against a number: the same 9_999 that is dust to
// six-decimal USDX is a fine floor for STABLE, whose minimum balance is one unit.
#[test]
fn create_branch_rejects_debt_floor_below_stable_minimum() {
	ExtBuilder::default().price(TOKEN_X, DEFAULT_PRICE).build_and_execute(|| {
		let config = BranchConfig { minimum_debt: USDX_ED - 1, ..usdx_branch_config() };

		assert_noop!(
			crate::Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				TOKEN_X,
				USDX,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				config.clone(),
				(),
			),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::MinimumDebtBelowStableMinimum)
		);

		assert_ok!(crate::Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			TOKEN_X,
			STABLE,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			config,
			(),
		));
	});
}

// Collateral is held to the same rule, and neither floor can be walked back
// after creation: the branch that admits a vault is the branch that has to
// carry it.
#[test]
fn vault_floors_below_the_asset_minimum_are_rejected_on_every_write() {
	ExtBuilder::default().build_and_execute(|| {
		set_price(XBT, FixedU128::from_rational(1u128, 1_000u128));
		let dust_collateral =
			BranchConfig { minimum_collateral: XBT_ED - 1, ..usdx_branch_config() };

		assert_noop!(
			crate::Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				XBT,
				USDX,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				dust_collateral,
				(),
			),
			Error::<Test>::InvalidBranchConfig(
				BranchConfigDefect::MinimumCollateralBelowCollateralMinimum
			)
		);

		register_realistic_market();

		assert_noop!(
			crate::Pallet::<Test>::set_param(
				RuntimeOrigin::signed(ADMIN),
				XBT,
				USDX,
				BranchConfigUpdate::MinimumCollateral(XBT_ED - 1)
			),
			Error::<Test>::InvalidBranchConfig(
				BranchConfigDefect::MinimumCollateralBelowCollateralMinimum
			)
		);
		assert_noop!(
			crate::Pallet::<Test>::set_param(
				RuntimeOrigin::signed(ADMIN),
				XBT,
				USDX,
				BranchConfigUpdate::MinimumDebt(USDX_ED - 1)
			),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::MinimumDebtBelowStableMinimum)
		);
	});
}

// An incremental borrow below the stable's ED is recipient-state-dependent:
// it cannot create a fresh account but lands fine on an existing one.
#[test]
fn borrow_below_stable_ed_depends_on_recipient() {
	ExtBuilder::default().build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, percent(5)));

		let borrow_to = |recipient: AccountId| {
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
		assert_noop!(borrow_to(999), TokenError::BelowMinimum);

		// The owner's account exists (funded by the open), so the same amount
		// lands as a sub-ED top-up.
		assert_ok!(borrow_to(1));
		assert_eq!(stable_balance(USDX, 1), 5_000 * USD + 5_000);
	});
}

// A fee below the stablecoin minimum must reach a fee account with no balance.
#[test]
fn sub_ed_fee_residual_lands_on_the_registration_touched_account() {
	ExtBuilder::default().build_and_execute(|| {
		// Start with a zero fee balance to test the sub-minimum credit.
		SpFeeShare::set(Permill::from_percent(100));
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, percent(5)));
		assert_eq!(stable_balance(USDX, FEE_DEST), 0, "touched at registration, still empty");

		// Route the full credit from here on.
		SpFeeShare::set(Permill::zero());

		// The 476-unit fee is below the 10_000-unit asset minimum.
		let minted_pre = market_state(XBT, USDX).debt.minted_interest;
		let issuance_pre = total_stable(USDX);
		advance_time(60_000);
		assert_ok!(poke(9, XBT, USDX, 1));

		let minted_delta = market_state(XBT, USDX).debt.minted_interest - minted_pre;
		assert_eq!(minted_delta, 476);
		assert_eq!(stable_balance(USDX, FEE_DEST), 476, "sub-ED credit landed");
		assert_eq!(total_stable(USDX) - issuance_pre, 476, "supply backs the recorded fee");
	});
}

// Interest accrual must roll back when the fee account cannot receive its credit.
#[test]
fn frozen_stable_fails_fee_resolution_loudly() {
	ExtBuilder::default().build_and_execute(|| {
		SpFeeShare::set(Permill::zero());
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, percent(5)));
		advance_time(60_000);
		assert_ok!(Assets::freeze_asset(RuntimeOrigin::signed(1), USDX));
		assert_noop!(poke(9, XBT, USDX, 1), Error::<Test>::FeeResolutionFailed);
	});
}

// A keeper leg below the collateral's ED to a fresh keeper reverts the whole
// transactional liquidation — the orchestrator must size non-zero legs to at
// least the collateral asset's minimum balance.
#[test]
fn liquidation_reverts_on_sub_ed_keeper_leg() {
	ExtBuilder::default().build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, percent(5)));
		assert_ok!(open(2, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, percent(5)));
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

// A redemption must either pay the recipient or return the stablecoin, so a collateral leg the
// recipient cannot receive (sub-ED to a fresh account) reverts the step; an ED-sized leg passes.
#[test]
fn redemption_reverts_on_sub_ed_recipient_leg() {
	ExtBuilder::default().build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, percent(5)));

		assert_eq!(collateral_balance(XBT, 997), 0, "recipient is fresh");
		assert_noop!(
			redeem_step(XBT, USDX, 1, 997, 100 * USD, XBT_ED - 1),
			TokenError::CannotCreate
		);

		assert_ok!(redeem_step(XBT, USDX, 1, 997, 100 * USD, XBT_ED));
		assert_eq!(collateral_balance(XBT, 997), XBT_ED, "ED-sized recipient leg paid");
	});
}

#[test]
fn repay_preserves_exact_supply_at_the_payer_minimum_balance() {
	for requested in [Some(1_000 * USD), Some(10_000 * USD), None] {
		for remainder in [0, USDX_ED / 2, USDX_ED] {
			ExtBuilder::default().build_and_execute(|| {
				register_realistic_market();
				assert_ok!(open(3, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, percent(5)));
				let debt = xbt_vault(3).debt.total();
				let payment = requested.map_or(debt, |amount| amount.min(debt));
				mint_stable(USDX, 4, payment + remainder);

				if remainder > 0 && remainder < USDX_ED {
					assert_noop!(repay(4, XBT, USDX, 3, requested), TokenError::FundsUnavailable);
				} else {
					let issuance = total_stable(USDX);
					assert_ok!(repay(4, XBT, USDX, 3, requested));
					assert_eq!(stable_balance(USDX, 4), remainder);
					assert_eq!(total_stable(USDX), issuance - payment);
					assert_eq!(xbt_vault(3).debt.total(), debt - payment);
				}
			});
		}
	}
}
