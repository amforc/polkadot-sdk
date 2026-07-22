//! Types stored or exposed by the Vaults pallet.

use crate::{math, Millis};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::arithmetic::{
	CheckedAdd, CheckedMul, FixedPointNumber, FixedPointOperand, FixedU128, Permill, Saturating,
	Zero,
};
pub use pusd_primitives::{BranchMode, StableListId as VaultListId, VaultStatus};
use scale_info::TypeInfo;

/// Reason a market is frozen.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrozenReason {
	/// The oracle has no valid price.
	OracleFailure,
	/// An authorized origin froze the market.
	Governance,
}

/// Stored state for a frozen market.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrozenState {
	/// Why the market is frozen.
	pub reason: FrozenReason,
	/// Time when the freeze began.
	pub entered_at: Millis,
}

/// Role of an asset in registered markets.
///
/// An asset cannot be both collateral and stable.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub enum AssetRole {
	/// The asset backs stable debt.
	Collateral,
	/// The asset is minted when debt is created.
	Stable,
}

/// Role and market count for an asset.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, PartialEq, Eq, Debug)]
pub struct AssetRoleUsage {
	/// Role shared by all market references.
	pub role: AssetRole,
	/// Number of markets using the asset in this role.
	pub markets: u32,
}

/// Global debt state for one collateral asset.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, PartialEq, Eq, Default)]
pub struct CollateralRisk<Balance> {
	/// Maximum debt across all markets using this collateral.
	///
	/// The value is measured in collateral units. Zero blocks new debt.
	pub debt_ceiling: Balance,
	/// Current debt across all markets using this collateral.
	///
	/// This value is derived from the market records. Stable assets are counted at the same unit
	/// value.
	pub outstanding: Balance,
}

impl<Balance: Zero> CollateralRisk<Balance> {
	/// Whether this record carries neither a governance ceiling nor outstanding debt.
	pub fn is_empty(&self) -> bool {
		self.debt_ceiling.is_zero() && self.outstanding.is_zero()
	}
}

/// Principal and interest stored for a vault.
///
/// [`Self::cancel`] returns the cancelled amount in the same form.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct VaultDebt<Balance> {
	/// Borrowed stable assets that have not been repaid.
	pub principal: Balance,
	/// Interest and fees that have not been repaid.
	pub interest: Balance,
}

impl<Balance: Ord + Saturating + Copy> VaultDebt<Balance> {
	/// Returns principal plus interest.
	pub fn total(&self) -> Balance {
		self.principal.saturating_add(self.interest)
	}

	/// Removes up to `amount`, paying interest before principal.
	///
	/// Returns the amount removed from each field.
	pub fn cancel(&mut self, amount: Balance) -> Self {
		let interest = core::cmp::min(amount, self.interest);
		self.interest = self.interest.saturating_sub(interest);
		let remaining = amount.saturating_sub(interest);
		let principal = core::cmp::min(remaining, self.principal);
		self.principal = self.principal.saturating_sub(principal);
		Self { principal, interest }
	}
}

/// Snapshot of the market redistribution totals.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RedistributionSnapshot {
	/// Cumulative collateral assigned per unit of stake.
	pub collateral_per_stake: FixedU128,
	/// Cumulative debt assigned per unit of stake.
	pub debt_per_stake: FixedU128,
	/// Cumulative market time multiplied by debt per unit of stake.
	pub debt_time_per_stake: FixedU128,
	/// Cumulative rate-weighted debt assigned per unit of stake.
	pub weight_per_stake: FixedU128,
}

