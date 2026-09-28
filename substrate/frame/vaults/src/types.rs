//! Types stored or exposed by the Vaults pallet.

use crate::{math, Millis};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::{
	arithmetic::{
		ArithmeticError, AtLeast32BitUnsigned, CheckedAdd, CheckedSub, FixedPointOperand,
		FixedU128, One, Permill, Rounding, Saturating, Zero,
	},
	deps::{frame_support::PalletError, sp_core::U256},
};
pub use pusd_primitives::{BranchMode, DebtCollateral, VaultStatus};
use scale_info::TypeInfo;

/// Identifies a Vaults list for one market and use case.
///
/// The runtime uses this value as a storage key for its `pallet-linked-list` instance. Each variant
/// identifies one `(collateral, stable)` market and one list.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub enum VaultListId<CollateralId, StableId> {
	/// Identifies the borrow-rate index, which sorts vaults by annual rate.
	#[codec(index = 0)]
	Rate(CollateralId, StableId),
	/// Identifies the `FinalRecovery` FIFO.
	#[codec(index = 1)]
	FinalRecovery(CollateralId, StableId),
}

#[cfg(feature = "runtime-benchmarks")]
impl<CollateralId: Default, StableId: Default> Default for VaultListId<CollateralId, StableId> {
	fn default() -> Self {
		Self::Rate(CollateralId::default(), StableId::default())
	}
}

/// Liquidation configuration for one `(collateral, stablecoin)` market.
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
pub struct LiquidationConfig<Balance> {
	/// Extra collateral value seized for debt cancelled by an offset.
	pub offset_penalty: Permill,
	/// Flat keeper compensation, in stablecoin value.
	pub keeper_flat_compensation_value: Balance,
	/// Share of seized collateral added to the flat keeper compensation.
	pub keeper_percent_compensation: Permill,
	/// Maximum keeper compensation, in stablecoin value.
	pub keeper_compensation_cap_value: Balance,
	/// Smallest keeper allowance or funding accepted for a direct contribution.
	///
	/// A smaller keeper-side amount skips JIT without blocking liquidation. The limit does not
	/// bind the residual the waterfall asks for: a smaller system ask still executes.
	pub minimum_jit_contribution: Balance,
	/// Extra collateral assigned to redistributed debt.
	///
	/// Final recovery also uses this as its bonus cap.
	pub redistribution_penalty: Permill,
}

/// Keeper-supplied terms for the direct contribution to one liquidation.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub struct JitTerms<Balance> {
	/// Maximum stable assets the keeper allows the call to burn for a direct contribution.
	///
	/// Zero disables the contribution.
	pub max_stable: Balance,
	/// Minimum collateral allocated to an executed JIT slice, excluding the keeper reward.
	///
	/// This absolute floor is not scaled down for a partial JIT execution. Keepers should set it
	/// for the smallest execution they would accept. A trade that would pay less is skipped and
	/// the liquidation proceeds without the contribution.
	pub min_collateral_out: Balance,
}

/// Complete observable result of one liquidation.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct LiquidationOutcome<Balance> {
	/// Debt and collateral settled by the active Stability Pool.
	pub active_pool: DebtCollateral<Balance>,
	/// Debt and collateral settled directly by the keeper.
	pub keeper_jit: DebtCollateral<Balance>,
	/// Debt and collateral settled by pending Stability deposits.
	pub pending_pool: DebtCollateral<Balance>,
	/// Debt and collateral redistributed to surviving vaults.
	pub redistribution: DebtCollateral<Balance>,
	/// Collateral paid to the keeper for executing the liquidation.
	pub keeper_reward: Balance,
	/// Collateral returned to the liquidated vault's owner.
	pub owner_surplus: Balance,
}

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

/// A debt amount split into principal and interest.
///
/// Vaults store their current debt in this form, and [`Self::cancel`] returns
/// the cancelled amount in the same form.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct DebtBreakdown<Balance> {
	/// Principal portion of the debt amount.
	pub principal: Balance,
	/// Interest and fee portion of the debt amount.
	pub interest: Balance,
}

impl<Balance: Ord + Saturating + Copy> DebtBreakdown<Balance> {
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

/// Cumulative redistribution amounts per unit of stake.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RedistributionAccumulators {
	/// Cumulative collateral assigned per unit of stake.
	pub collateral_per_stake: FixedU128,
	/// Cumulative principal assigned per unit of stake, `A = Σ d_k`.
	pub principal_per_stake: FixedU128,
	/// Each principal increment times its market interest time, `B = Σ d_k · τ_k`, in
	/// [`FixedU128`] inner units times milliseconds.
	///
	/// A vault's share accrued `rate · stake · (τ · ΔA − ΔB)` by market interest time `τ`, so its
	/// interest is exact whatever order vaults are touched in.
	pub principal_time_per_stake: Wide,
}

/// An unsigned 256-bit value stored as two limbs.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Wide {
	pub low: u128,
	pub high: u128,
}

impl Wide {
	pub(crate) fn from_wide(value: U256) -> Self {
		Self { low: value.low_u128(), high: (value >> 128).low_u128() }
	}

	pub(crate) fn to_wide(self) -> U256 {
		(U256::from(self.high) << 128) | U256::from(self.low)
	}

