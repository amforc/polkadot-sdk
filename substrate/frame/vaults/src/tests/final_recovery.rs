//! FinalRecovery queue behavior backed by `pallet-linked-list`.

use crate::{
	mock::*,
	tests::{rate_pct, vault_status},
};
use pallet_linked_list::SortedListInterface;
use pusd_primitives::VaultInterface;

fn low_recovery_price() -> FixedU128 {
	FixedU128::from_rational(1u128, 10u128)
}

fn enter_recovery(who: AccountId, rate: FixedU128) {
	set_price(DOT, FixedU128::from_rational(10u128, 1u128));
	assert_ok!(open(who, DOT, PUSD, 1_000, 500, rate));
	set_price(DOT, low_recovery_price());
	assert_ok!(crate::Pallet::<Test>::enter_final_recovery(
		RuntimeOrigin::signed(99),
		DOT,
		PUSD,
		who
	));
}

#[test]
fn final_recovery_queue_is_fifo_across_multiple_vaults() {
	build_and_execute(|| {
		register_market(DOT, PUSD);

		enter_recovery(1, rate_pct(1, 100));
		enter_recovery(2, rate_pct(2, 100));
		enter_recovery(3, rate_pct(3, 100));

		assert_eq!(
			crate::Pallet::<Test>::final_recovery_queue(DOT, PUSD, 10),
			alloc::vec![1, 2, 3]
		);
		assert_eq!(
			<crate::Pallet<Test> as VaultInterface>::next_redemption_target(&DOT, &PUSD, None)
				.map(|(owner, _status)| owner),
			Some(1)
		);
	});
}

#[test]
fn enter_final_recovery_rejects_non_last_eligible_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		set_price(DOT, low_recovery_price());

		assert_noop!(
			crate::Pallet::<Test>::enter_final_recovery(RuntimeOrigin::signed(99), DOT, PUSD, 1),
			crate::Error::<Test>::NotLastEligibleVault
		);
	});
}

// A debt-free Dormant vault remains a redistribution recipient. Thus, another vault is not the
// last eligible recipient and cannot enter final recovery.
#[test]
fn debt_free_husk_keeps_the_branch_out_of_final_recovery() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// The lower rate makes account 1 the first redemption target.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(redeem(DOT, PUSD, 4, 501));
		let husk_before = vault(DOT, PUSD, 1);
		assert_eq!(husk_before.debt.total(), 0);
		assert_eq!(husk_before.collateral, 950);
		assert!(vault_status(DOT, PUSD, 1).is_dormant());

		set_price(DOT, FixedU128::from_rational(1u128, 2u128));
		assert_noop!(
			crate::Pallet::<Test>::enter_final_recovery(RuntimeOrigin::signed(99), DOT, PUSD, 2),
			crate::Error::<Test>::NotLastEligibleVault
		);

		assert_ok!(redistribute_for_test(DOT, PUSD, 2, held(DOT, 2)));
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));

		// The sole recipient must drain both pending pools.
		let husk = vault(DOT, PUSD, 1);
		assert_eq!(husk.debt.principal, 501);
		assert_eq!(husk.collateral, 950 + 1_000);
		let state = branch_state(DOT, PUSD).expect("state");
		assert_eq!(state.debt.pending_redistribution_principal, 0);
		assert_eq!(state.pending_redistribution_collateral, 0);
		// Redistribution does not reactivate a Dormant vault.
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert!(crate::Pallet::<Test>::final_recovery_queue(DOT, PUSD, 10).is_empty());
	});
}

#[test]
fn enter_final_recovery_rejects_vault_above_mcr() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Price $10 → CR = 1000·10/500 = 2000% ≫ MCR 110%, far too healthy for FR.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));

		assert_noop!(
			crate::Pallet::<Test>::enter_final_recovery(RuntimeOrigin::signed(99), DOT, PUSD, 1),
			crate::Error::<Test>::CollateralizationRatioTooHealthy
		);
	});
}

