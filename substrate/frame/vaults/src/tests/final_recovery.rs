//! FinalRecovery queue behavior backed by `pallet-linked-list`.

use crate::{
	mock::*,
	tests::{
		assert_event, keeperless_branch, liquidation_outcome, percent, vault_events, vault_status,
		ONE_YEAR_MS,
	},
};
use frame::prelude::Pays;
use pallet_linked_list::SortedListInterface;
use stablecoin_primitives::VaultInterface;

/// Signs every fixture entry. It has no genesis balance, so its collateral is exactly what
/// recovery entries paid it.
const KEEPER: AccountId = 99;

/// What [`enter_recovery`] pays its keeper, priced as the pool-covered liquidation of the
/// fixture vault: 501 of debt at 5% seizes up to 5_270 at the crash price, so the whole 1_000
/// is seized, and the 10 flat converts to 100 plus 0.1% of the lot. The 260 penalty budget and
/// the cap are both larger.
const ENTRY_REWARD: Balance = 101;

fn low_recovery_price() -> FixedU128 {
	FixedU128::from_rational(1u128, 10u128)
}

fn enter_recovery(who: AccountId, rate: FixedU128) {
	set_price(DOT, DEFAULT_PRICE);
	assert_ok!(open(who, DOT, STABLE, 1_000, 500, rate));
	set_price(DOT, low_recovery_price());
	assert_ok!(enter_final_recovery(KEEPER, DOT, STABLE, who));
}

/// Returns `(owner, keeper, amount)` for every `FinalRecoveryRewardPaid` event of the block.
fn reward_events() -> Vec<(AccountId, AccountId, Balance)> {
	vault_events()
		.into_iter()
		.filter_map(|event| match event {
			crate::Event::FinalRecoveryRewardPaid { owner, keeper, amount, .. } => {
				Some((owner, keeper, amount))
			},
			_ => None,
		})
		.collect()
}

#[test]
fn final_recovery_queue_is_fifo_across_multiple_vaults() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(1));
		enter_recovery(2, percent(2));
		enter_recovery(3, percent(3));

		assert_eq!(
			LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), 10),
			alloc::vec![1, 2, 3]
		);
		assert_eq!(
			<crate::Pallet<Test> as VaultInterface>::next_redemption_target(&DOT, &STABLE, None)
				.map(|(owner, _status)| owner),
			Some(1)
		);
		// The timestamp and cooldown apply to the branch. Thus, only the first entry pays.
		assert_eq!(collateral_balance(DOT, KEEPER), ENTRY_REWARD);
		assert_eq!(market_state(DOT, STABLE).last_final_recovery_entry, Some(Timestamp::get()));
	});
}

#[test]
fn enter_final_recovery_is_an_idempotent_noop() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(5));
		advance_time(ONE_YEAR_MS);
		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, true));

		// Nothing moves, including the keeper reward and the event log, so
		// the caller is neither paid nor excused the fee.
		let post_info;
		assert_storage_noop!(
			post_info = enter_final_recovery(KEEPER, DOT, STABLE, 1).expect("a no-op succeeds")
		);

		assert_eq!(post_info.pays_fee, Pays::Yes);
	});
}

// Entering final recovery is the resolution a liquidation could not deliver, so it pays the
// keeper the liquidation reward out of the vault, and the vault keeps only the rest.
#[test]
fn final_recovery_entry_pays_the_keeper_from_the_vault() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_eq!(collateral_balance(DOT, KEEPER), 0, "keeper is fresh");

		enter_recovery(1, percent(5));

		let remaining = 1_000 - ENTRY_REWARD;
		assert_eq!(collateral_balance(DOT, KEEPER), ENTRY_REWARD);
		let vault = vault(DOT, STABLE, 1);
		assert_eq!(vault.collateral, remaining);
		assert_eq!(reward_events(), vec![(1, KEEPER, ENTRY_REWARD)]);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::VaultStatusChanged {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			old_status: crate::types::VaultStatus::Active,
			new_status: crate::types::VaultStatus::FinalRecovery,
		}));
	});
}