	pub(crate) fn checked_add(self, other: U256) -> Option<Self> {
		self.to_wide().checked_add(other).map(Self::from_wide)
	}

	pub(crate) fn checked_sub(self, other: U256) -> Option<Self> {
		self.to_wide().checked_sub(other).map(Self::from_wide)
	}

	pub fn is_zero(&self) -> bool {
		self.low == 0 && self.high == 0
	}
	/// Whole units of an accrual rate, rounded down, saturating at `Balance::max_value()`.
	pub fn whole<Balance: FixedPointOperand>(&self) -> Balance {
		use frame::arithmetic::FixedPointNumber;
		let whole = self.to_wide() / U256::from(FixedU128::DIV);
		u128::try_from(whole)
			.ok()
			.and_then(|whole| Balance::try_from(whole).ok())
			.unwrap_or_else(Balance::max_value)
	}
}

/// State of one vault.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct Vault<Balance> {
	/// Collateral assigned to this market.
	///
	/// The owner's on-chain hold may also back vaults in other stable-asset markets.
	pub collateral: Balance,
	/// Current principal and interest.
	pub debt: DebtBreakdown<Balance>,
	/// Annual interest rate chosen by the owner.
	pub annual_rate: FixedU128,
	/// Market interest time of the last vault update.
	pub last_interest_time: Millis,
	/// Interest charged ahead of accrual, below one unit of [`math::INTEREST_DENOMINATOR`].
	///
	/// A touch rounds accrued interest up and keeps the excess here, so the next touch charges
	/// only past it. The debt therefore never falls below its exact value, and touching a vault
	/// repeatedly cannot add units.
	pub interest_prepaid: u128,
	/// Wall-clock time of the last rate change.
	pub last_rate_update: Millis,
	/// Collateral used as redistribution stake.
	///
	/// This is zero for a vault in final recovery. Snapshot correction makes later allocations
	/// independent of touch order.
	pub redistribution_stake: Balance,
	/// Redistribution totals applied by the last vault update.
	pub redistribution_checkpoint: RedistributionAccumulators,
}

/// Stored form of one vault: the accounting row and the deposit that pays for it.
///
/// The deposit is kept out of [`Vault`] so accounting code can copy and compare rows freely; a
/// consideration ticket must be moved, never duplicated.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
pub struct VaultRecord<Balance, Deposit> {
	/// Accounting state.
	pub vault: Vault<Balance>,
	/// Refundable storage deposit, released when the row is removed.
	pub deposit: Deposit,
}

/// The part of one vault represented in branch-wide accounting.
///
/// Replacing this value is the single ordinary vault-to-branch accounting
/// primitive. Collateral is intentionally absent: it is managed separately
/// because redistributed collateral is already included in the branch total.
struct VaultContribution<Balance> {
	principal: Balance,
	interest: Balance,
	accrual_rate: U256,
	stake: Balance,
	stake_accrual_rate: U256,
	eligible_collateral: Balance,
}

impl<Balance: FixedPointOperand> VaultContribution<Balance> {
	fn zero() -> Self {
		Self {
			principal: Zero::zero(),
			interest: Zero::zero(),
			accrual_rate: U256::zero(),
			stake: Zero::zero(),
			stake_accrual_rate: U256::zero(),
			eligible_collateral: Zero::zero(),
		}
	}

	fn of(vault: Option<&Vault<Balance>>) -> Result<Self, ArithmeticError> {
		let Some(vault) = vault else { return Ok(Self::zero()) };
		Ok(Self {
			principal: vault.debt.principal,
			interest: vault.debt.interest,
			accrual_rate: math::accrual_rate(vault.debt.principal, vault.annual_rate),
			stake: vault.redistribution_stake,
			stake_accrual_rate: math::accrual_rate(vault.redistribution_stake, vault.annual_rate),
			eligible_collateral: if vault.redistribution_stake.is_zero() {
				Balance::zero()
			} else {
				vault.collateral
			},
		})
	}
}

impl<Balance> Vault<Balance> {
	/// Returns whether the rate-change cooldown has passed.
	pub(crate) const fn cooldown_elapsed(
		&self,
		config: &BranchConfig<Balance>,
		now: Millis,
	) -> bool {
		now.saturating_sub(self.last_rate_update) >= config.rate_adjustment_cooldown
	}
}

impl<Balance: Ord + Saturating + Copy> Vault<Balance> {
	/// The debt/collateral pair the CR gates read.
	pub fn position(&self) -> DebtCollateral<Balance> {
		DebtCollateral { debt: self.debt.total(), collateral: self.collateral }
	}
}

