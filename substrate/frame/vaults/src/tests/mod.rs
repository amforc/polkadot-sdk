//! `pallet-vaults` test suite.

mod basic_ops;
mod borrower_operations;
mod critical_threshold;
mod debt_in_front;
mod dormant_nomination;
mod events;
mod final_recovery;
mod governance;
mod hint_helpers;
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

use crate::mock::{
	AccountId, AssetId, Balance, FixedU128, Moment, RuntimeEvent, StableId, System, Test,
};

pub const ONE_DAY_MS: Moment = 24 * 3_600 * 1_000;
pub const ONE_YEAR_MS: Moment = pusd_primitives::MILLIS_PER_YEAR;

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

/// Asserts that the pallet emitted `event` in the current block.
pub fn assert_event(event: crate::Event<Test>) {
	System::assert_has_event(RuntimeEvent::Vaults(event));
}

/// Returns every pallet event emitted in the current block, in order.
pub fn vault_events() -> Vec<crate::Event<Test>> {
	System::events()
		.into_iter()
		.filter_map(|record| match record.event {
			RuntimeEvent::Vaults(event) => Some(event),
			_ => None,
		})
		.collect()
}

/// Returns the outcome carried by the first `VaultLiquidated` event of the block.
pub fn liquidation_outcome() -> crate::types::LiquidationOutcome<Balance> {
	vault_events()
		.into_iter()
		.find_map(|event| match event {
			crate::Event::VaultLiquidated { outcome, .. } => Some(outcome),
			_ => None,
		})
		.expect("liquidation event")
}
