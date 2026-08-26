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

//! Tests for pallet-assets-holder.

use crate::mock::*;

use frame_support::{
	assert_noop, assert_ok,
	traits::tokens::fungibles::{Inspect, InspectHold, MutateHold, UnbalancedHold},
};
use pallet_assets::BalanceOnHold;

const WHO: AccountId = 1;
const ASSET_ID: AssetId = 1;

fn test_hold(id: DummyHoldReason, amount: Balance) {
	assert_ok!(AssetsHolder::set_balance_on_hold(ASSET_ID, &id, &WHO, amount));
}

fn test_release(id: DummyHoldReason) {
	assert_ok!(AssetsHolder::set_balance_on_hold(ASSET_ID, &id, &WHO, 0));
}

mod impl_balance_on_hold {
	use super::*;

	#[test]
	fn balance_on_hold_works() {
		new_test_ext(|| {
			assert_eq!(
				<AssetsHolder as BalanceOnHold<_, _, _>>::balance_on_hold(ASSET_ID, &WHO),
				None
			);
			test_hold(DummyHoldReason::Governance, 1);
			assert_eq!(
				<AssetsHolder as BalanceOnHold<_, _, _>>::balance_on_hold(ASSET_ID, &WHO),
				Some(1u64)
			);
			test_hold(DummyHoldReason::Staking, 3);
			assert_eq!(
				<AssetsHolder as BalanceOnHold<_, _, _>>::balance_on_hold(ASSET_ID, &WHO),
				Some(4u64)
			);
			test_hold(DummyHoldReason::Governance, 2);
			assert_eq!(
				<AssetsHolder as BalanceOnHold<_, _, _>>::balance_on_hold(ASSET_ID, &WHO),
				Some(5u64)
			);
			// also test releasing works to reduce a balance, and finally releasing everything
			// resets to None
			test_release(DummyHoldReason::Governance);
			assert_eq!(
				<AssetsHolder as BalanceOnHold<_, _, _>>::balance_on_hold(ASSET_ID, &WHO),
				Some(3u64)
			);
			test_release(DummyHoldReason::Staking);
			assert_eq!(
				<AssetsHolder as BalanceOnHold<_, _, _>>::balance_on_hold(ASSET_ID, &WHO),
				None
			);
		});
	}

	#[test]
	#[should_panic = "The list of Holds should be empty before allowing an account to die"]
	fn died_fails_if_holds_exist() {
		new_test_ext(|| {
			test_hold(DummyHoldReason::Governance, 1);
			AssetsHolder::died(ASSET_ID, &WHO);
		});
	}

	#[test]
	fn died_works() {
		new_test_ext(|| {
			test_hold(DummyHoldReason::Governance, 1);
			test_release(DummyHoldReason::Governance);
			AssetsHolder::died(ASSET_ID, &WHO);
			assert!(BalancesOnHold::<Test>::get(ASSET_ID, WHO).is_none());
			assert!(Holds::<Test>::get(ASSET_ID, WHO).is_empty());
		});
	}
}

mod impl_hold_inspect {
	use super::*;

	#[test]
	fn total_balance_on_hold_works() {
		new_test_ext(|| {
			assert_eq!(AssetsHolder::total_balance_on_hold(ASSET_ID, &WHO), 0u64);
			test_hold(DummyHoldReason::Governance, 1);
			assert_eq!(AssetsHolder::total_balance_on_hold(ASSET_ID, &WHO), 1u64);
			test_hold(DummyHoldReason::Staking, 3);
			assert_eq!(AssetsHolder::total_balance_on_hold(ASSET_ID, &WHO), 4u64);
			test_hold(DummyHoldReason::Governance, 2);
			assert_eq!(AssetsHolder::total_balance_on_hold(ASSET_ID, &WHO), 5u64);
			// also test release to reduce a balance, and finally releasing everything resets to
			// 0
			test_release(DummyHoldReason::Governance);
			assert_eq!(AssetsHolder::total_balance_on_hold(ASSET_ID, &WHO), 3u64);
			test_release(DummyHoldReason::Staking);
			assert_eq!(AssetsHolder::total_balance_on_hold(ASSET_ID, &WHO), 0u64);
		});
	}