/// State of one vault.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct Vault<Balance> {
	/// Collateral assigned to this market.
	///
	/// The owner's on-chain hold may also back vaults in other stable-asset markets.
	pub collateral: Balance,
	/// Current principal and interest.
	pub debt: VaultDebt<Balance>,
	/// Annual interest rate chosen by the owner.
	pub annual_rate: FixedU128,
	/// Market interest time of the last vault update.
	pub last_interest_time: Millis,
	/// Wall-clock time of the last rate change.
	pub last_rate_update: Millis,
	/// Collateral used as redistribution stake.
	///
	/// This is zero during final recovery and otherwise equals [`Self::collateral`].
	pub redistribution_stake: Balance,
	/// Redistribution totals applied by the last vault update.
	pub redistribution_snapshot: RedistributionSnapshot,
}

impl<Balance> Vault<Balance> {
	/// Returns whether the rate-change cooldown has passed.
	pub(crate) fn cooldown_elapsed(&self, config: &BranchConfig<Balance>, now: Millis) -> bool {
		now.saturating_sub(self.last_rate_update) >= config.rate_adjustment_cooldown
	}
}

/// Risk parameters for one market.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct BranchConfig<Balance> {
	/// Ratio below which a vault may be liquidated or moved into final recovery.
	pub minimum_collateralization_ratio: FixedU128,
	/// Minimum vault ratio after borrowing or withdrawing collateral.
	pub initial_collateralization_ratio: FixedU128,
	/// Market ratio below which safety mode begins.
	pub safety_collateralization_ratio: FixedU128,
	/// Maximum market debt and upper bound for the automatic debt limit.
	pub debt_ceiling: Balance,
	/// Minimum debt for an active vault.
	pub minimum_debt: Balance,
	/// Minimum collateral required to open a vault.
	pub minimum_collateral: Balance,
	/// Lowest annual rate allowed for a vault.
	pub minimum_borrow_rate: FixedU128,
	/// Highest annual rate allowed for a vault.
	pub maximum_borrow_rate: FixedU128,
	/// Time period used to calculate upfront fees.
	pub upfront_fee_period: Millis,
	/// Minimum time between rate changes that do not charge an upfront fee.
	pub rate_adjustment_cooldown: Millis,
	/// Collateral penalty applied during liquidation.
	pub redistribution_penalty: Permill,
	/// Headroom above current debt for the automatic debt limit.
	///
	/// Zero disables automatic updates.
	pub ceiling_gap: Balance,
	/// Minimum time between automatic debt-limit increases.
	pub ceiling_ttl: Millis,
}

/// Debt totals for one market.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug, Default)]
pub struct BranchDebt<Balance> {
	/// Principal stored across all vaults.
	pub principal: Balance,
	/// Interest and fees minted by the market and not yet repaid.
	pub minted_interest: Balance,
	/// Liquidated principal waiting to be applied to vaults.
	pub pending_redistribution_principal: Balance,
	/// Debt that is not backed by a vault.
	pub bad_debt: Balance,
	/// Rate-weighted principal used for aggregate interest.
	///
	/// This includes pending redistributed debt at the market's average rate.
	pub weighted_principal_sum: Balance,
	/// Market interest time of the last aggregate interest update.
	pub last_interest_time: Millis,
}

impl<Balance: FixedPointOperand + Saturating> BranchDebt<Balance> {
	/// Returns all debt owed by the market.
	pub fn outstanding(&self) -> Balance {
		self.principal
			.saturating_add(self.minted_interest)
			.saturating_add(self.pending_redistribution_principal)
			.saturating_add(self.bad_debt)
	}
}

/// Redistribution stake totals for one market.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug, Default)]
pub struct BranchStakes<Balance> {
	/// Total stake of eligible vaults.
	pub total: Balance,
	/// Sum of each vault's annual rate multiplied by its stake.
	pub weighted_sum: Balance,
}

