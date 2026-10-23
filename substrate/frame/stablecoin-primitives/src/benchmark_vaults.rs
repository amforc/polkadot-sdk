//! Shared vault-engine fixtures for consumer benchmarks without a vault `Config` bound.
//!
//! Collateral units equal the asset's minimum balance; prices are per unit and stablecoin
//! amounts are raw. This keeps scenarios equivalent across runtime asset precisions.
//!
//! Register the branch before seeding. Setup order matters:
//!
//! - Do not run vault operations after `seed_redemption_targets`: they would absorb the pending
//!   redistribution shares it stages.
//! - The `stage_recovery_head` head must be the only stake-bearing vault. Use `elapsed` to accrue
//!   time; advancing time afterwards would invalidate its exact-par price.

use crate::Millis;
use alloc::vec::Vec;
use frame::deps::sp_runtime::{DispatchError, DispatchResult, FixedU128};

/// Vault parameters: collateral in minimum-balance units, debt in raw stablecoin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BenchmarkVault<Balance> {
	/// Collateral, in units of the collateral asset's minimum balance.
	pub collateral_units: u128,
	/// Debt, in stablecoin planck.
	pub debt: Balance,
	/// Annual rate, which positions the vault in the redemption order.
	pub annual_rate: FixedU128,
}

/// Redemption targets as seeded, in the order the redemption walk visits them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RedemptionTargetsSeed<AccountId, Balance> {
	/// Target owners, first redeemed first.
	pub owners: Vec<AccountId>,
	/// Fully accrued debt across all targets at return: a budget that redeems every one of them.
	pub total_debt: Balance,
}

/// A staged `FinalRecovery` head.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RecoveryHeadSeed<AccountId, Balance> {
	/// Owner of the staged vault.
	pub owner: AccountId,
	/// Fully accrued debt, in stablecoin planck: a settlement of this much clears it.
	pub debt: Balance,
	/// Collateral held by the vault, in collateral planck.
	pub collateral: Balance,
	/// Oracle price the head was staged at, per collateral planck.
	pub price: FixedU128,
}

/// Seeds real vault-engine state for benchmarks; see the [module docs](self) for the order.
///
/// Every method acts on the one branch [`Self::register_market`] creates, and fails rather than
/// seeding a state other than the one documented.
pub trait BenchmarkVaults<CollateralId, StableId, AccountId, Balance> {
	/// Idempotently registers and funds the branch, with a non-binding global debt ceiling.
	/// Returns `(collateral_id, stable_id)`.
	fn register_market() -> Result<(CollateralId, StableId), DispatchError>;

	/// Returns `units` collateral units in planck.
	fn collateral_units(units: u128) -> Balance;

	/// Quotes `stable_per_unit` stablecoin per collateral unit.
	fn set_unit_price(stable_per_unit: u128);

	/// Moves the clock forward and re-quotes the current price, so a staleness bound accepts it.
	fn advance_time(ms: Millis);

	/// Funds `owner` and opens `vault` for it at its rate-list position.
	fn open_vault(owner: &AccountId, vault: BenchmarkVault<Balance>) -> DispatchResult;

	/// Opens `count` active vaults ordered for the redemption walk and leaves each a pending
	/// redistribution share. Advances by `elapsed`, pricing the branch last.
	fn seed_redemption_targets(
		count: u32,
		elapsed: Millis,
	) -> Result<RedemptionTargetsSeed<AccountId, Balance>, DispatchError>;

	/// Accrues a `FinalRecovery` head for `elapsed`, then prices it at exact par so full
	/// settlement closes the vault. Requires no other stake-bearing vault.
	fn stage_recovery_head(
		elapsed: Millis,
	) -> Result<RecoveryHeadSeed<AccountId, Balance>, DispatchError>;
}
