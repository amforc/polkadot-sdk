//! Types stored or exposed by the Vaults pallet.

use crate::{math, Millis};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::{
	arithmetic::{
		ArithmeticError, AtLeast32BitUnsigned, CheckedAdd, CheckedSub, EnsureAdd, EnsureSub,
		FixedPointNumber, FixedPointOperand, FixedU128, One, Permill, Saturating, Zero,
	},
	deps::{frame_support::PalletError, sp_core::U256},
	prelude::ensure,
};
use scale_info::TypeInfo;
pub use stablecoin_primitives::{BranchMode, DebtCollateral, VaultStatus};

/// Identifies a Vaults list for one branch and use case.
///
/// The runtime uses this value as a storage key for its `pallet-linked-list` instance. Each variant
/// identifies one `(collateral, stable)` branch and one list.
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

/// Contains the liquidation result that [`crate::Pallet::execute_liquidation`] consumes.
///
/// External offset paths burn stablecoin internally. Thus, `debt_offset` is an amount and not a
/// credit. The other fields are credits because this pallet controls their final destinations.
#[must_use = "the settlement must be returned to execute_liquidation"]
pub struct LiquidationSettlement<CollateralCredit, Balance> {
	/// Debt that external offset paths burned.
	pub debt_offset: Balance,
	/// Collateral credit that the pallet sends to its redistribution account.
	pub redistribution_collateral: CollateralCredit,
	/// Collateral credit that the pallet returns to the liquidated owner.
	pub owner_surplus: CollateralCredit,
}

/// Contains the post-touch vault values for a liquidation settlement.
///
/// The settlement builder must use these values to size its settlement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LiquidationSnapshot<Balance> {
	/// Total debt after the pallet applies accrued interest.
	pub debt: Balance,
	/// Branch penalty for debt that the liquidation redistributes to other vaults.
	///
	/// Redistributed debt carries this premium. The premium must be at least the liquidation
	/// penalty that an offset pays. Vaults controls this parameter, and the settlement builder
	/// must use it.
	pub redistribution_penalty: Permill,
}

/// Reason a branch is frozen.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrozenReason {
	/// The oracle has no valid price.
	OracleFailure,
	/// An authorized origin froze the branch.
	Governance,
}

impl FrozenReason {
	/// Returns whether an administrator may lift a freeze with this reason.
	///
	/// An oracle freeze reflects a missing price, which no administrator can supply.
	pub(crate) const fn is_cleared_by_admin(self) -> bool {
		match self {
			Self::OracleFailure => false,
			Self::Governance => true,
		}
	}

	/// Returns whether a returning oracle price lifts a freeze with this reason.
	///
	/// A governance freeze stays until an administrator lifts it, whatever the price does.
	pub(crate) const fn is_cleared_by_oracle(self) -> bool {
		match self {
			Self::OracleFailure => true,
			Self::Governance => false,
		}
	}
}

/// Stored state for a frozen branch.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrozenState {
	/// Why the branch is frozen.
	pub reason: FrozenReason,
	/// Time when the freeze began.
	pub entered_at: Millis,
}

/// A debt amount split into principal and interest.
///
/// Vaults store their current debt in this form, and [`Self::cancel`] returns
/// the cancelled amount in the same form.
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
	/// Each principal increment times its branch interest time, `B = Σ d_k · τ_k`, in
	/// [`FixedU128`] inner units times milliseconds.
	///
	/// A vault's share accrued `rate · stake · (τ · ΔA − ΔB)` by branch interest time `τ`, so its
	/// interest is exact whatever order vaults are touched in.
	pub principal_time_per_stake: U256,
}

/// State of one vault.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct Vault<Balance> {
	/// Collateral assigned to this branch.
	///
	/// The owner's on-chain hold may also back vaults in other stable-asset branches.
	pub collateral: Balance,
	/// Current principal and interest.
	pub debt: DebtBreakdown<Balance>,
	/// Annual interest rate chosen by the owner.
	pub annual_rate: FixedU128,
	/// Branch interest time of the last vault update.
	pub last_interest_time: Millis,
	/// Interest charged ahead of accrual, below one unit of `math::INTEREST_DENOMINATOR`.
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
#[derive(Default)]
struct VaultContribution<Balance> {
	principal: Balance,
	interest: Balance,
	accrual_rate: U256,
	stake: Balance,
	stake_accrual_rate: U256,
	eligible_collateral: Balance,
}

