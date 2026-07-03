//! Storage and value types for `pallet-vaults`.

use crate::{math, Millis};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::arithmetic::{
	CheckedAdd, CheckedMul, FixedPointNumber, FixedPointOperand, FixedU128, Permill, Saturating,
	Zero,
};
use scale_info::TypeInfo;

pub use pusd_primitives::VaultStatus;

/// Branch operating mode. `Normal` and `Safety` are derived from live TCR;
/// `Frozen` is the only persisted mode.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
)]
pub enum BranchMode {
	Normal,
	Safety,
	Frozen,
}

/// Reason the branch was put into `Frozen` mode.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
)]
pub enum FrozenReason {
	OracleFailure,
	Governance,
}

/// Stored `Frozen` state attached to [`BranchState`] while frozen.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
)]
pub struct FrozenState {
	pub reason: FrozenReason,
	pub entered_at: Millis,
}

/// Logical linked-list partitions owned by this pallet, one pair of lists per
/// `(collateral_id, stable_id)` market.
///
/// `Rate(collateral, stable)` is the active-vault rate index.
/// `FinalRecovery(collateral, stable)` is the per-market recovery FIFO; each
/// append derives its stored priority from the current head, one above it.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Debug,
)]
pub enum VaultListId<CollateralId, StableId> {
	Rate(CollateralId, StableId),
	FinalRecovery(CollateralId, StableId),
}

impl<CollateralId: Default, StableId: Default> Default for VaultListId<CollateralId, StableId> {
	fn default() -> Self {
		Self::Rate(CollateralId::default(), StableId::default())
	}
}

/// Debt split by bucket: the state tracked on a vault row, and equally the
/// shape of a cancelled portion of it (a payment is itself a debt breakdown).
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct VaultDebt<Balance> {
	pub principal: Balance,
	pub interest: Balance,
}

impl<Balance: Ord + Saturating + Copy> VaultDebt<Balance> {
	pub fn total(&self) -> Balance {
		self.principal.saturating_add(self.interest)
	}

	/// Cancel up to `amount`, interest first, returning the cancelled split.
	pub fn cancel(&mut self, amount: Balance) -> Self {
		let interest = core::cmp::min(amount, self.interest);
		self.interest = self.interest.saturating_sub(interest);
		let remaining = amount.saturating_sub(interest);
		let principal = core::cmp::min(remaining, self.principal);
		self.principal = self.principal.saturating_sub(principal);
		Self { interest, principal }
	}
}

/// Snapshot of branch redistribution accumulators stamped at vault open and
/// re-stamped whenever pending redistribution is applied.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
	Default,
)]
pub struct RedistributionSnapshot {
	pub collateral_per_stake: FixedU128,
	pub debt_per_stake: FixedU128,
	pub debt_time_per_stake: FixedU128,
	pub weight_per_stake: FixedU128,
}

/// Per-vault state.
///
/// `collateral` is this market's collateral for the owner: the share of the
/// owner's `VaultCollateral` hold attributable to this `(collateral, stable)`
/// market. The owner's hold is shared across every stablecoin they back with
/// the same collateral asset, so the hold alone cannot represent one market's
/// collateral — the row carries it. It tracks the collateral in every
/// lifecycle state, `FinalRecovery` included.
///
/// `redistribution_stake` mirrors the vault's *current* eligible collateral:
/// it equals `collateral` while the vault is `Active` or `Dormant` and is zero
/// while the vault is in `FinalRecovery` (where `collateral` itself persists).
/// It is refreshed after every op that changes collateral or eligibility,
/// always after pending redistribution has been applied, so
/// `BranchStakes.total == Σ vault.redistribution_stake` over the live eligible
/// set.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct Vault<Balance> {
	pub collateral: Balance,
	pub debt: VaultDebt<Balance>,
	pub annual_rate: FixedU128,
	pub last_interest_time: Millis,
	pub last_rate_update: Millis,
	pub redistribution_stake: Balance,
	pub redistribution_snapshot: RedistributionSnapshot,
}

impl<Balance> Vault<Balance> {
	/// Whether the rate-adjustment cooldown has elapsed. A rate change is free of
	/// the upfront fee once `rate_adjustment_cooldown` has passed since the last
	/// one.
	pub(crate) fn cooldown_elapsed(&self, config: &BranchConfig<Balance>, now: Millis) -> bool {
		now.saturating_sub(self.last_rate_update) >= config.rate_adjustment_cooldown
	}

