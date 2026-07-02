use crate::{
	mock::*,
	pallet::{BranchStates, Vaults},
	tests::{rate_pct, vault_status},
};
use pallet_linked_list::SortedListInterface;
use pusd_primitives::{
	KeeperCompensation, LiquidationAllocation, OffsetAllocation, RedemptionAllocation,
	VaultRedemptionInterface,
};

const ONE_DAY_MS: Moment = 24 * 3_600 * 1_000;

// Behavior note: Dormant vaults can still be the target of `withdraw` and
// `repay` operations. The carve-outs are `change_rate` and collateral-only
// deposits that cannot revive the vault to `Debt >= MinimumDebt`.

#[test]
fn fully_redeemed_vault_becomes_dormant_and_leaves_rate_index() {
	build_and_execute(|| {
		register_default_branch();
		// A and B at distinct rates so the redemption order is deterministic
		// (tail-first picks A first as it has the lower rate).
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(2, 100)));

		// Let a full year accrue so the redemption must poke the target's pending
		// interest before cancelling — redeeming at the genesis instant would only
		// exercise this against pre-touch stored debt.
		advance_time(pusd_primitives::MILLIS_PER_YEAR);
		let now = pallet_timestamp::Pallet::<Test>::get();
		// Redeem more than the fully-accrued debt (500 principal + 1 open fee + 5 year
		// interest = 506) so acct 1's debt is cancelled in full.
		let target = redeem(DOT, 3, 1_000).expect("redeem ok");
		assert_eq!(target, 1);

		let v = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		assert!(vault_status(DOT, 1).is_dormant());
		assert_eq!(v.debt.principal + v.debt.interest, 0);
		// The redemption poked the target: its interest clock advanced to now.
		assert_eq!(
			v.last_interest_time,
			BranchStates::<Test>::get(DOT, PUSD).unwrap().interest_time(now)
		);
		let state = BranchStates::<Test>::get(DOT, PUSD).unwrap();
		assert_eq!(state.dormant_redemption_target, None);
		// Rate index no longer contains acct 1.
		assert!(!<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&rate_list(DOT),
			&1
		));
	});
}

#[test]
fn redeemed_below_min_debt_becomes_dormant() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(2, 100)));

		// MinimumDebt = 200 (from default_branch_config). Redeem so acct 1
		// has < 200 left.
		assert_ok!(redeem(DOT, 3, 350));
		let v = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		let total = v.debt.principal + v.debt.interest;
		// Open fee 1 (500 @ 1%) → total 501; redeem 350 cancels interest-first (1) then
		// 349 principal, leaving exactly 151, below MinimumDebt 200.
		assert_eq!(total, 151);
		assert!(vault_status(DOT, 1).is_dormant());
		let state = BranchStates::<Test>::get(DOT, PUSD).unwrap();
		assert_eq!(state.dormant_redemption_target, Some(1));
		assert!(!<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&rate_list(DOT),
			&1
		));
	});
}

#[test]
fn redeemed_above_min_debt_stays_active() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(2, 100)));

		// Redeem 200 — leaves acct 1 with ≈ 300 debt, well above MinimumDebt.
		assert_ok!(redeem(DOT, 3, 200));
		assert!(vault_status(DOT, 1).is_active());
		assert!(<LinkedList as SortedListInterface<VaultList, u64>>::contains(&rate_list(DOT), &1));
	});
}

#[test]
fn prepare_redemption_step_rejects_frozen_branch_and_missing_vault() {
	build_and_execute(|| {
		register_default_branch();
		assert_noop!(
			<crate::Pallet<Test> as VaultRedemptionInterface<
				AccountId,
				AssetId,
				StableId,
				Balance,
			>>::prepare_redemption_step(DOT, PUSD, 99),
			crate::Error::<Test>::VaultNotFound
		);
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(crate::Pallet::<Test>::enable_frozen_mode(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD
		));
		assert_noop!(
			<crate::Pallet<Test> as VaultRedemptionInterface<
				AccountId,
				AssetId,
				StableId,
				Balance,
			>>::prepare_redemption_step(DOT, PUSD, 1),
			crate::Error::<Test>::BranchFrozen
		);
	});
}