impl<Balance: Ord + Saturating + Copy + Zero + One> Vault<Balance> {
	/// Returns the values for one redemption step.
	///
	/// Projection and execution use this snapshot to keep all fields consistent. The branch
	/// parameters it carries are the ones `FinalRecovery` pricing consults.
	pub(crate) fn redemption_snapshot(
		&self,
		status: VaultStatus,
		config: &BranchConfig<Balance>,
	) -> pusd_primitives::RedemptionStepSnapshot<Balance> {
		pusd_primitives::RedemptionStepSnapshot {
			status,
			debt: self.debt.total(),
			collateral: self.collateral,
			redistribution_penalty: config.liquidation.redistribution_penalty,
			initial_collateralization_ratio: config.initial_collateralization_ratio,
			minimum_debt: config.minimum_debt,
		}
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
	/// Maximum market debt.
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
	/// Minimum time between a paid final-recovery entry and the next paid entry in this market.
	pub final_recovery_reward_cooldown: Millis,
	/// Penalties, keeper compensation, and direct-JIT limits used during liquidation.
	pub liquidation: LiquidationConfig<Balance>,
}

/// The smallest balance each of a market's two assets can hold.
///
/// A market's own amounts mean nothing on their own: a six-decimal stablecoin and an
/// eighteen-decimal one disagree about what "one" is. Both agree that a balance under the
/// asset's minimum cannot be held, which is what makes it the one floor every market can be
/// judged against.
pub struct AssetMinimums<Balance> {
	/// Smallest collateral balance an account can hold.
	pub collateral: Balance,
	/// Smallest stablecoin balance an account can hold.
	pub stable: Balance,
}

/// A way one [`BranchConfig`] contradicts itself or the assets it names.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, PalletError, PartialEq, Eq, Debug)]
pub enum BranchConfigDefect {
	/// The liquidation ratio is above the borrow ratio, so every new vault opens liquidatable.
	LiquidationRatioAboveInitial,
	/// The liquidation ratio is above the safety ratio, so the market liquidates before it
	/// enters safety mode.
	LiquidationRatioAboveSafety,
	/// The rate band is inverted, so no rate a vault could pick is inside it.
	MinimumBorrowRateAboveMaximum,
	/// The debt floor is zero, so a vault could open owing nothing and never be dormant.
	ZeroMinimumDebt,
	/// The collateral floor is zero, so every vault could open on dust.
	ZeroMinimumCollateral,
	/// The debt floor is under the stablecoin's minimum balance, so the smallest vault could
	/// not be paid what it borrows.
	MinimumDebtBelowStableMinimum,
	/// The collateral floor is under the collateral's minimum balance, so the smallest vault
	/// could hold less than an account can carry.
	MinimumCollateralBelowCollateralMinimum,
	/// Offsetting costs the borrower more than redistribution, inverting the waterfall order.
	OffsetPenaltyAboveRedistribution,
	/// The keeper's share of a seizure outgrows the offset penalty funding it.
	KeeperPercentExceedsPenalty,
	/// Keeper compensation is larger than the offset penalty on the smallest vault.
	KeeperCompensationExceedsPenalty,
}

impl<Balance: AtLeast32BitUnsigned + Copy> BranchConfig<Balance> {
	/// Returns how this configuration contradicts itself, or `None` when it is consistent.
	///
	/// Every amount here is denominated in the market's own assets, so its
	/// magnitude is the creator's choice; `minimums` is the one yardstick those
	/// assets themselves provide. This rejects only the combinations that
	/// contradict each other and so cannot describe a working market.
	/// Runtime-owned floors live in [`BranchConfigBounds::violation`].
	pub fn structural_defect(
		&self,
		minimums: &AssetMinimums<Balance>,
	) -> Option<BranchConfigDefect> {
		// A liquidation ratio above the borrow or safety ratio would open
		// vaults that are already liquidatable.
		if self.minimum_collateralization_ratio > self.initial_collateralization_ratio {
			return Some(BranchConfigDefect::LiquidationRatioAboveInitial);
		}
		if self.minimum_collateralization_ratio > self.safety_collateralization_ratio {
			return Some(BranchConfigDefect::LiquidationRatioAboveSafety);
		}
		// An empty rate band leaves no rate a vault could open or re-rate at, which stops
		// borrowing as surely as a zero debt limit does.
		if self.minimum_borrow_rate > self.maximum_borrow_rate {
			return Some(BranchConfigDefect::MinimumBorrowRateAboveMaximum);
		}
		if let Some(defect) = self.vault_floor_defect(minimums) {
			return Some(defect);
		}
		// Offsetting must stay the cheaper waterfall step, or liquidators
		// would prefer redistribution.
		if self.liquidation.offset_penalty > self.liquidation.redistribution_penalty {
			return Some(BranchConfigDefect::OffsetPenaltyAboveRedistribution);
		}
		if let Some(defect) = self.keeper_compensation_defect() {
			return Some(defect);
		}
		None
	}

	/// Returns how the vault floors fail to describe a vault worth carrying.
	///
	/// The floors are all that stands between a market and unbounded dust vaults. Each vault is
	/// a storage row, a sorted-list node, and a step a redemption may walk, and a vault owing
	/// less than the stablecoin's minimum balance pays for none of it. Measuring against the
	/// assets' own minimums holds every market to the same rule rather than the same number.
	fn vault_floor_defect(&self, minimums: &AssetMinimums<Balance>) -> Option<BranchConfigDefect> {
		// A zero debt floor also makes every husk active, since a vault is dormant exactly
		// while it owes less than the floor.
		if self.minimum_debt.is_zero() {
			return Some(BranchConfigDefect::ZeroMinimumDebt);
		}
		if self.minimum_collateral.is_zero() {
			return Some(BranchConfigDefect::ZeroMinimumCollateral);
		}
		if self.minimum_debt < minimums.stable {
			return Some(BranchConfigDefect::MinimumDebtBelowStableMinimum);
		}
		if self.minimum_collateral < minimums.collateral {
			return Some(BranchConfigDefect::MinimumCollateralBelowCollateralMinimum);
		}
		None
	}