	/// Existing principal the rate-change part of the borrow upfront fee is
	/// charged against: the current principal when `borrow` also moves the rate
	/// within the cooldown window, zero otherwise (a pure debt increase, or the
	/// cooldown has elapsed).
	pub(crate) fn rate_change_base(
		&self,
		maybe_new_rate: Option<FixedU128>,
		cooldown_elapsed: bool,
	) -> Balance
	where
		Balance: Zero + Copy,
	{
		if maybe_new_rate.is_some_and(|rate| rate != self.annual_rate) && !cooldown_elapsed {
			self.debt.principal
		} else {
			Balance::zero()
		}
	}
}

/// Branch governance/risk parameters.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct BranchConfig<Balance> {
	pub minimum_collateralization_ratio: FixedU128,
	pub initial_collateralization_ratio: FixedU128,
	pub safety_collateralization_ratio: FixedU128,
	pub debt_ceiling: Balance,
	pub minimum_debt: Balance,
	pub minimum_collateral: Balance,
	pub minimum_borrow_rate: FixedU128,
	pub maximum_borrow_rate: FixedU128,
	pub upfront_fee_period: Millis,
	pub rate_adjustment_cooldown: Millis,
	pub redistribution_penalty: Permill,
	/// Autoline headroom above current debt. `0` disables the autoline, leaving
	/// `debt_ceiling` as the static borrow cap.
	pub ceiling_gap: Balance,
	/// Minimum time between autoline ceiling increases (slow-up gate).
	pub ceiling_ttl: Millis,
}

/// Debt and interest aggregates for one collateral branch.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	PartialEq,
	Eq,
	Debug,
	Default,
)]
pub struct BranchDebt<Balance> {
	pub principal: Balance,
	pub minted_interest: Balance,
	pub pending_redistribution_principal: Balance,
	pub bad_debt: Balance,
	pub weighted_principal_sum: Balance,
	pub last_interest_time: Millis,
}

impl<Balance: FixedPointOperand + Saturating> BranchDebt<Balance> {
	/// Total outstanding stable liability: principal, minted interest, pending
	/// redistribution principal, and socialized bad debt. The canonical measure
	/// of a branch's stable exposure, used by the global debt ceiling and the
	/// market-emptiness check.
	pub fn outstanding(&self) -> Balance {
		self.principal
			.saturating_add(self.minted_interest)
			.saturating_add(self.pending_redistribution_principal)
			.saturating_add(self.bad_debt)
	}
}

/// Current-collateral redistribution stake totals for one collateral branch.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	PartialEq,
	Eq,
	Debug,
	Default,
)]
pub struct BranchStakes<Balance> {
	pub total: Balance,
	pub weighted_sum: Balance,
}

/// Per-branch accounting state.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct BranchState<AccountId, Balance> {
	pub total_collateral: Balance,
	pub debt: BranchDebt<Balance>,
	pub stakes: BranchStakes<Balance>,
	/// Debt that exists at the branch level but cannot be attributed to any
	/// specific vault (per-stake flooring residue from a redistribution).
	/// Swept into `bad_debt` when the branch empties of liability.
	pub ownerless_debt: Balance,
	/// Collateral that sits on the redistribution account but cannot be
	/// attributed; bookkeeping only, since the physical balance is already
	/// held there.
	pub ownerless_collateral: Balance,
	pub redistribution: RedistributionSnapshot,
	/// Wall-clock origin of the branch's interest timebase, shifted forward by
	/// every completed frozen window. Interest accrues in `interest_time(now)`
	/// units rather than raw wall-clock time so that freezing a branch suspends
	/// accrual without ever rewinding the clock. See [`Self::interest_time`].
	pub interest_epoch: Millis,
	pub dormant_redemption_target: Option<AccountId>,
	pub frozen: Option<FrozenState>,
	/// Autoline current line — the self-adjusting borrow cap, maintained while
	/// `ceiling_gap > 0` and bounded above by `debt_ceiling` (the line max).
	pub effective_ceiling: Balance,
	/// When `effective_ceiling` last increased; gates the slow-up.
	pub ceiling_last_inc: Millis,
}

impl<AccountId, Balance> BranchState<AccountId, Balance> {
	pub fn is_frozen(&self) -> bool {
		self.frozen.is_some()
	}

	pub fn interest_time(&self, now: Millis) -> Millis {
		let current_frozen =
			self.frozen.as_ref().map_or(0, |state| now.saturating_sub(state.entered_at));
		now.saturating_sub(self.interest_epoch).saturating_sub(current_frozen)
	}
}

impl<AccountId: PartialEq, Balance> BranchState<AccountId, Balance> {
	/// Clear the single dormant redemption slot, but only if it currently points
	/// at `owner`. No-op otherwise.
	pub fn release_dormant_target(&mut self, owner: &AccountId) {
		if self.dormant_redemption_target.as_ref() == Some(owner) {
			self.dormant_redemption_target = None;
		}
	}

