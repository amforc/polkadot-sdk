//! Bad-debt healing through `VaultInterface::heal`.

use crate::{mock::*, tests::rate_pct};
use pusd_primitives::VaultInterface;

/// Seed recorded bad debt directly: recording only ever happens inside the
/// vault pallet (recovery settlement / orphan-debt sweeps).
fn record(amount: Balance) {
	mutate_branch_state(DOT, PUSD, |state| {
		state.debt.bad_debt = state.debt.bad_debt.saturating_add(amount);
	});
}

/// Issue a fresh credit of `amount`, heal with it, and return the surplus
/// handed back (the unconsumed part of the credit).
fn heal(amount: Balance) -> Balance {
	let credit = <Assets as frame::traits::fungibles::Balanced<AccountId>>::issue(PUSD, amount);
	<crate::Pallet<Test> as VaultInterface>::heal(&DOT, credit).peek()
}

fn bad_debt() -> Balance {
	branch_state(DOT, PUSD).expect("branch state").debt.bad_debt
}

// Production heals `min(IF_balance, bad_debt)`: a partial heal is the IF not
// yet covering the whole recorded debt, cleared by a later top-up.
#[test]
fn heal_partial_then_exact_clears_bad_debt() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		record(1_000);

		assert_eq!(heal(400), 0, "fully consumed, no surplus");
		assert_eq!(bad_debt(), 600);
		assert_eq!(heal(600), 0);
		assert_eq!(bad_debt(), 0);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::BadDebtHealed {
			collateral_id: DOT,
			stable_id: PUSD,
			amount: 600,
		}));
	});
}

#[test]
fn heal_caps_at_recorded_and_returns_surplus() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		record(500);

		// Defensive: production heals `min(IF_balance, bad_debt)`, so over-supply
		// can't happen — the cap returns the surplus rather than trusting the caller.
		assert_eq!(heal(501), 1);
		assert_eq!(bad_debt(), 0);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::BadDebtHealed {
			collateral_id: DOT,
			stable_id: PUSD,
			amount: 500,
		}));

		// Nothing recorded any more: the whole credit comes back, and no
		// further `BadDebtHealed` lands (the test helper's issue/drop still
		// emits asset events, so count only ours).
		let healed_events = || {
			System::events()
				.into_iter()
				.filter(|e| {
					matches!(e.event, RuntimeEvent::Vaults(crate::Event::BadDebtHealed { .. }))
				})
				.count()
		};
		let before = healed_events();
		assert_eq!(heal(50), 50);
		assert_eq!(healed_events(), before, "no-op heal emits no BadDebtHealed");
	});
}

// The realistic dust path, end to end: a redistribution's per-stake flooring
// residue (501 of debt over stakes 1_000 + 999 strands exactly 1 unit) lands in
// `ownerless_debt`, is swept into `bad_debt` when the last husk closes, and is
// healed exactly.
#[test]
fn heal_clears_swept_flooring_dust() {
	use frame::traits::fungible::Mutate;
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 999, 500, rate_pct(5, 100)));
		assert_ok!(open(3, DOT, PUSD, 5_000, 500, rate_pct(5, 100))); // liquidatee

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(redistribute_for_test(DOT, PUSD, 3, 0));
		assert_eq!(branch_state(DOT, PUSD).unwrap().ownerless_debt, 1);

		// Empty the branch: overpay-repay both recipients, close both husks.
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&1, 1_000));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&2, 1_000));
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			10_000
		));
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(2),
			DOT,
			PUSD,
			2,
			10_000
		));
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), DOT, PUSD, None));
		let before_sweep = <crate::Pallet<Test> as VaultInterface>::stablecoin_debt(&PUSD);
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(2), DOT, PUSD, None));

		assert_eq!(bad_debt(), 1, "sweep promoted the flooring residue to bad debt");
		assert_eq!(
			<crate::Pallet<Test> as VaultInterface>::stablecoin_debt(&PUSD),
			before_sweep,
			"sweeping ownerless debt into bad debt must not change stablecoin debt"
		);
		assert_eq!(heal(1), 0);
		assert_eq!(bad_debt(), 0);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::BadDebtHealed {
			collateral_id: DOT,
			stable_id: PUSD,
			amount: 1,
		}));
	});
}