	/// Returns how keeper compensation would be paid out of the pool's principal cover.
	fn keeper_compensation_defect(&self) -> Option<BranchConfigDefect> {
		let penalty_rate = FixedU128::from(self.liquidation.offset_penalty);
		let keeper_rate = FixedU128::from(self.liquidation.keeper_percent_compensation);
		// Per unit of debt the seizure is `1 + offset_penalty` and the spare part is
		// `offset_penalty`. Both rates convert exactly, so this holds at every vault size.
		let seizure_rate = FixedU128::one().saturating_add(penalty_rate);
		if keeper_rate.saturating_mul(seizure_rate) > penalty_rate {
			return Some(BranchConfigDefect::KeeperPercentExceedsPenalty);
		}
		// Mirrors the rounding the waterfall applies: seizure rounds the penalty up, the
		// keeper's percentage share rounds down, and the cap applies to their sum. A cap
		// inside the penalty is what makes the terms payable in full, not what makes them
		// invalid.
		let penalty = self.liquidation.offset_penalty.mul_ceil(self.minimum_debt);
		let seizure = self.minimum_debt.saturating_add(penalty);
		let take = self
			.liquidation
			.keeper_flat_compensation_value
			.saturating_add(self.liquidation.keeper_percent_compensation.mul_floor(seizure))
			.min(self.liquidation.keeper_compensation_cap_value);
		if take > penalty {
			return Some(BranchConfigDefect::KeeperCompensationExceedsPenalty);
		}
		None
	}
}

/// Debt totals for one market.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug, Default)]
pub struct BranchDebt<Balance> {
	/// Principal stored across all vaults.
	pub principal: Balance,
	/// Liquidated principal assigned through the per-stake accumulator but not yet materialized.
	pub pending_redistribution_principal: Balance,
	/// Interest and fees minted by the market and not yet repaid.
	pub minted_interest: Balance,
	/// Minted aggregate interest not yet attributed to vault debt.
	///
	/// This is a subset of [`Self::minted_interest`], not an additional debt term.
	pub pending_interest_attribution: Balance,
	/// Interest vaults were charged, and the market minted, before its aggregate accrued it.
	pub interest_minted_ahead: Balance,
	/// Exact accrual rate of the market's principal, `Σ principal · rate-inner`.
	pub accrual_rate: Wide,
	/// Pending redistribution's subset of [`Self::accrual_rate`].
	pub pending_redistribution_accrual_rate: Wide,
	/// Market interest time of the last aggregate interest update.
	pub last_interest_time: Millis,
	/// Aggregate interest below one unit carried across refreshes, over
	/// [`math::INTEREST_DENOMINATOR`].
	pub aggregate_interest_remainder: u128,
}

impl<Balance: FixedPointOperand + Saturating> BranchDebt<Balance> {
	/// Returns all debt owed by the market.
	pub fn outstanding(&self) -> Balance {
		self.principal
			.saturating_add(self.pending_redistribution_principal)
			.saturating_add(self.minted_interest)
	}
}

impl<Balance: Zero> BranchDebt<Balance> {
	/// Returns whether all attributed interest state is zero.
	pub(crate) fn interest_ledger_settled(&self) -> bool {
		self.pending_interest_attribution.is_zero() &&
			self.accrual_rate.is_zero() &&
			self.pending_redistribution_accrual_rate.is_zero()
	}
}

impl<Balance: FixedPointOperand + Ord + Saturating + CheckedAdd + CheckedSub> BranchDebt<Balance> {
	/// Attributes vault interest and returns the part that requires new issuance.
	///
	/// The uncovered part runs ahead of the aggregate, which [`Self::accrue_aggregate`] offsets.
	/// This preserves `minted_interest == Σ vault interest + pending_interest_attribution`.
	pub(crate) fn attribute_interest(&mut self, amount: Balance) -> Option<Balance> {
		let covered = amount.min(self.pending_interest_attribution);
		let uncovered = amount.saturating_sub(covered);
		let pending_interest_attribution =
			self.pending_interest_attribution.checked_sub(&covered)?;
		let interest_minted_ahead = self.interest_minted_ahead.checked_add(&uncovered)?;
		let minted_interest = self.minted_interest.checked_add(&uncovered)?;
		self.pending_interest_attribution = pending_interest_attribution;
		self.interest_minted_ahead = interest_minted_ahead;
		self.minted_interest = minted_interest;
		Some(uncovered)
	}

