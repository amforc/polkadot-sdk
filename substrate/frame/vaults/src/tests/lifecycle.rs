//! Lifecycle smoke tests: a fast layer covering branch registration, vault
//! open/close happy paths and validation rejections, multi-asset routing,
//! frozen-mode blocking, and same-rate LIFO ordering. The deeper per-area
//! coverage lives in the sibling modules.

use crate::{
	mock::*,
	tests::{rate_pct, vault_status},
	types::BranchConfigUpdate,
};
use pallet_linked_list::SortedListInterface;

// Lazy redistribution has constant cost per vault operation, so it does not require a market
// vault limit. The fixture exceeds the former limit of 64.
#[test]
fn open_vault_count_is_not_capped_by_redistribution() {
	use frame::traits::fungible::Mutate;

	build_and_execute(|| {
		register_market(DOT, PUSD);
		for owner in 1_000..1_064 {
			assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&owner, 2_000));
			let rate = rate_pct(u128::from(owner - 999), 100);
			assert_ok!(open(owner, DOT, PUSD, 1_000, 500, rate));
		}
		assert_eq!(branch_state(DOT, PUSD).unwrap().vault_count, 64);

		let owner = 1_064;
		assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&owner, 2_000));
		assert_ok!(open(owner, DOT, PUSD, 1_000, 500, rate_pct(65, 100)));
		assert_eq!(branch_state(DOT, PUSD).unwrap().vault_count, 65);
		assert!(vault_exists(DOT, PUSD, owner));
	});
}

#[test]
fn register_branch_creates_state() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		let state = branch_state(DOT, PUSD).expect("branch registered");
		assert_eq!(state.total_collateral, 0);
		assert!(!state.is_frozen());
	});
}

#[test]
fn create_branch_requires_asset_owner_or_root() {
	build_and_execute(|| {
		// A non-owner of the stable asset cannot create a market for it.
		assert_noop!(
			crate::Pallet::<Test>::create_branch(
				RuntimeOrigin::signed(2),
				TOKEN_X,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			DispatchError::BadOrigin
		);
		// The stable asset's owner (acct 1 in genesis) can, locking a deposit.
		set_price(TOKEN_X, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(crate::Pallet::<Test>::create_branch(
			RuntimeOrigin::signed(1),
			TOKEN_X,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));
	});
}

#[test]
fn create_branch_rejects_unknown_asset() {
	build_and_execute(|| {
		let unknown = AssetId::WithId(999_999);
		set_price(unknown.clone(), FixedU128::from_rational(10u128, 1u128));
		assert_noop!(
			crate::Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				unknown,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			crate::Error::<Test>::UnknownCollateral
		);
	});
}

#[test]
fn create_branch_rejects_duplicate_collateral() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_noop!(
			crate::Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			crate::Error::<Test>::BranchAlreadyRegistered
		);
	});
}

#[test]
fn open_vault_holds_collateral_and_mints_pusd() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// 1000 DOT @ $10 = $10000 collateral; borrow 1000 pUSD with 5% rate.
		assert_ok!(open(1, DOT, PUSD, 1_000, 1_000, rate_pct(5, 100)));
		let v = vault(DOT, PUSD, 1);
		assert_eq!(v.debt.principal, 1_000);
		assert!(vault_status(DOT, PUSD, 1).is_active());
		assert_eq!(stable_balance(PUSD, 1), 1_000);
		assert_eq!(held(DOT, 1), 1_000);
		// Rate index contains the vault.
		assert!(<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&rate_list(DOT, PUSD),
			&1
		));
	});
}

#[test]
fn open_vault_rejects_existing_owner_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_noop!(
			open(1, DOT, PUSD, 2_000, 500, rate_pct(5, 100)),
			crate::Error::<Test>::VaultAlreadyExists
		);
	});
}