impl<Balance: FixedPointOperand + Default> VaultContribution<Balance> {
	fn of(vault: Option<&Vault<Balance>>) -> Result<Self, ArithmeticError> {
		let Some(vault) = vault else { return Ok(Self::default()) };
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
	/// Projection and execution use this snapshot to keep all fields consistent.
	pub(crate) fn redemption_snapshot(
		&self,
		status: VaultStatus,
		redistribution_penalty: Permill,
	) -> stablecoin_primitives::RedemptionStepSnapshot<Balance> {
		stablecoin_primitives::RedemptionStepSnapshot {
			status,
			debt: self.debt.total(),
			collateral: self.collateral,
			redistribution_penalty,
		}
	}
}

/// Risk parameters for one branch.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct BranchConfig<Balance> {
	/// Ratio below which a vault may be liquidated or moved into final recovery.
	pub minimum_collateralization_ratio: FixedU128,
	/// Minimum vault ratio after borrowing or withdrawing collateral.
	pub initial_collateralization_ratio: FixedU128,
	/// Branch ratio below which safety mode begins.
	pub safety_collateralization_ratio: FixedU128,
	/// Maximum branch debt.
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
}

/// The smallest balance each of a branch's two assets can hold.
///
/// A branch's own amounts mean nothing on their own: a six-decimal stablecoin and an
/// eighteen-decimal one disagree about what "one" is. Both agree that a balance under the
/// asset's minimum cannot be held, which is what makes it the one floor every branch can be
/// judged against.
pub(crate) struct AssetMinimums<Balance> {
	/// Smallest collateral balance an account can hold.
	pub(crate) collateral: Balance,
	/// Smallest stablecoin balance an account can hold.
	pub(crate) stable: Balance,
}

/// A way one [`BranchConfig`] contradicts itself or the assets it names.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, PalletError, PartialEq, Eq, Debug)]
pub enum BranchConfigDefect {
	/// The liquidation ratio is above the borrow ratio, so every new vault opens liquidatable.
	LiquidationRatioAboveInitial,
	/// The liquidation ratio is above the safety ratio, so the branch liquidates before it
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
}

impl<Balance: AtLeast32BitUnsigned + Copy> BranchConfig<Balance> {
	/// Checks this configuration is consistent, returning the first way it contradicts itself.
	///
	/// Every amount here is denominated in the branch's own assets, so its
	/// magnitude is the creator's choice; `minimums` is the one yardstick those
	/// assets themselves provide. This rejects only the combinations that
	/// contradict each other and so cannot describe a working branch.
	/// Runtime-owned floors live in [`BranchConfigBounds::ensure_within`].
	pub(crate) fn ensure_consistent(
		&self,
		minimums: &AssetMinimums<Balance>,
	) -> Result<(), BranchConfigDefect> {
		// A liquidation ratio above the borrow or safety ratio would open
		// vaults that are already liquidatable.
		ensure!(
			self.minimum_collateralization_ratio <= self.initial_collateralization_ratio,
			BranchConfigDefect::LiquidationRatioAboveInitial
		);
		ensure!(
			self.minimum_collateralization_ratio <= self.safety_collateralization_ratio,
			BranchConfigDefect::LiquidationRatioAboveSafety
		);
		// An empty rate band leaves no rate a vault could open or re-rate at, which stops
		// borrowing as surely as a zero debt limit does.
		ensure!(
			self.minimum_borrow_rate <= self.maximum_borrow_rate,
			BranchConfigDefect::MinimumBorrowRateAboveMaximum
		);
		self.ensure_vault_floors(minimums)
	}