	/// Records aggregate interest and returns the part that requires new issuance.
	///
	/// Interest vaults were already charged ahead is not minted again. This preserves the same
	/// identity as [`Self::attribute_interest`].
	pub(crate) fn accrue_aggregate(&mut self, amount: Balance) -> Option<Balance> {
		let charged = amount.min(self.interest_minted_ahead);
		let uncharged = amount.saturating_sub(charged);
		let interest_minted_ahead = self.interest_minted_ahead.checked_sub(&charged)?;
		let pending_interest_attribution =
			self.pending_interest_attribution.checked_add(&uncharged)?;
		let minted_interest = self.minted_interest.checked_add(&uncharged)?;
		self.interest_minted_ahead = interest_minted_ahead;
		self.pending_interest_attribution = pending_interest_attribution;
		self.minted_interest = minted_interest;
		Some(uncharged)
	}
}

/// Stablecoin-wide realized debt and projection of unminted aggregate interest.
///
/// Kept in step by `commit_branch` so `accrued_stablecoin_debt` is O(1)
/// instead of a walk over the uncapped market registry.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, PartialEq, Eq, Debug, Default)]
pub struct StablecoinDebtState<Balance> {
	/// Realized debt summed across every market issuing the stablecoin.
	pub outstanding: Balance,
	/// Σ `accrual_rate` over the coin's non-frozen markets.
	pub active_accrual_rate: Wide,
	/// Interest accrued up to `last_update` but not yet minted anywhere, over
	/// [`math::INTEREST_DENOMINATOR`], so every market's share adds and subtracts exactly.
	pub pending_interest: Wide,
	/// Time the projection was last advanced.
	pub last_update: Millis,
}

impl<Balance: Zero> StablecoinDebtState<Balance> {
	pub fn is_empty(&self) -> bool {
		self.outstanding.is_zero() &&
			self.active_accrual_rate.is_zero() &&
			self.pending_interest.is_zero()
	}
}

/// Redistribution stake totals for one market.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug, Default)]
pub struct RedistributionStakeTotals<Balance> {
	/// Total stake of eligible vaults.
	pub total: Balance,
	/// Exact sum of each eligible vault's `stake · annual_rate`.
	pub accrual_rate: Wide,
	/// Eligible vault collateral plus collateral still pending in redistribution custody.
	pub collateral_basis: Balance,
	/// Total stake captured after the latest redistribution.
	pub snapshot_total: Balance,
	/// Stake-bearing collateral captured after the latest redistribution.
	pub snapshot_collateral: Balance,
}

/// Accounting state for one market.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct BranchState<AccountId, Balance> {
	/// Total collateral held by vault owners or waiting for redistribution.
	pub total_collateral: Balance,
	/// Market debt totals.
	pub debt: BranchDebt<Balance>,
	/// Market redistribution stake totals.
	pub stakes: RedistributionStakeTotals<Balance>,
	/// Current lazy redistribution totals.
	pub redistribution: RedistributionAccumulators,
	/// Redistributed collateral held by the market account until vaults materialize it.
	pub pending_redistribution_collateral: Balance,
	/// Number of vault rows in this market.
	pub vault_count: u32,
	/// Wall-clock origin used to calculate market interest time.
	///
	/// Frozen periods move this value forward so interest does not accrue while frozen.
	pub interest_epoch: Millis,
	/// Dormant vault that must be redeemed before the rate list.
	pub dormant_redemption_target: Option<AccountId>,
	/// Time of the latest final-recovery entry in this market.
	pub last_final_recovery_entry: Option<Millis>,
	/// Frozen state, if the market is frozen.
	pub frozen: Option<FrozenState>,
}

impl<AccountId, Balance: Default + Zero> BranchState<AccountId, Balance> {
	/// Creates empty state for a new market.
	pub fn fresh(now: Millis) -> Self {
		Self {
			total_collateral: Balance::zero(),
			debt: BranchDebt::default(),
			stakes: RedistributionStakeTotals::default(),
			redistribution: RedistributionAccumulators::default(),
			pending_redistribution_collateral: Balance::zero(),
			vault_count: 0,
			interest_epoch: now,
			dormant_redemption_target: None,
			last_final_recovery_entry: None,
			frozen: None,
		}
	}
}

impl<AccountId, Balance> BranchState<AccountId, Balance> {
	/// Returns whether the market is frozen.
	pub const fn is_frozen(&self) -> bool {
		self.frozen.is_some()
	}

