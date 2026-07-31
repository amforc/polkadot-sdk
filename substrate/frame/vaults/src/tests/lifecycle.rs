//! Lifecycle smoke tests: a fast layer covering branch registration, vault
//! open/close happy paths and validation rejections, multi-asset routing,
//! frozen-mode blocking, and same-rate LIFO ordering. The deeper per-area
//! coverage lives in the sibling modules.

use crate::{
	mock::*,
	pallet::Vaults,
	tests::{rate_pct, vault_status},
	types::BranchConfigUpdate,
};
use pallet_linked_list::SortedListInterface;

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
				default_branch_config()
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
			default_branch_config()
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
				default_branch_config()
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
				default_branch_config()
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
		let v = Vaults::<Test>::get((DOT, PUSD, 1)).expect("vault stored");
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

// A frozen market keeps its rescue flows: deposit and repay are price-free and
// risk-decreasing, so owners can defend their vaults before the market
// unfreezes and liquidation resumes. Risk-increasing flows stay gated. The
// freeze window is excised exactly: a year of frozen time accrues nothing
// (5% on 501 would be 25), so the repay settles at the freeze-entry debt of
// 501 (500 borrowed + 1 upfront fee).
#[test]
fn frozen_branch_allows_repay_and_deposit() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			true
		));
		advance_time(pusd_primitives::MILLIS_PER_YEAR);

		let held_before = held(DOT, 1);
		assert_ok!(crate::Pallet::<Test>::deposit_collateral_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			100
		));
		assert_eq!(held(DOT, 1), held_before + 100);

		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, 200));
		assert_eq!(Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().debt.total(), 301);

		assert_noop!(
			crate::Pallet::<Test>::borrow(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				100,
				None,
				None,
				Position::endpoints_only()
			),
			crate::Error::<Test>::BranchFrozen
		);
		assert_noop!(
			crate::Pallet::<Test>::withdraw_collateral(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				100,
				None
			),
			crate::Error::<Test>::BranchFrozen
		);
	});
}

// The oracle-failure freeze is the case that motivates the carve-out: repay
// needs no price, so it must not be hostage to a dead oracle.
#[test]
fn oracle_frozen_branch_allows_repay() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		MockOracleAvailable::set(false);
		assert_ok!(crate::Pallet::<Test>::refresh_branch(RuntimeOrigin::signed(99), DOT, PUSD));
		assert!(branch_state(DOT, PUSD).unwrap().is_frozen());

		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, 200));
		// 301 = 500 borrowed + 1 upfront fee − 200 repaid.
		assert_eq!(Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().debt.total(), 301);
	});
}

// The one repay a frozen market still rejects: the final repayment of an
// empty-collateral vault would close it, and closing (a removal that may move
// value and needs a price for its safety commit) stays gated on an unfrozen
// market for every freeze reason.
#[test]
fn frozen_branch_rejects_closing_repay() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(6, 100)));
		// A redemption that takes all debt and all collateral leaves a
		// zero-zero husk that only a closing repay can remove.
		let snapshot =
			<crate::Pallet<Test> as pusd_primitives::VaultInterface>::project_redemption_snapshot(
				&DOT, &PUSD, &1,
			)
			.expect("snapshot");
		assert_ok!(redeem_step(DOT, PUSD, 1, 9, snapshot.debt, snapshot.collateral));
		let husk = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		assert_eq!(husk.debt.total(), 0);
		assert_eq!(husk.collateral, 0);
		assert!(vault_status(DOT, PUSD, 1).is_dormant());

		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			true
		));
		assert_noop!(
			crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, 5),
			crate::Error::<Test>::BranchFrozen
		);

		// Unfrozen, the same call closes the husk.
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			false
		));
		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, 5));
		assert!(Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
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
