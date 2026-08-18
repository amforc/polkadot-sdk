//! Realistic-scale market: 10-decimals collateral (`XBT`, ED 0.01 token)
//! against a 6-decimals stable (`USDX`, ED $0.01). The rest of the suite runs
//! unitless with min_balance 1, so it never exercises the ED interactions or
//! the decimals-offset price convention documented on `ProvidePrice` — this
//! module pins both. Anchor numbers: 1_000 XBT = 10^13 minor units, priced at
//! `10^-3` stable-per-collateral minor unit = $10_000; against 5_000 USDX
//! (5×10^9 minor units) of debt that is a 200% CR.

use crate::{mock::*, tests::rate_pct, types::BranchConfigUpdate, Error};
use frame::{
	arithmetic::Permill,
	prelude::TokenError,
	traits::{
		fungible::Inspect as FungibleInspect,
		fungibles::Mutate as FungiblesMutate,
		tokens::{Fortitude, Preservation},
		AccountTouch,
	},
};
use pusd_primitives::{collateralization_ratio, CollateralRatio, MILLIS_PER_YEAR};

fn vault(owner: AccountId) -> crate::types::Vault<Balance> {
	crate::mock::vault(XBT, USDX, owner)
}

/// Settings denominated for `USDX`: the debt floor is a scale-appropriate 200
/// coins, which is also what clears the stablecoin's own minimum balance.
fn usdx_branch_config() -> BranchConfig<Balance> {
	BranchConfig { minimum_debt: 200 * USD, ..default_branch_config() }
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
			minimum_collateral: XBT_ED,
			..usdx_branch_config()
		},
	);
}

#[test]
fn fee_account_pays_its_own_asset_account_deposit() {
	build_and_execute(|| {
		set_price(TOKEN_X, FixedU128::from_rational(10u128, 1u128));
		let deposit = <Assets as AccountTouch<StableId, AccountId>>::deposit_required(USDX);
		let spendable = |who| {
			<Balances as FungibleInspect<AccountId>>::reducible_balance(
				&who,
				Preservation::Expendable,
				Fortitude::Polite,
			)
		};
		let creator_spendable = spendable(PUSD_OWNER);
		let fee_spendable = spendable(FEE_DEST);

		assert_ok!(crate::Pallet::<Test>::create_branch(
			RuntimeOrigin::signed(PUSD_OWNER),
			TOKEN_X,
			USDX,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			usdx_branch_config(),
			(),
		));

		assert_eq!(
			spendable(PUSD_OWNER),
			creator_spendable -
				MarketDepositBase::get() -
				<Balances as FungibleInspect<AccountId>>::minimum_balance(),
			"the creator pays only the market's own refundable deposit",
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
	build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		assert_eq!(stable_balance(USDX, 1), 5_000 * USD, "borrowed amount minted at scale");

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
		.expect("cr");
		assert_eq!(cr, expected);
		let CollateralRatio::Ratio(expected) = expected else { panic!("vault carries debt") };
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
			Some(10_000 * USD)
		));
		assert!(crate::Pallet::<Test>::vault_status(XBT, USDX, 1).expect("status").is_dormant());
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), XBT, USDX, None));
		assert_eq!(held(XBT, 1), 0);
		assert_eq!(collateral_balance(XBT, 1), 100_000_000 * XBT_UNIT, "genesis balance restored");
	});
}

// The debt floor has to clear the stablecoin's minimum balance, or the smallest
// vault the market admits could never be paid what it borrows. The floor is
// judged against the coin, not against a number: the same 9_999 that is dust to
// six-decimal USDX is a fine floor for PUSD, whose minimum balance is one unit.
#[test]
fn create_branch_rejects_debt_floor_below_stable_minimum() {
	build_and_execute(|| {
		set_price(TOKEN_X, FixedU128::from_rational(10u128, 1u128));
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
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			config,
			(),
		));
	});
}

// Collateral is held to the same rule, and neither floor can be walked back
// after creation: the market that admits a vault is the market that has to
// carry it.
#[test]
fn vault_floors_below_the_asset_minimum_are_rejected_on_every_write() {
	build_and_execute(|| {
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

// A fee below the stablecoin minimum must reach a fee account with no balance.
#[test]
fn sub_ed_fee_residual_lands_on_the_registration_touched_account() {
	build_and_execute(|| {
		// Start with a zero fee balance to test the sub-minimum credit.
		SpFeeShare::set(Permill::from_percent(100));
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		assert_eq!(stable_balance(USDX, FEE_DEST), 0, "touched at registration, still empty");

		// Route the full credit from here on.
		SpFeeShare::set(Permill::zero());

		// The 475-unit fee is below the 10_000-unit asset minimum.
		let minted_pre = branch_state(XBT, USDX).expect("state").debt.minted_interest;
		let issuance_pre = total_stable(USDX);
		advance_time(60_000);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), XBT, USDX, 1));
		let minted_delta =
			branch_state(XBT, USDX).expect("state").debt.minted_interest - minted_pre;
		assert_eq!(minted_delta, 475);
		assert_eq!(stable_balance(USDX, FEE_DEST), 475, "sub-ED credit landed");
		assert_eq!(total_stable(USDX) - issuance_pre, 475, "supply backs the recorded fee");
	});
}