	#[test]
	fn balance_on_hold_works() {
		new_test_ext(|| {
			assert_eq!(
				<AssetsHolder as InspectHold<_>>::balance_on_hold(
					ASSET_ID,
					&DummyHoldReason::Governance,
					&WHO
				),
				0u64
			);
			test_hold(DummyHoldReason::Governance, 1);
			assert_eq!(
				<AssetsHolder as InspectHold<_>>::balance_on_hold(
					ASSET_ID,
					&DummyHoldReason::Governance,
					&WHO
				),
				1u64
			);
			test_hold(DummyHoldReason::Staking, 3);
			assert_eq!(
				<AssetsHolder as InspectHold<_>>::balance_on_hold(
					ASSET_ID,
					&DummyHoldReason::Staking,
					&WHO
				),
				3u64
			);
			test_hold(DummyHoldReason::Staking, 2);
			assert_eq!(
				<AssetsHolder as InspectHold<_>>::balance_on_hold(
					ASSET_ID,
					&DummyHoldReason::Staking,
					&WHO
				),
				2u64
			);
			// also test release to reduce a balance, and finally releasing everything resets to
			// 0
			test_release(DummyHoldReason::Governance);
			assert_eq!(
				<AssetsHolder as InspectHold<_>>::balance_on_hold(
					ASSET_ID,
					&DummyHoldReason::Governance,
					&WHO
				),
				0u64
			);
			test_release(DummyHoldReason::Staking);
			assert_eq!(
				<AssetsHolder as InspectHold<_>>::balance_on_hold(
					ASSET_ID,
					&DummyHoldReason::Staking,
					&WHO
				),
				0u64
			);
		});
	}
}

mod impl_hold_unbalanced {
	use super::*;

	// Note: Tests for `handle_dust`, `write_balance`, `set_total_issuance`, `decrease_balance`
	// and `increase_balance` are intentionally left out without testing, since:
	// 1. It is expected these methods are tested within `pallet-assets`, and
	// 2. There are no valid cases that can be directly asserted using those methods in
	// the scope of this pallet.

	#[test]
	fn set_balance_on_hold_works() {
		new_test_ext(|| {
			assert_eq!(Holds::<Test>::get(ASSET_ID, WHO).to_vec(), vec![]);
			assert_eq!(BalancesOnHold::<Test>::get(ASSET_ID, WHO), None);
			// Adding balance on hold works
			assert_ok!(AssetsHolder::set_balance_on_hold(
				ASSET_ID,
				&DummyHoldReason::Governance,
				&WHO,
				1
			));
			assert_eq!(
				Holds::<Test>::get(ASSET_ID, WHO).to_vec(),
				vec![IdAmount { id: DummyHoldReason::Governance, amount: 1 }]
			);
			assert_eq!(BalancesOnHold::<Test>::get(ASSET_ID, WHO), Some(1));
			// Increasing hold works
			assert_ok!(AssetsHolder::set_balance_on_hold(
				ASSET_ID,
				&DummyHoldReason::Governance,
				&WHO,
				3
			));
			assert_eq!(
				Holds::<Test>::get(ASSET_ID, WHO).to_vec(),
				vec![IdAmount { id: DummyHoldReason::Governance, amount: 3 }]
			);
			assert_eq!(BalancesOnHold::<Test>::get(ASSET_ID, WHO), Some(3));
			// Adding new balance on hold works
			assert_ok!(AssetsHolder::set_balance_on_hold(
				ASSET_ID,
				&DummyHoldReason::Staking,
				&WHO,
				2
			));
			assert_eq!(
				Holds::<Test>::get(ASSET_ID, WHO).to_vec(),
				vec![
					IdAmount { id: DummyHoldReason::Governance, amount: 3 },
					IdAmount { id: DummyHoldReason::Staking, amount: 2 }
				]
			);
			assert_eq!(BalancesOnHold::<Test>::get(ASSET_ID, WHO), Some(5));

			// Note: Assertion skipped to meet @gavofyork's suggestion of matching the number of
			// variant count with the number of enum's variants.
			// // Adding more than max holds fails
			// assert_noop!(
			// 	AssetsHolder::set_balance_on_hold(ASSET_ID, &DummyHoldReason::Other, &WHO, 1),
			// 	Error::<Test>::TooManyHolds
			// );

			// Decreasing balance on hold works
			assert_ok!(AssetsHolder::set_balance_on_hold(
				ASSET_ID,
				&DummyHoldReason::Staking,
				&WHO,
				1
			));
			assert_eq!(
				Holds::<Test>::get(ASSET_ID, WHO).to_vec(),
				vec![
					IdAmount { id: DummyHoldReason::Governance, amount: 3 },
					IdAmount { id: DummyHoldReason::Staking, amount: 1 }
				]
			);
			assert_eq!(BalancesOnHold::<Test>::get(ASSET_ID, WHO), Some(4));
			// Decreasing until removal of balance on hold works
			assert_ok!(AssetsHolder::set_balance_on_hold(
				ASSET_ID,
				&DummyHoldReason::Governance,
				&WHO,
				0
			));
			assert_eq!(
				Holds::<Test>::get(ASSET_ID, WHO).to_vec(),
				vec![IdAmount { id: DummyHoldReason::Staking, amount: 1 }]
			);
			assert_eq!(BalancesOnHold::<Test>::get(ASSET_ID, WHO), Some(1));
			// Clearing ol all holds works
			assert_ok!(AssetsHolder::set_balance_on_hold(
				ASSET_ID,
				&DummyHoldReason::Staking,
				&WHO,
				0
			));
			assert_eq!(Holds::<Test>::get(ASSET_ID, WHO).to_vec(), vec![]);
			assert_eq!(BalancesOnHold::<Test>::get(ASSET_ID, WHO), None);
		});
	}
}