// A keeper that meets `LastVaultCannotBeLiquidated` loses nothing by switching calls: the two
// resolutions of identical vaults pay identical rewards.
#[test]
fn final_recovery_entry_pays_what_liquidation_pays() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(5)));
		// The pool covers the first liquidation whole, so nothing redistributes onto vault 2.
		ActiveSpCapacity::set(&1_000);
		set_price(DOT, low_recovery_price());
		let liquidator_before = collateral_balance(DOT, 3);
		let entrant_before = collateral_balance(DOT, 4);

		assert_ok!(liquidate(3, DOT, STABLE, 1, 0, 0));
		let post_info =
			enter_final_recovery(4, DOT, STABLE, 2).expect("last vault enters recovery");

		let liquidation_reward = liquidation_outcome().keeper_reward;
		assert_eq!(liquidation_reward, ENTRY_REWARD);
		assert_eq!(collateral_balance(DOT, 3) - liquidator_before, liquidation_reward);
		assert_eq!(collateral_balance(DOT, 4) - entrant_before, liquidation_reward);
		assert_eq!(post_info.pays_fee, Pays::No);
		assert_eq!(reward_events().last(), Some(&(2, 4, ENTRY_REWARD)));
	});
}

// Exit is permissionless and unpaid. Without a cooldown, a caller can repeatedly move a vault
// across the MCR and collect rewards. Only a paid entry resets the branch cooldown: an unpaid one
// must not push the next honest keeper's reward further out.
#[test]
fn final_recovery_reward_cooldown_blocks_flipping() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(5));
		assert_eq!(collateral_balance(DOT, KEEPER), ENTRY_REWARD);
		let first_entry = market_state(DOT, STABLE).last_final_recovery_entry.expect("stamped");
		let cooldown = market_config(DOT, STABLE).final_recovery_reward_cooldown;
		let flip = |keeper: AccountId| {
			set_price(DOT, DEFAULT_PRICE);
			assert_ok!(exit_final_recovery(keeper, DOT, STABLE, 1));
			set_price(DOT, low_recovery_price());
			enter_final_recovery(keeper, DOT, STABLE, 1).expect("eligible entry succeeds")
		};

		// Inside the cooldown the flip moves the vault, stays fee-free, pays nothing, and leaves
		// the timestamp where the paid entry put it.
		advance_time(cooldown / 2);
		let post_info = flip(KEEPER);

		assert_eq!(post_info.pays_fee, Pays::No);
		assert!(vault_status(DOT, STABLE, 1).is_final_recovery());
		assert_eq!(reward_events(), vec![(1, KEEPER, ENTRY_REWARD)]);
		assert_eq!(collateral_balance(DOT, KEEPER), ENTRY_REWARD);
		assert_eq!(held(DOT, 1), 1_000 - ENTRY_REWARD);
		assert_eq!(market_state(DOT, STABLE).last_final_recovery_entry, Some(first_entry));

		// One cooldown from the paid entry is sufficient, unpaid flips in between notwithstanding.
		// The re-entry is priced on what the vault still holds: the flat 100 plus 0.1% of 899,
		// which rounds to nothing. This paid entry moves the timestamp.
		advance_time(cooldown / 2);
		flip(KEEPER);

		assert_eq!(reward_events(), vec![(1, KEEPER, ENTRY_REWARD), (1, KEEPER, 100)]);
		assert_eq!(collateral_balance(DOT, KEEPER), ENTRY_REWARD + 100);
		assert_eq!(held(DOT, 1), 1_000 - ENTRY_REWARD - 100);
		assert_eq!(
			market_state(DOT, STABLE).last_final_recovery_entry,
			Some(first_entry + cooldown)
		);

		// A branch may switch the protection off, after which every entry pays.
		assert_ok!(crate::Pallet::<Test>::set_param(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			STABLE,
			crate::types::BranchConfigUpdate::FinalRecoveryRewardCooldown(0)
		));
		flip(KEEPER);

		assert_eq!(collateral_balance(DOT, KEEPER), ENTRY_REWARD + 100 + 100);
	});
}

