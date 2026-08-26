// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
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

//! Considerations that hold one of several `fungibles` assets.

#[cfg(feature = "runtime-benchmarks")]
use super::regular::Balanced;
use super::{hold::Mutate as MutateHold, regular::Inspect};
use crate::{
	storage::with_storage_layer,
	traits::{
		tokens::{
			ConversionToAssetBalance,
			Fortitude::Force,
			Precision::{BestEffort, Exact},
		},
		AssetFootprint, Consideration, Contains, Footprint, MaybeConsideration,
	},
};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use core::marker::PhantomData;
use frame_support_procedural::{CloneNoBound, DebugNoBound, EqNoBound, PartialEqNoBound};
use scale_info::TypeInfo;
use sp_arithmetic::traits::Zero;
use sp_core::Get;
#[cfg(feature = "runtime-benchmarks")]
use sp_runtime::Saturating;
use sp_runtime::{traits::Convert, DispatchError, TokenError};

/// A filter for assets whose balance can keep an account alive.
///
/// For a [`fungible::UnionOf`](crate::traits::fungible::UnionOf), this filter accepts the native
/// asset and all sufficient assets.
pub struct SufficientAssets<F, AccountId>(PhantomData<(F, AccountId)>);
impl<F: Inspect<AccountId>, AccountId> Contains<F::AssetId> for SufficientAssets<F, AccountId> {
	fn contains(asset: &F::AssetId) -> bool {
		F::is_sufficient(asset.clone())
	}
}

/// Sets the lower limit of an `Inner` conversion to the asset's minimum balance.
///
/// A skewed or thin quote can produce dust. This adapter returns at least the asset's minimum
/// balance. It returns `Inner` errors unchanged.
pub struct AtLeastMinimumBalance<F, Inner, AccountId>(PhantomData<(F, Inner, AccountId)>);
impl<F, Inner, AccountId> ConversionToAssetBalance<F::Balance, F::AssetId, F::Balance>
	for AtLeastMinimumBalance<F, Inner, AccountId>
where
	F: Inspect<AccountId>,
	Inner: ConversionToAssetBalance<F::Balance, F::AssetId, F::Balance>,
{
	type Error = Inner::Error;
	fn to_asset_balance(balance: F::Balance, asset: F::AssetId) -> Result<F::Balance, Self::Error> {
		let amount = Inner::to_asset_balance(balance, asset.clone())?;
		Ok(amount.max(F::minimum_balance(asset)))
	}
}

/// Converts an [`AssetFootprint`] to the asset and amount for a multi-asset hold.
///
/// `Price` calculates the footprint price in `Fallback` units. The deposit uses `Fallback` if the
/// price is zero or `Accept` rejects the proposed asset. It also uses `Fallback` when the proposed
/// asset is `Fallback`. Otherwise, `Repricing` converts the price to the proposed asset.
///
/// `Fallback` does not use `Repricing`. If a nonzero price converts to zero, the conversion returns
/// [`TokenError::BelowMinimum`]. Thus, a quote cannot waive the deposit.
pub struct AssetFootprintPrice<Accept, Fallback, Price, Repricing>(
	PhantomData<(Accept, Fallback, Price, Repricing)>,
);
impl<AssetId, Balance, Accept, Fallback, Price, Repricing>
	Convert<AssetFootprint<AssetId>, Result<(AssetId, Balance), DispatchError>>
	for AssetFootprintPrice<Accept, Fallback, Price, Repricing>
where
	AssetId: Eq + Clone,
	Balance: Zero,
	Accept: Contains<AssetId>,
	Fallback: Get<AssetId>,
	Price: Convert<Footprint, Balance>,
	Repricing: ConversionToAssetBalance<Balance, AssetId, Balance>,
	Repricing::Error: Into<DispatchError>,
{
	fn convert(
		AssetFootprint { asset, footprint }: AssetFootprint<AssetId>,
	) -> Result<(AssetId, Balance), DispatchError> {
		let price = Price::convert(footprint);
		let fallback = Fallback::get();
		if price.is_zero() || asset == fallback || !Accept::contains(&asset) {
			return Ok((fallback, price));
		}
		let amount = Repricing::to_asset_balance(price, asset.clone()).map_err(Into::into)?;
		if amount.is_zero() {
			return Err(TokenError::BelowMinimum.into());
		}
		Ok((asset, amount))
	}
}