/// Accounting state for one market.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct BranchState<AccountId, Balance> {
	/// Total collateral held by vault owners or waiting for redistribution.
	pub total_collateral: Balance,
	/// Market debt totals.
	pub debt: BranchDebt<Balance>,
	/// Market redistribution stake totals.
	pub stakes: BranchStakes<Balance>,
	/// Redistribution debt lost to per-stake rounding.
	///
	/// This becomes bad debt when no vault liability remains.
	pub ownerless_debt: Balance,
	/// Redistribution collateral lost to per-stake rounding.
	pub ownerless_collateral: Balance,
	/// Current redistribution totals.
	pub redistribution: RedistributionSnapshot,
	/// Wall-clock origin used to calculate market interest time.
	///
	/// Frozen periods move this value forward so interest does not accrue while frozen.
	pub interest_epoch: Millis,
	/// Dormant vault that must be redeemed before the rate list.
	pub dormant_redemption_target: Option<AccountId>,
	/// Frozen state, if the market is frozen.
	pub frozen: Option<FrozenState>,
	/// Current automatic debt limit.
	///
	/// This value cannot exceed [`BranchConfig::debt_ceiling`].
	pub effective_ceiling: Balance,
	/// Time when the automatic debt limit last increased.
	pub ceiling_last_inc: Millis,
}

impl<AccountId, Balance: Default + Zero + Ord + Copy> BranchState<AccountId, Balance> {
	/// Creates empty state for a new market.
	///
	/// The automatic debt limit starts at its gap or at the market debt limit, whichever is lower.
	pub fn fresh(config: &BranchConfig<Balance>, now: Millis) -> Self {
		let effective_ceiling = if config.ceiling_gap.is_zero() {
			config.debt_ceiling
		} else {
			config.ceiling_gap.min(config.debt_ceiling)
		};
		Self {
			total_collateral: Balance::zero(),
			debt: BranchDebt::default(),
			stakes: BranchStakes::default(),
			ownerless_debt: Balance::zero(),
			ownerless_collateral: Balance::zero(),
			redistribution: RedistributionSnapshot::default(),
			interest_epoch: now,
			dormant_redemption_target: None,
			frozen: None,
			effective_ceiling,
			ceiling_last_inc: now,
		}
	}
}

impl<AccountId, Balance> BranchState<AccountId, Balance> {
	/// Returns whether the market is frozen.
	pub fn is_frozen(&self) -> bool {
		self.frozen.is_some()
	}

	/// Returns market interest time at `now`.
	///
	/// Time before market creation and time spent frozen are excluded.
	pub fn interest_time(&self, now: Millis) -> Millis {
		let current_frozen =
			self.frozen.as_ref().map_or(0, |state| now.saturating_sub(state.entered_at));
		now.saturating_sub(self.interest_epoch).saturating_sub(current_frozen)
	}
}

impl<AccountId: PartialEq, Balance> BranchState<AccountId, Balance> {
	/// Clears the dormant redemption target if it matches `owner`.
	pub fn release_dormant_target(&mut self, owner: &AccountId) {
		if self.dormant_redemption_target.as_ref() == Some(owner) {
			self.dormant_redemption_target = None;
		}
	}

	/// Sets `owner` as the dormant redemption target.
	///
	/// Returns `false` if another owner already holds the slot.
	pub fn try_park_dormant_target(&mut self, owner: AccountId) -> bool {
		match &self.dormant_redemption_target {
			Some(existing) if existing != &owner => false,
			_ => {
				self.dormant_redemption_target = Some(owner);
				true
			},
		}
	}
}

impl<AccountId, Balance: FixedPointOperand + Saturating> BranchState<AccountId, Balance> {
	/// Adds a vault to the market totals.
	pub fn attach_vault(&mut self, vault: &Vault<Balance>) {
		let rate_x_debt = vault.annual_rate.saturating_mul_int(vault.debt.principal);
		let rate_x_stake = vault.annual_rate.saturating_mul_int(vault.redistribution_stake);
		self.debt.principal = self.debt.principal.saturating_add(vault.debt.principal);
		self.debt.minted_interest = self.debt.minted_interest.saturating_add(vault.debt.interest);
		self.debt.weighted_principal_sum =
			self.debt.weighted_principal_sum.saturating_add(rate_x_debt);
		self.stakes.weighted_sum = self.stakes.weighted_sum.saturating_add(rate_x_stake);
		self.stakes.total = self.stakes.total.saturating_add(vault.redistribution_stake);
	}