	/// Park `owner` in the dormant redemption slot, returning `false` (without
	/// mutating) when a *different* debt-bearing vault already holds it.
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
	/// Add a vault's full contribution to branch debt/stake aggregates.
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

	/// Subtract a vault's full contribution from the branch aggregates.
	///
	/// Mirrors the addition done at vault open: every writer that mutates
	/// `(debt.principal, debt.interest, redistribution_stake)` for a vault must
	/// keep this sum-of-contributions invariant intact, so removal is the
	/// exact inverse — recompute the same `(rate * debt, rate * stake)`
	/// products and subtract.
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

	pub fn add_collateral(&mut self, amount: Balance) {
		self.total_collateral = self.total_collateral.saturating_add(amount);
	}

	pub fn remove_collateral(&mut self, amount: Balance) {
		self.total_collateral = self.total_collateral.saturating_sub(amount);
	}

	/// Apply a debt payment to the branch aggregates. `principal_after` is the
	/// paying vault's principal *after* `VaultDebt::cancel` ran.
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

	/// Set a vault's redistribution stake after collateral or eligibility has
	/// changed, swapping its contribution in the branch stake aggregates.
	/// Mutates the row and the aggregates together so
	/// `stakes.total == Σ vault.redistribution_stake` cannot drift. The vault
	/// rate is unchanged here; rate moves go through [`Self::change_vault_rate`].
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

	/// Fold `principal` of pending redistributed debt into `vault` at its own
	/// rate, mutating the vault row and the branch aggregates together.
	///
	/// Redistribution recorded the debt at the branch-average rate (see
	/// [`Self::record_redistribution`]); on touch, the receiving vault re-prices
	/// its share at its own `annual_rate`, so the average-rate weighting
	/// accumulated since the vault's snapshot (`weight_per_stake` delta) is
	/// swapped out for the vault's own-rate contribution.
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

	/// True when no debt-bearing or stake-bearing vault row remains attached
	/// (branch principal, stake, and pending redistribution all zero). Interest
	/// drift and bad debt may still remain to be swept, and collateral may still
	/// sit in the redistribution account; this marks the last-vault *settlement*
	/// point. For the *removal* precondition use [`Self::is_removable`].
	pub fn is_empty_of_liability(&self) -> bool {
		self.debt.principal.is_zero() &&
			self.stakes.total.is_zero() &&
			self.debt.pending_redistribution_principal.is_zero()
	}

	/// True when the market carries no residual liability at all: no debt
	/// (principal, minted interest, pending redistribution, or socialized bad
	/// debt), no stake, and no collateral still locked in the redistribution
	/// account. The precondition for removing the market.
	pub fn is_removable(&self) -> bool {
		self.debt.outstanding().is_zero() &&
			self.stakes.total.is_zero() &&
			self.total_collateral.is_zero()
	}

	/// Record unbacked circulating debt against the branch ledger.
	pub fn record_bad_debt(&mut self, amount: Balance) {
		self.debt.bad_debt = self.debt.bad_debt.saturating_add(amount);
	}

	/// Burn recorded bad debt (saturating; callers cap `amount` at
	/// `debt.bad_debt` where exactness matters).
	pub fn heal_bad_debt(&mut self, amount: Balance) {
		self.debt.bad_debt = self.debt.bad_debt.saturating_sub(amount);
	}

	/// Sweep the orphan debt counters into `bad_debt`, returning the swept
	/// amount.
	pub fn sweep_orphan_debt(&mut self) -> Balance {
		let orphan = self.debt.minted_interest.saturating_add(self.ownerless_debt);
		self.debt.minted_interest = Balance::zero();
		self.ownerless_debt = Balance::zero();
		self.debt.bad_debt = self.debt.bad_debt.saturating_add(orphan);
		orphan
	}
}

impl<AccountId, Balance: FixedPointOperand + Ord> BranchState<AccountId, Balance> {
	/// Fold one liquidation's redistribution into the branch accumulators.
	///
	/// The redistributed debt is recorded at the branch-average rate (its
	/// weighting is corrected to each recipient's own rate when the recipient
	/// absorbs its share, see [`Self::absorb_redistributed_debt`]). Per-stake
	/// flooring residue lands in the ownerless buckets. Returns `None`
	/// when a per-stake increment overflows; the accumulators are only written
	/// once every increment has been validated.
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
		// Must match `pending_touch_for`'s interest-time origin.
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

/// Atomic update to a single field of `BranchConfig`.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub enum BranchConfigUpdate<Balance> {
	MinimumCollateralizationRatio(FixedU128),
	InitialCollateralizationRatio(FixedU128),
	SafetyCollateralizationRatio(FixedU128),
	DebtCeiling(Balance),
	MinimumDebt(Balance),
	MinimumCollateral(Balance),
	BorrowRateBounds {
		min: FixedU128,
		max: FixedU128,
	},
	UpfrontFeePeriod(Millis),
	RateAdjustmentCooldown(Millis),
	RedistributionPenalty(Permill),
	/// Autoline headroom above current debt. `0` disables the autoline,
	/// pinning the borrow cap to the static `debt_ceiling`.
	CeilingGap(Balance),
	/// Minimum time between autoline ceiling increases (the slow-up gate).
	CeilingTtl(Millis),
}