#[test]
fn last_unsafe_dormant_enters_recovery_and_releases_its_slot() {
	ExtBuilder::default().build_and_execute(|| {
		let config = crate::BranchConfig { upfront_fee_period: 0, ..default_branch_config() };
		register_market_with(DOT, STABLE, DEFAULT_PRICE, config);
		assert_ok!(open(1, DOT, STABLE, 1_040, 500, percent(5)));
		assert_ok!(redeem(DOT, STABLE, 3, 400));
		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 100);
		assert_eq!(vault(DOT, STABLE, 1).collateral, 1_000);
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, Some(1));

		// Even the last Dormant vault must be strictly below MCR, not merely at it.
		set_price(DOT, FixedU128::from_rational(11, 100));
		assert_noop!(
			enter_final_recovery(KEEPER, DOT, STABLE, 1),
			crate::Error::<Test>::CollateralizationRatioTooHealthy
		);
		set_price(DOT, FixedU128::from_rational(1, 100));
		assert_noop!(
			liquidate(KEEPER, DOT, STABLE, 1, 0, 0),
			crate::Error::<Test>::LastVaultCannotBeLiquidated
		);

		assert_ok!(enter_final_recovery(KEEPER, DOT, STABLE, 1));

		assert!(vault_status(DOT, STABLE, 1).is_final_recovery());
		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 100);
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);
		assert_eq!(LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), 10), vec![1]);
		assert_event(crate::Event::VaultStatusChanged {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			old_status: crate::VaultStatus::Dormant,
			new_status: crate::VaultStatus::FinalRecovery,
		});
	});
}

#[test]
fn unsafe_dormant_with_another_stake_bearer_uses_liquidation() {
	ExtBuilder::default().build_and_execute(|| {
		let config = crate::BranchConfig { upfront_fee_period: 0, ..default_branch_config() };
		register_market_with(DOT, STABLE, DEFAULT_PRICE, config);
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(repay(2, DOT, STABLE, 2, None));
		assert_ok!(redeem(DOT, STABLE, 3, 400));
		// The liquidation below must clear a pointer that is really set.
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, Some(1));
		set_price(DOT, low_recovery_price());

		// A debt-free husk still counts as another stake bearer.
		assert_noop!(
			enter_final_recovery(KEEPER, DOT, STABLE, 1),
			crate::Error::<Test>::NotLastEligibleVault
		);

		assert_ok!(liquidate(KEEPER, DOT, STABLE, 1, 0, 0));

		assert!(!vault_exists(DOT, STABLE, 1));
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);
		assert!(LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), 10).is_empty());
	});
}

// A reward that would take every collateral unit is skipped. The vault still enters recovery,
// keeps its collateral, pays no fee, and leaves the reward cooldown untouched.
#[test]
fn exhausting_recovery_reward_is_skipped_and_the_vault_still_enters() {
	ExtBuilder::default().build_and_execute(|| {
		let config = crate::BranchConfig { upfront_fee_period: 0, ..default_branch_config() };
		register_market_with(DOT, STABLE, DEFAULT_PRICE, config.clone());
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		let price = FixedU128::from_rational(1, 100);
		set_price(DOT, price);
		assert_eq!(
			crate::liquidation::final_recovery_keeper_reward(
				1_000,
				500,
				price,
				&config.liquidation
			),
			Some(1_000)
		);

		let post_info =
			enter_final_recovery(KEEPER, DOT, STABLE, 1).expect("recovery still enters");

		assert_eq!(post_info.pays_fee, Pays::No);
		assert_eq!(reward_events(), vec![]);
		assert_eq!(collateral_balance(DOT, KEEPER), 0);
		assert_eq!(vault(DOT, STABLE, 1).collateral, 1_000);
		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 500);
		assert!(vault_status(DOT, STABLE, 1).is_final_recovery());
		assert_eq!(market_state(DOT, STABLE).last_final_recovery_entry, None);
		assert_eq!(LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), 10), vec![1]);
	});
}

// A debt-free Dormant vault remains a redistribution recipient. Thus, another vault is not the
// last eligible recipient and cannot enter final recovery.
#[test]
fn debt_free_husk_keeps_the_branch_out_of_final_recovery() {
	keeperless_branch().build_and_execute(|| {
		// The lower rate makes account 1 the first redemption target.
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(1)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(2)));
		assert_ok!(redeem(DOT, STABLE, 4, 501));
		let husk_before = vault(DOT, STABLE, 1);
		assert_eq!(husk_before.debt.total(), 0);
		assert_eq!(husk_before.collateral, 950);
		assert!(vault_status(DOT, STABLE, 1).is_dormant());

		set_price(DOT, FixedU128::from_rational(1u128, 2u128));
		assert_noop!(
			enter_final_recovery(99, DOT, STABLE, 2),
			crate::Error::<Test>::NotLastEligibleVault
		);

		assert_ok!(liquidate(99, DOT, STABLE, 2, 0, 0));
		assert_ok!(poke(9, DOT, STABLE, 1));

		// The sole recipient must drain both pending pools.
		let husk = vault(DOT, STABLE, 1);
		assert_eq!(husk.debt.principal, 501);
		assert_eq!(husk.collateral, 950 + 1_000);
		let state = market_state(DOT, STABLE);
		assert_eq!(state.debt.pending_redistribution_principal, 0);
		assert_eq!(state.pending_redistribution_collateral, 0);
		// Redistribution does not reactivate a Dormant vault.
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		assert!(LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), 10).is_empty());
	});
}