	/// Checks the vault floors describe a vault worth carrying.
	///
	/// The floors are all that stands between a branch and unbounded dust vaults. Each vault is
	/// a storage row, a sorted-list node, and a step a redemption may walk, and a vault owing
	/// less than the stablecoin's minimum balance pays for none of it. Measuring against the
	/// assets' own minimums holds every branch to the same rule rather than the same number.
	fn ensure_vault_floors(
		&self,
		minimums: &AssetMinimums<Balance>,
	) -> Result<(), BranchConfigDefect> {
		// A zero debt floor also makes every husk active, since a vault is dormant exactly
		// while it owes less than the floor.
		ensure!(!self.minimum_debt.is_zero(), BranchConfigDefect::ZeroMinimumDebt);
		ensure!(!self.minimum_collateral.is_zero(), BranchConfigDefect::ZeroMinimumCollateral);
		ensure!(
			self.minimum_debt >= minimums.stable,
			BranchConfigDefect::MinimumDebtBelowStableMinimum
		);
		ensure!(
			self.minimum_collateral >= minimums.collateral,
			BranchConfigDefect::MinimumCollateralBelowCollateralMinimum
		);
		Ok(())
	}
}

/// Debt totals for one branch.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug, Default)]
pub struct BranchDebt<Balance> {
	/// Principal stored across all vaults.
	pub principal: Balance,
	/// Liquidated principal assigned through the per-stake accumulator but not yet materialized.
	pub pending_redistribution_principal: Balance,
	/// Interest and fees minted by the branch and not yet repaid.
	pub minted_interest: Balance,
	/// Minted aggregate interest not yet attributed to vault debt.
	///
	/// This is a subset of [`Self::minted_interest`], not an additional debt term.
	pub pending_interest_attribution: Balance,
	/// Interest vaults were charged, and the branch minted, before its aggregate accrued it.
	pub interest_minted_ahead: Balance,
	/// Exact accrual rate of the branch's principal, `Σ principal · rate-inner`.
	pub accrual_rate: U256,
	/// Pending redistribution's subset of [`Self::accrual_rate`].
	pub pending_redistribution_accrual_rate: U256,
	/// Branch interest time of the last aggregate interest update.
	pub last_interest_time: Millis,
	/// Aggregate interest below one unit carried across refreshes, over
	/// `math::INTEREST_DENOMINATOR`.
	pub aggregate_interest_remainder: u128,
}

impl<Balance: FixedPointOperand + Saturating> BranchDebt<Balance> {
	/// Returns all debt owed by the branch.
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
/// instead of a walk over the uncapped branch registry.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, PartialEq, Eq, Debug, Default)]
pub struct StablecoinDebtState<Balance> {
	/// Realized debt summed across every branch issuing the stablecoin.
	pub outstanding: Balance,
	/// Σ `accrual_rate` over the coin's non-frozen branches.
	pub active_accrual_rate: U256,
	/// Interest accrued up to `last_update` but not yet minted anywhere, over
	/// `math::INTEREST_DENOMINATOR`, so every branch's share adds and subtracts exactly.
	pub pending_interest: U256,
	/// Time the projection was last advanced.
	pub last_update: Millis,
}

impl<Balance: Zero> StablecoinDebtState<Balance> {
	/// Returns whether the record carries no debt and no projected interest, so it need not be
	/// stored.
	pub fn is_empty(&self) -> bool {
		self.outstanding.is_zero() &&
			self.active_accrual_rate.is_zero() &&
			self.pending_interest.is_zero()
	}
}

/// Redistribution stake totals for one branch.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug, Default)]
pub struct RedistributionStakeTotals<Balance> {
	/// Total stake of eligible vaults.
	pub total: Balance,
	/// Exact sum of each eligible vault's `stake · annual_rate`.
	pub accrual_rate: U256,
	/// Eligible vault collateral plus collateral still pending in redistribution custody.
	pub collateral_basis: Balance,
	/// Total stake captured after the latest redistribution.
	pub snapshot_total: Balance,
	/// Stake-bearing collateral captured after the latest redistribution.
	pub snapshot_collateral: Balance,
}

/// Accounting state for one branch.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct BranchState<AccountId, Balance> {
	/// Total collateral held by vault owners or waiting for redistribution.
	pub total_collateral: Balance,
	/// Branch debt totals.
	pub debt: BranchDebt<Balance>,
	/// Branch redistribution stake totals.
	pub stakes: RedistributionStakeTotals<Balance>,
	/// Current lazy redistribution totals.
	pub redistribution: RedistributionAccumulators,
	/// Redistributed collateral held by the branch account until vaults materialize it.
	pub pending_redistribution_collateral: Balance,
	/// Number of vault rows in this branch.
	pub vault_count: u32,
	/// Wall-clock origin used to calculate branch interest time.
	///
	/// Frozen periods move this value forward so interest does not accrue while frozen.
	pub interest_epoch: Millis,
	/// Dormant vault that must be redeemed before the rate list.
	pub dormant_redemption_target: Option<AccountId>,
	/// Frozen state, if the branch is frozen.
	pub frozen: Option<FrozenState>,
}