	/// Returns whether a new final-recovery entry pays its keeper at `now`.
	pub(crate) const fn final_recovery_reward_due(
		&self,
		config: &BranchConfig<Balance>,
		now: Millis,
	) -> bool {
		match self.last_final_recovery_entry {
			None => true,
			Some(entered_at) => {
				now.saturating_sub(entered_at) >= config.final_recovery_reward_cooldown
			},
		}
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

impl<AccountId, Balance: FixedPointOperand + Saturating + CheckedAdd + CheckedSub + One>
	BranchState<AccountId, Balance>
{
	/// Replace one vault's complete contribution to the market totals.
	///
	/// `None` is the zero contribution, so the same checked primitive handles
	/// creation, mutation, and removal. Underflow means the branch/vault
	/// accounting was already inconsistent; it must not be hidden by saturating
	/// arithmetic.
	pub(crate) fn replace_vault(
		&mut self,
		before: Option<&Vault<Balance>>,
		after: Option<&Vault<Balance>>,
	) -> Result<(), ArithmeticError> {
		let before = VaultContribution::of(before)?;
		let after = VaultContribution::of(after)?;
		let shifted = |current: Balance, old: Balance, new: Balance| {
			current
				.checked_sub(&old)
				.ok_or(ArithmeticError::Underflow)?
				.checked_add(&new)
				.ok_or(ArithmeticError::Overflow)
		};

		let principal = shifted(self.debt.principal, before.principal, after.principal)?;
		let minted_interest = shifted(self.debt.minted_interest, before.interest, after.interest)?;
		let shifted_wide = |current: Wide, old: U256, new: U256| {
			current
				.checked_sub(old)
				.ok_or(ArithmeticError::Underflow)?
				.checked_add(new)
				.ok_or(ArithmeticError::Overflow)
		};
		let accrual_rate =
			shifted_wide(self.debt.accrual_rate, before.accrual_rate, after.accrual_rate)?;
		let stake = shifted(self.stakes.total, before.stake, after.stake)?;
		let stake_accrual_rate = shifted_wide(
			self.stakes.accrual_rate,
			before.stake_accrual_rate,
			after.stake_accrual_rate,
		)?;
		let collateral_basis = shifted(
			self.stakes.collateral_basis,
			before.eligible_collateral,
			after.eligible_collateral,
		)?;
		self.debt.principal = principal;
		self.debt.minted_interest = minted_interest;
		self.debt.accrual_rate = accrual_rate;
		self.stakes.total = stake;
		self.stakes.accrual_rate = stake_accrual_rate;
		self.stakes.collateral_basis = collateral_basis;
		Ok(())
	}

	/// Recomputes one vault's stake from the latest redistribution snapshots.
	///
	/// Snapshot correction makes redistribution shares independent of collateral touch order.
	/// Nonzero collateral maps to at least one stake unit. `None` identifies arithmetic overflow.
	pub(crate) fn stake_for(&self, collateral: Balance) -> Option<Balance> {
		if collateral.is_zero() {
			return Some(Balance::zero());
		}
		if self.stakes.snapshot_collateral.is_zero() {
			return Some(collateral);
		}
		let stake = pusd_primitives::mul_div_floor(
			collateral,
			self.stakes.snapshot_total,
			self.stakes.snapshot_collateral,
		)?;
		// An eligible vault needs nonzero stake to receive liability and drain the final residue.
		if stake.is_zero() {
			return Some(Balance::one());
		}
		Some(stake)
	}

	/// Removes a materialized redistribution share from the branch-side pending pools.
	pub(crate) fn consume_redistribution(
		&mut self,
		redistribution: DebtCollateral<Balance>,
		accrual_rate: U256,
	) -> Result<(), ArithmeticError> {
		self.move_pending(redistribution, accrual_rate, false)
			.ok_or(ArithmeticError::Underflow)
	}

	/// Posts to (`post`) or takes from the pending redistribution pools.
	///
	/// Recording and consuming a redistribution move the same five fields in opposite directions.
	fn move_pending(
		&mut self,
		amounts: DebtCollateral<Balance>,
		accrual_rate: U256,
		post: bool,
	) -> Option<()> {
		let balance = |current: Balance, amount: Balance| {
			if post {
				current.checked_add(&amount)
			} else {
				current.checked_sub(&amount)
			}
		};
		let rate = |current: Wide| {
			if post {
				current.checked_add(accrual_rate)
			} else {
				current.checked_sub(accrual_rate)
			}
		};
		self.debt.pending_redistribution_principal =
			balance(self.debt.pending_redistribution_principal, amounts.debt)?;
		self.pending_redistribution_collateral =
			balance(self.pending_redistribution_collateral, amounts.collateral)?;
		self.stakes.collateral_basis = balance(self.stakes.collateral_basis, amounts.collateral)?;
		self.debt.pending_redistribution_accrual_rate =
			rate(self.debt.pending_redistribution_accrual_rate)?;
		self.debt.accrual_rate = rate(self.debt.accrual_rate)?;
		Some(())
	}

	/// Records one liquidation residual in the per-stake accumulators.
	///
	/// Pending pools retain the complete principal and collateral. Increments round down, so
	/// vaults never claim more than the pools hold, and the final stake bearer receives the
	/// residue.
	pub(crate) fn record_redistribution(
		&mut self,
		redistributed: DebtCollateral<Balance>,
		now: Millis,
	) -> Option<()> {
		if self.stakes.total.is_zero() {
			return None;
		}
		let principal_per_stake =
			math::redistribution_per_stake(redistributed.debt, self.stakes.total)?;
		let collateral_per_stake =
			math::redistribution_per_stake(redistributed.collateral, self.stakes.total)?;
		// Posted from the stored increment, which is what vaults claim against, and rounded down
		// so the projection never runs ahead of what vaults will owe.
		let increment = U256::from(principal_per_stake.into_inner());
		let posted_accrual_rate =
			math::scale_wide(self.stakes.accrual_rate.to_wide(), increment, Rounding::Down)?;
		let principal_time = increment.checked_mul(U256::from(self.interest_time(now)))?;

		self.redistribution = RedistributionAccumulators {
			collateral_per_stake: self
				.redistribution
				.collateral_per_stake
				.checked_add(&collateral_per_stake)?,
			principal_per_stake: self
				.redistribution
				.principal_per_stake
				.checked_add(&principal_per_stake)?,
			principal_time_per_stake: self
				.redistribution
				.principal_time_per_stake
				.checked_add(principal_time)?,
		};
		self.move_pending(redistributed, posted_accrual_rate, true)?;
		self.stakes.snapshot_total = self.stakes.total;
		self.stakes.snapshot_collateral = self.stakes.collateral_basis;
		Some(())
	}

	/// Returns whether no vault liability remains.
	///
	/// Interest is paid before principal, so a live market cannot have interest without principal.
	/// Use [`Self::is_removable`] to also check debt-free vaults, collateral, and stake.
	pub fn is_empty_of_liability(&self) -> bool {
		self.debt.principal.is_zero() && self.debt.pending_redistribution_principal.is_zero()
	}

	/// Returns whether the market has no debt, stake, or collateral.
	pub fn is_removable(&self) -> bool {
		self.debt.outstanding().is_zero() &&
			self.debt.interest_ledger_settled() &&
			self.debt.aggregate_interest_remainder == 0 &&
			self.debt.interest_minted_ahead.is_zero() &&
			self.stakes.total.is_zero() &&
			self.pending_redistribution_collateral.is_zero() &&
			self.total_collateral.is_zero()
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
	/// Sets the minimum time between a final-recovery entry and the next paid entry.
	FinalRecoveryRewardCooldown(Millis),
	/// Sets the extra collateral seized for debt cancelled by an offset.
	OffsetPenalty(Permill),
	/// Sets the flat keeper compensation, in stablecoin value.
	KeeperFlatCompensationValue(Balance),
	/// Sets the share of seized collateral added to the flat keeper compensation.
	KeeperPercentCompensation(Permill),
	/// Sets the maximum keeper compensation, in stablecoin value.
	KeeperCompensationCapValue(Balance),
	/// Sets the smallest direct keeper contribution.
	MinimumJitContribution(Balance),
	/// Sets the extra collateral assigned to redistributed debt.
	RedistributionPenalty(Permill),
}

impl<Balance: PartialOrd + Copy> BranchConfigUpdate<Balance> {
	/// Applies this update to `config`.
	pub const fn apply_to(self, config: &mut BranchConfig<Balance>) {
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
			Self::FinalRecoveryRewardCooldown(v) => config.final_recovery_reward_cooldown = v,
			Self::OffsetPenalty(v) => config.liquidation.offset_penalty = v,
			Self::KeeperFlatCompensationValue(v) => {
				config.liquidation.keeper_flat_compensation_value = v
			},
			Self::KeeperPercentCompensation(v) => {
				config.liquidation.keeper_percent_compensation = v
			},
			Self::KeeperCompensationCapValue(v) => {
				config.liquidation.keeper_compensation_cap_value = v
			},
			Self::MinimumJitContribution(v) => config.liquidation.minimum_jit_contribution = v,
			Self::RedistributionPenalty(v) => config.liquidation.redistribution_penalty = v,
		}
	}

	/// Returns the administrator role required for this update.
	pub const fn required_level(&self) -> AdminLevel {
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
			Self::FinalRecoveryRewardCooldown(_) |
			Self::OffsetPenalty(_) |
			Self::KeeperFlatCompensationValue(_) |
			Self::KeeperPercentCompensation(_) |
			Self::KeeperCompensationCapValue(_) |
			Self::MinimumJitContribution(_) |
			Self::RedistributionPenalty(_) => AdminLevel::Full,
		}
	}

	/// Returns whether this update only reduces risk.
	///
	/// A defensive update raises ratio limits, lowers the debt limit, or narrows the rate range.
	/// Nothing else is defensive: an update that needs a full administrator changes what the
	/// market charges or seizes, which is a policy decision rather than a risk reduction.
	/// [`required_level`] turns an emergency administrator away from those before this is asked,
	/// so the `false` below is what keeps the answer right if that order ever changes.
	///
	/// [`required_level`]: BranchConfigUpdate::required_level
	pub fn is_defensive(&self, config: &BranchConfig<Balance>) -> bool {
		match self {
			Self::MinimumCollateralizationRatio(v) => *v >= config.minimum_collateralization_ratio,
			Self::InitialCollateralizationRatio(v) => *v >= config.initial_collateralization_ratio,
			Self::SafetyCollateralizationRatio(v) => *v >= config.safety_collateralization_ratio,
			Self::DebtCeiling(v) => *v <= config.debt_ceiling,
			Self::BorrowRateBounds { min, max } => {
				*max <= config.maximum_borrow_rate && *min >= config.minimum_borrow_rate
			},
			Self::MinimumDebt(_) |
			Self::MinimumCollateral(_) |
			Self::UpfrontFeePeriod(_) |
			Self::RateAdjustmentCooldown(_) |
			Self::FinalRecoveryRewardCooldown(_) |
			Self::OffsetPenalty(_) |
			Self::KeeperFlatCompensationValue(_) |
			Self::KeeperPercentCompensation(_) |
			Self::KeeperCompensationCapValue(_) |
			Self::MinimumJitContribution(_) |
			Self::RedistributionPenalty(_) => false,
		}
	}
}

/// Limits for market configuration.
#[derive(Encode, TypeInfo)]
pub struct BranchConfigBounds {
	/// Lowest allowed liquidation and final recovery ratio.
	pub min_minimum_collateralization_ratio: FixedU128,
	/// Lowest allowed ratio after borrowing or withdrawing collateral.
	pub min_initial_collateralization_ratio: FixedU128,
	/// Lowest allowed market safety-mode ratio.
	pub min_safety_collateralization_ratio: FixedU128,
	/// Highest allowed annual rate.
	pub max_borrow_rate: FixedU128,
}

#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, PalletError, PartialEq, Eq, Debug)]
pub enum BoundViolation {
	/// The liquidation ratio is below the runtime's floor.
	MinimumCollateralizationRatioTooLow,
	/// The borrow ratio is below the runtime's floor.
	InitialCollateralizationRatioTooLow,
	/// The safety-mode ratio is below the runtime's floor.
	SafetyCollateralizationRatioTooLow,
	/// The annual rate cap is above the runtime's ceiling.
	BorrowRateTooHigh,
}

impl BranchConfigBounds {
	/// Returns the limit `config` breaches, or `None` when it is within all of them.
	pub fn violation<Balance>(&self, config: &BranchConfig<Balance>) -> Option<BoundViolation> {
		if config.minimum_collateralization_ratio < self.min_minimum_collateralization_ratio {
			return Some(BoundViolation::MinimumCollateralizationRatioTooLow);
		}
		if config.initial_collateralization_ratio < self.min_initial_collateralization_ratio {
			return Some(BoundViolation::InitialCollateralizationRatioTooLow);
		}
		if config.safety_collateralization_ratio < self.min_safety_collateralization_ratio {
			return Some(BoundViolation::SafetyCollateralizationRatioTooLow);
		}
		if config.maximum_borrow_rate > self.max_borrow_rate {
			return Some(BoundViolation::BorrowRateTooHigh);
		}
		None
	}
}

/// Administrator role for one market.
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
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
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
	use frame::arithmetic::FixedPointNumber;

