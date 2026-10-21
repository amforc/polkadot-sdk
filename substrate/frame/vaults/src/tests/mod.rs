//! `pallet-vaults` test suite.

mod basic_ops;
#[cfg(feature = "runtime-benchmarks")]
mod benchmark_seeding;
mod borrower_operations;
mod critical_threshold;
mod debt_in_front;
mod dormant_nomination;
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

/// `n` percent, exactly.
pub const fn percent(n: u128) -> FixedU128 {
	FixedU128::from_inner(n * (<FixedU128 as frame::arithmetic::FixedPointNumber>::DIV / 100))
}

pub fn vault_status(
	collateral: AssetId,
	stable: StableId,
	owner: AccountId,
) -> crate::types::VaultStatus {
	assert!(crate::Vaults::<Test>::contains_key((&collateral, &stable, &owner)), "vault exists");
	crate::Pallet::<Test>::vault_status_of(&collateral, &stable, &owner)
}

/// Asserts that the pallet emitted `event` in the current block.
#[track_caller]
pub fn assert_event(event: crate::Event<Test>) {
	System::assert_has_event(RuntimeEvent::Vaults(event));
}

/// Returns every pallet event emitted in the current block, in order.
pub fn vault_events() -> Vec<crate::Event<Test>> {
	System::read_events_for_pallet::<crate::Event<Test>>()
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

/// Checks accounting at the current checkpoint, including all stablecoin aggregates and supply.
#[track_caller]
pub fn assert_invariants() {
	crate::mock::check_invariants();
}

/// Asserts success and checks accounting before any subsequent operation can repair it.
#[track_caller]
pub fn assert_ok_and_invariants<R: core::fmt::Debug>(
	result: Result<R, frame::prelude::DispatchError>,
) {
	frame::testing_prelude::assert_ok!(result);
	assert_invariants();
}
