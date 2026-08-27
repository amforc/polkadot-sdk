//! Tests for the refundable storage deposit charged on `open_vault`.
//!
//! The mock settles in the collateral when it is native or sufficient and in native otherwise,
//! and re-prices sufficient collateral through the oracle or a mock fallback quote.

use crate::{
	mock::*,
	pallet::Vaults as VaultRows,
	tests::rate_pct,
	types::{Vault, VaultListId},
};
use frame::{
	deps::codec::{Encode, MaxEncodedLen},
	prelude::TokenError,
	traits::Footprint,
};
use linked_list_interface::SortedListInterface;

const OWNER: AccountId = 1;

fn default_rate() -> FixedU128 {
	rate_pct(5, 100)
}

fn repay_all(owner: AccountId, collateral: AssetId, stable: StableId) {
	// Headroom for the upfront fee that the debt includes beyond the minted principal.
	mint_stable(stable, owner, 100);
	assert_ok!(Vaults::repay_for(
		RuntimeOrigin::signed(owner),
		collateral,
		stable,
		owner,
		Some(10_000)
	));
}

#[test]
fn footprint_includes_vault_record_and_rate_list_node() {
	build_and_execute(|| {
		let footprint = Vaults::vault_footprint(&DOT, &PUSD, &OWNER);
		assert_eq!(footprint.asset, DOT);
		let vault_key = VaultRows::<Test>::hashed_key_for((&DOT, &PUSD, &OWNER)).len();
		let ticket = DOT.encoded_size() + Balance::max_encoded_len();
		let node = <LinkedList as SortedListInterface<_, _>>::node_footprint(
			&VaultListId::Rate(DOT, PUSD),
			&OWNER,
		);
		let expected =
			vault_key + Vault::<Balance>::max_encoded_len() + ticket + node.size as usize;
		assert_eq!(footprint.footprint, Footprint::from_parts(1, expected));
	});
}

#[test]
fn native_collateral_pays_the_deposit_in_native() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		let free_before = collateral_balance(DOT, OWNER);

		assert_ok!(open(OWNER, DOT, PUSD, 1_000, 500, default_rate()));

		assert_eq!(vault_deposit_held(DOT, OWNER), VAULT_DEPOSIT);
		// The collateral and its storage deposit use separate holds.
		assert_eq!(held(DOT, OWNER), 1_000);
		assert_eq!(collateral_balance(DOT, OWNER), free_before - 1_000 - VAULT_DEPOSIT);
	});
}

#[test]
fn sufficient_collateral_pays_the_deposit_in_that_asset_at_the_oracle_rate() {
	build_and_execute(|| {
		// 250 native at 10 each are worth 2_500; at 40 per ETH that is 62.5, rounded up.
		register_market_with(ETH, PUSD, FixedU128::from_rational(40, 1), default_branch_config());
		set_price(DOT, FixedU128::from_rational(10, 1));
		let free_before = collateral_balance(ETH, OWNER);

		assert_ok!(open(OWNER, ETH, PUSD, 1_000, 500, default_rate()));

		assert_eq!(vault_deposit_held(ETH, OWNER), 63);
		assert_eq!(vault_deposit_held(DOT, OWNER), 0);
		assert_eq!(collateral_balance(ETH, OWNER), free_before - 1_000 - 63);
	});
}

#[test]
fn unpriceable_sufficient_collateral_rejects_the_open_atomically() {
	build_and_execute(|| {
		register_market(TOKEN_X, PUSD);
		MockFallbackRate::set(None);

		assert_noop!(
			open(OWNER, TOKEN_X, PUSD, 1_000, 500, default_rate()),
			DispatchError::Unavailable
		);
	});
}

#[test]
fn insufficient_collateral_falls_back_to_native() {
	build_and_execute(|| {
		register_market(INSUFFICIENT, PUSD);
		let native_before = collateral_balance(DOT, ADMIN);

		assert_ok!(open(ADMIN, INSUFFICIENT, PUSD, 1_000, 500, default_rate()));

		assert_eq!(vault_deposit_held(DOT, ADMIN), VAULT_DEPOSIT);
		assert_eq!(vault_deposit_held(INSUFFICIENT, ADMIN), 0);
		assert_eq!(held(INSUFFICIENT, ADMIN), 1_000);
		assert_eq!(collateral_balance(DOT, ADMIN), native_before - VAULT_DEPOSIT);
	});
}

#[test]
fn close_refunds_the_original_ticket_to_the_owner_after_a_price_change() {
	build_and_execute(|| {
		register_market_with(ETH, PUSD, FixedU128::from_rational(40, 1), default_branch_config());
		set_price(DOT, FixedU128::from_rational(10, 1));
		assert_ok!(open(OWNER, ETH, PUSD, 1_000, 500, default_rate()));
		assert_eq!(vault_deposit_held(ETH, OWNER), 63);
		repay_all(OWNER, ETH, PUSD);

		// The current quote is now lower, but the stored ticket still refunds the original 63.
		set_price(ETH, FixedU128::from_rational(80, 1));
		let owner_before = collateral_balance(ETH, OWNER);
		let recipient_before = collateral_balance(ETH, 2);

		assert_ok!(Vaults::close_vault(RuntimeOrigin::signed(OWNER), ETH, PUSD, Some(2)));

		assert_eq!(collateral_balance(ETH, 2), recipient_before + 1_000);
		assert_eq!(collateral_balance(ETH, OWNER), owner_before + 63);
		assert_eq!(vault_deposit_held(ETH, OWNER), 0);
	});
}

#[test]
fn owner_short_of_the_deposit_cannot_open() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		const NEWCOMER: AccountId = 11;
		// Collateral, deposit, and the existential deposit a hold must leave free.
		let exact = 1_000 + VAULT_DEPOSIT + 1;
		mint_collateral(DOT, NEWCOMER, exact - 1);

		assert_noop!(
			open(NEWCOMER, DOT, PUSD, 1_000, 500, default_rate()),
			TokenError::FundsUnavailable
		);

		mint_collateral(DOT, NEWCOMER, 1);
		assert_ok!(open(NEWCOMER, DOT, PUSD, 1_000, 500, default_rate()));
		assert_eq!(vault_deposit_held(DOT, NEWCOMER), VAULT_DEPOSIT);
	});
}

#[test]
fn markets_on_one_collateral_aggregate_the_hold() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(DOT, EUSD);
		assert_ok!(open(OWNER, DOT, PUSD, 1_000, 500, default_rate()));
		assert_ok!(open(OWNER, DOT, EUSD, 1_000, 500, default_rate()));
		assert_eq!(vault_deposit_held(DOT, OWNER), 2 * VAULT_DEPOSIT);

		repay_all(OWNER, DOT, EUSD);
		assert_ok!(Vaults::close_vault(RuntimeOrigin::signed(OWNER), DOT, EUSD, None));

		assert_eq!(vault_deposit_held(DOT, OWNER), VAULT_DEPOSIT);
	});
}
