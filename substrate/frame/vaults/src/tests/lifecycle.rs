//! Lifecycle smoke tests: a fast layer covering branch registration, vault
//! open/close happy paths and validation rejections, and frozen-mode blocking.
//! The deeper per-area coverage lives in the sibling modules.

use crate::{
	mock::*,
	tests::{percent, vault_status, ONE_DAY_MS, ONE_YEAR_MS},
	types::BranchConfigUpdate,
};
use pallet_linked_list::SortedListInterface;

// Only the stable asset's owner or root may register a branch, and a pair registers once. The
// positive owner path and its deposit are covered in `governance.rs`.
#[test]
fn create_branch_rejects_non_owner_and_duplicate_pair() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// A non-owner of the stable asset cannot create a branch for it.
		assert_noop!(
			crate::Pallet::<Test>::create_branch(
				RuntimeOrigin::signed(2),
				TOKEN_X,
				STABLE,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			DispatchError::BadOrigin
		);

		assert_noop!(
			crate::Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				STABLE,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			crate::Error::<Test>::BranchAlreadyRegistered
		);
	});
}

// Each position violates one opening constraint and must leave storage unchanged.
#[test]
fn open_vault_rejects_invalid_positions() {
	let config = BranchConfig { debt_ceiling: 100_000_000, ..default_branch_config() };
	ExtBuilder::default().branch_with(DOT, STABLE, config).build_and_execute(|| {
		assert_noop!(
			open(1, DOT, STABLE, 1_000, 100, percent(5)),
			crate::Error::<Test>::DebtBelowMinimum
		);
		assert_noop!(
			open(1, DOT, STABLE, 1_000, 500, FixedU128::from_rational(0, 1)),
			crate::Error::<Test>::RateOutOfBounds
		);
		assert_noop!(
			open(1, DOT, STABLE, 1_000, 500, percent(401)),
			crate::Error::<Test>::RateOutOfBounds
		);
		assert_noop!(
			open(1, DOT, STABLE, 100_000_000_000, 200_000_000, percent(5)),
			crate::Error::<Test>::DebtCeilingExceeded
		);
		assert_noop!(
			open(1, DOT, STABLE, 100, 1_000, percent(5)),
			crate::Error::<Test>::UnsafeCollateralizationRatio
		);

		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));

		assert_noop!(
			open(1, DOT, STABLE, 2_000, 500, percent(5)),
			crate::Error::<Test>::VaultAlreadyExists
		);
	});
}

#[test]
fn defensive_manager_can_only_tighten_selected_risk_parameters() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(crate::Pallet::<Test>::set_param(
			RuntimeOrigin::signed(EMERGENCY_ADMIN),
			DOT,
			STABLE,
			BranchConfigUpdate::MinimumCollateralizationRatio(percent(120))
		));
		assert_noop!(
			crate::Pallet::<Test>::set_param(
				RuntimeOrigin::signed(EMERGENCY_ADMIN),
				DOT,
				STABLE,
				BranchConfigUpdate::MinimumCollateralizationRatio(percent(109))
			),
			crate::Error::<Test>::DefensiveActionNotDefensive
		);

		assert_ok!(crate::Pallet::<Test>::set_param(
			RuntimeOrigin::signed(EMERGENCY_ADMIN),
			DOT,
			STABLE,
			BranchConfigUpdate::DebtCeiling(50_000_000)
		));
		assert_noop!(
			crate::Pallet::<Test>::set_param(
				RuntimeOrigin::signed(EMERGENCY_ADMIN),
				DOT,
				STABLE,
				BranchConfigUpdate::DebtCeiling(200_000_000)
			),
			crate::Error::<Test>::DefensiveActionNotDefensive
		);

		assert_noop!(
			crate::Pallet::<Test>::set_param(
				RuntimeOrigin::signed(EMERGENCY_ADMIN),
				DOT,
				STABLE,
				BranchConfigUpdate::MinimumDebt(300)
			),
			crate::Error::<Test>::NotBranchAdmin
		);
	});
}