	fn make_branch_state(principal: u128, accrual_rate: U256) -> BranchState<u64, u128> {
		BranchState {
			total_collateral: 0,
			debt: BranchDebt {
				principal,
				pending_redistribution_principal: 0,
				minted_interest: 0,
				pending_interest_attribution: 0,
				interest_minted_ahead: 0,
				accrual_rate: Wide::from_wide(accrual_rate),
				pending_redistribution_accrual_rate: Wide::default(),
				last_interest_time: 0,
				aggregate_interest_remainder: 0,
			},
			stakes: RedistributionStakeTotals::default(),
			redistribution: RedistributionAccumulators::default(),
			pending_redistribution_collateral: 0,
			vault_count: 0,
			interest_epoch: 0,
			dormant_redemption_target: None,
			last_final_recovery_entry: None,
			frozen: None,
		}
	}

	#[test]
	fn replace_vault_swaps_full_contribution() {
		// Subtraction must retain the fractional accrual rate.
		let rate = FixedU128::from_rational(3u128, 10u128);
		let mut state = make_branch_state(10, U256::from(3 * FixedU128::DIV));
		let before = Vault {
			collateral: 0,
			debt: DebtBreakdown { interest: 0, principal: 10 },
			annual_rate: rate,
			last_interest_time: 0,
			interest_prepaid: 0,
			last_rate_update: 0,
			redistribution_stake: 0,
			redistribution_checkpoint: RedistributionAccumulators::default(),
		};
		let mut after = before.clone();
		after.debt.principal = 9;
		state.replace_vault(Some(&before), Some(&after)).unwrap();
		assert_eq!(state.debt.principal, 9);
		assert_eq!(state.debt.accrual_rate.to_wide(), U256::from(27 * FixedU128::DIV / 10));
	}