#[test]
fn apply_redemption_rejects_frozen_branch() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		// A frozen branch must reject settlement, like every other price-dependent
		// path; the gate fires before any vault is touched.
		assert_ok!(crate::Pallet::<Test>::enable_frozen_mode(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD
		));
		assert_noop!(
			<crate::Pallet<Test> as VaultRedemptionInterface<
				AccountId,
				AssetId,
				StableId,
				Balance,
			>>::apply_redemption(
				DOT,
				PUSD,
				1,
				3,
				RedemptionAllocation {
					debt_to_cancel: 100,
					collateral_to_redeemer: 10,
					fee_collateral_retained: 0,
				}
			),
			crate::Error::<Test>::BranchFrozen
		);
	});
}

#[test]
fn apply_redemption_rejects_invalid_allocations_without_state_change() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(2, 100)));
		let vault_pre = Vaults::<Test>::get((DOT, PUSD, 1)).expect("vault stored");
		let held_pre = held(DOT, 1);

		assert_noop!(
			<crate::Pallet<Test> as VaultRedemptionInterface<
				AccountId,
				AssetId,
				StableId,
				Balance,
			>>::apply_redemption(
				DOT,
				PUSD,
				1,
				3,
				RedemptionAllocation {
					debt_to_cancel: vault_pre.debt.total() + 1,
					collateral_to_redeemer: 0,
					fee_collateral_retained: 0,
				}
			),
			crate::Error::<Test>::InvalidRedemptionAllocation
		);
		assert_noop!(
			<crate::Pallet<Test> as VaultRedemptionInterface<
				AccountId,
				AssetId,
				StableId,
				Balance,
			>>::apply_redemption(
				DOT,
				PUSD,
				1,
				3,
				RedemptionAllocation {
					debt_to_cancel: 0,
					collateral_to_redeemer: held_pre + 1,
					fee_collateral_retained: 0,
				}
			),
			crate::Error::<Test>::InvalidRedemptionAllocation
		);

		assert_eq!(Vaults::<Test>::get((DOT, PUSD, 1)).unwrap(), vault_pre);
		assert_eq!(held(DOT, 1), held_pre);
	});
}

// A redemption's fee is *retained collateral*, not a pUSD fee routed through
// `FeeHandler`: the redeemer pays pUSD for `debt_to_cancel` (the pUSD leg is
// owned by the redemptions pallet) and receives `collateral_to_redeemer`, while
// `fee_collateral_retained` stays locked on the vault — the redeemer simply
// receives less collateral than the debt they cancelled. Only
// `collateral_to_redeemer` leaves the vault's hold.
#[test]
fn redemption_with_retained_fee_leaves_fee_collateral_on_vault() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(2, 100)));
		let held_pre = held(DOT, 1);
		let coll_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().collateral;
		let redeemer_pre = collateral_balance(DOT, 3);

		assert_ok!(<crate::Pallet<Test> as VaultRedemptionInterface<
			AccountId,
			AssetId,
			StableId,
			Balance,
		>>::apply_redemption(
			DOT,
			PUSD,
			1,
			3,
			RedemptionAllocation {
				debt_to_cancel: 100,
				collateral_to_redeemer: 10,
				fee_collateral_retained: 5,
			}
		));

		// Only the redeemer's 10 leaves the hold; the 5-unit fee stays locked on
		// the vault (it does not move to the redeemer or through `FeeHandler`).
		assert_eq!(held(DOT, 1), held_pre - 10);
		assert_eq!(collateral_balance(DOT, 3), redeemer_pre + 10);
		let coll_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().collateral;
		assert_eq!(coll_post, coll_pre - 10, "fee_collateral_retained stays on the vault");
	});
}