// The collateral twin of the debt-dust path, end to end: a redistribution's
// per-stake flooring residue (501 of collateral over stakes 1_000 + 999
// strands exactly 1 unit) lands in `ownerless_collateral`, is slashed off the
// redistribution account and routed to `OrphanCollateralHandler` when the last
// husk closes, and the healed market becomes removable.
#[test]
fn orphan_collateral_swept_when_last_husk_closes() {
	use frame::traits::fungible::Mutate;
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// The handler resolves native DOT; fund the destination so a one-unit
		// deposit cannot fail below the existential deposit.
		assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&ORPHAN_DEST, 100));
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 999, 500, rate_pct(5, 100)));
		assert_ok!(open(3, DOT, PUSD, 5_000, 500, rate_pct(5, 100))); // liquidatee

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		// 501 of collateral over stakes 1_000 + 999: the double floor
		// (per-stake, then × stake) distributes 250 + 250 and strands 1.
		assert_ok!(redistribute_for_test(DOT, PUSD, 3, 501));
		assert_eq!(branch_state(DOT, PUSD).unwrap().ownerless_collateral, 1);

		// Empty the branch: overpay-repay both recipients, close both husks.
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&1, 1_000));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&2, 1_000));
		for who in [1u64, 2] {
			assert_ok!(crate::Pallet::<Test>::repay_for(
				RuntimeOrigin::signed(who),
				DOT,
				PUSD,
				who,
				10_000
			));
		}
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), DOT, PUSD, None));
		let dest_before = collateral_balance(DOT, ORPHAN_DEST);
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(2), DOT, PUSD, None));

		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.ownerless_collateral, 0);
		assert_eq!(state.total_collateral, 0);
		assert_eq!(collateral_balance(DOT, ORPHAN_DEST), dest_before + 1);
		let redistribution_account = crate::Pallet::<Test>::redistribution_account(&DOT, &PUSD);
		assert_eq!(held(DOT, redistribution_account), 0);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::OrphanCollateralSwept {
			collateral_id: DOT,
			stable_id: PUSD,
			amount: 1,
		}));

		// The debt-dust twin was swept alongside; heal it and the market is
		// removable — the deadlock this sweep exists to prevent.
		assert_eq!(bad_debt(), 1);
		assert_eq!(heal(1), 0);
		assert_ok!(crate::Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
	});
}

// Per-vault flooring can also strand debt in the pending-redistribution
// counter itself: 501 of debt per-staked over two equal stakes of 1_000 gives
// each consumer floor(250.5) = 250, leaving 1 that no vault can ever consume.
// The close sweep folds it into bad debt — its stablecoin is circulating, so
// only healing may retire it — and the emptied market stays removable.
#[test]
fn pending_residue_swept_to_bad_debt_when_last_husk_closes() {
	use frame::traits::fungible::Mutate;
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(3, DOT, PUSD, 5_000, 500, rate_pct(5, 100))); // liquidatee

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		let redistributed = redistribute_for_test(DOT, PUSD, 3, 0).expect("redistributes");
		// 501 over 2_000 stakes is exact per stake, so no record-time dust.
		assert_eq!(redistributed, 501);
		assert_eq!(branch_state(DOT, PUSD).unwrap().ownerless_debt, 0);

		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&1, 1_000));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&2, 1_000));
		for who in [1u64, 2] {
			assert_ok!(crate::Pallet::<Test>::repay_for(
				RuntimeOrigin::signed(who),
				DOT,
				PUSD,
				who,
				10_000
			));
		}
		// Both consumers took floor(0.2505 × 1_000) = 250; one unit is stranded.
		assert_eq!(branch_state(DOT, PUSD).unwrap().debt.pending_redistribution_principal, 1);

		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), DOT, PUSD, None));
		let before_sweep = <crate::Pallet<Test> as VaultInterface>::stablecoin_debt(&PUSD);
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(2), DOT, PUSD, None));

		let state = branch_state(DOT, PUSD).unwrap();
		assert_eq!(state.debt.pending_redistribution_principal, 0);
		assert_eq!(state.debt.weighted_principal_sum, 0);
		assert_eq!(bad_debt(), 1, "the unconsumable residue became healable bad debt");
		assert_eq!(
			<crate::Pallet<Test> as VaultInterface>::stablecoin_debt(&PUSD),
			before_sweep,
			"sweeping the pending residue into bad debt must not change stablecoin debt"
		);
		assert_eq!(heal(1), 0);
		assert_ok!(crate::Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
	});
}