mod impl_hold_mutate {
	use super::*;
	use frame_support::traits::tokens::{Fortitude, Precision, Preservation};
	use sp_runtime::TokenError;

	#[test]
	fn hold_works() {
		super::new_test_ext(|| {
			// Holding some `amount` would decrease the asset account balance and change the
			// reducible balance, while total issuance is preserved.
			assert_ok!(AssetsHolder::hold(ASSET_ID, &DummyHoldReason::Governance, &WHO, 10));
			assert_eq!(Assets::balance(ASSET_ID, &WHO), 90);
			// Reducible balance is tested once to ensure token balance model is compliant.
			assert_eq!(
				Assets::reducible_balance(
					ASSET_ID,
					&WHO,
					Preservation::Expendable,
					Fortitude::Force
				),
				89
			);
			assert_eq!(
				<AssetsHolder as InspectHold<_>>::balance_on_hold(
					ASSET_ID,
					&DummyHoldReason::Governance,
					&WHO
				),
				10
			);
			assert_eq!(AssetsHolder::total_balance_on_hold(ASSET_ID, &WHO), 10);
			// Holding preserves `total_balance`
			assert_eq!(Assets::total_balance(ASSET_ID, &WHO), 100);
			// Holding preserves `total_issuance`
			assert_eq!(Assets::total_issuance(ASSET_ID), 100);

			// Increasing the amount on hold for the same reason has the same effect as described
			// above in `set_balance_on_hold_works`, while total issuance is preserved.
			// Consideration: holding for an amount `x` will increase the already amount on hold by
			// `x`.
			assert_ok!(AssetsHolder::hold(ASSET_ID, &DummyHoldReason::Governance, &WHO, 20));
			assert_eq!(Assets::balance(ASSET_ID, &WHO), 70);
			assert_eq!(
				<AssetsHolder as InspectHold<_>>::balance_on_hold(
					ASSET_ID,
					&DummyHoldReason::Governance,
					&WHO
				),
				30
			);
			assert_eq!(AssetsHolder::total_balance_on_hold(ASSET_ID, &WHO), 30);
			assert_eq!(Assets::total_issuance(ASSET_ID), 100);

			// Holding some amount for a different reason has the same effect as described above in
			// `set_balance_on_hold_works`, while total issuance is preserved.
			assert_ok!(AssetsHolder::hold(ASSET_ID, &DummyHoldReason::Staking, &WHO, 20));
			assert_eq!(Assets::balance(ASSET_ID, &WHO), 50);
			assert_eq!(
				<AssetsHolder as InspectHold<_>>::balance_on_hold(
					ASSET_ID,
					&DummyHoldReason::Staking,
					&WHO
				),
				20
			);
			assert_eq!(AssetsHolder::total_balance_on_hold(ASSET_ID, &WHO), 50);
			assert_eq!(Assets::total_issuance(ASSET_ID), 100);
		});
	}

