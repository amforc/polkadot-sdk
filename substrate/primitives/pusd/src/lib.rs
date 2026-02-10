// This file is part of Substrate.

// Copyright (C) 2020-2025 Amforc AG.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Shared primitives for pUSD pallets.
//!
//! This crate provides common types and traits used by the vaults, auctions and PSM pallets.
//!
//! # Types
//!
//! - [`CappedValue`]: Balance value with type-safe bounded mutations
//! - [`DebtComponents`]: Breakdown of debt (principal, interest, penalty) for liquidations
//! - [`PaymentBreakdown`]: How a payment is distributed during auction takes
//!
//! # Traits
//!
//! - [`AuctionsHandler`]: Vaults → Auctions (start liquidation auctions)
//! - [`CollateralManager`]: Auctions → Vaults (execute purchases, complete auctions)
//! - [`VaultsInterface`]: PSM → Vaults (query debt ceiling)

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode, MaxEncodedLen};
use core::marker::PhantomData;
use frame_support::{
	pallet_prelude::{DispatchError, DispatchResult},
	traits::{tokens::Balance, Get},
};
use scale_info::TypeInfo;
use sp_runtime::{
	traits::{CheckedAdd, Zero},
	FixedPointOperand, FixedU128, Saturating,
};
/// Debt components for liquidation auctions.
///
/// Represents the breakdown of debt that must be recovered during a liquidation auction.
/// Used when starting auctions and internally by the auctions pallet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, TypeInfo, MaxEncodedLen)]
pub struct DebtComponents<Balance> {
	/// Principal debt - burned to maintain pUSD peg.
	pub principal: Balance,
	/// Accrued interest - burned (was already minted to Insurance Fund during accrual).
	pub interest: Balance,
	/// Liquidation penalty - transferred to Insurance Fund.
	pub penalty: Balance,
}

impl<Balance: Saturating + Copy> DebtComponents<Balance> {
	/// Create new debt components.
	pub const fn new(principal: Balance, interest: Balance, penalty: Balance) -> Self {
		Self { principal, interest, penalty }
	}

	/// Total debt to recover from the auction.
	pub fn total(&self) -> Balance {
		self.principal.saturating_add(self.interest).saturating_add(self.penalty)
	}
}

/// Breakdown of how a payment is distributed during auction `take()`.
///
/// Mirrors [`DebtComponents`] structure - tracks how much of each component was paid.
/// Use the computed methods [`burn()`](Self::burn) and [`insurance_fund()`](Self::insurance_fund)
/// to determine how to process the payment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, TypeInfo, MaxEncodedLen)]
pub struct PaymentBreakdown<Balance> {
	/// Principal portion paid (for `CurrentLiquidationAmount` tracking).
	pub principal_paid: Balance,
	/// Interest portion paid (burned; was already minted to IF during accrual).
	pub interest_paid: Balance,
	/// Penalty portion paid (transferred to Insurance Fund).
	pub penalty_paid: Balance,
}

impl<Balance: Saturating + Copy> PaymentBreakdown<Balance> {
	/// Create new payment breakdown.
	pub const fn new(
		principal_paid: Balance,
		interest_paid: Balance,
		penalty_paid: Balance,
	) -> Self {
		Self { principal_paid, interest_paid, penalty_paid }
	}

	/// Amount to burn (principal + interest).
	///
	/// Interest is burned because it was already minted to the Insurance Fund
	/// when it accrued. Burning it on repayment balances the supply.
	pub fn burn(&self) -> Balance {
		self.principal_paid.saturating_add(self.interest_paid)
	}

	/// Amount to transfer to Insurance Fund (penalty).
	///
	/// The penalty is transferred to the IF, which temporarily holds the keeper's
	/// share until auction completion.
	pub const fn insurance_fund(&self) -> Balance {
		self.penalty_paid
	}

	/// Total payment amount.
	pub fn total(&self) -> Balance {
		self.burn().saturating_add(self.penalty_paid)
	}
}

/// A balance value with a type-safe bound on its maximum.
///
/// The type parameter `M` must implement `Get<B>` to provide the maximum value.
///
/// # Usage
///
/// ```ignore
/// // Option A: limit from a storage value.
/// #[pallet::storage]
/// pub type MaxLiquidationAmount<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;
/// pub type CappedValueOf<T> = CappedValue<BalanceOf<T>, MaxLiquidationAmount<T>>;
///
/// // Option B: limit from a constant.
/// #[pallet::constant]
/// type MaxAmount: Get<BalanceOf<Self>>;
/// pub type CappedValueOf<T> = CappedValue<BalanceOf<T>, T::MaxAmount>;
///
/// // Storage uses the capped type
/// #[pallet::storage]
/// pub type CurrentLiquidationAmount<T: Config> =
///     StorageValue<_, CappedValueOf<T>, ValueQuery>;
///
/// // Increment - limit auto-fetched from M
/// CurrentLiquidationAmount::<T>::try_mutate(|v| v.try_add(amount))?;
///
/// // Decrement (always safe, uses Saturating trait)
/// CurrentLiquidationAmount::<T>::mutate(|v| {
///     *v = v.saturating_sub(CappedValueOf::<T>::new_unchecked(amount));
/// });
///
/// // Read via Deref
/// let current: BalanceOf<T> = *CurrentLiquidationAmount::<T>::get();
///
/// // Check remaining headroom before the cap
/// let headroom = CurrentLiquidationAmount::<T>::get().remaining_capacity();
/// ```
#[derive(PartialEq, Eq, Encode, Decode, TypeInfo, MaxEncodedLen)]
#[scale_info(skip_type_params(M))]
pub struct CappedValue<B, M>(B, PhantomData<M>);