/// Opens vaults 1 and 2, then redeems vault 1 down to Dormant dust while vault 2 stays Active.
fn open_active_and_dormant_vaults() {
	assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(50)));
	assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(60)));
	assert_ok!(redeem(DOT, STABLE, 3, 350));
	assert!(vault_status(DOT, STABLE, 1).is_dormant());
}

// A frozen branch rejects every operation that raises risk or needs a price. Each operation
// succeeds right before the freeze, so the freeze is the only guard left to reject it.
#[test]
fn frozen_branch_blocks_user_ops() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		open_active_and_dormant_vaults();
		hypothetically!({
			assert_ok!(open(3, DOT, STABLE, 1_000, 500, percent(5)));
		});
		hypothetically!({
			assert_ok!(borrow(2, DOT, STABLE, 100, None));
		});
		hypothetically!({
			assert_ok!(withdraw_collateral(2, DOT, STABLE, 1, None));
		});
		hypothetically!({
			assert_ok!(change_rate(2, DOT, STABLE, percent(70)));
		});
		hypothetically!({
			assert_ok!(nominate_dormant(9, DOT, STABLE, 1));
		});

		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, true));

		assert_noop!(
			open(3, DOT, STABLE, 1_000, 500, percent(5)),
			crate::Error::<Test>::BranchFrozen
		);
		assert_noop!(borrow(2, DOT, STABLE, 100, None), crate::Error::<Test>::BranchFrozen);
		assert_noop!(
			withdraw_collateral(2, DOT, STABLE, 1, None),
			crate::Error::<Test>::BranchFrozen
		);
		assert_noop!(change_rate(2, DOT, STABLE, percent(70)), crate::Error::<Test>::BranchFrozen);
		assert_noop!(nominate_dormant(9, DOT, STABLE, 1), crate::Error::<Test>::BranchFrozen);
	});
}

// A frozen branch rejects dormant activation too. It succeeds right before the freeze, so the
// freeze is the only guard left to reject it.
#[test]
fn frozen_branch_blocks_dormant_activation() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		open_active_and_dormant_vaults();
		// The elapsed time lets the Dormant dust of 155 accrue 78 at 50%, past the 200 minimum
		// that activation needs and nomination forbids, so `frozen_branch_blocks_user_ops`
		// checks nomination before any time passes.
		advance_time(ONE_YEAR_MS);
		let activate =
			|| activate_dormant(9, DOT, STABLE, 1).map(|_| ()).map_err(|error| error.error);
		hypothetically!({
			assert_ok!(activate());
		});

		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, true));

		assert_noop!(activate(), crate::Error::<Test>::BranchFrozen);
	});
}

fn freeze_by_governance() {
	assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, true));
	assert_eq!(branch_mode(DOT, STABLE), Some(BranchMode::Frozen));
}

// A freeze blocks what raises risk or needs a price, but owners must still be able to protect a
// vault before liquidations resume. A deposit and a repayment need no price and only lower risk. A
// full payoff that leaves collateral behind parks a husk and closes nothing, so it is allowed too,
// while the close that would release the collateral stays blocked.
#[test]
fn frozen_branch_accepts_risk_reducing_ops() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		// Cover the upfront fee that the borrowed principal does not include.
		mint_stable(STABLE, 1, 10);
		freeze_by_governance();

		assert_ok!(deposit_collateral(2, DOT, STABLE, 1, 100));

		assert_eq!(held(DOT, 1), 1_100);
		assert_eq!(vault(DOT, STABLE, 1).collateral, 1_100);
		assert_eq!(
			branch_mode(DOT, STABLE),
			Some(BranchMode::Frozen),
			"the deposit does not unfreeze"
		);

		let debt_before = vault(DOT, STABLE, 1).debt.total();
		assert_ok!(repay(1, DOT, STABLE, 1, Some(100)));

		assert_eq!(vault(DOT, STABLE, 1).debt.total(), debt_before - 100);
		assert!(vault_status(DOT, STABLE, 1).is_active());
		assert_eq!(branch_mode(DOT, STABLE), Some(BranchMode::Frozen));

		assert_ok!(repay(1, DOT, STABLE, 1, None));

		let husk = vault(DOT, STABLE, 1);
		assert_eq!(husk.debt.total(), 0);
		assert_eq!(husk.collateral, 1_100);
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		assert!(
			!<LinkedList as SortedListInterface<VaultList, AccountId>>::contains(
				&VaultList::Rate(DOT, STABLE),
				&1
			),
			"the husk left the rate index"
		);
		assert!(vault_exists(DOT, STABLE, 1), "a frozen branch retains collateral");
		assert_noop!(close_vault(1, DOT, STABLE, None), crate::Error::<Test>::BranchFrozen);
	});
}