	fn new_test_ext() -> sp_io::TestExternalities {
		super::new_test_ext(|| {
			assert_ok!(AssetsHolder::hold(ASSET_ID, &DummyHoldReason::Governance, &WHO, 30));
			assert_ok!(AssetsHolder::hold(ASSET_ID, &DummyHoldReason::Staking, &WHO, 20));
		})
	}

	#[test]
	fn release_works() {
		// Releasing up to some amount will increase the balance by the released
		// amount, while preserving total issuance.
		new_test_ext().execute_with(|| {
			assert_ok!(AssetsHolder::release(
				ASSET_ID,
				&DummyHoldReason::Governance,
				&WHO,
				20,
				Precision::Exact,
			));
			assert_eq!(
				<AssetsHolder as InspectHold<_>>::balance_on_hold(
					ASSET_ID,
					&DummyHoldReason::Governance,
					&WHO
				),
				10
			);
			assert_eq!(Assets::balance(ASSET_ID, WHO), 70);
		});

		// Releasing over the max amount on hold with `BestEffort` will increase the
		// balance by the previously amount on hold, while preserving total issuance.
		new_test_ext().execute_with(|| {
			assert_ok!(AssetsHolder::release(
				ASSET_ID,
				&DummyHoldReason::Governance,
				&WHO,
				31,
				Precision::BestEffort,
			));
			assert_eq!(
				<AssetsHolder as InspectHold<_>>::balance_on_hold(
					ASSET_ID,
					&DummyHoldReason::Governance,
					&WHO
				),
				0
			);
			assert_eq!(Assets::balance(ASSET_ID, WHO), 80);
		});

		// Releasing over the max amount on hold with `Exact` will fail.
		new_test_ext().execute_with(|| {
			assert_noop!(
				AssetsHolder::release(
					ASSET_ID,
					&DummyHoldReason::Governance,
					&WHO,
					31,
					Precision::Exact,
				),
				TokenError::FundsUnavailable
			);
		});
	}

	#[test]
	fn burn_held_works() {
		// Burning works, reducing total issuance and `total_balance`.
		new_test_ext().execute_with(|| {
			assert_ok!(AssetsHolder::burn_held(
				ASSET_ID,
				&DummyHoldReason::Governance,
				&WHO,
				1,
				Precision::BestEffort,
				Fortitude::Polite
			));
			assert_eq!(Assets::total_balance(ASSET_ID, &WHO), 99);
			assert_eq!(Assets::total_issuance(ASSET_ID), 99);
		});

		// Burning by an amount up to the balance on hold with `Exact` works, reducing balance on
		// hold up to the given amount.
		new_test_ext().execute_with(|| {
			assert_ok!(AssetsHolder::burn_held(
				ASSET_ID,
				&DummyHoldReason::Governance,
				&WHO,
				10,
				Precision::Exact,
				Fortitude::Polite
			));
			assert_eq!(AssetsHolder::total_balance_on_hold(ASSET_ID, &WHO), 40);
			assert_eq!(Assets::balance(ASSET_ID, WHO), 50);
		});

		// Burning by an amount over the balance on hold with `BestEffort` works, reducing balance
		// on hold up to the given amount.
		new_test_ext().execute_with(|| {
			assert_ok!(AssetsHolder::burn_held(
				ASSET_ID,
				&DummyHoldReason::Governance,
				&WHO,
				31,
				Precision::BestEffort,
				Fortitude::Polite
			));
			assert_eq!(AssetsHolder::total_balance_on_hold(ASSET_ID, &WHO), 20);
			assert_eq!(Assets::balance(ASSET_ID, WHO), 50);
		});

		// Burning by an amount over the balance on hold with `Exact` fails.
		new_test_ext().execute_with(|| {
			assert_noop!(
				AssetsHolder::burn_held(
					ASSET_ID,
					&DummyHoldReason::Governance,
					&WHO,
					31,
					Precision::Exact,
					Fortitude::Polite
				),
				TokenError::FundsUnavailable
			);
		});
	}

	#[test]
	fn burn_all_held_works() {
		new_test_ext().execute_with(|| {
			// Burning all balance on hold works as burning passing it as amount with `BestEffort`
			assert_ok!(AssetsHolder::burn_all_held(
				ASSET_ID,
				&DummyHoldReason::Governance,
				&WHO,
				Precision::BestEffort,
				Fortitude::Polite,
			));
			assert_eq!(AssetsHolder::total_balance_on_hold(ASSET_ID, &WHO), 20);
			assert_eq!(Assets::balance(ASSET_ID, WHO), 50);
		});
	}