#[test]
fn dormant_pointer_clears_when_last_dormant_fully_redeemed() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(2, 100)));
		// Push acct 1 to Dormant via partial-below-MinDebt.
		assert_ok!(redeem(DOT, 3, 350));
		let state = BranchStates::<Test>::get(DOT, PUSD).unwrap();
		assert_eq!(state.dormant_redemption_target, Some(1));
		// Now redeem acct 1's full residual. next_redemption_target prefers
		// dormant_redemption_target, so this hits acct 1 again.
		let v = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		let residual = v.debt.principal + v.debt.interest;
		let target = redeem(DOT, 3, residual).expect("redeem residual ok");
		assert_eq!(target, 1);
		let state = BranchStates::<Test>::get(DOT, PUSD).unwrap();
		assert_eq!(state.dormant_redemption_target, None);
	});
}

// `activate_dormant` is the permissionless revival path: touch never re-activates
// a Dormant vault, but once its fully-accrued debt is back at/above
// MinimumDebt a hint-bearing `activate_dormant` flips it to Active and clears the
// slot.
#[test]
fn activate_dormant_revives_when_accrued_debt_reaches_minimum() {
	build_and_execute(|| {
		register_default_branch();
		// Vault 1 carries the lower rate so it sits at the redemption tail; its
		// rate is still high enough that accrued interest lifts the dormant
		// remainder back over MinimumDebt (200) within a year.
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(50, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(60, 100)));
		assert_ok!(redeem(DOT, 3, 350));
		assert!(vault_status(DOT, 1).is_dormant());
		assert_eq!(
			BranchStates::<Test>::get(DOT, PUSD).unwrap().dormant_redemption_target,
			Some(1)
		);

		advance_time(365 * ONE_DAY_MS);
		// Touch alone never re-activates a Dormant, even past MinimumDebt.
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		assert!(vault_status(DOT, 1).is_dormant(), "touch never re-activates a Dormant");

		assert_ok!(crate::Pallet::<Test>::activate_dormant(
			RuntimeOrigin::signed(9),
			DOT,
			PUSD,
			1,
			Position::endpoints_only()
		));
		assert!(vault_status(DOT, 1).is_active());
		assert!(<LinkedList as SortedListInterface<VaultList, u64>>::contains(&rate_list(DOT), &1));
		assert_eq!(BranchStates::<Test>::get(DOT, PUSD).unwrap().dormant_redemption_target, None);
	});
}

#[test]
fn activate_dormant_rejects_below_minimum_debt() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(redeem(DOT, 3, 350)); // vault 1 → Dormant, debt ~150 < 200
		assert!(vault_status(DOT, 1).is_dormant());
		assert_noop!(
			crate::Pallet::<Test>::activate_dormant(
				RuntimeOrigin::signed(9),
				DOT,
				PUSD,
				1,
				Position::endpoints_only()
			),
			crate::Error::<Test>::DebtBelowMinimum
		);
		assert!(vault_status(DOT, 1).is_dormant());
	});
}

#[test]
fn activate_dormant_rejects_active_vault() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(5, 100)));
		assert_noop!(
			crate::Pallet::<Test>::activate_dormant(
				RuntimeOrigin::signed(9),
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
fn dormant_vault_with_residual_accrues_interest() {
	build_and_execute(|| {
		register_default_branch();
		// Distinct rates so the redemption deterministically targets vault 1 (the
		// lower-rate tail); at equal rates the LIFO tie-break would send it to vault 2
		// and this test would then read an untouched Active vault (green for the wrong
		// reason).
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(50, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(60, 100)));
		let target = redeem(DOT, 3, 350).expect("redeem ok");
		assert_eq!(target, 1);
		// Vault 1 (open fee 5 → total 505) is redeemed by 350: interest-first cancels 5,
		// then 345 principal, leaving a 155 residual below MinimumDebt 200 → Dormant.
		assert!(vault_status(DOT, 1).is_dormant());
		assert_eq!(
			BranchStates::<Test>::get(DOT, PUSD).unwrap().dormant_redemption_target,
			Some(1)
		);
		let v_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		assert_eq!(v_pre.debt.principal, 155);
		assert_eq!(v_pre.debt.interest, 0);

		advance_time(365 * ONE_DAY_MS); // ~1 year (365 days)
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(2), DOT, PUSD, 1));
		let v_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		// The Dormant residual keeps accruing: floor(155 * 0.5 * 365days / year) = 77.
		assert_eq!(v_post.debt.interest, 77);
	});
}

