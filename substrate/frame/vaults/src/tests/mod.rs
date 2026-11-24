//! `pallet-vaults` test suite.

mod basic_ops;
mod borrower_operations;
mod critical_threshold;
mod debt_in_front;
mod dormant_nomination;
mod final_recovery;
mod governance;
mod hint_helpers;
mod interest_rate;
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

use crate::{
	mock::{
		default_branch_config, AccountId, AssetId, Balance, ExtBuilder, FixedU128, Moment,
		RuntimeEvent, StableId, System, Test, DOT, STABLE,
	},
	BranchConfig, LiquidationConfig,
};

pub const ONE_DAY_MS: Moment = 24 * 3_600 * 1_000;
pub const ONE_YEAR_MS: Moment = stablecoin_primitives::MILLIS_PER_YEAR;

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

/// A `(DOT, STABLE)` branch with default settings but no keeper compensation, so a liquidation
/// the empty mocked pool cannot absorb redistributes the vault's whole debt and everything it
/// seizes.
pub fn keeperless_branch() -> ExtBuilder {
	let config = default_branch_config();
	let liquidation = LiquidationConfig { keeper_compensation_cap_value: 0, ..config.liquidation };
	ExtBuilder::default().branch_with(DOT, STABLE, BranchConfig { liquidation, ..config })
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

/// Asserts success and checks accounting before any subsequent operation can repair it.
#[track_caller]
pub fn assert_ok_and_invariants<R: core::fmt::Debug>(
	result: Result<R, frame::prelude::DispatchError>,
) {
	frame::testing_prelude::assert_ok!(result);
	crate::mock::check_invariants();
}