/// Redeems all of vault 1's collateral, leaving a row that holds only debt.
fn empty_vault_one() {
	assert_ok!(redeem_step(DOT, STABLE, 1, 7, 200, 1_000));
	assert_eq!(vault(DOT, STABLE, 1).collateral, 0);
}

/// Repays the empty vault 1 in full and asserts that the repayment closed it without touching the
/// branch's collateral or freeze.
fn assert_repayment_closes_empty_vault_one() {
	let before = market_state(DOT, STABLE);

	assert_ok!(repay(1, DOT, STABLE, 1, None));

	assert!(!vault_exists(DOT, STABLE, 1));
	assert_eq!(vault_deposit_held(DOT, 1), 0);
	assert!(!<LinkedList as SortedListInterface<VaultList, AccountId>>::contains(
		&VaultList::Rate(DOT, STABLE),
		&1
	));
	let after = market_state(DOT, STABLE);
	assert_eq!(after.vault_count, before.vault_count - 1);
	assert_eq!(after.total_collateral, before.total_collateral);
	assert_eq!(after.frozen, before.frozen);
	assert_eq!(after.dormant_redemption_target, None);
}

// Empty-row cleanup remains available under either freeze reason and before an oracle failure has
// been persisted as a freeze. The oracle is down in every test below, and each freeze state is
// covered on both the last-vault and the nonempty-branch cleanup paths.
#[test]
fn repayment_closes_last_empty_vault_while_oracle_is_down() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		empty_vault_one();
		MockOracleAvailable::set(&false);

		assert_repayment_closes_empty_vault_one();
	});
}

#[test]
fn repayment_closes_last_empty_vault_under_oracle_freeze() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		empty_vault_one();
		MockOracleAvailable::set(&false);
		assert_ok!(refresh_branch(99, DOT, STABLE));

		assert_repayment_closes_empty_vault_one();
	});
}

#[test]
fn repayment_closes_last_empty_vault_under_governance_freeze() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		empty_vault_one();
		freeze_by_governance();
		MockOracleAvailable::set(&false);

		assert_repayment_closes_empty_vault_one();
	});
}

#[test]
fn repayment_closes_last_empty_vault_under_governance_and_oracle_freeze() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		empty_vault_one();
		freeze_by_governance();
		MockOracleAvailable::set(&false);
		assert_ok!(refresh_branch(99, DOT, STABLE));

		assert_repayment_closes_empty_vault_one();
	});
}

#[test]
fn repayment_closes_empty_vault_beside_another_while_oracle_is_down() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(5)));
		empty_vault_one();
		MockOracleAvailable::set(&false);

		assert_repayment_closes_empty_vault_one();
	});
}

#[test]
fn repayment_closes_empty_vault_beside_another_under_oracle_freeze() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(5)));
		empty_vault_one();
		MockOracleAvailable::set(&false);
		assert_ok!(refresh_branch(99, DOT, STABLE));

		assert_repayment_closes_empty_vault_one();
	});
}

#[test]
fn repayment_closes_empty_vault_beside_another_under_governance_freeze() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(5)));
		empty_vault_one();
		freeze_by_governance();
		MockOracleAvailable::set(&false);

		assert_repayment_closes_empty_vault_one();
	});
}