	/// Removes a vault from the market totals.
	///
	/// This is the exact inverse of [`Self::attach_vault`].
	pub fn detach_vault(&mut self, vault: &Vault<Balance>) {
		let rate_x_debt = vault.annual_rate.saturating_mul_int(vault.debt.principal);
		let rate_x_stake = vault.annual_rate.saturating_mul_int(vault.redistribution_stake);
		self.debt.principal = self.debt.principal.saturating_sub(vault.debt.principal);
		self.debt.minted_interest = self.debt.minted_interest.saturating_sub(vault.debt.interest);
		self.debt.weighted_principal_sum =
			self.debt.weighted_principal_sum.saturating_sub(rate_x_debt);
		self.stakes.weighted_sum = self.stakes.weighted_sum.saturating_sub(rate_x_stake);
		self.stakes.total = self.stakes.total.saturating_sub(vault.redistribution_stake);
	}

	/// Adds collateral to the market total.
	pub fn add_collateral(&mut self, amount: Balance) {
		self.total_collateral = self.total_collateral.saturating_add(amount);
	}

	/// Removes collateral from the market total.
	pub fn remove_collateral(&mut self, amount: Balance) {
		self.total_collateral = self.total_collateral.saturating_sub(amount);
	}

	/// Applies a vault payment to the market totals.
	///
	/// `principal_after` is the vault principal after [`VaultDebt::cancel`].
	pub fn apply_debt_payment(
		&mut self,
		payment: VaultDebt<Balance>,
		rate: FixedU128,
		principal_after: Balance,
	) {
		self.debt.principal = self.debt.principal.saturating_sub(payment.principal);
		self.debt.minted_interest = self.debt.minted_interest.saturating_sub(payment.interest);
		let principal_before = principal_after.saturating_add(payment.principal);
		self.debt.weighted_principal_sum = self
			.debt
			.weighted_principal_sum
			.saturating_sub(rate.saturating_mul_int(principal_before))
			.saturating_add(rate.saturating_mul_int(principal_after));
	}

	/// Replaces a vault's rate-weighted debt and stake.
	pub fn change_vault_rate(
		&mut self,
		old_rate: FixedU128,
		new_rate: FixedU128,
		principal: Balance,
		stake: Balance,
	) {
		self.debt.weighted_principal_sum = self
			.debt
			.weighted_principal_sum
			.saturating_sub(old_rate.saturating_mul_int(principal))
			.saturating_add(new_rate.saturating_mul_int(principal));
		self.stakes.weighted_sum = self
			.stakes
			.weighted_sum
			.saturating_sub(old_rate.saturating_mul_int(stake))
			.saturating_add(new_rate.saturating_mul_int(stake));
	}

	/// Sets a vault's redistribution stake and updates the market totals.
	///
	/// The vault rate is not changed.
	pub fn set_vault_stake(&mut self, vault: &mut Vault<Balance>, new_stake: Balance) {
		let old_stake = vault.redistribution_stake;
		self.stakes.total = self.stakes.total.saturating_sub(old_stake).saturating_add(new_stake);
		self.stakes.weighted_sum = self
			.stakes
			.weighted_sum
			.saturating_sub(vault.annual_rate.saturating_mul_int(old_stake))
			.saturating_add(vault.annual_rate.saturating_mul_int(new_stake));
		vault.redistribution_stake = new_stake;
	}