// The custody boundary of the sweep: if the redistribution account's hold
// falls short of the swept amount, settlement must not proceed. The defensive
// path panics in tests; in production builds the extrinsic returns
// `Corruption` and the dispatch layer rolls the whole close back.
#[test]
#[should_panic = "Defensive failure has been triggered"]
fn orphan_sweep_hold_shortfall_is_defensive() {
	use frame::traits::{fungible::Mutate, fungibles::MutateHold, tokens::Precision};
	build_and_execute_defensive(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 999, 500, rate_pct(5, 100)));
		assert_ok!(open(3, DOT, PUSD, 5_000, 500, rate_pct(5, 100))); // liquidatee

		set_price(DOT, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(redistribute_for_test(DOT, PUSD, 3, 501));

		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&1, 1_000));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&2, 1_000));
		for who in [1u64, 2] {
			assert_ok!(crate::Pallet::<Test>::repay_for(
				RuntimeOrigin::signed(who),
				DOT,
				PUSD,
				who,
				10_000
			));
		}
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), DOT, PUSD, None));

		// Corrupt custody: strip the stranded unit off the redistribution hold.
		let redistribution_account = crate::Pallet::<Test>::redistribution_account(&DOT, &PUSD);
		assert_ok!(<VaultCollateralAssets as MutateHold<AccountId>>::release(
			DOT,
			&HoldReason::VaultCollateral.into(),
			&redistribution_account,
			1,
			Precision::Exact,
		));

		// The terminal close finds the hold short and must abort.
		let _ = crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(2), DOT, PUSD, None);
	});
}

// The multi-asset twin of the terminal sweep: `TOKEN_X` custody runs through
// `pallet-assets` + `pallet-assets-holder` (the union's issued side), not
// native `Balances`. Same numbers, same residue, same routing.
#[test]
fn orphan_collateral_swept_for_issued_collateral() {
	use frame::traits::{
		fungible::Mutate,
		fungibles::{Balanced, Mutate as FungiblesMutate},
	};
	build_and_execute(|| {
		register_market(TOKEN_X, PUSD);
		// Give the handler destination an asset account to receive into.
		assert_ok!(<Assets as FungiblesMutate<AccountId>>::mint_into(TOKEN_X_ID, &ORPHAN_DEST, 1));
		let redistribution_account = crate::Pallet::<Test>::redistribution_account(&TOKEN_X, &PUSD);
		assert_eq!(collateral_balance(TOKEN_X, redistribution_account), 1, "branch seed");
		assert_ok!(open(1, TOKEN_X, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, TOKEN_X, PUSD, 999, 500, rate_pct(5, 100)));
		assert_ok!(open(3, TOKEN_X, PUSD, 5_000, 500, rate_pct(5, 100))); // liquidatee

		set_price(TOKEN_X, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(redistribute_for_test(TOKEN_X, PUSD, 3, 501));
		assert_eq!(branch_state(TOKEN_X, PUSD).unwrap().ownerless_collateral, 1);

		set_price(TOKEN_X, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&1, 1_000));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&2, 1_000));
		for who in [1u64, 2] {
			assert_ok!(crate::Pallet::<Test>::repay_for(
				RuntimeOrigin::signed(who),
				TOKEN_X,
				PUSD,
				who,
				10_000
			));
			assert_ok!(crate::Pallet::<Test>::close_vault(
				RuntimeOrigin::signed(who),
				TOKEN_X,
				PUSD,
				None
			));
		}

		assert_eq!(collateral_balance(TOKEN_X, ORPHAN_DEST), 2, "pre-fund 1 + swept 1");
		let state = branch_state(TOKEN_X, PUSD).unwrap();
		assert_eq!(state.total_collateral, 0);
		assert_eq!(state.ownerless_collateral, 0);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::OrphanCollateralSwept {
			collateral_id: TOKEN_X,
			stable_id: PUSD,
			amount: 1,
		}));

		// Heal the debt-side dust; the emptied market is removable.
		let credit = <Assets as Balanced<AccountId>>::issue(PUSD, 1);
		assert_eq!(<crate::Pallet<Test> as VaultInterface>::heal(&TOKEN_X, credit).peek(), 0);
		assert_ok!(crate::Pallet::<Test>::remove_branch(
			RuntimeOrigin::signed(ADMIN),
			TOKEN_X,
			PUSD
		));
	});
}