#[test]
fn open_vault_below_min_debt_rejected() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_noop!(
			open(1, DOT, PUSD, 1_000, 100, rate_pct(5, 100)), // < min_debt 200
			crate::Error::<Test>::DebtBelowMinimum
		);
	});
}

#[test]
fn open_vault_rate_out_of_bounds_rejected() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Below the branch `minimum_borrow_rate` (0.1%).
		assert_noop!(
			open(1, DOT, PUSD, 1_000, 500, rate_pct(0, 1)),
			crate::Error::<Test>::RateOutOfBounds
		);
		// Above the branch `maximum_borrow_rate` (400%): the cap is the
		// per-branch config bound, not a hard-coded 100%.
		assert_noop!(
			open(1, DOT, PUSD, 1_000, 500, rate_pct(401, 100)),
			crate::Error::<Test>::RateOutOfBounds
		);
	});
}

#[test]
fn open_vault_exceeds_ceiling_rejected() {
	build_and_execute(|| {
		// Own the ceiling under test rather than depend on the mock default.
		let config = BranchConfig { debt_ceiling: 100_000_000, ..default_branch_config() };
		register_market_with(DOT, PUSD, FixedU128::from_rational(10, 1), config);
		assert_noop!(
			open(1, DOT, PUSD, 100_000_000_000, 200_000_000, rate_pct(5, 100)),
			crate::Error::<Test>::DebtCeilingExceeded
		);
	});
}

#[test]
fn open_vault_below_icr_rejected() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// 100 DOT @ $10 = $1000; borrow 1000 pUSD => CR=100% < ICR 120%.
		assert_err!(
			open(1, DOT, PUSD, 100, 1_000, rate_pct(5, 100)),
			crate::Error::<Test>::UnsafeCollateralizationRatio
		);
	});
}

#[test]
fn defensive_manager_can_only_tighten_selected_risk_parameters() {
	build_and_execute(|| {
		register_market(DOT, PUSD);

		assert_ok!(crate::Pallet::<Test>::set_param(
			RuntimeOrigin::signed(EMERGENCY_ADMIN),
			DOT,
			PUSD,
			BranchConfigUpdate::MinimumCollateralizationRatio(rate_pct(120, 100))
		));
		assert_noop!(
			crate::Pallet::<Test>::set_param(
				RuntimeOrigin::signed(EMERGENCY_ADMIN),
				DOT,
				PUSD,
				BranchConfigUpdate::MinimumCollateralizationRatio(rate_pct(109, 100))
			),
			crate::Error::<Test>::DefensiveActionNotDefensive
		);

		assert_ok!(crate::Pallet::<Test>::set_param(
			RuntimeOrigin::signed(EMERGENCY_ADMIN),
			DOT,
			PUSD,
			BranchConfigUpdate::DebtCeiling(50_000_000)
		));
		assert_noop!(
			crate::Pallet::<Test>::set_param(
				RuntimeOrigin::signed(EMERGENCY_ADMIN),
				DOT,
				PUSD,
				BranchConfigUpdate::DebtCeiling(200_000_000)
			),
			crate::Error::<Test>::DefensiveActionNotDefensive
		);

		assert_noop!(
			crate::Pallet::<Test>::set_param(
				RuntimeOrigin::signed(EMERGENCY_ADMIN),
				DOT,
				PUSD,
				BranchConfigUpdate::MinimumDebt(300)
			),
			crate::Error::<Test>::NotBranchAdmin
		);
	});
}

#[test]
fn same_rate_lifo_redemption_order() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Three vaults at the same rate, in order: 1, 2, 3.
		for who in 1u64..=3 {
			assert_ok!(open(who, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		}
		// Tail-first iteration produces 3, 2, 1 (LIFO).
		let tail = LinkedList::iter_from_tail(rate_list(DOT, PUSD), 5);
		assert_eq!(tail, alloc::vec![3, 2, 1]);
	});
}