#[test]
fn final_recovery_middle_exit_splices_queue() {
	build_and_execute(|| {
		register_market(DOT, PUSD);

		enter_recovery(1, rate_pct(1, 100));
		enter_recovery(2, rate_pct(2, 100));
		enter_recovery(3, rate_pct(3, 100));

		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(crate::Pallet::<Test>::exit_final_recovery(
			RuntimeOrigin::signed(42),
			DOT,
			PUSD,
			2,
			Position::endpoints_only()
		));

		assert!(vault_status(DOT, PUSD, 2).is_active());
		assert_eq!(crate::Pallet::<Test>::final_recovery_queue(DOT, PUSD, 10), alloc::vec![1, 3]);
	});
}

#[test]
fn exit_final_recovery_rejects_when_cr_still_below_mcr() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		enter_recovery(1, rate_pct(5, 100));

		assert_noop!(
			crate::Pallet::<Test>::exit_final_recovery(
				RuntimeOrigin::signed(99),
				DOT,
				PUSD,
				1,
				Position::endpoints_only()
			),
			crate::Error::<Test>::CollateralizationRatioTooLow
		);
		assert!(vault_status(DOT, PUSD, 1).is_final_recovery());
		assert_eq!(crate::Pallet::<Test>::final_recovery_queue(DOT, PUSD, 10), alloc::vec![1]);
	});
}

#[test]
fn exit_final_recovery_rejects_non_final_recovery_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// A plain Active vault is not in the FIFO, so exiting it is invalid.
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_noop!(
			crate::Pallet::<Test>::exit_final_recovery(
				RuntimeOrigin::signed(99),
				DOT,
				PUSD,
				1,
				Position::endpoints_only()
			),
			crate::Error::<Test>::InvalidVaultStatus
		);
	});
}

#[test]
fn frozen_branch_rejects_enter_final_recovery() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			true
		));
		// The frozen check precedes the CR / last-eligible checks.
		assert_noop!(
			crate::Pallet::<Test>::enter_final_recovery(RuntimeOrigin::signed(99), DOT, PUSD, 1),
			crate::Error::<Test>::BranchFrozen
		);
	});
}

#[test]
fn frozen_branch_rejects_exit_final_recovery() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		enter_recovery(1, rate_pct(5, 100));
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			true
		));
		assert_noop!(
			crate::Pallet::<Test>::exit_final_recovery(
				RuntimeOrigin::signed(99),
				DOT,
				PUSD,
				1,
				Position::endpoints_only()
			),
			crate::Error::<Test>::BranchFrozen
		);
		assert!(vault_status(DOT, PUSD, 1).is_final_recovery());
	});
}

// Final recovery is the only status that suspends redistribution eligibility. Full settlement
// returns the residual collateral to the recipient set as a Dormant vault.
#[test]
fn redemption_zeroing_final_recovery_vault_makes_it_dormant() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		enter_recovery(1, rate_pct(5, 100));
		// Restore the price so the recipient's collateral payout is affordable.
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		let full = vault(DOT, PUSD, 1).debt.total();

		assert_ok!(redeem_from(DOT, PUSD, 1, 7, full));

		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::VaultStatusChanged {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			old_status: crate::types::VaultStatus::FinalRecovery,
			new_status: crate::types::VaultStatus::Dormant,
		}));
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert!(crate::Pallet::<Test>::final_recovery_queue(DOT, PUSD, 10).is_empty());
		let vault = vault(DOT, PUSD, 1);
		assert_eq!(vault.debt.total(), 0);
		assert_eq!(vault.collateral, 950);
		assert_eq!(held(DOT, 1), 950, "the residual stays held until the owner closes the husk");
		assert_eq!(vault.redistribution_stake, 950);
		let state = branch_state(DOT, PUSD).expect("state");
		assert_eq!(state.stakes.total, 950);
		assert_eq!(state.debt.minted_interest, 0);
	});
}