	/// Moves redistributed principal into a vault.
	///
	/// Redistribution first uses the market's average rate. This method replaces that weighting
	/// with the receiving vault's rate.
	pub fn absorb_redistributed_debt(&mut self, vault: &mut Vault<Balance>, principal: Balance) {
		self.debt.pending_redistribution_principal =
			self.debt.pending_redistribution_principal.saturating_sub(principal);
		self.debt.principal = self.debt.principal.saturating_add(principal);
		let delta_weight_per_stake = self
			.redistribution
			.weight_per_stake
			.saturating_sub(vault.redistribution_snapshot.weight_per_stake);
		let weight_to_remove =
			delta_weight_per_stake.saturating_mul_int(vault.redistribution_stake);
		let principal_before = vault.debt.principal;
		vault.debt.principal = vault.debt.principal.saturating_add(principal);
		self.debt.weighted_principal_sum = self
			.debt
			.weighted_principal_sum
			.saturating_sub(weight_to_remove)
			.saturating_sub(vault.annual_rate.saturating_mul_int(principal_before))
			.saturating_add(vault.annual_rate.saturating_mul_int(vault.debt.principal));
	}

	/// Returns whether no vault liability remains.
	///
	/// Other debt or collateral may still remain. Use [`Self::is_removable`] before removing the
	/// market.
	pub fn is_empty_of_liability(&self) -> bool {
		self.debt.principal.is_zero() &&
			self.stakes.total.is_zero() &&
			self.debt.pending_redistribution_principal.is_zero()
	}

	/// Returns whether the market has no debt, stake, or collateral.
	pub fn is_removable(&self) -> bool {
		self.debt.outstanding().is_zero() &&
			self.stakes.total.is_zero() &&
			self.total_collateral.is_zero()
	}

	/// Adds bad debt to the market.
	pub fn record_bad_debt(&mut self, amount: Balance) {
		self.debt.bad_debt = self.debt.bad_debt.saturating_add(amount);
	}

	/// Removes bad debt from the market.
	///
	/// The subtraction saturates at zero.
	pub fn heal_bad_debt(&mut self, amount: Balance) {
		self.debt.bad_debt = self.debt.bad_debt.saturating_sub(amount);
	}

	/// Moves ownerless debt and remaining interest into bad debt.
	///
	/// Returns the amount moved.
	pub fn sweep_orphan_debt(&mut self) -> Balance {
		let orphan = self.debt.minted_interest.saturating_add(self.ownerless_debt);
		self.debt.minted_interest = Balance::zero();
		self.ownerless_debt = Balance::zero();
		self.debt.bad_debt = self.debt.bad_debt.saturating_add(orphan);
		orphan
	}
}

impl<AccountId, Balance: FixedPointOperand + Ord> BranchState<AccountId, Balance> {
	/// Records debt and collateral redistributed by one liquidation.
	///
	/// Debt is first weighted at the market's average rate. Rounding remains in the ownerless
	/// fields. Returns `None` if an accumulator would overflow.
	pub fn record_redistribution(
		&mut self,
		redistributed_debt: Balance,
		redistributed_collateral: Balance,
		now: Millis,
	) -> Option<()> {
		let avg_rate = math::average_branch_rate(self.stakes.weighted_sum, self.stakes.total);
		let debt_per_stake = math::redistribution_per_stake(redistributed_debt, self.stakes.total)?;
		let collateral_per_stake =
			math::redistribution_per_stake(redistributed_collateral, self.stakes.total)?;
		let weight_per_stake =
			math::redistribution_weight_per_stake(redistributed_debt, avg_rate, self.stakes.total)?;
		// Use the same market clock as `pending_touch_for`.
		let now_fp = FixedU128::saturating_from_integer(self.interest_time(now));
		let debt_time_increment = now_fp.checked_mul(&debt_per_stake)?;

		self.redistribution = RedistributionSnapshot {
			debt_per_stake: self.redistribution.debt_per_stake.checked_add(&debt_per_stake)?,
			collateral_per_stake: self
				.redistribution
				.collateral_per_stake
				.checked_add(&collateral_per_stake)?,
			debt_time_per_stake: self
				.redistribution
				.debt_time_per_stake
				.checked_add(&debt_time_increment)?,
			weight_per_stake: self
				.redistribution
				.weight_per_stake
				.checked_add(&weight_per_stake)?,
		};

		let distributed_debt = debt_per_stake.saturating_mul_int(self.stakes.total);
		self.debt.pending_redistribution_principal =
			self.debt.pending_redistribution_principal.saturating_add(distributed_debt);
		self.debt.weighted_principal_sum = self
			.debt
			.weighted_principal_sum
			.saturating_add(avg_rate.saturating_mul_int(redistributed_debt));
		let debt_dust = redistributed_debt.saturating_sub(distributed_debt);
		self.ownerless_debt = self.ownerless_debt.saturating_add(debt_dust);
		let distributed_collateral = collateral_per_stake.saturating_mul_int(self.stakes.total);
		let collateral_dust = redistributed_collateral.saturating_sub(distributed_collateral);
		self.ownerless_collateral = self.ownerless_collateral.saturating_add(collateral_dust);
		Some(())
	}
}