#[test]
fn open_vault_on_multi_asset_branch() {
	// Exercises the right-hand side of the `fungible::UnionOf`: opening a
	// vault on `TOKEN_X` (a foreign asset in `pallet-assets`) instead of
	// native DOT. Confirms the union routes hold operations to
	// `pallet-assets-holder` for non-native ids.
	build_and_execute(|| {
		register_market(TOKEN_X, PUSD);
		assert_ok!(open(1, TOKEN_X, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_eq!(held(TOKEN_X, 1), 1_000);
	});
}

#[test]
fn frozen_branch_blocks_user_ops() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			true
		));
		assert_noop!(
			open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)),
			crate::Error::<Test>::BranchFrozen
		);
	});
}

fn freeze_by_governance() {
	assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
		RuntimeOrigin::signed(ADMIN),
		DOT,
		PUSD,
		true
	));
	assert_eq!(branch_mode(DOT, PUSD), Some(BranchMode::Frozen));
}

// A collateral deposit needs no price and only lowers risk, so a freeze must not block an owner
// from protecting the vault before liquidations resume.
#[test]
fn frozen_branch_accepts_collateral_deposit() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		freeze_by_governance();

		assert_ok!(crate::Pallet::<Test>::deposit_collateral_for(
			RuntimeOrigin::signed(2),
			DOT,
			PUSD,
			1,
			100
		));

		assert_eq!(held(DOT, 1), 1_100);
		assert_eq!(vault(DOT, PUSD, 1).collateral, 1_100);
		assert_eq!(branch_state(DOT, PUSD).expect("state").total_collateral, 1_100);
		assert_eq!(
			branch_mode(DOT, PUSD),
			Some(BranchMode::Frozen),
			"the deposit does not unfreeze"
		);
	});
}

// A partial repayment burns stablecoin without a price, so it stays available while frozen.
#[test]
fn frozen_branch_accepts_partial_repayment() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		let debt_before = vault(DOT, PUSD, 1).debt.total();
		freeze_by_governance();

		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			Some(100)
		));

		assert_eq!(vault(DOT, PUSD, 1).debt.total(), debt_before - 100);
		assert!(vault_status(DOT, PUSD, 1).is_active());
		assert_eq!(branch_mode(DOT, PUSD), Some(BranchMode::Frozen));
	});
}

// A full payoff that leaves collateral behind only parks a husk; it closes nothing, so the freeze
// does not stand in its way.
#[test]
fn frozen_branch_accepts_full_repayment_that_leaves_a_husk() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// Cover the upfront fee that the borrowed principal does not include.
		mint_stable(PUSD, 1, 10);
		freeze_by_governance();

		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, None));

		let husk = vault(DOT, PUSD, 1);
		assert_eq!(husk.debt.total(), 0);
		assert_eq!(husk.collateral, 1_000);
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert!(
			!<LinkedList as SortedListInterface<VaultList, u64>>::contains(
				&rate_list(DOT, PUSD),
				&1
			),
			"the husk left the rate index"
		);
		assert!(vault_exists(DOT, PUSD, 1), "a frozen branch never closes a row");
	});
}

// Closing a vault is a lifecycle exit the freeze must hold back, whatever the freeze reason. The
// only repayment that closes is one on a vault that already lost all its collateral.
#[test]
fn frozen_branch_rejects_repayment_that_would_close_the_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// Drain the collateral through a settlement that keeps the debt above the minimum, leaving
		// a debt-bearing row with nothing held.
		assert_ok!(redeem_step(DOT, PUSD, 1, 7, 200, 1_000));
		assert_eq!(vault(DOT, PUSD, 1).collateral, 0);
		freeze_by_governance();

		assert_noop!(
			crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, None),
			crate::Error::<Test>::BranchFrozen
		);

		// Once the freeze lifts, the same payoff closes the empty row.
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			false
		));
		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, None));
		assert!(!vault_exists(DOT, PUSD, 1));
	});
}