// Accepted dust policy: `resolve` creates the destination account when the
// amount meets the asset's minimum balance, so delivery only fails for
// sub-minimum dust. On an asset with minimum balance 100, the one-unit sweep
// cannot be placed: the credit is dropped and the dust burns. The sweep still
// settles, the event still fires, and the market still empties.
#[test]
fn orphan_collateral_burns_when_handler_cannot_place_it() {
	use frame::traits::{
		fungible::Mutate,
		fungibles::{Balanced, Inspect, Mutate as FungiblesMutate},
	};
	const HI_ED_ID: AssetIdForAssets = 777;
	const HI_ED: AssetId = AssetId::WithId(HI_ED_ID);
	build_and_execute(|| {
		assert_ok!(Assets::force_create(RuntimeOrigin::root(), HI_ED_ID, 1, true, 100));
		for who in 1u64..=3 {
			assert_ok!(<Assets as FungiblesMutate<AccountId>>::mint_into(
				HI_ED_ID, &who, 1_000_000
			));
		}
		register_market_with(
			HI_ED,
			PUSD,
			FixedU128::from_rational(10u128, 1u128),
			default_branch_config(),
		);
		// Registration supplied the free minimum balance needed by the
		// redistribution account. `ORPHAN_DEST` stays unfunded on purpose.
		let redistribution_account = crate::Pallet::<Test>::redistribution_account(&HI_ED, &PUSD);
		assert_eq!(collateral_balance(HI_ED, redistribution_account), 100, "branch seed");
		assert_ok!(open(1, HI_ED, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, HI_ED, PUSD, 999, 500, rate_pct(5, 100)));
		assert_ok!(open(3, HI_ED, PUSD, 5_000, 500, rate_pct(5, 100))); // liquidatee

		set_price(HI_ED, FixedU128::from_rational(5u128, 100u128));
		assert_ok!(redistribute_for_test(HI_ED, PUSD, 3, 501));

		set_price(HI_ED, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&1, 1_000));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&2, 1_000));
		for who in [1u64, 2] {
			assert_ok!(crate::Pallet::<Test>::repay_for(
				RuntimeOrigin::signed(who),
				HI_ED,
				PUSD,
				who,
				10_000
			));
		}
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), HI_ED, PUSD, None));
		let issuance_before = <Assets as Inspect<AccountId>>::total_issuance(HI_ED_ID);
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(2), HI_ED, PUSD, None));

		assert_eq!(collateral_balance(HI_ED, ORPHAN_DEST), 0);
		assert_eq!(
			<Assets as Inspect<AccountId>>::total_issuance(HI_ED_ID),
			issuance_before - 1,
			"the dropped credit burned the dust"
		);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::OrphanCollateralSwept {
			collateral_id: HI_ED,
			stable_id: PUSD,
			amount: 1,
		}));

		let credit = <Assets as Balanced<AccountId>>::issue(PUSD, 1);
		assert_eq!(<crate::Pallet<Test> as VaultInterface>::heal(&HI_ED, credit).peek(), 0);
		assert_ok!(crate::Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), HI_ED, PUSD));
	});
}

// Heal is infallible: a market unknown on either axis touches no ledger and
// hands the whole credit back.
#[test]
fn heal_unknown_market_returns_the_credit_whole() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		record(100);

		// Unknown collateral axis.
		let credit = <Assets as frame::traits::fungibles::Balanced<AccountId>>::issue(PUSD, 10);
		let surplus = <crate::Pallet<Test> as VaultInterface>::heal(&TOKEN_X, credit);
		assert_eq!(surplus.peek(), 10);
		drop(surplus);

		// Unknown stable axis: the credit's coin has no market on `DOT`.
		let credit = <Assets as frame::traits::fungibles::Balanced<AccountId>>::issue(EUSD, 10);
		let surplus = <crate::Pallet<Test> as VaultInterface>::heal(&DOT, credit);
		assert_eq!(surplus.peek(), 10);
		assert_eq!(surplus.asset(), EUSD);
		drop(surplus);

		assert_eq!(bad_debt(), 100, "the registered market is untouched");
	});
}