impl<AccountId, Balance: Default + Zero> BranchState<AccountId, Balance> {
	/// Creates empty state for a new branch.
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
			frozen: None,
		}
	}
}

impl<AccountId, Balance> BranchState<AccountId, Balance> {
	/// Returns whether the branch is frozen.
	pub const fn is_frozen(&self) -> bool {
		self.frozen.is_some()
	}

	/// Returns branch interest time at `now`.
	///
	/// Time before branch creation and time spent frozen are excluded.
	pub fn interest_time(&self, now: Millis) -> Millis {
		let current_frozen =
			self.frozen.as_ref().map_or(0, |state| now.saturating_sub(state.entered_at));
		now.saturating_sub(self.interest_epoch).saturating_sub(current_frozen)
	}
}

impl<AccountId: PartialEq, Balance> BranchState<AccountId, Balance> {
	/// Clears the dormant redemption target if it matches `owner`.
	pub(crate) fn release_dormant_target(&mut self, owner: &AccountId) {
		if self.dormant_redemption_target.as_ref() == Some(owner) {
			self.dormant_redemption_target = None;
		}
	}

	/// Sets `owner` as the dormant redemption target.
	///
	/// Returns `false` if another owner already holds the slot.
	pub(crate) fn try_park_dormant_target(&mut self, owner: AccountId) -> bool {
		match &self.dormant_redemption_target {
			Some(existing) if existing != &owner => false,
			_ => {
				self.dormant_redemption_target = Some(owner);
				true
			},
		}
	}
}