// A healthy Active vault takes neither transition: entry would pay a keeper out of a healthy
// owner's collateral, and the vault is not in the FIFO to exit.
#[test]
fn final_recovery_transitions_reject_a_healthy_active_vault() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// Price 10 → CR = 1000·10/500 = 2000% ≫ MCR 110%, far too healthy for FR.
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));

		assert_noop!(
			enter_final_recovery(99, DOT, STABLE, 1),
			crate::Error::<Test>::CollateralizationRatioTooHealthy
		);
		assert_noop!(
			exit_final_recovery(99, DOT, STABLE, 1),
			crate::Error::<Test>::InvalidVaultStatus
		);
	});
}

// A governance freeze blocks both recovery transitions, ahead of every status, CR and
// last-eligible check.
#[test]
fn frozen_branch_rejects_final_recovery_transitions() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(5));
		// At the restored price vault 1 could exit, and the healthy vault 2 would fail entry as
		// too healthy: the freeze must reject both first.
		set_price(DOT, DEFAULT_PRICE);
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, true));

		assert_noop!(enter_final_recovery(99, DOT, STABLE, 2), crate::Error::<Test>::BranchFrozen);
		assert_noop!(exit_final_recovery(99, DOT, STABLE, 1), crate::Error::<Test>::BranchFrozen);
	});
}

// Final recovery is the only status that suspends redistribution eligibility. A settlement
// that leaves residual collateral keeps the owner's claim on it as a Dormant vault.
#[test]
fn redemption_zeroing_final_recovery_vault_makes_it_dormant() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(5));
		// Restore the price so the recipient's collateral payout is affordable.
		set_price(DOT, DEFAULT_PRICE);
		let full = vault(DOT, STABLE, 1).debt.total();

		assert_ok!(redeem_from(DOT, STABLE, 1, 7, full));

		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::VaultStatusChanged {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			old_status: crate::types::VaultStatus::FinalRecovery,
			new_status: crate::types::VaultStatus::Dormant,
		}));
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		assert!(LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), 10).is_empty());
		// The redemption prices the collateral the entry reward left: 501 of debt at 10 takes 50.
		let residual = 1_000 - ENTRY_REWARD - 50;
		let vault = vault(DOT, STABLE, 1);
		assert_eq!(vault.debt.total(), 0);
		assert_eq!(vault.collateral, residual);
		assert_eq!(vault.redistribution_stake, residual);
		assert_eq!(market_state(DOT, STABLE).debt.minted_interest, 0);
	});
}

// FinalRecovery owns last-vault settlement, so neither the owner nor an ordinary liquidation may
// move the vault while it is queued.
#[test]
fn final_recovery_blocks_borrow_withdraw_rate_change_and_liquidation() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(5));

		assert_noop!(borrow(1, DOT, STABLE, 100, None), crate::Error::<Test>::VaultInFinalRecovery);
		assert_noop!(
			withdraw_collateral(1, DOT, STABLE, 1, None),
			crate::Error::<Test>::VaultInFinalRecovery
		);
		assert_noop!(
			change_rate(1, DOT, STABLE, percent(7)),
			crate::Error::<Test>::InvalidVaultStatus
		);
		assert_noop!(
			liquidate(9, DOT, STABLE, 1, 0, 0),
			crate::Error::<Test>::VaultInFinalRecovery
		);
	});
}