/// A [`Consideration`] that holds a `fungibles` asset for a footprint.
///
/// `D` converts each footprint to an asset and amount. The runtime policy in `D` selects the asset.
/// The default footprint is an [`AssetFootprint`] priced by [`AssetFootprintPrice`]. The ticket
/// stores the selected asset and amount. It uses these recorded values for release if the policy
/// later changes.
///
/// This type applies [`fungible::HoldConsideration`](crate::traits::fungible::HoldConsideration) to
/// multiple assets.
#[derive(
	CloneNoBound,
	EqNoBound,
	PartialEqNoBound,
	Encode,
	Decode,
	DecodeWithMemTracking,
	TypeInfo,
	MaxEncodedLen,
	DebugNoBound,
)]
#[scale_info(skip_type_params(A, F, R, D, Fp))]
#[codec(mel_bound())]
pub struct HoldConsideration<A, F, R, D, Fp = AssetFootprint<<F as Inspect<A>>::AssetId>>(
	F::AssetId,
	F::Balance,
	PhantomData<fn() -> (A, R, D, Fp)>,
)
where
	F: MutateHold<A>;
impl<
		A: 'static + Eq,
		#[cfg(not(feature = "runtime-benchmarks"))] F: 'static + MutateHold<A, AssetId: Send + Sync>,
		#[cfg(feature = "runtime-benchmarks")] F: 'static + MutateHold<A, AssetId: Send + Sync> + Balanced<A>,
		R: 'static + Get<F::Reason>,
		D: 'static + Convert<Fp, Result<(F::AssetId, F::Balance), DispatchError>>,
		Fp: 'static,
	> Consideration<A, Fp> for HoldConsideration<A, F, R, D, Fp>
{
	fn new(who: &A, footprint: Fp) -> Result<Self, DispatchError> {
		let (asset, amount) = D::convert(footprint)?;
		F::hold(asset.clone(), &R::get(), who, amount)?;
		Ok(Self(asset, amount, PhantomData))
	}
	fn update(self, who: &A, footprint: Fp) -> Result<Self, DispatchError> {
		let (asset, amount) = D::convert(footprint)?;
		if asset == self.0 {
			if self.1 > amount {
				F::release(asset.clone(), &R::get(), who, self.1 - amount, Exact)?;
			} else if amount > self.1 {
				F::hold(asset.clone(), &R::get(), who, amount - self.1)?;
			}
		} else {
			// The policy selected a different asset. The storage layer makes the release and new
			// hold atomic.
			with_storage_layer(|| {
				F::release(self.0.clone(), &R::get(), who, self.1, Exact)?;
				F::hold(asset.clone(), &R::get(), who, amount)
			})?;
		}
		Ok(Self(asset, amount, PhantomData))
	}
	fn drop(self, who: &A) -> Result<(), DispatchError> {
		F::release(self.0, &R::get(), who, self.1, Exact).map(|_| ())
	}
	fn burn(self, who: &A) {
		let _ = F::burn_held(self.0, &R::get(), who, self.1, BestEffort, Force);
	}
	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful(who: &A, footprint: Fp) {
		// The pricing is the benchmark's responsibility: `D` has no hook to make a quote
		// available, so a failed conversion leaves the account unfunded and `new` fails.
		let Ok((asset, amount)) = D::convert(footprint) else { return };
		// `F` has no `mint_into` method, so the benchmark deposits the funds.
		let funding = F::minimum_balance(asset.clone()).saturating_add(amount);
		let _ = F::deposit(asset, who, funding, Exact);
	}
}
impl<
		A: 'static + Eq,
		#[cfg(not(feature = "runtime-benchmarks"))] F: 'static + MutateHold<A, AssetId: Send + Sync>,
		#[cfg(feature = "runtime-benchmarks")] F: 'static + MutateHold<A, AssetId: Send + Sync> + Balanced<A>,
		R: 'static + Get<F::Reason>,
		D: 'static + Convert<Fp, Result<(F::AssetId, F::Balance), DispatchError>>,
		Fp: 'static,
	> MaybeConsideration<A, Fp> for HoldConsideration<A, F, R, D, Fp>
{
	fn is_none(&self) -> bool {
		self.1.is_zero()
	}
}