// Dormant vaults keep stake and receive redistribution gains on touch. We drive
// a genuine redistribution (not an offset) of vault 3's debt, so the dormant
// vault's principal *strictly* increases by its stake-weighted share — pinning
// `>` (not `>=`) since the 200-debt redistribution is not tiny.
#[test]
fn dormant_vault_receives_redistribution_gains_on_touch() {
	build_and_execute(|| {
		register_default_branch();
		// Distinct rates so the rate-index tail is deterministic — acct 1 at
		// the lower rate sits at the tail, where the redemption helper picks
		// it first.
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(open(3, DOT, 200, 200, rate_pct(5, 100)));
		assert_ok!(redeem(DOT, 4, 700)); // pushes acct 1 to Dormant

		// Drop the price so acct 3's CR falls below MCR — the vault pallet
		// refuses liquidation of a vault whose CR is at/above MCR. 1.0 puts
		// vault 3 (200 collateral, ~200 debt) under the 110% MCR while leaving vaults
		// 1 and 2 above it.
		let v_dormant_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		set_price(DOT, FixedU128::from_rational(1u128, 1u128));
		// Redistribute vault 3's whole debt across the recipients (no offset).
		let coll_3 = held(DOT, 3);
		assert_ok!(liquidate_with(DOT, 3, |_| LiquidationAllocation {
			offset: OffsetAllocation { recipient: 0, debt: 0, collateral: 0 },
			redistribution_collateral: coll_3,
			keeper: KeeperCompensation { recipient: 3, collateral: 0 },
		}));
		// Touch acct 1 so the interest-time lag closes and redistribution gains land on it.
		let pending_pre = BranchStates::<Test>::get(DOT, PUSD)
			.unwrap()
			.debt
			.pending_redistribution_principal;
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(2), DOT, PUSD, 1));
		let v_dormant_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		// The Dormant vault receives its exact stake-weighted share of the redistributed
		// principal on touch.
		let gained = v_dormant_post.debt.principal - v_dormant_pre.debt.principal;
		assert_eq!(gained, 97);
		// The branch's pending redistribution pool is drawn down by exactly that share.
		let pending_post = BranchStates::<Test>::get(DOT, PUSD)
			.unwrap()
			.debt
			.pending_redistribution_principal;
		assert_eq!(pending_pre - pending_post, gained);
	});
}

// `borrow` requires `vault.debt.principal >= config.minimum_debt` after the
// operation. Borrowing on a Dormant vault that doesn't reach the threshold
// reverts.
#[test]
fn dormant_borrow_below_min_debt_reverts() {
	build_and_execute(|| {
		register_default_branch();
		// Distinct rates so the rate-index tail is deterministic — acct 1 at
		// the lower rate sits at the tail, where the redemption helper picks
		// it first.
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(redeem(DOT, 3, 480)); // pushes acct 1 to Dormant with tiny debt
								   // Borrow 1 — total debt would be far below MinimumDebt 200.
		assert_noop!(
			crate::Pallet::<Test>::borrow(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				1,
				None,
				None,
				Position::endpoints_only()
			),
			crate::Error::<Test>::DebtBelowMinimum
		);
	});
}