/// Update to one market parameter.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub enum BranchConfigUpdate<Balance> {
	/// Sets the liquidation and final recovery ratio.
	MinimumCollateralizationRatio(FixedU128),
	/// Sets the ratio required after borrowing or withdrawing collateral.
	InitialCollateralizationRatio(FixedU128),
	/// Sets the market safety-mode ratio.
	SafetyCollateralizationRatio(FixedU128),
	/// Sets the maximum market debt.
	DebtCeiling(Balance),
	/// Sets the minimum debt for an active vault.
	MinimumDebt(Balance),
	/// Sets the minimum collateral required to open a vault.
	MinimumCollateral(Balance),
	/// Sets the allowed annual rate range.
	BorrowRateBounds {
		/// Lowest allowed rate.
		min: FixedU128,
		/// Highest allowed rate.
		max: FixedU128,
	},
	/// Sets the period used to calculate upfront fees.
	UpfrontFeePeriod(Millis),
	/// Sets the rate-change cooldown.
	RateAdjustmentCooldown(Millis),
	/// Sets the collateral penalty applied during liquidation.
	RedistributionPenalty(Permill),
	/// Sets headroom for the automatic debt limit.
	///
	/// Zero disables automatic updates.
	CeilingGap(Balance),
	/// Sets the delay between automatic debt-limit increases.
	CeilingTtl(Millis),
}

impl<Balance: PartialOrd + Copy> BranchConfigUpdate<Balance> {
	/// Applies this update to `config`.
	pub fn apply_to(self, config: &mut BranchConfig<Balance>) {
		match self {
			Self::MinimumCollateralizationRatio(v) => config.minimum_collateralization_ratio = v,
			Self::InitialCollateralizationRatio(v) => config.initial_collateralization_ratio = v,
			Self::SafetyCollateralizationRatio(v) => config.safety_collateralization_ratio = v,
			Self::DebtCeiling(v) => config.debt_ceiling = v,
			Self::MinimumDebt(v) => config.minimum_debt = v,
			Self::MinimumCollateral(v) => config.minimum_collateral = v,
			Self::BorrowRateBounds { min, max } => {
				config.minimum_borrow_rate = min;
				config.maximum_borrow_rate = max;
			},
			Self::UpfrontFeePeriod(v) => config.upfront_fee_period = v,
			Self::RateAdjustmentCooldown(v) => config.rate_adjustment_cooldown = v,
			Self::RedistributionPenalty(v) => config.redistribution_penalty = v,
			Self::CeilingGap(v) => config.ceiling_gap = v,
			Self::CeilingTtl(v) => config.ceiling_ttl = v,
		}
	}