	#[test]
	fn done_held_works() {
		new_test_ext().execute_with(|| {
			System::assert_has_event(
				Event::<Test>::Held {
					who: WHO,
					asset_id: ASSET_ID,
					reason: DummyHoldReason::Governance,
					amount: 30,
				}
				.into(),
			);
		});
	}

	#[test]
	fn done_release_works() {
		new_test_ext().execute_with(|| {
			assert_ok!(AssetsHolder::release(
				ASSET_ID,
				&DummyHoldReason::Governance,
				&WHO,
				31,
				Precision::BestEffort
			));
			System::assert_has_event(
				Event::<Test>::Released {
					who: WHO,
					asset_id: ASSET_ID,
					reason: DummyHoldReason::Governance,
					amount: 30,
				}
				.into(),
			);
		});
	}

	#[test]
	fn done_burn_held_works() {
		new_test_ext().execute_with(|| {
			assert_ok!(AssetsHolder::burn_all_held(
				ASSET_ID,
				&DummyHoldReason::Governance,
				&WHO,
				Precision::BestEffort,
				Fortitude::Polite,
			));
			System::assert_has_event(
				Event::<Test>::Burned {
					who: WHO,
					asset_id: ASSET_ID,
					reason: DummyHoldReason::Governance,
					amount: 30,
				}
				.into(),
			);
		});
	}
}

mod consideration {
	use super::*;
	use frame_support::{
		parameter_types,
		traits::{
			fungibles::{
				AssetFootprintPrice, AtLeastMinimumBalance, HoldConsideration, Mutate,
				SufficientAssets,
			},
			tokens::{ConversionToAssetBalance, FallbackOnUnavailable, Fortitude, Precision},
			AssetFootprint, Consideration, ConstU64, Contains, Footprint, LinearStoragePrice,
			MaybeConsideration,
		},
	};
	use sp_runtime::{traits::Convert, DispatchError, TokenError};

	/// This sufficient asset has a minimum balance of ten.
	const SUFFICIENT: AssetId = 2;
	const INSUFFICIENT: AssetId = 3;
	const UNKNOWN: AssetId = 99;
	const POOR: AccountId = 2;

	parameter_types! {
		pub const GovernanceReason: DummyHoldReason = DummyHoldReason::Governance;
		pub const FallbackAsset: AssetId = ASSET_ID;
	}

	struct TimesTen;
	impl ConversionToAssetBalance<Balance, AssetId, Balance> for TimesTen {
		type Error = DispatchError;
		fn to_asset_balance(balance: Balance, _: AssetId) -> Result<Balance, DispatchError> {
			Ok(balance * 10)
		}
	}

	struct Unavailable;
	impl ConversionToAssetBalance<Balance, AssetId, Balance> for Unavailable {
		type Error = DispatchError;
		fn to_asset_balance(_: Balance, _: AssetId) -> Result<Balance, DispatchError> {
			Err(DispatchError::Unavailable)
		}
	}

	struct Unsupported;
	impl ConversionToAssetBalance<Balance, AssetId, Balance> for Unsupported {
		type Error = DispatchError;
		fn to_asset_balance(_: Balance, _: AssetId) -> Result<Balance, DispatchError> {
			Err(TokenError::Unsupported.into())
		}
	}

	struct Worthless;
	impl ConversionToAssetBalance<Balance, AssetId, Balance> for Worthless {
		type Error = DispatchError;
		fn to_asset_balance(_: Balance, _: AssetId) -> Result<Balance, DispatchError> {
			Ok(0)
		}
	}

	/// The price is one balance unit per byte.
	type Price = LinearStoragePrice<ConstU64<0>, ConstU64<1>, Balance>;
	type Policy<Repricing> = AssetFootprintPrice<
		SufficientAssets<AssetsHolder, AccountId>,
		FallbackAsset,
		Price,
		Repricing,
	>;
	type Ticket = HoldConsideration<AccountId, AssetsHolder, GovernanceReason, Policy<TimesTen>>;

	fn footprint(asset: AssetId, bytes: usize) -> AssetFootprint<AssetId> {
		AssetFootprint::new(asset, Footprint::from_parts(1, bytes))
	}

	fn held(asset: AssetId, who: AccountId) -> Balance {
		<AssetsHolder as InspectHold<AccountId>>::balance_on_hold(
			asset,
			&DummyHoldReason::Governance,
			&who,
		)
	}