// TODO: I don't really like this manual `impl`, I'm open to suggestions!
// 
// Manual impls: the derive macros would require `M: Trait`, but `M` is typically a
// `StorageValue` type which doesn't implement `Debug`, `Clone`, or `Copy`.
// These impls only bound `B`.
impl<B: core::fmt::Debug, M> core::fmt::Debug for CappedValue<B, M> {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.debug_tuple("CappedValue").field(&self.0).finish()
	}
}

impl<B: Clone, M> Clone for CappedValue<B, M> {
	fn clone(&self) -> Self {
		Self(self.0.clone(), PhantomData)
	}
}

impl<B: Copy, M> Copy for CappedValue<B, M> {}

/// Error returned by [`CappedValue::try_new`] and [`CappedValue::try_add`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CappedValueError {
	/// The addition overflowed.
	Overflow,
	/// The new value would exceed the configured maximum.
	ExceedsMax,
}

impl<B: Default, M> Default for CappedValue<B, M> {
	fn default() -> Self {
		Self(B::default(), PhantomData)
	}
}

impl<B: Zero, M> CappedValue<B, M> {
	/// Create a new capped balance initialized to zero.
	pub fn zero() -> Self {
		Self(B::zero(), PhantomData)
	}
}

impl<B, M> CappedValue<B, M> {
	/// Create a new capped balance with the given value, without validating against the cap.
	///
	/// Use this for genesis, migrations, or tests where storage may not be available.
	/// Prefer [`try_new`](Self::try_new) at runtime when storage is accessible.
	pub const fn new_unchecked(value: B) -> Self {
		Self(value, PhantomData)
	}

	/// Get the inner value.
	pub fn into_inner(self) -> B {
		self.0
	}
}

impl<B: CheckedAdd + Ord + Copy, M: Get<B>> CappedValue<B, M> {
	/// Create a new capped balance, validating that `value` does not exceed the cap.
	///
	/// The maximum is fetched from `M` at call time.
	pub fn try_new(value: B) -> Result<Self, CappedValueError> {
		let max = M::get();
		if value > max {
			return Err(CappedValueError::ExceedsMax);
		}
		Ok(Self(value, PhantomData))
	}

	/// Try to add `amount`, failing if result exceeds the max or overflows.
	///
	/// The maximum is fetched from `M` at call time.
	pub fn try_add(&mut self, amount: B) -> Result<(), CappedValueError> {
		let max = M::get();
		let new_value = self.0.checked_add(&amount).ok_or(CappedValueError::Overflow)?;
		if new_value > max {
			return Err(CappedValueError::ExceedsMax);
		}
		self.0 = new_value;
		Ok(())
	}
}

impl<B: Saturating + Ord + Copy, M: Get<B>> Saturating for CappedValue<B, M> {
	fn saturating_add(self, rhs: Self) -> Self {
		let max = M::get();
		let result = self.0.saturating_add(rhs.0);
		if result > max {
			Self(max, PhantomData)
		} else {
			Self(result, PhantomData)
		}
	}

	fn saturating_sub(self, rhs: Self) -> Self {
		Self(self.0.saturating_sub(rhs.0), PhantomData)
	}

	fn saturating_mul(self, rhs: Self) -> Self {
		let max = M::get();
		let result = self.0.saturating_mul(rhs.0);
		if result > max {
			Self(max, PhantomData)
		} else {
			Self(result, PhantomData)
		}
	}

	fn saturating_pow(self, exp: usize) -> Self {
		let max = M::get();
		let result = self.0.saturating_pow(exp);
		if result > max {
			Self(max, PhantomData)
		} else {
			Self(result, PhantomData)
		}
	}
}

impl<B: Saturating + Ord + Copy + Zero, M: Get<B>> CappedValue<B, M> {
	/// Returns how much can still be added before hitting the cap.
	///
	/// Returns zero if the current value already meets or exceeds the cap.
	pub fn remaining_capacity(&self) -> B {
		let max = M::get();
		if self.0 >= max {
			B::zero()
		} else {
			max.saturating_sub(self.0)
		}
	}
}