#[test]
fn final_recovery_blocks_borrow_repay_withdraw_and_change_rate() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		enter_recovery(1, rate_pct(5, 100));

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
			crate::Error::<Test>::VaultInFinalRecovery
		);
		assert_noop!(
			crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, Some(100)),
			crate::Error::<Test>::VaultInFinalRecovery
		);
		assert_noop!(
			crate::Pallet::<Test>::withdraw_collateral(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				1,
				None
			),
			crate::Error::<Test>::VaultInFinalRecovery
		);
		assert_noop!(
			crate::Pallet::<Test>::change_rate(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				rate_pct(7, 100),
				Position::endpoints_only()
			),
			crate::Error::<Test>::InvalidVaultStatus
		);
	});
}

#[test]
fn exit_final_recovery_to_dormant_when_debt_below_minimum() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		enter_recovery(1, rate_pct(5, 100));
		enter_recovery(2, rate_pct(5, 100));
		// Restore price so CR sits comfortably above MCR after partial redemption.
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		// Redeem most of vault 1's debt — pulls it below MinimumDebt (200) but
		// leaves a non-zero residual, so it stays in the FR FIFO.
		assert_ok!(redeem_from(DOT, PUSD, 1, 99, 350));
		let v = vault(DOT, PUSD, 1);
		assert!(v.debt.total() > 0);
		assert!(v.debt.total() < 200);
		// The Dormant-exit branch never re-inserts into the rate index, so the
		// hint is ignored. Pass a deliberately invalid one to prove it.
		assert_ok!(crate::Pallet::<Test>::exit_final_recovery(
			RuntimeOrigin::signed(99),
			DOT,
			PUSD,
			1,
			Position::between(999, 998)
		));
		// Vault is no longer in FR FIFO and not in the rate index — it's Dormant.
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_eq!(crate::Pallet::<Test>::final_recovery_queue(DOT, PUSD, 10), alloc::vec![2]);
		let state = branch_state(DOT, PUSD).expect("state");
		assert_eq!(state.dormant_redemption_target, Some(1));
	});
}

// A FinalRecovery vault that would exit into a
// sub-MinimumDebt Dormant cannot displace an occupied dormant slot, the exit is
// rejected and the vault stays in FinalRecovery.
#[test]
fn exit_final_recovery_rejected_when_dormant_slot_occupied() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		enter_recovery(1, rate_pct(5, 100));
		enter_recovery(2, rate_pct(5, 100));
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		// Push both FR vaults below MinimumDebt but non-zero.
		assert_ok!(redeem_from(DOT, PUSD, 1, 99, 350));
		assert_ok!(redeem_from(DOT, PUSD, 2, 98, 350));

		// First exit parks vault 1 in the (empty) dormant slot.
		assert_ok!(crate::Pallet::<Test>::exit_final_recovery(
			RuntimeOrigin::signed(99),
			DOT,
			PUSD,
			1,
			Position::endpoints_only()
		));
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_eq!(branch_state(DOT, PUSD).unwrap().dormant_redemption_target, Some(1));

		// Vault 2 also needs the slot, but it is held by a different debt-bearing
		// vault → the exit is rejected and vault 2 stays in FinalRecovery.
		assert_noop!(
			crate::Pallet::<Test>::exit_final_recovery(
				RuntimeOrigin::signed(99),
				DOT,
				PUSD,
				2,
				Position::endpoints_only()
			),
			crate::Error::<Test>::DormantTargetOccupied
		);
		assert!(vault_status(DOT, PUSD, 2).is_final_recovery());
		assert_eq!(branch_state(DOT, PUSD).unwrap().dormant_redemption_target, Some(1));
	});
}

#[test]
fn deposit_into_final_recovery_keeps_stake_zero() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		enter_recovery(1, rate_pct(5, 100));

		let before = branch_state(DOT, PUSD).expect("branch state");
		assert_ok!(crate::Pallet::<Test>::deposit_collateral_for(
			RuntimeOrigin::signed(2),
			DOT,
			PUSD,
			1,
			10_000
		));

		// The collateral lands on the hold and in the branch total, but the
		// vault stays excluded from stake accounting while in the FIFO.
		let vault = vault(DOT, PUSD, 1);
		assert_eq!(vault.redistribution_stake, 0);
		let after = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(after.stakes.total, before.stakes.total);
		assert_eq!(after.total_collateral, before.total_collateral + 10_000);
		assert_eq!(held(DOT, 1), 1_000 + 10_000);
		assert!(vault_status(DOT, PUSD, 1).is_final_recovery());
	});
}