	#[test]
	fn replace_vault_full_payoff_clears_contribution() {
		let rate = FixedU128::from_rational(3u128, 10u128);
		let mut state = make_branch_state(10, U256::from(3 * FixedU128::DIV));
		let before = Vault {
			collateral: 0,
			debt: DebtBreakdown { interest: 0, principal: 10 },
			annual_rate: rate,
			last_interest_time: 0,
			interest_prepaid: 0,
			last_rate_update: 0,
			redistribution_stake: 0,
			redistribution_checkpoint: RedistributionAccumulators::default(),
		};
		let mut after = before.clone();
		after.debt.principal = 0;
		state.replace_vault(Some(&before), Some(&after)).unwrap();
		assert_eq!(state.debt.principal, 0);
		assert!(state.debt.accrual_rate.is_zero());
	}

	#[test]
	fn replace_vault_rejects_inconsistent_preimage_without_partial_update() {
		let rate = FixedU128::from_rational(3u128, 10u128);
		let mut state = make_branch_state(0, U256::zero());
		let before = Vault {
			collateral: 0,
			debt: DebtBreakdown { interest: 0, principal: 1 },
			annual_rate: rate,
			last_interest_time: 0,
			interest_prepaid: 0,
			last_rate_update: 0,
			redistribution_stake: 0,
			redistribution_checkpoint: RedistributionAccumulators::default(),
		};
		let state_before = state.clone();

		assert_eq!(state.replace_vault(Some(&before), None), Err(ArithmeticError::Underflow));
		assert_eq!(state, state_before);
	}
}