impl<Balance: PartialOrd + Copy> BranchConfigUpdate<Balance> {
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

	/// Admin tier required to apply this update. The risk parameters an
	/// `Emergency` admin may tighten are `Emergency`-gated; the rest are
	/// `Full`-only.
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

	/// Whether applying this update to `config` is risk-reducing: raising a
	/// collateralization floor, lowering the debt ceiling, or narrowing the rate
	/// band. Only consulted for `Emergency`-tier callers; `Full`-only variants
	/// (never reached by an `Emergency` admin) report `true`.
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

/// Governance envelope a permissionless market's config must sit inside.
///
/// Markets sharing a collateral share its risk, so a creator's `Full` autonomy
/// is bounded by these floors and ceilings — validated at `create_branch` and
/// on every `set_param` update. The per-collateral `GlobalDebtCeiling[C] > 0`
/// gate (governance's collateral allow-list) is enforced separately on the
/// borrow path.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct BranchConfigGuard<Balance> {
	pub min_minimum_collateralization_ratio: FixedU128,
	pub min_initial_collateralization_ratio: FixedU128,
	pub min_safety_collateralization_ratio: FixedU128,
	pub min_minimum_debt: Balance,
	pub min_minimum_collateral: Balance,
	pub max_borrow_rate: FixedU128,
	/// Cap on a market's static `debt_ceiling` (the autoline `line_max`).
	pub max_branch_line: Balance,
	/// Cap on the autoline headroom a market may keep above its current debt.
	/// Bounds how large a single ceiling step can be.
	pub max_ceiling_gap: Balance,
	/// Floor on the minimum time between autoline ceiling increases. Bounds how
	/// fast a market may ratchet its ceiling up.
	pub min_ceiling_ttl: Millis,
}

impl<Balance: PartialOrd + Copy + Zero> BranchConfigGuard<Balance> {
	/// Whether `config` sits within the envelope (ratios at or above the floors,
	/// rate and line at or below the ceilings). The autoline slow-up floor
	/// (`min_ceiling_ttl`) only binds when the autoline is enabled (`ceiling_gap > 0`).
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

/// Per-market admin authorization tier.
///
/// `Full` may move any parameter within the [`BranchConfigGuard`] envelope and
/// remove an empty market. `Emergency` may only take risk-reducing actions:
/// freeze, raise collateralization thresholds, lower the debt ceiling, or reduce
/// the max borrow rate.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
)]
pub enum AdminLevel {
	Full,
	Emergency,
}

/// The two admins of a market, bundled so the same-typed `full_admin` and
/// `emergency_admin` cannot be silently swapped at a call site — end to end,
/// from the `create_branch`/`set_branch_admins` call arguments into storage.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct BranchAdmins<PalletsOrigin> {
	/// May move any param within the envelope and remove an empty market.
	pub full_admin: PalletsOrigin,
	/// May only take risk-reducing actions (freeze, tighten).
	pub emergency_admin: PalletsOrigin,
}

/// Per-market admin origins and the refundable creation deposit, stored
/// together and torn down together by `remove_branch`. The deposit stays keyed
/// by the depositor *account* regardless of who admins the market.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct BranchAdminInfo<PalletsOrigin, AccountId, Consideration> {
	pub admins: BranchAdmins<PalletsOrigin>,
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
		// rate = 0.3: floor(0.3 * 10) = 3 and floor(0.3 * 9) = 2. The naive
		// `floor(rate * delta)` update would subtract floor(0.3 * 1) = 0 and
		// strand the weighted sum at 3.
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
		// Vault: principal 10 at rate 0.5 → own contribution floor(0.5 · 10) = 5.
		// Avg-rate weighting accumulated since the snapshot:
		// (0.3 − 0.1) · stake 10 = 2. Absorbing 3 re-prices the share:
		// 20 − 2 − 5 + floor(0.5 · 13) = 20 − 2 − 5 + 6 = 19.
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