impl<
		AccountId,
		Balance: FixedPointOperand + Saturating + CheckedAdd + CheckedSub + One + Default,
	> BranchState<AccountId, Balance>
{
	/// Replace one vault's complete contribution to the branch totals.
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
		let principal = shifted(self.debt.principal, before.principal, after.principal)?;
		let minted_interest = shifted(self.debt.minted_interest, before.interest, after.interest)?;
		let accrual_rate =
			shifted(self.debt.accrual_rate, before.accrual_rate, after.accrual_rate)?;
		let stake = shifted(self.stakes.total, before.stake, after.stake)?;
		let stake_accrual_rate =
			shifted(self.stakes.accrual_rate, before.stake_accrual_rate, after.stake_accrual_rate)?;
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
		let stake = stablecoin_primitives::mul_div_floor(
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
		let rate = |current: U256| {
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
	///
	/// Returns `None` on overflow, and when no stake exists to receive the residual. Callers rule
	/// the second case out first, so `None` reaching them means overflow.
	pub(crate) fn record_redistribution(
		&mut self,
		redistributed: DebtCollateral<Balance>,
		now: Millis,
	) -> Option<()> {
		if self.stakes.total.is_zero() {
			return None;
		}
		let principal_per_stake =
			FixedU128::checked_from_rational(redistributed.debt, self.stakes.total)?;
		let collateral_per_stake =
			FixedU128::checked_from_rational(redistributed.collateral, self.stakes.total)?;
		// Posted from the stored increment, which is what vaults claim against, and rounded down
		// so the projection never runs ahead of what vaults will owe.
		let increment = U256::from(principal_per_stake.into_inner());
		let posted_accrual_rate =
			math::scale_wide(self.stakes.accrual_rate, increment, math::Direction::Down)?;
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
	/// Interest is paid before principal, so a live branch cannot have interest without principal.
	/// Use [`Self::is_removable`] to also check debt-free vaults, collateral, and stake.
	pub(crate) fn is_empty_of_liability(&self) -> bool {
		self.debt.principal.is_zero() && self.debt.pending_redistribution_principal.is_zero()
	}

	/// Returns whether the branch has no debt, stake, or collateral.
	pub(crate) fn is_removable(&self) -> bool {
		self.debt.outstanding().is_zero() &&
			self.debt.interest_ledger_settled() &&
			self.debt.aggregate_interest_remainder == 0 &&
			self.debt.interest_minted_ahead.is_zero() &&
			self.stakes.total.is_zero() &&
			self.pending_redistribution_collateral.is_zero() &&
			self.total_collateral.is_zero()
	}
}

/// Update to one branch parameter.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BranchConfigUpdate<Balance> {
	/// Sets the liquidation and final recovery ratio.
	///
	/// Applies to every existing vault at once: raising it makes vaults below the new ratio
	/// liquidatable.
	MinimumCollateralizationRatio(FixedU128),
	/// Sets the ratio required after borrowing or withdrawing collateral.
	///
	/// Existing vaults below the new ratio stay open and keep their status. Only their next
	/// borrow or collateral withdrawal must reach it.
	InitialCollateralizationRatio(FixedU128),
	/// Sets the branch safety-mode ratio.
	SafetyCollateralizationRatio(FixedU128),
	/// Sets the maximum branch debt.
	DebtCeiling(Balance),
	/// Sets the minimum debt for an active vault.
	///
	/// Existing vaults keep their status. An active vault already below the new minimum stays in
	/// the rate list until a redemption lowers its debt or a full repayment clears it. It then
	/// becomes dormant if collateral remains, or closes if empty. Its partial repayments must
	/// leave at least the new minimum, and its borrows must raise the principal to it.
	MinimumDebt(Balance),
	/// Sets the minimum collateral required to open a vault.
	///
	/// Only opening checks it: existing vaults below the new minimum are unaffected.
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
	/// Sets the extra collateral assigned to redistributed debt.
	RedistributionPenalty(Permill),
}

impl<Balance: PartialOrd + Copy> BranchConfigUpdate<Balance> {
	/// Applies this update to `config`.
	pub(crate) const fn apply_to(self, config: &mut BranchConfig<Balance>) {
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
		}
	}

	/// Returns the same parameter carrying the value `config` holds for it.
	///
	/// Read before [`Self::apply_to`], this is the value the update replaces.
	pub(crate) const fn current_in(self, config: &BranchConfig<Balance>) -> Self {
		match self {
			Self::MinimumCollateralizationRatio(_) => {
				Self::MinimumCollateralizationRatio(config.minimum_collateralization_ratio)
			},
			Self::InitialCollateralizationRatio(_) => {
				Self::InitialCollateralizationRatio(config.initial_collateralization_ratio)
			},
			Self::SafetyCollateralizationRatio(_) => {
				Self::SafetyCollateralizationRatio(config.safety_collateralization_ratio)
			},
			Self::DebtCeiling(_) => Self::DebtCeiling(config.debt_ceiling),
			Self::MinimumDebt(_) => Self::MinimumDebt(config.minimum_debt),
			Self::MinimumCollateral(_) => Self::MinimumCollateral(config.minimum_collateral),
			Self::BorrowRateBounds { .. } => Self::BorrowRateBounds {
				min: config.minimum_borrow_rate,
				max: config.maximum_borrow_rate,
			},
			Self::UpfrontFeePeriod(_) => Self::UpfrontFeePeriod(config.upfront_fee_period),
			Self::RateAdjustmentCooldown(_) => {
				Self::RateAdjustmentCooldown(config.rate_adjustment_cooldown)
			},
			Self::RedistributionPenalty(_) => {
				Self::RedistributionPenalty(config.redistribution_penalty)
			},
		}
	}

	/// Returns the administrator role required for this update.
	pub(crate) const fn required_level(&self) -> AdminLevel {
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
			Self::RedistributionPenalty(_) => AdminLevel::Full,
		}
	}

	/// Returns whether this update only reduces risk.
	///
	/// A defensive update raises ratio limits, lowers the debt limit, or narrows the rate range.
	/// Nothing else is defensive: an update that needs a full administrator changes what the
	/// branch charges or seizes, which is a policy decision rather than a risk reduction.
	/// [`required_level`] turns an emergency administrator away from those before this is asked,
	/// so the `false` below is what keeps the answer right if that order ever changes.
	///
	/// [`required_level`]: BranchConfigUpdate::required_level
	pub(crate) fn is_defensive(&self, config: &BranchConfig<Balance>) -> bool {
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
			Self::RedistributionPenalty(_) => false,
		}
	}
}