	/// Returns the administrator role required for this update.
	pub fn required_level(&self) -> AdminLevel {
		match self {
			Self::MinimumCollateralizationRatio(_) |
			Self::InitialCollateralizationRatio(_) |
			Self::SafetyCollateralizationRatio(_) |
			Self::DebtCeiling(_) |
			Self::BorrowRateBounds { .. } => AdminLevel::Emergency,
			Self::MinimumDebt(_) |
			Self::MinimumCollateral(_) |
			Self::UpfrontFeePeriod(_) |
			Self::RateAdjustmentCooldown(_) |
			Self::RedistributionPenalty(_) |
			Self::CeilingGap(_) |
			Self::CeilingTtl(_) => AdminLevel::Full,
		}
	}

	/// Returns whether this update only reduces risk.
	///
	/// A defensive update raises ratio limits, lowers the debt limit, or narrows the rate range.
	pub fn is_defensive(&self, config: &BranchConfig<Balance>) -> bool {
		match self {
			Self::MinimumCollateralizationRatio(v) => *v >= config.minimum_collateralization_ratio,
			Self::InitialCollateralizationRatio(v) => *v >= config.initial_collateralization_ratio,
			Self::SafetyCollateralizationRatio(v) => *v >= config.safety_collateralization_ratio,
			Self::DebtCeiling(v) => *v <= config.debt_ceiling,
			Self::BorrowRateBounds { min, max } => {
				*max <= config.maximum_borrow_rate && *min >= config.minimum_borrow_rate
			},
			_ => true,
		}
	}
}

/// Global limits for market configuration.
pub struct BranchConfigGuard<Balance> {
	/// Lowest allowed liquidation and final recovery ratio.
	pub min_minimum_collateralization_ratio: FixedU128,
	/// Lowest allowed ratio after borrowing or withdrawing collateral.
	pub min_initial_collateralization_ratio: FixedU128,
	/// Lowest allowed market safety-mode ratio.
	pub min_safety_collateralization_ratio: FixedU128,
	/// Lowest allowed minimum debt.
	pub min_minimum_debt: Balance,
	/// Lowest allowed minimum collateral.
	pub min_minimum_collateral: Balance,
	/// Highest allowed annual rate.
	pub max_borrow_rate: FixedU128,
	/// Highest allowed market debt limit.
	pub max_branch_line: Balance,
	/// Highest allowed headroom for the automatic debt limit.
	pub max_ceiling_gap: Balance,
	/// Shortest allowed delay between automatic debt-limit increases.
	pub min_ceiling_ttl: Millis,
}

impl<Balance: PartialOrd + Copy + Zero> BranchConfigGuard<Balance> {
	/// Returns whether `config` is within the global limits.
	///
	/// The minimum increase delay applies only when automatic debt-limit updates are enabled.
	pub fn permits(&self, config: &BranchConfig<Balance>) -> bool {
		config.minimum_collateralization_ratio >= self.min_minimum_collateralization_ratio &&
			config.initial_collateralization_ratio >= self.min_initial_collateralization_ratio &&
			config.safety_collateralization_ratio >= self.min_safety_collateralization_ratio &&
			config.minimum_debt >= self.min_minimum_debt &&
			config.minimum_collateral >= self.min_minimum_collateral &&
			config.maximum_borrow_rate <= self.max_borrow_rate &&
			config.debt_ceiling <= self.max_branch_line &&
			config.ceiling_gap <= self.max_ceiling_gap &&
			(config.ceiling_gap.is_zero() || config.ceiling_ttl >= self.min_ceiling_ttl)
	}
}

/// Administrator role for one market.
#[derive(Clone, Copy)]
pub enum AdminLevel {
	/// May manage all market settings and lifecycle actions.
	Full,
	/// May freeze the market or reduce risk.
	Emergency,
}

/// Administrator accounts for one market.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct BranchAdmins<AccountId> {
	/// Account with full control of the market.
	pub full_admin: AccountId,
	/// Account allowed to freeze the market or reduce risk.
	pub emergency_admin: AccountId,
}