	fn new_test_ext() -> sp_io::TestExternalities {
		super::new_test_ext(|| {
			assert_ok!(Assets::force_create(RuntimeOrigin::root(), SUFFICIENT, 0, true, 10));
			assert_ok!(Assets::force_create(RuntimeOrigin::root(), INSUFFICIENT, 0, false, 1));
			assert_ok!(<Assets as Mutate<AccountId>>::mint_into(SUFFICIENT, &WHO, 1_000));
		})
	}

	#[test]
	fn sufficient_assets_works() {
		new_test_ext().execute_with(|| {
			type Filter = SufficientAssets<AssetsHolder, AccountId>;
			assert!(Filter::contains(&ASSET_ID));
			assert!(Filter::contains(&SUFFICIENT));
			assert!(!Filter::contains(&INSUFFICIENT));
			assert!(!Filter::contains(&UNKNOWN));
		});
	}

	#[test]
	fn asset_footprint_price_works() {
		// The policy selects the asset and reprices it.
		new_test_ext().execute_with(|| {
			assert_eq!(Policy::<TimesTen>::convert(footprint(SUFFICIENT, 5)), Ok((SUFFICIENT, 50)));
			// The policy does not reprice the fallback asset.
			assert_eq!(Policy::<TimesTen>::convert(footprint(ASSET_ID, 5)), Ok((ASSET_ID, 5)));
		});

		// The policy returns the conversion error. The fallback does not use the converter.
		new_test_ext().execute_with(|| {
			assert_eq!(
				Policy::<Unavailable>::convert(footprint(SUFFICIENT, 5)),
				Err(DispatchError::Unavailable)
			);
			assert_eq!(
				Policy::<Unavailable>::convert(footprint(INSUFFICIENT, 5)),
				Ok((ASSET_ID, 5))
			);
		});

		// The policy never waives a priced deposit, but a zero footprint price creates a free
		// ticket.
		new_test_ext().execute_with(|| {
			assert_eq!(
				Policy::<Worthless>::convert(footprint(SUFFICIENT, 5)),
				Err(TokenError::BelowMinimum.into())
			);
			assert_eq!(Policy::<Worthless>::convert(footprint(SUFFICIENT, 0)), Ok((ASSET_ID, 0)));
		});
	}

	#[test]
	fn at_least_minimum_balance_works() {
		new_test_ext().execute_with(|| {
			type Floored<Inner> = AtLeastMinimumBalance<AssetsHolder, Inner, AccountId>;
			// The quote is floored to the minimum balance of the asset.
			assert_eq!(Floored::<Worthless>::to_asset_balance(5, SUFFICIENT), Ok(10));
			assert_eq!(Floored::<TimesTen>::to_asset_balance(5, SUFFICIENT), Ok(50));
			assert_eq!(
				Floored::<Unavailable>::to_asset_balance(5, SUFFICIENT),
				Err(DispatchError::Unavailable)
			);
		});
	}

	#[test]
	fn fallback_on_unavailable_works() {
		// The secondary is consulted only when the primary has no quote.
		type Fallback = FallbackOnUnavailable<Unavailable, TimesTen>;
		type NoFallback = FallbackOnUnavailable<Unsupported, TimesTen>;
		type BothUnavailable = FallbackOnUnavailable<Unavailable, Unavailable>;
		assert_eq!(Fallback::to_asset_balance(5, SUFFICIENT), Ok(50));
		assert_eq!(
			NoFallback::to_asset_balance(5, SUFFICIENT),
			Err(TokenError::Unsupported.into())
		);
		assert_eq!(
			BothUnavailable::to_asset_balance(5, SUFFICIENT),
			Err(DispatchError::Unavailable)
		);
	}

	#[test]
	fn hold_consideration_new_works() {
		// Creating a ticket holds the deposit in the selected asset.
		new_test_ext().execute_with(|| {
			let ticket = Ticket::new(&WHO, footprint(SUFFICIENT, 5)).expect("WHO is funded");
			assert!(!ticket.is_none());
			assert_eq!(held(SUFFICIENT, WHO), 50);
			assert_eq!(held(ASSET_ID, WHO), 0);
		});

		// Creating a ticket without funds fails without leaving a hold.
		new_test_ext().execute_with(|| {
			assert_noop!(
				Ticket::new(&POOR, footprint(SUFFICIENT, 5)),
				TokenError::FundsUnavailable
			);
		});

		// Creating a ticket for a free footprint creates a ticket that holds nothing.
		new_test_ext().execute_with(|| {
			let ticket = Ticket::new(&WHO, footprint(SUFFICIENT, 0)).expect("free ticket");
			assert!(ticket.is_none());
			assert_ok!(ticket.drop(&WHO));
		});
	}