// Interest accrual must roll back when the fee account cannot receive its credit.
#[test]
fn frozen_stable_fails_fee_resolution_loudly() {
	build_and_execute(|| {
		SpFeeShare::set(Permill::zero());
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		advance_time(60_000);
		assert_ok!(Assets::freeze_asset(RuntimeOrigin::signed(1), USDX));
		assert_noop!(
			crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), XBT, USDX, 1),
			Error::<Test>::FeeResolutionFailed
		);
	});
}

/// Prices the market so a 1_000 XBT vault is far under water — 10^13 × 10^-5 = 10^8 stable
/// minor units against ~5_000 USDX of debt — and sizes keeper compensation as one flat value:
/// at this price a value of `v` buys `v × 10^5` collateral minor units, so 500 is half the ED
/// and 2_000 is twice it.
fn crash_price_with_flat_keeper_value(value: Balance) {
	set_price(XBT, FixedU128::from_rational(1u128, 100_000u128));
	let set = |update| Vaults::set_param(RuntimeOrigin::signed(ADMIN), XBT, USDX, update);
	assert_ok!(set(BranchConfigUpdate::KeeperPercentCompensation(Permill::zero())));
	assert_ok!(set(BranchConfigUpdate::KeeperCompensationCapValue(value)));
	assert_ok!(set(BranchConfigUpdate::KeeperFlatCompensationValue(value)));
}

// A keeper leg below the collateral's ED to a fresh keeper is planned out rather than failing
// the liquidation: an unsafe vault left in the market is worse than an unpaid keeper, and
// holding an account that can receive collateral is the keeper's own responsibility. The freed
// reward falls through to the waterfall exactly as a skipped JIT leg does.
#[test]
fn liquidation_plans_out_a_sub_ed_keeper_leg() {
	build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		assert_ok!(open(2, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		crash_price_with_flat_keeper_value(500);
		let custody = Vaults::redistribution_account(&XBT, &USDX);
		let custody_before = held(XBT, custody);

		assert_eq!(collateral_balance(XBT, 998), 0, "keeper is fresh");
		assert_ok!(liquidate(998, XBT, USDX, 1, 0, 0));
		assert!(!crate::pallet::Vaults::<Test>::contains_key((XBT, USDX, 1)), "vault liquidated");
		assert_eq!(collateral_balance(XBT, 998), 0, "the keeper stayed unpaid");
		// The vault is far under water, so its whole collateral is seized; with the reward
		// planned out, all of it reaches redistribution custody.
		assert_eq!(held(XBT, custody) - custody_before, 1_000 * XBT_UNIT);
	});
}

// The JIT collateral share is the keeper's other collateral leg and follows the same rule: a
// share a fresh keeper cannot receive drops the trade instead of failing the liquidation, and
// the keeper's stablecoin stays untouched. The minimum contribution buys a share of ~2×10^5
// minor units here, far below the 10^8 ED.
#[test]
fn liquidation_drops_a_jit_share_the_keeper_cannot_receive() {
	build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		assert_ok!(open(2, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		crash_price_with_flat_keeper_value(500);
		mint_stable(USDX, 998, USD);
		let custody = Vaults::redistribution_account(&XBT, &USDX);
		let custody_before = held(XBT, custody);

		assert_eq!(collateral_balance(XBT, 998), 0, "keeper is fresh");
		assert_ok!(liquidate(998, XBT, USDX, 1, 100, 0));
		assert!(!crate::pallet::Vaults::<Test>::contains_key((XBT, USDX, 1)), "vault liquidated");
		assert_eq!(
			stable_balance(USDX, 998),
			USD,
			"no JIT burn for a share the keeper cannot take"
		);
		assert_eq!(collateral_balance(XBT, 998), 0, "the keeper received nothing");
		assert_eq!(held(XBT, custody) - custody_before, 1_000 * XBT_UNIT);
	});
}

