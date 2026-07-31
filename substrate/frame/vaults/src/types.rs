//! Types stored or exposed by the Vaults pallet.

use crate::{math, Millis};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame::{
	arithmetic::{
		ArithmeticError, AtLeast32BitUnsigned, CheckedAdd, CheckedSub, FixedPointNumber,
		FixedPointOperand, FixedU128, One, Permill, Saturating, Zero,
	},
	deps::{
		frame_support::PalletError,
		sp_core::{U256, U512},
	},
};
use pusd_primitives::MILLIS_PER_YEAR;
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
	/// Smallest direct keeper contribution; prevents dust burns.
	pub minimum_jit_contribution: Balance,
	/// Extra collateral assigned to redistributed debt.
	///
	/// Final recovery also uses this as its bonus cap.
	pub redistribution_penalty: Permill,
}

/// Keeper-supplied terms for the direct contribution to one liquidation.
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
pub struct JitTerms<Balance> {
	/// Maximum stable assets the keeper allows the call to burn for a direct contribution.
	///
	/// Zero disables the contribution.
	pub max_stable: Balance,
	/// Minimum collateral allocated to an executed JIT slice, excluding the keeper reward.
	///
	/// This absolute floor is not scaled down for a partial JIT execution. Keepers should set it
	/// for the smallest execution they would accept.
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
	/// Cumulative principal assigned per unit of stake.
	pub principal_per_stake: FixedU128,
	/// Cumulative pending weight assigned per unit of rate-weighted stake.
	pub weight_per_weighted_stake: FixedU128,
	/// Cumulative `weight_per_weighted_stake * liquidation_interest_time` anchor.
	pub weight_time_per_weighted_stake: WeightTime,
}

#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WeightTime {
	pub low: u128,
	pub high: u128,
}

impl WeightTime {
	pub(crate) fn from_wide(value: U256) -> Self {
		Self { low: value.low_u128(), high: (value >> 128).low_u128() }
	}

	pub(crate) fn to_wide(self) -> U256 {
		(U256::from(self.high) << 128) | U256::from(self.low)
	}

	pub(crate) fn checked_add(self, other: Self) -> Option<Self> {
		self.to_wide().checked_add(other.to_wide()).map(Self::from_wide)
	}

	pub(crate) fn checked_sub(self, other: Self) -> Option<Self> {
		self.to_wide().checked_sub(other.to_wide()).map(Self::from_wide)
	}

	pub(crate) fn is_zero(self) -> bool {
		self.low == 0 && self.high == 0
	}
}

/// Groups pending redistribution weight with its time anchor.
///
/// Both values must move together to keep the claimed and remaining time anchors valid.
#[derive(Clone, Copy)]
pub(crate) struct RedistributionAttribution<Balance> {
	weight: InterestWeight<Balance>,
	weight_time: WeightTime,
}

impl<Balance: FixedPointOperand + CheckedAdd + CheckedSub + One>
	RedistributionAttribution<Balance>
{
	pub(crate) fn zero() -> Self {
		Self { weight: InterestWeight::zero(), weight_time: WeightTime::default() }
	}

	pub(crate) fn is_zero(&self) -> bool {
		self.weight.is_zero()
	}

	/// Returns a claim that gives both pool parts valid time anchors at `now`.
	///
	/// The claim conserves weight and weight-time. A claim for all weight receives the exact pool
	/// complement.
	pub(crate) fn claim(
		candidate: InterestWeight<Balance>,
		desired_weight_time: WeightTime,
		available_weight: InterestWeight<Balance>,
		available_weight_time: WeightTime,
		now: Millis,
	) -> Result<Self, ArithmeticError> {
		let available_raw = available_weight.raw();
		let available_time = available_weight_time.to_wide();
		let available_time_cap =
			available_raw.checked_mul(U256::from(now)).ok_or(ArithmeticError::Overflow)?;
		if available_time > available_time_cap {
			return Err(ArithmeticError::Underflow);
		}

		let candidate_raw = candidate.raw();
		if candidate_raw >= available_raw {
			return Ok(Self { weight: available_weight, weight_time: available_weight_time });
		}

		let remaining_raw =
			available_raw.checked_sub(candidate_raw).ok_or(ArithmeticError::Underflow)?;
		let remaining_time_cap =
			remaining_raw.checked_mul(U256::from(now)).ok_or(ArithmeticError::Overflow)?;
		let minimum_claim_time = if available_time > remaining_time_cap {
			available_time
				.checked_sub(remaining_time_cap)
				.ok_or(ArithmeticError::Underflow)?
		} else {
			U256::zero()
		};
		let claim_time_cap = candidate_raw
			.checked_mul(U256::from(now))
			.ok_or(ArithmeticError::Overflow)?
			.min(available_time);
		if minimum_claim_time > claim_time_cap {
			return Err(ArithmeticError::Underflow);
		}

		let desired = desired_weight_time.to_wide();
		let claimed_time = desired.max(minimum_claim_time).min(claim_time_cap);
		Ok(Self { weight: candidate, weight_time: WeightTime::from_wide(claimed_time) })
	}

	pub(crate) fn pending_interest(
		&self,
		now: Millis,
	) -> Result<PendingInterest<Balance>, ArithmeticError> {
		PendingInterest::from_weight_time(&self.weight, self.weight_time, now)
	}
}

