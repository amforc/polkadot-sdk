//! Tests for the refundable storage deposit charged on `open_vault`.
//!
//! The mock settles in the collateral when it is native or sufficient and in native otherwise,
//! and re-prices sufficient collateral through the oracle or a mock fallback quote.

use crate::{mock::*, tests::percent};
use frame::{prelude::TokenError, traits::Footprint};

const OWNER: AccountId = 1;

fn default_rate() -> FixedU128 {
	percent(5)
}

fn repay_all(owner: AccountId, collateral: AssetId, stable: StableId) {
	// Headroom for the upfront fee that the debt includes beyond the minted principal.
	mint_stable(stable, owner, 100);
	assert_ok!(repay(owner, collateral, stable, owner, Some(10_000)));
}

#[test]
fn footprint_includes_vault_record_and_rate_list_node() {
	ExtBuilder::default().build_and_execute(|| {
		let footprint = Vaults::vault_footprint(&DOT, &STABLE, &OWNER);

		assert_eq!(footprint.asset, DOT);
		// One item of 398 bytes: the 93-byte hashed vault key, the 176-byte max-encoded `Vault`,
		// the 17-byte deposit ticket (1-byte asset plus 16-byte balance), and the 112-byte
		// rate-list node.
		assert_eq!(footprint.footprint, Footprint::from_parts(1, 398));
	});
}

#[test]
fn native_collateral_pays_the_deposit_in_native() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		let free_before = collateral_balance(DOT, OWNER);

		assert_ok!(open(OWNER, DOT, STABLE, 1_000, 500, default_rate()));

		assert_eq!(vault_deposit_held(DOT, OWNER), VAULT_DEPOSIT);
		// The collateral and its storage deposit use separate holds.
		assert_eq!(held(DOT, OWNER), 1_000);
		assert_eq!(collateral_balance(DOT, OWNER), free_before - 1_000 - VAULT_DEPOSIT);
	});
}

#[test]
fn sufficient_collateral_pays_the_deposit_in_that_asset_at_the_oracle_rate() {
	// 250 native at 10 each are worth 2_500; at 40 per ETH that is 62.5, rounded up.
	ExtBuilder::default()
		.price(DOT, DEFAULT_PRICE)
		.price(ETH, FixedU128::from_u32(40))
		.branch(ETH, STABLE)
		.build_and_execute(|| {
			let free_before = collateral_balance(ETH, OWNER);

			assert_ok!(open(OWNER, ETH, STABLE, 1_000, 500, default_rate()));

			assert_eq!(vault_deposit_held(ETH, OWNER), 63);
			assert_eq!(vault_deposit_held(DOT, OWNER), 0);
			assert_eq!(collateral_balance(ETH, OWNER), free_before - 1_000 - 63);
		});
}

#[test]
fn unpriceable_sufficient_collateral_rejects_the_open_atomically() {
	ExtBuilder::default().branch(TOKEN_X, STABLE).build_and_execute(|| {
		MockFallbackRate::set(None);

		assert_noop!(
			open(OWNER, TOKEN_X, STABLE, 1_000, 500, default_rate()),
			DispatchError::Unavailable
		);
	});
}

#[test]
fn insufficient_collateral_falls_back_to_native() {
	ExtBuilder::default().branch(INSUFFICIENT, STABLE).build_and_execute(|| {
		let native_before = collateral_balance(DOT, ADMIN);

		assert_ok!(open(ADMIN, INSUFFICIENT, STABLE, 1_000, 500, default_rate()));

		assert_eq!(vault_deposit_held(DOT, ADMIN), VAULT_DEPOSIT);
		assert_eq!(vault_deposit_held(INSUFFICIENT, ADMIN), 0);
		assert_eq!(held(INSUFFICIENT, ADMIN), 1_000);
		assert_eq!(collateral_balance(DOT, ADMIN), native_before - VAULT_DEPOSIT);
	});
}

#[test]
fn close_refunds_the_original_ticket_to_the_owner_after_a_price_change() {
	ExtBuilder::default()
		.price(DOT, DEFAULT_PRICE)
		.price(ETH, FixedU128::from_u32(40))
		.branch(ETH, STABLE)
		.build_and_execute(|| {
			assert_ok!(open(OWNER, ETH, STABLE, 1_000, 500, default_rate()));
			assert_eq!(vault_deposit_held(ETH, OWNER), 63);
			repay_all(OWNER, ETH, STABLE);

			// The current quote is now lower, but the stored ticket still refunds the original 63.
			set_price(ETH, FixedU128::from_u32(80));
			let owner_before = collateral_balance(ETH, OWNER);
			let recipient_before = collateral_balance(ETH, 2);

			assert_ok!(close_vault(OWNER, ETH, STABLE, Some(2)));

			assert_eq!(collateral_balance(ETH, 2), recipient_before + 1_000);
			assert_eq!(collateral_balance(ETH, OWNER), owner_before + 63);
			assert_eq!(vault_deposit_held(ETH, OWNER), 0);
		});
}

#[test]
fn owner_short_of_the_deposit_cannot_open() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		const NEWCOMER: AccountId = 11;
		// A balance that covers the collateral but is below the deposit alone.
		mint_collateral(DOT, NEWCOMER, VAULT_DEPOSIT - 1);

		assert_noop!(
			open(NEWCOMER, DOT, STABLE, 100, 200, default_rate()),
			TokenError::FundsUnavailable
		);

		// Collateral, deposit, and the existential deposit a hold must leave free.
		let exact = 1_000 + VAULT_DEPOSIT + 1;
		mint_collateral(DOT, NEWCOMER, exact - VAULT_DEPOSIT);

		assert_noop!(
			open(NEWCOMER, DOT, STABLE, 1_000, 500, default_rate()),
			TokenError::FundsUnavailable
		);

		mint_collateral(DOT, NEWCOMER, 1);

		assert_ok!(open(NEWCOMER, DOT, STABLE, 1_000, 500, default_rate()));

		assert_eq!(vault_deposit_held(DOT, NEWCOMER), VAULT_DEPOSIT);
	});
}

#[test]
fn markets_on_one_collateral_aggregate_the_hold() {
	ExtBuilder::default()
		.branch(DOT, STABLE)
		.branch(DOT, EUSD)
		.build_and_execute(|| {
			assert_ok!(open(OWNER, DOT, STABLE, 1_000, 500, default_rate()));
			assert_ok!(open(OWNER, DOT, EUSD, 1_000, 500, default_rate()));

			assert_eq!(vault_deposit_held(DOT, OWNER), 2 * VAULT_DEPOSIT);

			repay_all(OWNER, DOT, EUSD);
			assert_ok!(close_vault(OWNER, DOT, EUSD, None));

			assert_eq!(vault_deposit_held(DOT, OWNER), VAULT_DEPOSIT);
		});
}