impl<AccountId> BranchAdmins<AccountId> {
	/// Maps both accounts with `f` while preserving their roles.
	pub fn try_map<Target, E>(
		self,
		f: impl Fn(AccountId) -> Result<Target, E>,
	) -> Result<BranchAdmins<Target>, E> {
		Ok(BranchAdmins {
			full_admin: f(self.full_admin)?,
			emergency_admin: f(self.emergency_admin)?,
		})
	}
}

/// Complete record for one registered market.
///
/// It is created and removed as one record.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct Branch<AccountId, Balance, Consideration> {
	/// Market risk parameters.
	pub config: BranchConfig<Balance>,
	/// Market accounting state.
	pub state: BranchState<AccountId, Balance>,
	/// Market administrator accounts.
	pub admins: BranchAdmins<AccountId>,
	/// Creator and refundable deposit, if one was charged.
	pub deposit: Option<(AccountId, Consideration)>,
}

#[cfg(test)]
mod tests {
	use super::*;

	fn make_branch_state(principal: u128, weighted: u128) -> BranchState<u64, u128> {
		BranchState {
			total_collateral: 0,
			debt: BranchDebt {
				principal,
				minted_interest: 0,
				pending_redistribution_principal: 0,
				bad_debt: 0,
				weighted_principal_sum: weighted,
				last_interest_time: 0,
			},
			stakes: BranchStakes { total: 0, weighted_sum: 0 },
			ownerless_debt: 0,
			ownerless_collateral: 0,
			redistribution: RedistributionSnapshot::default(),
			interest_epoch: 0,
			dormant_redemption_target: None,
			frozen: None,
			effective_ceiling: 0,
			ceiling_last_inc: 0,
		}
	}

	#[test]
	fn apply_debt_payment_swaps_full_contribution() {
		// The weighted contribution falls from 3 to 2. Subtracting
		// `floor(rate * payment)` would subtract zero and leave the wrong value.
		let rate = FixedU128::from_rational(3u128, 10u128);
		let mut state = make_branch_state(10, 3);
		state.apply_debt_payment(VaultDebt { interest: 0, principal: 1 }, rate, 9);
		assert_eq!(state.debt.principal, 9);
		assert_eq!(state.debt.weighted_principal_sum, 2);
	}

	#[test]
	fn apply_debt_payment_full_payoff_clears_contribution() {
		let rate = FixedU128::from_rational(3u128, 10u128);
		let mut state = make_branch_state(10, 3);
		state.apply_debt_payment(VaultDebt { interest: 0, principal: 10 }, rate, 0);
		assert_eq!(state.debt.principal, 0);
		assert_eq!(state.debt.weighted_principal_sum, 0);
	}

	#[test]
	fn absorb_redistributed_debt_swaps_avg_rate_weighting_for_own_rate() {
		// Remove 2 of average-rate weight and 5 of old vault weight. Then add 6 for
		// the new principal: 20 - 2 - 5 + 6 = 19.
		let rate = FixedU128::from_rational(5u128, 10u128);
		let mut state = make_branch_state(30, 20);
		state.debt.pending_redistribution_principal = 6;
		state.redistribution.weight_per_stake = FixedU128::from_rational(3u128, 10u128);
		let mut vault = Vault {
			collateral: 10,
			debt: VaultDebt { principal: 10, interest: 0 },
			annual_rate: rate,
			last_interest_time: 0,
			last_rate_update: 0,
			redistribution_stake: 10,
			redistribution_snapshot: RedistributionSnapshot {
				weight_per_stake: FixedU128::from_rational(1u128, 10u128),
				..RedistributionSnapshot::default()
			},
		};
		state.absorb_redistributed_debt(&mut vault, 3);
		assert_eq!(vault.debt.principal, 13);
		assert_eq!(state.debt.principal, 33);
		assert_eq!(state.debt.pending_redistribution_principal, 3);
		assert_eq!(state.debt.weighted_principal_sum, 19);
	}
}