/// Stores division residue between redistributions.
///
/// These values are not debt or collateral. They preserve subunit allocation precision.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RedistributionCarry {
	pub principal: u128,
	pub collateral: u128,
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
	/// Sub-unit interest carried across touches, below [`PendingInterest::DENOMINATOR`].
	///
	/// A touch realizes whole stablecoin units. A terminal settlement charges one unit for a
	/// nonzero remainder.
	pub interest_remainder: u128,
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
	weighted_principal: InterestWeight<Balance>,
	stake: Balance,
	weighted_stake: InterestWeight<Balance>,
	eligible_collateral: Balance,
}

impl<Balance: FixedPointOperand + CheckedAdd + CheckedSub + One> VaultContribution<Balance> {
	fn zero() -> Self {
		Self {
			principal: Zero::zero(),
			interest: Zero::zero(),
			weighted_principal: InterestWeight::zero(),
			stake: Zero::zero(),
			weighted_stake: InterestWeight::zero(),
			eligible_collateral: Zero::zero(),
		}
	}

	fn of(vault: Option<&Vault<Balance>>) -> Result<Self, ArithmeticError> {
		let Some(vault) = vault else { return Ok(Self::zero()) };
		Ok(Self {
			principal: vault.debt.principal,
			interest: vault.debt.interest,
			weighted_principal: InterestWeight::from_principal_rate(
				vault.debt.principal,
				vault.annual_rate,
			)
			.ok_or(ArithmeticError::Overflow)?,
			stake: vault.redistribution_stake,
			weighted_stake: InterestWeight::from_principal_rate(
				vault.redistribution_stake,
				vault.annual_rate,
			)
			.ok_or(ArithmeticError::Overflow)?,
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

impl<Balance: Zero + One> Vault<Balance> {
	/// Returns the protocol-favoring unit due on a terminal settlement.
	///
	/// Zero when no fraction is carried.
	pub fn terminal_interest_charge(&self) -> Balance {
		if self.interest_remainder == 0 {
			Balance::zero()
		} else {
			Balance::one()
		}
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
			terminal_interest_charge: self.terminal_interest_charge(),
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
	/// Rate-weighted principal used for debt projections.
	pub weighted_principal: InterestWeight<Balance>,
	/// Pending redistribution's subset of [`Self::weighted_principal`].
	pub pending_redistribution_weight: InterestWeight<Balance>,
	/// Market interest time of the last aggregate interest update.
	pub last_interest_time: Millis,
	/// Sub-unit aggregate interest carried across refreshes, below
	/// [`PendingInterest::DENOMINATOR`].
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
	///
	/// This excludes `aggregate_interest_remainder`, which has no owner after the branch becomes
	/// empty.
	pub(crate) fn interest_ledger_settled(&self) -> bool {
		self.pending_interest_attribution.is_zero() &&
			self.weighted_principal.is_zero() &&
			self.pending_redistribution_weight.is_zero()
	}
}

impl<Balance: FixedPointOperand + Ord + Saturating + CheckedAdd + CheckedSub> BranchDebt<Balance> {
	/// Attributes `amount` and returns the part that requires new issuance.
	///
	/// This preserves `minted_interest == Σ vault interest + pending_interest_attribution`.
	pub(crate) fn attribute_interest(&mut self, amount: Balance) -> Option<Balance> {
		let covered = amount.min(self.pending_interest_attribution);
		let uncovered = amount.saturating_sub(covered);
		self.pending_interest_attribution =
			self.pending_interest_attribution.checked_sub(&covered)?;
		self.minted_interest = self.minted_interest.checked_add(&uncovered)?;
		Some(uncovered)
	}
}

/// An exact interest amount split at the shared interest denominator.
///
/// Represents `principal × rate-inner × millis` as `interest * DENOMINATOR + remainder`.
///
/// The split representation permits exact aggregate addition and subtraction within the stored
/// integer types.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug, Default)]
pub struct PendingInterest<Balance> {
	/// Whole interest units: `numerator / DENOMINATOR`, rounded down.
	pub interest: Balance,
	/// Sub-unit residue: `numerator % DENOMINATOR`.
	pub remainder: u128,
}

/// An annual rate-weighted principal with its fixed-point fraction retained.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct InterestWeight<Balance> {
	/// Whole weight units: `Σ principal · rate / FixedU128::DIV`, rounded down.
	pub whole: Balance,
	/// Sub-unit residue below [`FixedU128::DIV`].
	pub remainder: u128,
}

/// `whole * denominator + remainder`: the wide numerator one split limb represents.
fn join_wide(whole: u128, remainder: u128, denominator: u128) -> U256 {
	U256::from(whole) * U256::from(denominator) + U256::from(remainder)
}

/// Splits a wide numerator at `denominator` into whole `Balance` units and a
/// sub-unit residue. `None` when the whole part overflows `Balance`.
fn split_wide<Balance: FixedPointOperand>(
	numerator: U256,
	denominator: u128,
) -> Option<(Balance, u128)> {
	let (whole, remainder) = numerator.div_mod(U256::from(denominator));
	if whole > U256::from(u128::MAX) {
		return None;
	}
	Some((Balance::try_from(whole.low_u128()).ok()?, remainder.low_u128()))
}

/// Adds two `(whole, remainder)` limbs at `denominator`, carrying into the whole part.
fn limb_add<Balance: CheckedAdd + One>(
	lhs: (&Balance, u128),
	rhs: (&Balance, u128),
	denominator: u128,
) -> Option<(Balance, u128)> {
	let mut whole = lhs.0.checked_add(rhs.0)?;
	let mut remainder = lhs.1.checked_add(rhs.1)?;
	if remainder >= denominator {
		remainder -= denominator;
		whole = whole.checked_add(&Balance::one())?;
	}
	Some((whole, remainder))
}

/// Subtracts two `(whole, remainder)` limbs at `denominator`, borrowing from the whole part.
fn limb_sub<Balance: CheckedSub + One>(
	lhs: (&Balance, u128),
	rhs: (&Balance, u128),
	denominator: u128,
) -> Option<(Balance, u128)> {
	let mut whole = lhs.0.checked_sub(rhs.0)?;
	let remainder = if lhs.1 >= rhs.1 {
		lhs.1 - rhs.1
	} else {
		whole = whole.checked_sub(&Balance::one())?;
		lhs.1 + denominator - rhs.1
	};
	Some((whole, remainder))
}

impl<Balance: Zero> InterestWeight<Balance> {
	pub fn zero() -> Self {
		Self { whole: Balance::zero(), remainder: 0 }
	}

	pub fn is_zero(&self) -> bool {
		self.whole.is_zero() && self.remainder == 0
	}
}

impl<Balance: FixedPointOperand + CheckedAdd + CheckedSub + One> InterestWeight<Balance> {
	pub(crate) fn raw(&self) -> U256 {
		join_wide(self.whole.unique_saturated_into(), self.remainder, FixedU128::DIV)
	}

	pub(crate) fn from_raw(raw: U256) -> Option<Self> {
		let (whole, remainder) = split_wide(raw, FixedU128::DIV)?;
		Some(Self { whole, remainder })
	}

	pub(crate) fn from_principal_rate(principal: Balance, rate: FixedU128) -> Option<Self> {
		let product = U256::from(principal.unique_saturated_into()) * U256::from(rate.into_inner());
		let (whole, remainder) = split_wide(product, FixedU128::DIV)?;
		Some(Self { whole, remainder })
	}

	pub(crate) fn checked_add(&self, other: &Self) -> Option<Self> {
		let (whole, remainder) = limb_add(
			(&self.whole, self.remainder),
			(&other.whole, other.remainder),
			FixedU128::DIV,
		)?;
		Some(Self { whole, remainder })
	}

	pub(crate) fn checked_sub(&self, other: &Self) -> Option<Self> {
		let (whole, remainder) = limb_sub(
			(&self.whole, self.remainder),
			(&other.whole, other.remainder),
			FixedU128::DIV,
		)?;
		Some(Self { whole, remainder })
	}

	/// Rate-weighted principal posted for one redistribution, rounded once upward.
	pub(crate) fn posted_redistribution(
		principal: Balance,
		weighted_stake: &Self,
		total_stake: Balance,
	) -> Option<Self> {
		let denominator = total_stake.unique_saturated_into();
		if denominator == 0 || weighted_stake.is_zero() {
			return None;
		}
		let numerator = U512::from(principal.unique_saturated_into())
			.checked_mul(U512::from(weighted_stake.raw()))?;
		let (quotient, remainder) = numerator.div_mod(U512::from(denominator));
		let ceiled =
			if remainder.is_zero() { quotient } else { quotient.checked_add(U512::one())? };
		Self::from_raw(U256::try_from(ceiled).ok()?)
	}

	/// Fixed-point allocation ratio for pending weight over rate-weighted stake.
	pub(crate) fn redistribution_ratio(&self, total_weighted_stake: &Self) -> Option<FixedU128> {
		let denominator = total_weighted_stake.raw();
		if denominator.is_zero() {
			return None;
		}
		let numerator = U512::from(self.raw()).checked_mul(U512::from(FixedU128::DIV))?;
		let ratio = numerator / U512::from(denominator);
		if ratio > U512::from(u128::MAX) {
			return None;
		}
		Some(FixedU128::from_inner(ratio.low_u128()))
	}

	/// Floors one accumulator ratio against this rate-weighted stake.
	pub(crate) fn apply_redistribution_ratio(&self, ratio: FixedU128) -> Option<Self> {
		let numerator = U512::from(self.raw()).checked_mul(U512::from(ratio.into_inner()))?;
		Self::from_raw(U256::try_from(numerator / U512::from(FixedU128::DIV)).ok()?)
	}

	/// Floors one cumulative time ratio against this rate-weighted stake.
	pub(crate) fn apply_weight_time_ratio(&self, ratio: WeightTime) -> Option<WeightTime> {
		let numerator = U512::from(self.raw()).checked_mul(U512::from(ratio.to_wide()))?;
		Some(WeightTime::from_wide(U256::try_from(numerator / U512::from(FixedU128::DIV)).ok()?))
	}

	/// `self − before + after`: swaps one contribution inside an aggregate.
	pub(crate) fn shifted(&self, before: &Self, after: &Self) -> Option<Self> {
		self.checked_sub(before)?.checked_add(after)
	}
}

impl<Balance: FixedPointOperand + CheckedAdd + CheckedSub + One> PendingInterest<Balance> {
	/// The shared interest denominator: one whole unit of interest per year of
	/// one whole unit of rate-weighted principal.
	pub const DENOMINATOR: u128 = FixedU128::DIV * MILLIS_PER_YEAR as u128;

	/// The exact `weight * elapsed` numerator, in split form.
	///
	/// Returns `None` when the divided product overflows `Balance`.
	pub(crate) fn from_interest_weight(
		weight: InterestWeight<Balance>,
		elapsed: Millis,
	) -> Option<Self> {
		let numerator =
			join_wide(weight.whole.unique_saturated_into(), weight.remainder, FixedU128::DIV) *
				U256::from(elapsed);
		let (interest, remainder) = split_wide(numerator, Self::DENOMINATOR)?;
		Some(Self { interest, remainder })
	}

	pub(crate) fn from_principal_rate_millis(
		principal: Balance,
		rate: FixedU128,
		elapsed: Millis,
	) -> Option<Self> {
		let numerator = (U256::from(principal.unique_saturated_into()) *
			U256::from(rate.into_inner()))
		.checked_mul(U256::from(elapsed))?;
		let (interest, remainder) = split_wide(numerator, Self::DENOMINATOR)?;
		Some(Self { interest, remainder })
	}

	/// Interest accrued by a pending weight since its liquidation-time anchor.
	pub(crate) fn from_weight_time(
		weight: &InterestWeight<Balance>,
		weight_time: WeightTime,
		now: Millis,
	) -> Result<Self, ArithmeticError> {
		let numerator = weight
			.raw()
			.checked_mul(U256::from(now))
			.ok_or(ArithmeticError::Overflow)?
			.checked_sub(weight_time.to_wide())
			.ok_or(ArithmeticError::Underflow)?;
		let (interest, remainder) =
			split_wide(numerator, Self::DENOMINATOR).ok_or(ArithmeticError::Overflow)?;
		Ok(Self { interest, remainder })
	}

	pub fn checked_add(&self, other: &Self) -> Option<Self> {
		let (interest, remainder) = limb_add(
			(&self.interest, self.remainder),
			(&other.interest, other.remainder),
			Self::DENOMINATOR,
		)?;
		Some(Self { interest, remainder })
	}

	pub fn checked_sub(&self, other: &Self) -> Option<Self> {
		let (interest, remainder) = limb_sub(
			(&self.interest, self.remainder),
			(&other.interest, other.remainder),
			Self::DENOMINATOR,
		)?;
		Some(Self { interest, remainder })
	}

	/// Whole interest units, rounded up. `None` when the round-up overflows.
	pub fn ceil(&self) -> Option<Balance> {
		if self.remainder == 0 {
			Some(self.interest)
		} else {
			self.interest.checked_add(&Balance::one())
		}
	}
}

impl<Balance: Zero> PendingInterest<Balance> {
	/// A carry-only amount: no whole units, just a sub-unit residue.
	pub(crate) fn from_remainder(remainder: u128) -> Self {
		Self { interest: Balance::zero(), remainder }
	}

	pub fn is_zero(&self) -> bool {
		self.interest.is_zero() && self.remainder == 0
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
	/// Σ `weighted_principal` over the coin's non-frozen markets.
	pub active_weighted_principal: InterestWeight<Balance>,
	/// Interest accrued up to `last_update` but not yet minted anywhere.
	pub pending_interest: PendingInterest<Balance>,
	/// Time the projection was last advanced.
	pub last_update: Millis,
}

impl<Balance: Zero> StablecoinDebtState<Balance> {
	pub fn is_empty(&self) -> bool {
		self.outstanding.is_zero() &&
			self.active_weighted_principal.is_zero() &&
			self.pending_interest.is_zero()
	}
}

/// Redistribution stake totals for one market.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug, Default)]
pub struct RedistributionStakeTotals<Balance> {
	/// Total stake of eligible vaults.
	pub total: Balance,
	/// Exact sum of each eligible vault's `stake * annual_rate`.
	pub weighted: InterestWeight<Balance>,
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
	/// Sub-unit division phases carried into the next redistribution.
	pub redistribution_carry: RedistributionCarry,
	/// Redistributed collateral held by the market account until vaults materialize it.
	pub pending_redistribution_collateral: Balance,
	/// Liquidation-time anchor for pending rate-weighted principal.
	pub pending_redistribution_weight_time: WeightTime,
	/// Number of vault rows in this market.
	pub vault_count: u32,
	/// Wall-clock origin used to calculate market interest time.
	///
	/// Frozen periods move this value forward so interest does not accrue while frozen.
	pub interest_epoch: Millis,
	/// Dormant vault that must be redeemed before the rate list.
	pub dormant_redemption_target: Option<AccountId>,
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
			redistribution_carry: RedistributionCarry::default(),
			pending_redistribution_collateral: Balance::zero(),
			pending_redistribution_weight_time: WeightTime::default(),
			vault_count: 0,
			interest_epoch: now,
			dormant_redemption_target: None,
			frozen: None,
		}
	}
}

impl<AccountId, Balance> BranchState<AccountId, Balance> {
	/// Returns whether the market is frozen.
	pub const fn is_frozen(&self) -> bool {
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

		let shifted_weight = |current: InterestWeight<Balance>| {
			current
				.shifted(&before.weighted_principal, &after.weighted_principal)
				.ok_or(ArithmeticError::Underflow)
		};
		let shifted_stake_weight = |current: InterestWeight<Balance>| {
			current
				.shifted(&before.weighted_stake, &after.weighted_stake)
				.ok_or(ArithmeticError::Underflow)
		};

		let principal = shifted(self.debt.principal, before.principal, after.principal)?;
		let minted_interest = shifted(self.debt.minted_interest, before.interest, after.interest)?;
		let weighted_principal = shifted_weight(self.debt.weighted_principal)?;
		let stake = shifted(self.stakes.total, before.stake, after.stake)?;
		let weighted_stake = shifted_stake_weight(self.stakes.weighted)?;
		let collateral_basis = shifted(
			self.stakes.collateral_basis,
			before.eligible_collateral,
			after.eligible_collateral,
		)?;
		self.debt.principal = principal;
		self.debt.minted_interest = minted_interest;
		self.debt.weighted_principal = weighted_principal;
		self.stakes.total = stake;
		self.stakes.weighted = weighted_stake;
		self.stakes.collateral_basis = collateral_basis;
		Ok(())
	}

	/// Recomputes one vault's stake from the latest redistribution snapshots.
	///
	/// Snapshot correction makes redistribution weights independent of collateral touch order.
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
		attribution: RedistributionAttribution<Balance>,
	) -> Result<(), ArithmeticError> {
		self.debt.pending_redistribution_principal = self
			.debt
			.pending_redistribution_principal
			.checked_sub(&redistribution.debt)
			.ok_or(ArithmeticError::Underflow)?;
		self.pending_redistribution_collateral = self
			.pending_redistribution_collateral
			.checked_sub(&redistribution.collateral)
			.ok_or(ArithmeticError::Underflow)?;
		self.stakes.collateral_basis = self
			.stakes
			.collateral_basis
			.checked_sub(&redistribution.collateral)
			.ok_or(ArithmeticError::Underflow)?;
		self.debt.pending_redistribution_weight = self
			.debt
			.pending_redistribution_weight
			.checked_sub(&attribution.weight)
			.ok_or(ArithmeticError::Underflow)?;
		self.pending_redistribution_weight_time = self
			.pending_redistribution_weight_time
			.checked_sub(attribution.weight_time)
			.ok_or(ArithmeticError::Underflow)?;
		self.debt.weighted_principal = self
			.debt
			.weighted_principal
			.checked_sub(&attribution.weight)
			.ok_or(ArithmeticError::Underflow)?;
		Ok(())
	}

	/// Records one liquidation residual in the per-stake accumulators.
	///
	/// Pending pools retain the complete principal and collateral. The final stake bearer receives
	/// the exact pool complement.
	pub(crate) fn record_redistribution(
		&mut self,
		redistributed: DebtCollateral<Balance>,
		now: Millis,
	) -> Option<()> {
		if self.stakes.total.is_zero() {
			return None;
		}
		let posted_weight = InterestWeight::posted_redistribution(
			redistributed.debt,
			&self.stakes.weighted,
			self.stakes.total,
		)?;
		let (principal_per_stake, principal_carry) = math::redistribution_per_stake_with_carry(
			redistributed.debt,
			self.stakes.total,
			self.redistribution_carry.principal,
		)?;
		let (collateral_per_stake, collateral_carry) = math::redistribution_per_stake_with_carry(
			redistributed.collateral,
			self.stakes.total,
			self.redistribution_carry.collateral,
		)?;
		let weight_per_weighted_stake =
			posted_weight.redistribution_ratio(&self.stakes.weighted)?;
		let interest_time = self.interest_time(now);
		let weight_time_per_weighted_stake = WeightTime::from_wide(
			U256::from(weight_per_weighted_stake.into_inner())
				.checked_mul(U256::from(interest_time))?,
		);
		let posted_weight_time =
			WeightTime::from_wide(posted_weight.raw().checked_mul(U256::from(interest_time))?);

		self.redistribution = RedistributionAccumulators {
			collateral_per_stake: self
				.redistribution
				.collateral_per_stake
				.checked_add(&collateral_per_stake)?,
			principal_per_stake: self
				.redistribution
				.principal_per_stake
				.checked_add(&principal_per_stake)?,
			weight_per_weighted_stake: self
				.redistribution
				.weight_per_weighted_stake
				.checked_add(&weight_per_weighted_stake)?,
			weight_time_per_weighted_stake: self
				.redistribution
				.weight_time_per_weighted_stake
				.checked_add(weight_time_per_weighted_stake)?,
		};
		self.redistribution_carry =
			RedistributionCarry { principal: principal_carry, collateral: collateral_carry };
		self.debt.pending_redistribution_principal =
			self.debt.pending_redistribution_principal.checked_add(&redistributed.debt)?;
		self.pending_redistribution_collateral =
			self.pending_redistribution_collateral.checked_add(&redistributed.collateral)?;
		self.stakes.collateral_basis =
			self.stakes.collateral_basis.checked_add(&redistributed.collateral)?;
		self.debt.pending_redistribution_weight =
			self.debt.pending_redistribution_weight.checked_add(&posted_weight)?;
		self.pending_redistribution_weight_time =
			self.pending_redistribution_weight_time.checked_add(posted_weight_time)?;
		self.debt.weighted_principal = self.debt.weighted_principal.checked_add(&posted_weight)?;
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
			self.stakes.total.is_zero() &&
			self.pending_redistribution_collateral.is_zero() &&
			self.pending_redistribution_weight_time.is_zero() &&
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

	fn make_branch_state(principal: u128, weighted: u128) -> BranchState<u64, u128> {
		BranchState {
			total_collateral: 0,
			debt: BranchDebt {
				principal,
				pending_redistribution_principal: 0,
				minted_interest: 0,
				pending_interest_attribution: 0,
				weighted_principal: InterestWeight { whole: weighted, remainder: 0 },
				pending_redistribution_weight: InterestWeight::zero(),
				last_interest_time: 0,
				aggregate_interest_remainder: 0,
			},
			stakes: RedistributionStakeTotals::default(),
			redistribution: RedistributionAccumulators::default(),
			redistribution_carry: RedistributionCarry::default(),
			pending_redistribution_collateral: 0,
			pending_redistribution_weight_time: WeightTime::default(),
			vault_count: 0,
			interest_epoch: 0,
			dormant_redemption_target: None,
			frozen: None,
		}
	}

	#[test]
	fn replace_vault_swaps_full_contribution() {
		// Subtraction must retain the fractional weight.
		let rate = FixedU128::from_rational(3u128, 10u128);
		let mut state = make_branch_state(10, 3);
		let before = Vault {
			collateral: 0,
			debt: DebtBreakdown { interest: 0, principal: 10 },
			annual_rate: rate,
			last_interest_time: 0,
			interest_remainder: 0,
			last_rate_update: 0,
			redistribution_stake: 0,
			redistribution_checkpoint: RedistributionAccumulators::default(),
		};
		let mut after = before.clone();
		after.debt.principal = 9;
		state.replace_vault(Some(&before), Some(&after)).unwrap();
		assert_eq!(state.debt.principal, 9);
		assert_eq!(
			state.debt.weighted_principal,
			InterestWeight { whole: 2, remainder: 7 * (FixedU128::DIV / 10) }
		);
	}

	#[test]
	fn replace_vault_full_payoff_clears_contribution() {
		let rate = FixedU128::from_rational(3u128, 10u128);
		let mut state = make_branch_state(10, 3);
		let before = Vault {
			collateral: 0,
			debt: DebtBreakdown { interest: 0, principal: 10 },
			annual_rate: rate,
			last_interest_time: 0,
			interest_remainder: 0,
			last_rate_update: 0,
			redistribution_stake: 0,
			redistribution_checkpoint: RedistributionAccumulators::default(),
		};
		let mut after = before.clone();
		after.debt.principal = 0;
		state.replace_vault(Some(&before), Some(&after)).unwrap();
		assert_eq!(state.debt.principal, 0);
		assert_eq!(state.debt.weighted_principal, InterestWeight { whole: 0, remainder: 0 });
	}

	#[test]
	fn replace_vault_rejects_inconsistent_preimage_without_partial_update() {
		let rate = FixedU128::from_rational(3u128, 10u128);
		let mut state = make_branch_state(0, 0);
		let before = Vault {
			collateral: 0,
			debt: DebtBreakdown { interest: 0, principal: 1 },
			annual_rate: rate,
			last_interest_time: 0,
			interest_remainder: 0,
			last_rate_update: 0,
			redistribution_stake: 0,
			redistribution_checkpoint: RedistributionAccumulators::default(),
		};
		let state_before = state.clone();

		assert_eq!(state.replace_vault(Some(&before), None), Err(ArithmeticError::Underflow));
		assert_eq!(state, state_before);
	}

	const YEAR: u64 = MILLIS_PER_YEAR;

	#[test]
	fn pending_interest_split_matches_direct_divmod() {
		// Small enough that `weight * elapsed` fits `u128`, so the split can be
		// checked against the direct computation.
		let weight: u128 = 1_000_000_007;
		let elapsed: u64 = 123_456_789;
		let numerator = weight * u128::from(elapsed);
		let split = PendingInterest::from_interest_weight(
			InterestWeight { whole: weight, remainder: 0 },
			elapsed,
		)
		.unwrap();
		assert_eq!(split.interest, numerator / u128::from(YEAR));
		assert_eq!(split.remainder, (numerator % u128::from(YEAR)) * FixedU128::DIV);
	}

	#[test]
	fn pending_interest_split_exact_beyond_u128_numerator() {
		// `weight * elapsed` overflows `u128`, but a year-multiple weight pins
		// the exact split: `interest = k * elapsed`, `remainder = 0`.
		let k: u128 = u128::MAX / u128::from(YEAR) / 2;
		let weight = k * u128::from(YEAR);
		let elapsed: u64 = 400;
		assert!(weight.checked_mul(u128::from(elapsed)).is_none());
		let split = PendingInterest::from_interest_weight(
			InterestWeight { whole: weight, remainder: 0 },
			elapsed,
		)
		.unwrap();
		assert_eq!(split.interest, k * u128::from(elapsed));
		assert_eq!(split.remainder, 0);
	}

	#[test]
	fn pending_interest_split_overflowing_interest_is_none() {
		// The divided product itself exceeds `u128`.
		let weight = u128::MAX / 2;
		let elapsed = 4 * YEAR;
		assert!(PendingInterest::from_interest_weight(
			InterestWeight { whole: weight, remainder: 0 },
			elapsed
		)
		.is_none());
	}

	#[test]
	fn pending_interest_add_carries_and_sub_borrows() {
		let denominator = FixedU128::DIV * u128::from(YEAR);
		let a = PendingInterest::<u128> { interest: 5, remainder: denominator - 1 };
		let b = PendingInterest { interest: 2, remainder: 3 };
		let sum = a.checked_add(&b).unwrap();
		assert_eq!(sum, PendingInterest { interest: 8, remainder: 2 });
		assert_eq!(sum.checked_sub(&b).unwrap(), a);
		assert_eq!(sum.checked_sub(&a).unwrap(), b);
	}

	#[test]
	fn pending_interest_sub_below_zero_is_none() {
		let a = PendingInterest::<u128> { interest: 1, remainder: 0 };
		let b = PendingInterest::<u128> { interest: 0, remainder: 1 };
		// Same total ordering the aggregate relies on: `a - b` borrows into the
		// interest limb, `b - a` underflows.
		assert_eq!(
			a.checked_sub(&b).unwrap(),
			PendingInterest { interest: 0, remainder: FixedU128::DIV * u128::from(YEAR) - 1 }
		);
		assert!(b.checked_sub(&a).is_none());
	}

	#[test]
	fn pending_interest_ceil_rounds_any_remainder_up() {
		assert_eq!(PendingInterest::<u128> { interest: 7, remainder: 0 }.ceil().unwrap(), 7);
		assert_eq!(PendingInterest::<u128> { interest: 7, remainder: 1 }.ceil().unwrap(), 8);
		assert!(PendingInterest::<u128> { interest: u128::MAX, remainder: 1 }.ceil().is_none());
	}

	#[test]
	fn interest_weight_from_principal_rate_is_exact() {
		// This input has no rate-weight residue.
		let rate = FixedU128::from_rational(47u128, 1_000u128);
		let weight = InterestWeight::<u128>::from_principal_rate(10u128.pow(21), rate).unwrap();
		assert_eq!(weight, InterestWeight { whole: 47 * 10u128.pow(18), remainder: 0 });
		// A one-unit principal has only rate-weight residue.
		let dust = InterestWeight::<u128>::from_principal_rate(1, rate).unwrap();
		assert_eq!(dust, InterestWeight { whole: 0, remainder: 47 * 10u128.pow(15) });
	}

	#[test]
	fn interest_weight_add_sub_round_trips_across_the_carry() {
		let a = InterestWeight::<u128> { whole: 5, remainder: FixedU128::DIV - 1 };
		let b = InterestWeight { whole: 2, remainder: 3 };
		let sum = a.checked_add(&b).unwrap();
		assert_eq!(sum, InterestWeight { whole: 8, remainder: 2 });
		assert_eq!(sum.checked_sub(&b).unwrap(), a);
		assert_eq!(sum.checked_sub(&a).unwrap(), b);
		// Weight subtraction must reject an underflow.
		assert!(b.checked_sub(&a).is_none());
	}
}