impl<B, M> core::ops::Deref for CappedValue<B, M> {
	type Target = B;
	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

impl<B: PartialEq, M> PartialEq<B> for CappedValue<B, M> {
	fn eq(&self, other: &B) -> bool {
		self.0 == *other
	}
}

/// Trait for the Vaults pallet to delegate auction lifecycle to the Auctions pallet.
///
/// Implemented by the Auctions pallet, called by the Vaults pallet when a vault
/// needs to be liquidated.
pub trait AuctionsHandler<AccountId, Balance> {
	/// Start a new auction for liquidating vault collateral.
	///
	/// Called by the Vaults pallet when a vault becomes undercollateralized.
	/// Returns the auction ID on success.
	///
	/// # Parameters
	///
	/// - `vault_owner`: Account whose vault is being liquidated
	/// - `collateral_amount`: Amount of collateral to auction
	/// - `debt`: Debt breakdown to recover (principal, interest, penalty)
	/// - `keeper`: Account that triggered liquidation (receives keeper incentive)
	///
	/// # Errors
	///
	/// Returns an error if the circuit breaker is active or the oracle price is unavailable.
	fn start_auction(
		vault_owner: AccountId,
		collateral_amount: Balance,
		debt: DebtComponents<Balance>,
		keeper: AccountId,
	) -> Result<u32, DispatchError>;
}

/// Trait for the Auctions pallet to call back into Vaults for asset operations.
///
/// This trait decouples the auction logic from the asset management:
/// - Auctions pallet manages auction state (price decay, staleness, incentives computation)
/// - Vaults pallet handles all asset operations (holds, transfers, pricing, minting/burning)
pub trait CollateralManager<AccountId> {
	/// The balance type used for collateral and debt amounts.
	type Balance: Balance + FixedPointOperand;

	/// Get current collateral price from oracle.
	///
	/// Returns the normalized price: `smallest_pUSD_units / smallest_collateral_unit`.
	/// Used by auctions for `restart_auction()` to set new starting price.
	fn get_dot_price() -> Option<FixedU128>;

	/// Execute a purchase: collect pUSD from buyer, transfer collateral to recipient.
	///
	/// Called during `take()`. This function:
	/// 1. Burns `payment.burn()` pUSD from the buyer (principal + interest)
	/// 2. Transfers `payment.insurance_fund()` pUSD from buyer to Insurance Fund
	/// 3. Releases `collateral_amount` from the vault owner's Seized hold
	/// 4. Transfers the collateral to the recipient
	/// 5. Reduces `CurrentLiquidationAmount` by `payment.principal_paid`
	///
	/// # Errors
	///
	/// Returns an error if the buyer has insufficient pUSD or the collateral transfer fails.
	fn execute_purchase(
		buyer: &AccountId,
		collateral_amount: Self::Balance,
		payment: PaymentBreakdown<Self::Balance>,
		recipient: &AccountId,
		vault_owner: &AccountId,
	) -> DispatchResult;

	/// Complete an auction: pay keeper, return excess collateral, record any shortfall.
	///
	/// Called when auction finishes (tab satisfied or lot exhausted).
	///
	/// # Parameters
	///
	/// - `vault_owner`: Original vault owner (receives excess collateral)
	/// - `remaining_collateral`: Excess collateral to return to owner
	/// - `shortfall`: Uncollected debt (becomes bad debt)
	/// - `keeper`: Account that triggered/restarted the auction
	/// - `keeper_incentive`: pUSD amount to pay keeper (from IF, funded by penalty)
	///
	/// # Errors
	///
	/// Returns an error if the keeper payment or collateral release fails.
	fn complete_auction(
		vault_owner: &AccountId,
		remaining_collateral: Self::Balance,
		shortfall: Self::Balance,
		keeper: &AccountId,
		keeper_incentive: Self::Balance,
	) -> DispatchResult;

	/// Execute a surplus auction purchase: buyer sends collateral, receives pUSD from IF.
	///
	/// Called during `take_surplus()`. This function:
	/// 1. Transfers `pusd_amount` pUSD from the Insurance Fund to the recipient
	/// 2. Transfers `collateral_amount` from the buyer to the `FeeHandler`
	///
	/// # Errors
	///
	/// Returns an error if the buyer has insufficient collateral or IF has insufficient pUSD.
	fn execute_surplus_purchase(
		buyer: &AccountId,
		recipient: &AccountId,
		pusd_amount: Self::Balance,
		collateral_amount: Self::Balance,
	) -> DispatchResult;

	/// Get the Insurance Fund's pUSD balance.
	///
	/// Used to check if surplus auctions can be started (IF balance > threshold).
	fn get_insurance_fund_balance() -> Self::Balance;

	/// Get the total pUSD supply.
	///
	/// Used with `get_insurance_fund_balance()` to calculate whether the
	/// Insurance Fund exceeds the surplus auction threshold.
	fn get_total_pusd_supply() -> Self::Balance;

	/// Transfer surplus pUSD from Insurance Fund via configured handler.
	///
	/// Used in DirectTransfer mode to send surplus directly to treasury
	/// without going through an auction. The destination is determined
	/// by the runtime's `SurplusHandler` configuration.
	///
	/// # Parameters
	/// - `amount`: Amount of pUSD to transfer from Insurance Fund
	///
	/// # Errors
	/// Returns an error if the Insurance Fund has insufficient pUSD.
	fn transfer_surplus(amount: Self::Balance) -> DispatchResult;
}