#[test]
fn refresh_branch_persists_frozen_on_oracle_failure() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		MockOracleAvailable::set(false);
		assert_ok!(crate::Pallet::<Test>::refresh_branch(RuntimeOrigin::signed(99), DOT, PUSD));
		let state = branch_state(DOT, PUSD).expect("state");
		let frozen = state.frozen.expect("frozen persisted");
		assert!(matches!(frozen.reason, crate::FrozenReason::OracleFailure));
	});
}

// External observers must not see the most permissive mode while prices are
// unknowable: `mode()` reports `Frozen` for a failing oracle even before
// `refresh_branch` persists the freeze.
#[test]
fn mode_reports_frozen_while_oracle_unavailable() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_eq!(branch_mode(DOT, PUSD), Some(BranchMode::Normal));
		MockOracleAvailable::set(false);
		assert_eq!(branch_mode(DOT, PUSD), Some(BranchMode::Frozen));
		MockOracleAvailable::set(true);
		assert_eq!(branch_mode(DOT, PUSD), Some(BranchMode::Normal));
	});
}

#[test]
fn refresh_branch_clears_oracle_frozen() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		MockOracleAvailable::set(false);
		assert_ok!(crate::Pallet::<Test>::refresh_branch(RuntimeOrigin::signed(99), DOT, PUSD));
		assert!(branch_state(DOT, PUSD).unwrap().is_frozen());
		// Oracle still down → second refresh is a no-op (already frozen for
		// the same reason).
		assert_ok!(crate::Pallet::<Test>::refresh_branch(RuntimeOrigin::signed(99), DOT, PUSD));
		assert!(branch_state(DOT, PUSD).unwrap().is_frozen());
		// Restore oracle and refresh → unfreezes.
		MockOracleAvailable::set(true);
		assert_ok!(crate::Pallet::<Test>::refresh_branch(RuntimeOrigin::signed(99), DOT, PUSD));
		assert!(!branch_state(DOT, PUSD).unwrap().is_frozen());
	});
}

#[test]
fn refresh_branch_does_not_clear_governance_frozen() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			true
		));
		assert_ok!(crate::Pallet::<Test>::refresh_branch(RuntimeOrigin::signed(99), DOT, PUSD));
		assert!(branch_state(DOT, PUSD).unwrap().is_frozen());
	});
}

#[test]
fn governance_clear_clears_governance_frozen() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Defensive (acct 999) cannot clear governance Frozen — needs Full.
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			true
		));
		assert_noop!(
			crate::Pallet::<Test>::set_governance_frozen(
				RuntimeOrigin::signed(EMERGENCY_ADMIN),
				DOT,
				PUSD,
				false
			),
			crate::Error::<Test>::NotBranchAdmin
		);
		// Full clears governance Frozen.
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			false
		));
		assert!(!branch_state(DOT, PUSD).unwrap().is_frozen());
	});
}

#[test]
fn governance_clear_is_noop_for_oracle_frozen() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		MockOracleAvailable::set(false);
		assert_ok!(crate::Pallet::<Test>::refresh_branch(RuntimeOrigin::signed(99), DOT, PUSD));
		assert!(branch_state(DOT, PUSD).unwrap().is_frozen());
		// Governance clear refuses oracle-Frozen state — branch stays frozen.
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			false
		));
		assert!(branch_state(DOT, PUSD).unwrap().is_frozen());
	});
}

#[test]
fn frozen_poke_pins_interest_time_without_minting() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			true
		));
		let before = branch_state(DOT, PUSD).expect("branch state");

		let elapsed: Moment = 24 * 3_600 * 1_000;
		advance_time(elapsed);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));

		let after = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(after.debt.minted_interest, before.debt.minted_interest, "no mint while frozen");
		assert_eq!(
			after.debt.last_interest_time, before.debt.last_interest_time,
			"interest clock pinned across the frozen window"
		);
		assert!(after.is_frozen(), "poke does not clear Frozen");
	});
}