#[test]
fn final_recovery_rescue_deposit_then_exit() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		enter_recovery(1, rate_pct(5, 100));

		// At the crash price the vault is deep underwater; top up enough
		// collateral that the fully-accrued CR clears the MCR again, then
		// exit. Stake re-syncs from the hold on the way out.
		assert_ok!(crate::Pallet::<Test>::deposit_collateral_for(
			RuntimeOrigin::signed(2),
			DOT,
			PUSD,
			1,
			10_000
		));
		assert_ok!(crate::Pallet::<Test>::exit_final_recovery(
			RuntimeOrigin::signed(99),
			DOT,
			PUSD,
			1,
			Position::endpoints_only()
		));

		assert!(vault_status(DOT, PUSD, 1).is_active());
		let vault = vault(DOT, PUSD, 1);
		assert_eq!(vault.redistribution_stake, held(DOT, 1));
		assert!(crate::Pallet::<Test>::final_recovery_queue(DOT, PUSD, 10).is_empty());
	});
}

// The final-recovery FIFO blocks the Dormant target and the rate index. The fixture creates the
// Dormant target through the FIFO to preserve the normal lifecycle.
#[test]
fn redemption_queue_gates_on_final_recovery() {
	build_and_execute(|| {
		register_market(DOT, PUSD);

		enter_recovery(1, rate_pct(1, 100));
		enter_recovery(2, rate_pct(2, 100));
		enter_recovery(3, rate_pct(3, 100));

		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_eq!(redeem(DOT, PUSD, 10, 350).expect("redeemed"), 1);
		assert_ok!(crate::Pallet::<Test>::exit_final_recovery(
			RuntimeOrigin::signed(99),
			DOT,
			PUSD,
			1,
			Position::endpoints_only()
		));
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_eq!(branch_state(DOT, PUSD).unwrap().dormant_redemption_target, Some(1));

		assert_ok!(open(4, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(open(5, DOT, PUSD, 1_000, 500, rate_pct(4, 100)));

		assert_eq!(crate::Pallet::<Test>::redemption_queue(DOT, PUSD, 10), alloc::vec![2]);
	});
}

#[test]
fn final_recovery_re_entry_queues_behind_with_strict_priorities() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		enter_recovery(1, rate_pct(1, 100));
		enter_recovery(2, rate_pct(2, 100));

		// Exit 1 while priced back up, then push it under again: it re-enters
		// behind 2 — the queue is FIFO by entry time, not by first entry.
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(crate::Pallet::<Test>::exit_final_recovery(
			RuntimeOrigin::signed(42),
			DOT,
			PUSD,
			1,
			Position::endpoints_only()
		));
		set_price(DOT, low_recovery_price());
		assert_ok!(crate::Pallet::<Test>::enter_final_recovery(
			RuntimeOrigin::signed(99),
			DOT,
			PUSD,
			1
		));
		assert_eq!(crate::Pallet::<Test>::final_recovery_queue(DOT, PUSD, 10), alloc::vec![2, 1]);

		// The stored priorities stay strictly distinct (newest greatest), so
		// the linked list's permissionless re-anchoring can never legally
		// relocate a member.
		let list = VaultList::FinalRecovery(DOT, PUSD);
		let p1 = <LinkedList as SortedListInterface<VaultList, AccountId>>::priority(&list, &1)
			.expect("member");
		let p2 = <LinkedList as SortedListInterface<VaultList, AccountId>>::priority(&list, &2)
			.expect("member");
		assert!(p1 > p2, "re-entered vault must carry a strictly greater priority");
	});
}