// Compensation settles before the JIT share, so a reward that clears the ED opens the keeper's
// account and a sub-ED JIT share follows into it: the payability check reads the combined
// intake, not each leg alone.
#[test]
fn reward_above_ed_carries_a_sub_ed_jit_share() {
	build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		assert_ok!(open(2, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		crash_price_with_flat_keeper_value(2_000);
		mint_stable(USDX, 998, USD);
		let custody = Vaults::redistribution_account(&XBT, &USDX);
		let custody_before = held(XBT, custody);

		assert_eq!(collateral_balance(XBT, 998), 0, "keeper is fresh");
		assert_ok!(liquidate(998, XBT, USDX, 1, 100, 0));
		assert_eq!(stable_balance(USDX, 998), USD - 100, "the JIT trade executed");
		let reward = 2 * XBT_ED;
		let jit_share = collateral_balance(XBT, 998) - reward;
		assert!(jit_share > 0, "the JIT share was paid");
		assert!(jit_share < XBT_ED, "the JIT share alone would not have opened the account");
		assert_eq!(
			held(XBT, custody) - custody_before + reward + jit_share,
			1_000 * XBT_UNIT,
			"seized collateral is conserved across custody and the keeper"
		);
	});
}

// Redistribution custody on an issued collateral: `pallet-assets` keeps the
// minimum balance untouchable once an account holds anything, so seizing into
// custody only works because registration seeded it. The rest of the suite
// redistributes native collateral, where a provider reference waives the ED and
// hides this.
#[test]
fn redistribution_custody_holds_issued_collateral() {
	build_and_execute(|| {
		register_realistic_market();
		let custody = Vaults::redistribution_account(&XBT, &USDX);
		assert_eq!(collateral_balance(XBT, custody), XBT_ED, "registration seeded custody");

		// Vaults 1 and 2 sit at par, vault 3 at 400%, so two liquidations still
		// leave an eligible recipient.
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		assert_ok!(open(2, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		assert_ok!(open(3, XBT, USDX, 4_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		set_price(XBT, FixedU128::from_rational(1u128, 2_000u128));

		// No Stability capital, so the whole seizure lands in custody.
		assert_ok!(liquidate(9, XBT, USDX, 1, 0, 0));
		let first = held(XBT, custody);
		assert!(first > 0, "seizure reached custody");
		assert_eq!(first, branch_state(XBT, USDX).unwrap().pending_redistribution_collateral);
		assert_eq!(collateral_balance(XBT, custody), XBT_ED, "the seed stays free");

		// The seed is a float, not a consumable: the next seizure holds too.
		assert_ok!(liquidate(9, XBT, USDX, 2, 0, 0));
		assert!(held(XBT, custody) > first, "second seizure added to custody");
		assert_eq!(collateral_balance(XBT, custody), XBT_ED);

		// Materializing drains the hold and leaves the seed behind.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), XBT, USDX, 3));
		assert_eq!(held(XBT, custody), 0);
		assert_eq!(collateral_balance(XBT, custody), XBT_ED);
	});
}

// A JIT target inside `(balance - ED, balance)` rounds down to the preserving
// limit. The exact burn then succeeds under `Preserve` and leaves the keeper's
// stable account alive at exactly the ED.
#[test]
fn liquidation_jit_rounds_dead_zone_to_preserving_limit() {
	build_and_execute(|| {
		register_realistic_market();
		assert_ok!(open(1, XBT, USDX, 1_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		assert_ok!(open(2, XBT, USDX, 2_000 * XBT_UNIT, 5_000 * USD, rate_pct(5, 100)));
		// Vault 1 falls just below par while vault 2 remains healthy.
		set_price(XBT, FixedU128::from_rational(1u128, 2_000u128));

		let funded = 200 * USD;
		let keeper_balance = funded + USDX_ED;
		mint_stable(USDX, 3, keeper_balance);
		// Settle the remainder through the pending pool so this test isolates
		// the keeper debit from issued-collateral redistribution custody.
		PendingSpCapacity::set(10_000 * USD);
		let issuance_before = total_stable(USDX);

		// `keeper_balance - 1` is above the preserving limit `funded` but below
		// the full expendable balance, so `reducible_debit` must round it down.
		assert_eq!(
			pusd_primitives::reducible_debit::<Assets, _>(USDX, &3, keeper_balance - 1),
			(funded, Preservation::Preserve)
		);
		assert_ok!(liquidate(3, XBT, USDX, 1, keeper_balance - 1, 0));

		assert_eq!(stable_balance(USDX, 3), USDX_ED, "keeper account survives at ED");
		assert_eq!(issuance_before - total_stable(USDX), funded, "exact rounded burn");
		assert!(crate::pallet::Vaults::<Test>::get((XBT, USDX, 1)).is_none());
	});
}

// A redemption must either pay the recipient or return the stablecoin, so a collateral leg the
// recipient cannot receive (sub-ED to a fresh account) reverts the step; an ED-sized leg passes.
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
			Some(10_000 * USD)
		));
		assert_eq!(stable_balance(USDX, 3), 0, "payer reaped, sub-ED remainder gone");
		assert_eq!(total_stable(USDX), issuance_pre - debt - 5_000, "remainder burned from supply");
	});
}