// Repayment burns stablecoin and can only raise the vault's ratio, the same reason it stays open
// in Safety Mode. A partial payoff leaves the vault queued and outside stake accounting.
#[test]
fn final_recovery_accepts_partial_repayment_and_stays_queued() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(5));
		let debt_before = vault(DOT, STABLE, 1).debt.total();

		assert_ok!(repay(1, DOT, STABLE, 1, Some(200)));

		let vault = vault(DOT, STABLE, 1);
		assert_eq!(vault.debt.total(), debt_before - 200);
		assert!(vault_status(DOT, STABLE, 1).is_final_recovery());
		assert_eq!(LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), 10), vec![1]);
	});
}

// Repaying enough to clear the MCR is a legitimate rescue: the vault can then exit and rejoin
// the rate index with its stake restored.
#[test]
fn final_recovery_repayment_then_exit_rejoins_rate_index() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(5));

		// The 899 of collateral left after the entry reward is worth 449 at 0.5 against 501 of
		// debt: still below the 110% MCR.
		set_price(DOT, FixedU128::from_rational(1u128, 2u128));
		assert_noop!(
			exit_final_recovery(99, DOT, STABLE, 1),
			crate::Error::<Test>::CollateralizationRatioTooLow
		);

		// 449 of value over 301 of debt clears the MCR.
		assert_ok!(repay(1, DOT, STABLE, 1, Some(200)));
		assert_ok!(exit_final_recovery(99, DOT, STABLE, 1));

		assert!(vault_status(DOT, STABLE, 1).is_active());
		assert_eq!(vault(DOT, STABLE, 1).redistribution_stake, held(DOT, 1));
		assert!(LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), 10).is_empty());
	});
}

// A full payoff resolves the vault the same way a full redemption does: it leaves the queue as a
// debt-free Dormant husk that keeps its collateral and rejoins stake accounting.
#[test]
fn final_recovery_full_repayment_leaves_dormant_husk() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(5));
		// Cover the upfront fee that the borrowed principal does not include.
		mint_stable(STABLE, 1, 10);

		assert_ok!(repay(1, DOT, STABLE, 1, None));

		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::VaultStatusChanged {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			old_status: crate::types::VaultStatus::FinalRecovery,
			new_status: crate::types::VaultStatus::Dormant,
		}));
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		assert!(LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), 10).is_empty());
		let residual = 1_000 - ENTRY_REWARD;
		let vault = vault(DOT, STABLE, 1);
		assert_eq!(vault.debt.total(), 0);
		assert_eq!(vault.collateral, residual);
		assert_eq!(vault.redistribution_stake, residual);
		assert!(vault_exists(DOT, STABLE, 1), "a husk with collateral is not closed");
	});
}

// A freeze must not lock owners out of the one action that can still rescue a recovery vault.
#[test]
fn frozen_branch_accepts_deposit_into_final_recovery_vault() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(5));
		assert_ok!(set_governance_frozen(ADMIN, DOT, STABLE, true));

		assert_ok!(deposit_collateral(2, DOT, STABLE, 1, 10_000));

		assert_eq!(held(DOT, 1), 1_000 - ENTRY_REWARD + 10_000);
		assert!(vault_status(DOT, STABLE, 1).is_final_recovery());
	});
}

// A sub-minimum exit parks the vault Dormant without touching the rate index, and the single
// dormant slot it takes cannot be displaced: a second such exit is rejected and that vault stays
// in FinalRecovery.
#[test]
fn exit_final_recovery_below_minimum_parks_dormant_once() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(5));
		enter_recovery(2, percent(5));
		// Restore price so CR sits comfortably above MCR after partial redemption.
		set_price(DOT, DEFAULT_PRICE);
		// Push both FR vaults below MinimumDebt (200) but non-zero, so they stay in the FR FIFO.
		assert_ok!(redeem_from(DOT, STABLE, 1, 99, 350));
		assert_ok!(redeem_from(DOT, STABLE, 2, 98, 350));
		let v = vault(DOT, STABLE, 1);
		assert!(v.debt.total() > 0);
		assert!(v.debt.total() < 200);

		// The Dormant-exit branch never re-inserts into the rate index, so the
		// hint is ignored. Pass a deliberately invalid one to prove it.
		assert_ok!(crate::Pallet::<Test>::exit_final_recovery(
			RuntimeOrigin::signed(99),
			DOT,
			STABLE,
			1,
			Position::between(999, 998)
		));

		// Vault is no longer in FR FIFO and not in the rate index — it's Dormant.
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		assert_eq!(
			LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), 10),
			alloc::vec![2]
		);
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, Some(1));

		// Vault 2 also needs the slot, but it is held by a different debt-bearing
		// vault → the exit is rejected and vault 2 stays in FinalRecovery.
		assert_noop!(
			exit_final_recovery(99, DOT, STABLE, 2),
			crate::Error::<Test>::DormantTargetOccupied
		);
	});
}