// Reactivation model, and how it interacts with batching. A Dormant vault is
// revived only by `borrow` crossing MinimumDebt (or `activate_dormant` once
// accrued debt has) — never by a collateral deposit, which is rejected while
// Dormant because it cannot revive the vault. So the batch a user submits to
// "borrow with a collateral top-up" is `[borrow, deposit]`, borrow first: the
// borrow revives the vault to Active, after which the deposit is accepted. A
// `[deposit, borrow]` batch would instead fail at the deposit leg and roll back.
#[test]
fn dormant_revived_by_borrow_then_accepts_deposit() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(redeem(DOT, 3, 350));
		assert!(vault_status(DOT, 1).is_dormant());
		// While Dormant it is out of the rate index and parked as the redemption target.
		assert!(!<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&rate_list(DOT),
			&1
		));
		assert_eq!(
			BranchStates::<Test>::get(DOT, PUSD).unwrap().dormant_redemption_target,
			Some(1)
		);

		// A deposit alone cannot revive a Dormant vault → rejected (so it must not
		// lead a batch).
		assert_noop!(
			crate::Pallet::<Test>::deposit_collateral_for(
				RuntimeOrigin::signed(1),
				DOT,
				PUSD,
				1,
				100
			),
			crate::Error::<Test>::DebtBelowMinimum
		);

		// Borrow across MinimumDebt revives it to Active...
		assert_ok!(crate::Pallet::<Test>::borrow(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			500,
			None,
			None,
			Position::endpoints_only()
		));
		assert!(vault_status(DOT, 1).is_active());
		// ...re-inserted into the rate index and the dormant slot cleared.
		assert!(<LinkedList as SortedListInterface<VaultList, u64>>::contains(&rate_list(DOT), &1));
		assert_eq!(BranchStates::<Test>::get(DOT, PUSD).unwrap().dormant_redemption_target, None);

		// ...after which the deposit leg of the batch is accepted.
		let held_before = held(DOT, 1);
		assert_ok!(crate::Pallet::<Test>::deposit_collateral_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			100
		));
		assert_eq!(held(DOT, 1), held_before + 100);
	});
}

#[test]
fn dormant_vault_cannot_change_rate() {
	build_and_execute(|| {
		register_default_branch();
		// Distinct rates so the rate-index tail is deterministic — acct 1 at
		// the lower rate sits at the tail, where the redemption helper picks
		// it first.
		assert_ok!(open(1, DOT, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(redeem(DOT, 3, 350));
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
fn redemption_is_path_independent_across_chunks() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(TOKEN_X, PUSD);
		// Identical vaults on the two markets (same owner, collateral, debt, rate).
		assert_ok!(open(1, DOT, 1_000, 1_000, rate_pct(5, 100)));
		assert_ok!(open_market(1, TOKEN_X, PUSD, 1_000, 1_000, rate_pct(5, 100)));
		let dot_pre = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		let tokenx_pre = Vaults::<Test>::get((TOKEN_X, PUSD, 1)).unwrap();
		assert_eq!(dot_pre.debt.total(), tokenx_pre.debt.total());
		let dot_held_pre = held(DOT, 1);
		let tokenx_held_pre = held(TOKEN_X, 1);
		let redeemer_dot_pre = collateral_balance(DOT, 3);
		let redeemer_tokenx_pre = collateral_balance(TOKEN_X, 3);

		// Many: three 100-unit redemptions against the DOT vault (stays Active > 200).
		for _ in 0..3 {
			assert_eq!(redeem(DOT, 3, 100).expect("redeem ok"), 1);
		}
		// One: a single 300-unit redemption against the identical TOKEN_X vault.
		assert_eq!(redeem_market(TOKEN_X, PUSD, 3, 300).expect("redeem ok"), 1);

		let dot_post = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		let tokenx_post = Vaults::<Test>::get((TOKEN_X, PUSD, 1)).unwrap();
		// Exactly 300 debt cancelled on each path, leaving identical residual debt
		// (including the interest-first split of principal vs interest).
		assert_eq!(dot_pre.debt.total() - dot_post.debt.total(), 300);
		assert_eq!(dot_post.debt.principal, tokenx_post.debt.principal);
		assert_eq!(dot_post.debt.interest, tokenx_post.debt.interest);
		// Exactly 30 collateral released on each path (3 * floor(100/10) == floor(300/10)).
		assert_eq!(dot_held_pre - held(DOT, 1), 30);
		assert_eq!(tokenx_held_pre - held(TOKEN_X, 1), 30);
		assert_eq!(collateral_balance(DOT, 3) - redeemer_dot_pre, 30);
		assert_eq!(collateral_balance(TOKEN_X, 3) - redeemer_tokenx_pre, 30);
	});
}