/// Limits for branch configuration.
#[derive(Encode, TypeInfo)]
pub struct BranchConfigBounds {
	/// Lowest allowed liquidation and final recovery ratio.
	pub min_minimum_collateralization_ratio: FixedU128,
	/// Lowest allowed ratio after borrowing or withdrawing collateral.
	pub min_initial_collateralization_ratio: FixedU128,
	/// Lowest allowed branch safety-mode ratio.
	pub min_safety_collateralization_ratio: FixedU128,
	/// Highest allowed annual rate.
	pub max_borrow_rate: FixedU128,
}

/// A runtime limit in [`BranchConfigBounds`] that a branch configuration breaches.
///
/// Reported through `Error::ConfigOutsideEnvelope`.
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
	/// Checks `config` is within every limit, returning the first one it breaches.
	pub(crate) fn ensure_within<Balance>(
		&self,
		config: &BranchConfig<Balance>,
	) -> Result<(), BoundViolation> {
		ensure!(
			config.minimum_collateralization_ratio >= self.min_minimum_collateralization_ratio,
			BoundViolation::MinimumCollateralizationRatioTooLow
		);
		ensure!(
			config.initial_collateralization_ratio >= self.min_initial_collateralization_ratio,
			BoundViolation::InitialCollateralizationRatioTooLow
		);
		ensure!(
			config.safety_collateralization_ratio >= self.min_safety_collateralization_ratio,
			BoundViolation::SafetyCollateralizationRatioTooLow
		);
		ensure!(
			config.maximum_borrow_rate <= self.max_borrow_rate,
			BoundViolation::BorrowRateTooHigh
		);
		Ok(())
	}
}

/// Administrator role for one branch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AdminLevel {
	/// May manage all branch settings and lifecycle actions.
	Full,
	/// May freeze the branch or reduce risk.
	Emergency,
}

impl AdminLevel {
	/// Returns whether this role may act where `required` is needed.
	///
	/// A full administrator holds every emergency power as well.
	pub(crate) const fn satisfies(self, required: Self) -> bool {
		match (self, required) {
			(Self::Full, Self::Full | Self::Emergency) | (Self::Emergency, Self::Emergency) => true,
			(Self::Emergency, Self::Full) => false,
		}
	}

	/// Returns whether this role may only apply updates that reduce risk.
	pub(crate) const fn may_only_reduce_risk(self) -> bool {
		match self {
			Self::Full => false,
			Self::Emergency => true,
		}
	}
}

/// Administrator accounts for one branch.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
)]
pub struct BranchAdmins<AccountId> {
	/// Account with full control of the branch.
	pub full_admin: AccountId,
	/// Account allowed to freeze the branch or reduce risk.
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

/// Complete record for one registered branch.
///
/// It is created and removed as one record.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo)]
pub struct Branch<AccountId, Balance, Consideration> {
	/// Branch risk parameters.
	pub config: BranchConfig<Balance>,
	/// Branch accounting state.
	pub state: BranchState<AccountId, Balance>,
	/// Branch administrator accounts.
	pub admins: BranchAdmins<AccountId>,
	/// Creator and refundable deposit, if one was charged.
	pub deposit: Option<(AccountId, Consideration)>,
}

/// Moves `current` from `old` to `new` without going below zero in between.
fn shifted<N: EnsureAdd + EnsureSub>(current: N, old: N, new: N) -> Result<N, ArithmeticError> {
	current.ensure_sub(old)?.ensure_add(new)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn make_branch_state(principal: u128, accrual_rate: U256) -> BranchState<u64, u128> {
		BranchState {
			debt: BranchDebt { principal, accrual_rate, ..BranchDebt::default() },
			..BranchState::fresh(0)
		}
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