	#[test]
	fn hold_consideration_update_works() {
		// Updating the ticket adjusts the hold in the selected asset without touching an
		// unrelated hold for the same reason.
		new_test_ext().execute_with(|| {
			assert_ok!(<AssetsHolder as MutateHold<AccountId>>::hold(
				SUFFICIENT,
				&DummyHoldReason::Governance,
				&WHO,
				7
			));
			let ticket = Ticket::new(&WHO, footprint(SUFFICIENT, 5)).expect("WHO is funded");
			assert_eq!(held(SUFFICIENT, WHO), 57);

			let ticket = ticket.update(&WHO, footprint(SUFFICIENT, 8)).expect("WHO is funded");
			assert_eq!(held(SUFFICIENT, WHO), 87);
			let ticket = ticket.update(&WHO, footprint(SUFFICIENT, 3)).expect("WHO is funded");
			assert_eq!(held(SUFFICIENT, WHO), 37);

			// The policy selects a different asset and moves the complete deposit.
			let ticket = ticket.update(&WHO, footprint(INSUFFICIENT, 3)).expect("WHO is funded");
			assert_eq!(held(SUFFICIENT, WHO), 7);
			assert_eq!(held(ASSET_ID, WHO), 3);

			assert_ok!(ticket.drop(&WHO));
			assert_eq!(held(SUFFICIENT, WHO), 7);
			assert_eq!(held(ASSET_ID, WHO), 0);
		});

		// Switching asset is all or nothing: the fallback asset has only 100 units, so it
		// cannot hold 500 units and the original hold is preserved.
		new_test_ext().execute_with(|| {
			let ticket = Ticket::new(&WHO, footprint(SUFFICIENT, 5)).expect("WHO is funded");
			assert_noop!(
				ticket.update(&WHO, footprint(INSUFFICIENT, 500)),
				TokenError::FundsUnavailable
			);
		});

		// Switching asset after the hold was partially burned fails.
		new_test_ext().execute_with(|| {
			let ticket = Ticket::new(&WHO, footprint(SUFFICIENT, 5)).expect("WHO is funded");
			assert_ok!(<AssetsHolder as MutateHold<AccountId>>::burn_held(
				SUFFICIENT,
				&DummyHoldReason::Governance,
				&WHO,
				1,
				Precision::Exact,
				Fortitude::Force,
			));
			assert_noop!(
				ticket.update(&WHO, footprint(INSUFFICIENT, 3)),
				TokenError::FundsUnavailable
			);
		});
	}

	#[test]
	fn hold_consideration_drop_works() {
		// Dropping the ticket after the hold was partially burned fails.
		new_test_ext().execute_with(|| {
			let ticket = Ticket::new(&WHO, footprint(SUFFICIENT, 5)).expect("WHO is funded");
			assert_ok!(<AssetsHolder as MutateHold<AccountId>>::burn_held(
				SUFFICIENT,
				&DummyHoldReason::Governance,
				&WHO,
				1,
				Precision::Exact,
				Fortitude::Force,
			));
			assert_eq!(held(SUFFICIENT, WHO), 49);

			assert_noop!(ticket.drop(&WHO), TokenError::FundsUnavailable);
		});
	}

	#[test]
	fn hold_consideration_burn_works() {
		// Burning the ticket destroys the held amount, reducing `total_balance`.
		new_test_ext().execute_with(|| {
			let total_before = <Assets as Inspect<AccountId>>::total_balance(ASSET_ID, &WHO);
			let ticket = Ticket::new(&WHO, footprint(INSUFFICIENT, 5)).expect("WHO is funded");
			assert_eq!(held(ASSET_ID, WHO), 5);

			ticket.burn(&WHO);

			assert_eq!(held(ASSET_ID, WHO), 0);
			assert_eq!(
				<Assets as Inspect<AccountId>>::total_balance(ASSET_ID, &WHO),
				total_before - 5
			);
		});
	}
}
