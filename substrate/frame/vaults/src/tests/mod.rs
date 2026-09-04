//! `pallet-vaults` test suite.

mod basic_ops;
mod borrower_operations;
mod critical_threshold;
mod debt_in_front;
mod events;
mod final_recovery;
mod governance;
mod hint_helpers;
mod idle_walk;
mod interest_rate;
mod last_vault;
mod lifecycle;
mod liquidation;
mod multi_market;
mod rate_index;
mod realistic_scale;
mod redemptions;
mod redistribution_accounting;
mod risk_controls;
mod stablecoin_markets;
mod vault_deposit;

use crate::mock::{AccountId, AssetId, FixedU128, StableId, Test};

pub fn rate_pct(num: u128, denom: u128) -> FixedU128 {
	FixedU128::from_rational(num, denom)
}

pub fn vault_status(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
) -> crate::types::VaultStatus {
	crate::Pallet::<Test>::vault_status(collateral, stable, owner).expect("vault status")
}