// Leaving the FIFO re-inserts the vault into the rate index, which needs caller-supplied hints,
// so a recovered price alone must not move the vault out on a touch.
#[test]
fn poke_does_not_exit_final_recovery() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(5));
		set_price(DOT, DEFAULT_PRICE);

		assert_ok!(poke(99, DOT, STABLE, 1));

		assert!(vault_status(DOT, STABLE, 1).is_final_recovery());

		assert_ok!(exit_final_recovery(99, DOT, STABLE, 1));

		assert!(vault_status(DOT, STABLE, 1).is_active());
	});
}

// The final-recovery FIFO blocks the Dormant target and the rate index. The fixture creates the
// Dormant target through the FIFO to preserve the normal lifecycle.
#[test]
fn redemption_queue_gates_on_final_recovery() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(1));
		enter_recovery(2, percent(2));
		enter_recovery(3, percent(3));

		set_price(DOT, DEFAULT_PRICE);
		assert_eq!(redeem(DOT, STABLE, 10, 350).expect("redeemed"), 1);
		assert_ok!(exit_final_recovery(99, DOT, STABLE, 1));
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, Some(1));

		assert_ok!(open(4, DOT, STABLE, 1_000, 500, percent(2)));
		assert_ok!(open(5, DOT, STABLE, 1_000, 500, percent(4)));

		assert_eq!(crate::Pallet::<Test>::redemption_queue(DOT, STABLE, 10), alloc::vec![2]);
	});
}

#[test]
fn final_recovery_re_entry_queues_behind_with_strict_priorities() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(1));
		enter_recovery(2, percent(2));

		// Exit 1 while priced back up, then push it under again: it re-enters
		// behind 2 — the queue is FIFO by entry time, not by first entry.
		set_price(DOT, DEFAULT_PRICE);
		assert_ok!(exit_final_recovery(42, DOT, STABLE, 1));
		set_price(DOT, low_recovery_price());
		assert_ok!(enter_final_recovery(KEEPER, DOT, STABLE, 1));

		assert_eq!(
			LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), 10),
			alloc::vec![2, 1]
		);

		// The stored priorities stay strictly distinct (newest greatest), so
		// the linked list's permissionless re-anchoring can never legally
		// relocate a member.
		let list = VaultList::FinalRecovery(DOT, STABLE);
		let p1 = <LinkedList as SortedListInterface<VaultList, AccountId>>::priority(&list, &1)
			.expect("member");
		let p2 = <LinkedList as SortedListInterface<VaultList, AccountId>>::priority(&list, &2)
			.expect("member");
		assert!(p1 > p2, "re-entered vault must carry a strictly greater priority");
	});
}

// A settlement that pays out the whole collateral against the whole debt leaves an empty row.
// The pallet closes it: the row is removed, the deposit returns to the owner, and the branch
// forgets the vault. The close skips the mode gate, so it works in the stressed branch a
// recovery settlement usually runs in.
#[test]
fn full_settlement_of_a_final_recovery_vault_closes_it() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_recovery(1, percent(5));
		assert!(vault_deposit_held(DOT, 1) > 0);

		let full =
			<crate::Pallet<Test> as VaultInterface>::project_redemption_snapshot(&DOT, &STABLE, &1)
				.expect("snapshot")
				.debt;
		let all_collateral = vault(DOT, STABLE, 1).collateral;
		assert_ok!(redeem_step(DOT, STABLE, 1, 7, full, all_collateral));

		assert!(!vault_exists(DOT, STABLE, 1));
		assert_eq!(held(DOT, 1), 0);
		assert_eq!(vault_deposit_held(DOT, 1), 0);
		assert!(LinkedList::iter_from_tail(VaultList::FinalRecovery(DOT, STABLE), 10).is_empty());
		assert_eq!(market_state(DOT, STABLE).dormant_redemption_target, None);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::VaultClosed {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
			recipient: 1,
			collateral: 0,
		}));
	});
}