#[test]
fn repayment_closes_empty_vault_beside_another_under_governance_and_oracle_freeze() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(5)));
		empty_vault_one();
		freeze_by_governance();
		MockOracleAvailable::set(&false);
		assert_ok!(refresh_branch(99, DOT, STABLE));

		assert_repayment_closes_empty_vault_one();
	});
}

// External observers must not see the most permissive mode while prices are unknowable, so
// `mode()` reports `Frozen` for a failing oracle even before `refresh_branch` persists the freeze.
// Only a refresh with a working oracle lifts the persisted freeze; the governance clear does not.
#[test]
fn oracle_freeze_is_reported_persisted_and_cleared_only_by_refresh() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		assert_eq!(branch_mode(DOT, STABLE), Some(BranchMode::Normal));

		MockOracleAvailable::set(&false);

		assert_eq!(branch_mode(DOT, STABLE), Some(BranchMode::Frozen));
		assert_eq!(market_state(DOT, STABLE).frozen, None, "the derived freeze is not persisted");

		MockOracleAvailable::set(&true);

		assert_eq!(branch_mode(DOT, STABLE), Some(BranchMode::Normal));

		MockOracleAvailable::set(&false);
		assert_ok!(refresh_branch(99, DOT, STABLE));

		let frozen = market_state(DOT, STABLE).frozen.expect("freeze persisted");
		assert_eq!(frozen.reason, crate::FrozenReason::OracleFailure);

		// Oracle still down, so a second refresh is a no-op for the same reason.
		assert_ok!(refresh_branch(99, DOT, STABLE));

		assert!(market_state(DOT, STABLE).is_frozen());

		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, false));

		assert!(market_state(DOT, STABLE).is_frozen(), "governance cannot clear an oracle freeze");

		MockOracleAvailable::set(&true);
		assert_ok!(refresh_branch(99, DOT, STABLE));

		assert!(!market_state(DOT, STABLE).is_frozen());
	});
}

// A governance freeze must pin the interest clock: neither a branch refresh nor a vault poke may
// mint interest for the frozen window or clear the freeze.
#[test]
fn governance_freeze_pins_interest_across_refresh_and_poke() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, true));
		let before = market_state(DOT, STABLE);

		advance_time(ONE_DAY_MS);
		assert_ok!(refresh_branch(99, DOT, STABLE));

		let after = market_state(DOT, STABLE);
		assert!(after.is_frozen());
		assert_eq!(after.debt.minted_interest, before.debt.minted_interest, "no mint while frozen");

		assert_ok!(poke(9, DOT, STABLE, 1));

		let after = market_state(DOT, STABLE);
		assert_eq!(after.debt.minted_interest, before.debt.minted_interest, "no mint while frozen");
		assert_eq!(
			after.debt.last_interest_time, before.debt.last_interest_time,
			"interest clock pinned across the frozen window"
		);
		assert!(after.is_frozen(), "poke does not clear Frozen");
	});
}

#[test]
fn governance_clear_clears_governance_frozen() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// The emergency admin can pull the freeze but cannot clear it. That needs the full admin.
		assert_ok!(set_governance_frozen(EMERGENCY_ADMIN, DOT, STABLE, true));
		assert!(market_state(DOT, STABLE).is_frozen());

		assert_noop!(
			set_governance_frozen(EMERGENCY_ADMIN, DOT, STABLE, false),
			crate::Error::<Test>::NotBranchAdmin
		);

		// Full clears governance Frozen.
		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, false));

		assert!(!market_state(DOT, STABLE).is_frozen());
		crate::tests::assert_event(crate::Event::ModeChanged {
			collateral_id: DOT,
			stable_id: STABLE,
			old_mode: BranchMode::Normal,
			new_mode: BranchMode::Frozen,
		});
		crate::tests::assert_event(crate::Event::ModeChanged {
			collateral_id: DOT,
			stable_id: STABLE,
			old_mode: BranchMode::Frozen,
			new_mode: BranchMode::Normal,
		});
	});
}
