use crate::{
	mock::*,
	pallet::Vaults,
	tests::{rate_pct, vault_status},
};
use pallet_linked_list::SortedListInterface;

// Opening a vault from an account whose free balance is below the requested
// collateral fails at the token layer: the `fungible::hold` call returns an
// error. Account 100 is not funded by genesis (only 1..=10 are).
#[test]
fn open_vault_fails_without_balance() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert!(open(100, DOT, PUSD, 1_000, 500, rate_pct(5, 100)).is_err());
	});
}

#[test]
fn adjust_vault_via_deposit_then_borrow() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// +200 collateral.
		assert_ok!(crate::Pallet::<Test>::deposit_collateral_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			200
		));
		// +300 debt (no rate change). `None` recipient defaults to the owner.
		assert_ok!(crate::Pallet::<Test>::borrow(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			300,
			None,
			None,
			Position::endpoints_only()
		));
		assert_eq!(held(DOT, 1), 1_200);
		let v = Vaults::<Test>::get((DOT, PUSD, 1)).expect("vault stored");
		assert_eq!(v.debt.principal, 800);
		// Each op charges a 1-unit upfront fee (open 500 & borrow 300 at 5%), both
		// recorded as debt: debt.interest = 2, total debt = 802.
		assert_eq!(v.debt.interest, 2);
		assert_eq!(v.debt.total(), 802);
		// Each 1-unit fee is split per `SpFeeShare` before the residual reaches
		// FEE_DEST (Permill multiplication rounds 75% of 1 up to 1, leaving 0).
		let residual_per_fee = 1u128 - SpFeeShare::get() * 1u128;
		assert_eq!(stable_balance(PUSD, FEE_DEST), 2 * residual_per_fee);
		// Branch aggregate mirrors the vault principal.
		assert_eq!(branch_state(DOT, PUSD).unwrap().debt.principal, 800);
		// pUSD net to user: initial 500 + 300 borrowed. The upfront fee is recorded as
		// debt, not deducted from the minted pUSD the user receives.
		assert_eq!(stable_balance(PUSD, 1), 800);
	});
}

#[test]
fn borrow_with_recipient_mints_to_recipient_not_owner() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 2_000, 500, rate_pct(5, 100)));
		let owner_pre = stable_balance(PUSD, 1);
		let recipient_pre = stable_balance(PUSD, 4);

		assert_ok!(crate::Pallet::<Test>::borrow(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			300,
			None,
			Some(4),
			Position::endpoints_only()
		));

		assert_eq!(stable_balance(PUSD, 1), owner_pre);
		assert_eq!(stable_balance(PUSD, 4), recipient_pre + 300);
		let v = Vaults::<Test>::get((DOT, PUSD, 1)).expect("vault stored");
		assert_eq!(v.debt.principal, 800);
	});
}

#[test]
fn withdraw_collateral_with_recipient_transfers_to_recipient() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 3_000, 500, rate_pct(5, 100)));
		let recipient_pre = collateral_balance(DOT, 4);

		assert_ok!(crate::Pallet::<Test>::withdraw_collateral(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			250,
			Some(4)
		));

		assert_eq!(held(DOT, 1), 2_750);
		assert_eq!(collateral_balance(DOT, 4), recipient_pre + 250);
	});
}

#[test]
fn repay_for_by_third_party_burns_payer_balance_and_updates_owner_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 2_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 2_000, 500, rate_pct(5, 100)));
		let payer_pre = stable_balance(PUSD, 2);
		let v_pre = Vaults::<Test>::get((DOT, PUSD, 1)).expect("vault stored");

		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(2), DOT, PUSD, 1, 100));

		assert_eq!(stable_balance(PUSD, 2), payer_pre - 100);
		let v_post = Vaults::<Test>::get((DOT, PUSD, 1)).expect("vault stored");
		assert_eq!(v_post.debt.total(), v_pre.debt.total() - 100);
	});
}

#[test]
fn close_vault_with_recipient_releases_collateral_to_recipient() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		let v = Vaults::<Test>::get((DOT, PUSD, 1)).expect("vault stored");
		let total = v.debt.total();
		assert_eq!(redeem(DOT, PUSD, 3, total).expect("redeem ok"), 1);
		assert!(vault_status(DOT, PUSD, 1).is_dormant());

		let residual = held(DOT, 1);
		let recipient_pre = collateral_balance(DOT, 4);
		assert_ok!(crate::Pallet::<Test>::close_vault(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			Some(4)
		));

		assert!(Vaults::<Test>::get((DOT, PUSD, 1)).is_none());
		assert_eq!(held(DOT, 1), 0);
		assert_eq!(collateral_balance(DOT, 4), recipient_pre + residual);
	});
}

// Repaying an Active vault's debt to zero does NOT close it: the collateral
// stays held and the row survives as a zero-debt Dormant husk, out of the rate
// index. The owner reclaims the collateral with an explicit `close_vault`.
// Auto-closing would forbid repaying purely to improve branch TCR in Safety
// mode, which we deliberately allow.
#[test]
fn repay_for_to_zero_leaves_dormant_husk() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		let v = Vaults::<Test>::get((DOT, PUSD, 1)).expect("vault stored");
		let total = v.debt.principal + v.debt.interest;
		assert_ok!(<Pusd as frame::traits::fungible::Mutate<u64>>::transfer(
			&2,
			&1,
			v.debt.interest,
			frame::traits::tokens::Preservation::Expendable,
		));
		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, total));

		// Row survives as a zero-debt husk with its collateral still held.
		let husk = Vaults::<Test>::get((DOT, PUSD, 1)).expect("husk survives");
		assert_eq!(husk.debt.total(), 0, "debt cleared to zero");
		assert_eq!(held(DOT, 1), 1_000, "collateral stays held by the vault");
		assert!(vault_status(DOT, PUSD, 1).is_dormant(), "zero-debt vault is Dormant");
		assert!(
			!<LinkedList as SortedListInterface<VaultList, u64>>::contains(
				&rate_list(DOT, PUSD),
				&1
			),
			"husk left the rate index"
		);
		assert!(
			!System::events()
				.iter()
				.any(|e| matches!(e.event, RuntimeEvent::Vaults(crate::Event::VaultClosed { .. }))),
			"repay-to-zero does not auto-close"
		);

		// The owner reclaims the collateral with an explicit close.
		let collateral_before = collateral_balance(DOT, 1);
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), DOT, PUSD, None));
		assert!(Vaults::<Test>::get((DOT, PUSD, 1)).is_none(), "close removes the row");
		assert_eq!(held(DOT, 1), 0, "collateral released on close");
		assert_eq!(
			collateral_balance(DOT, 1),
			collateral_before + 1_000,
			"owner received the collateral"
		);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::VaultClosed {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			recipient: 1,
		}));
	});
}

// Poking a nonexistent vault is an error, not a silent success — a typo'd
// owner must not look like a completed refresh.
#[test]
fn poke_missing_vault_errors() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_noop!(
			crate::Pallet::<Test>::poke(RuntimeOrigin::signed(1), DOT, PUSD, 99),
			crate::Error::<Test>::VaultNotFound
		);
	});
}

// `repay_for` caps at the outstanding debt: over-asking burns only what is
// owed and leaves the vault as a zero-debt Dormant husk (no auto-close), with
// a single `Repaid` carrying the actual (capped) amount.
#[test]
fn repay_overpay_burns_only_debt_and_leaves_husk() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		// Acct 2's minted pUSD funds the surplus over acct 1's own balance.
		assert_ok!(<Pusd as frame::traits::fungible::Mutate<u64>>::transfer(
			&2,
			&1,
			400,
			frame::traits::tokens::Preservation::Expendable,
		));
		let v = Vaults::<Test>::get((DOT, PUSD, 1)).expect("vault stored");
		let total = v.debt.principal + v.debt.interest;
		let balance_before = stable_balance(PUSD, 1);
		assert!(balance_before > total, "overpay setup needs a surplus");

		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			balance_before
		));

		assert_eq!(stable_balance(PUSD, 1), balance_before - total, "only the debt burned");
		let husk = Vaults::<Test>::get((DOT, PUSD, 1)).expect("husk survives");
		assert_eq!(husk.debt.total(), 0, "debt cleared");
		assert_eq!(held(DOT, 1), 1_000, "collateral untouched by repay");
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::Repaid {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			from: 1,
			amount: total,
		}));
		assert!(
			!System::events()
				.iter()
				.any(|e| matches!(e.event, RuntimeEvent::Vaults(crate::Event::VaultClosed { .. }))),
			"overpay-to-zero does not auto-close"
		);
	});
}

// A sub-minimum Dormant residual cannot be partially repaid (any non-zero
// remainder below MinimumDebt is `DebtWouldBecomeDust`), so the owner must clear
// it to exactly zero. The overpay cap turns that from an exact-amount guessing
// game into "send at least the dust"; the cleared vault is left as a husk.
#[test]
fn repay_overpay_rescues_subminimum_dormant_vault() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		// Redeem acct 1 (the rate-index tail) down to exactly MinimumDebt - 1 (199),
		// the largest sub-minimum residual, so it parks in the Dormant slot.
		let debt = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().debt.total();
		assert_ok!(redeem(DOT, PUSD, 3, debt - 199));
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		let residual = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().debt.total();
		assert_eq!(residual, 199, "residual is MinimumDebt - 1");

		let balance_before = stable_balance(PUSD, 1);
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			balance_before
		));

		assert_eq!(
			stable_balance(PUSD, 1),
			balance_before - residual,
			"only the dust residual burned"
		);
		let husk = Vaults::<Test>::get((DOT, PUSD, 1)).expect("husk survives");
		assert_eq!(husk.debt.total(), 0, "sub-minimum dust cleared to zero");
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
	});
}

// A redemption-driven Dormant residual repaid to zero is left as a husk (its
// collateral persists) and frees the branch's `dormant_redemption_target` slot.
#[test]
fn repay_for_to_zero_on_dormant_leaves_husk_and_releases_slot() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		// Push acct 1 to Dormant with a small residual debt; it parks the slot.
		assert_ok!(redeem(DOT, PUSD, 3, 350));
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_eq!(
			branch_state(DOT, PUSD).unwrap().dormant_redemption_target,
			Some(1),
			"sub-minimum redemption parked acct 1 in the dormant slot"
		);
		let total = Vaults::<Test>::get((DOT, PUSD, 1)).unwrap().debt.total();
		assert!(total > 0);
		let held_before = held(DOT, 1);
		assert!(held_before > 0, "collateral persists on the dormant row");
		assert_ok!(<Pusd as frame::traits::fungible::Mutate<u64>>::transfer(
			&2,
			&1,
			total.saturating_sub(stable_balance(PUSD, 1)),
			frame::traits::tokens::Preservation::Expendable,
		));
		assert_ok!(crate::Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, total));

		let husk = Vaults::<Test>::get((DOT, PUSD, 1)).expect("husk survives");
		assert_eq!(husk.debt.total(), 0);
		assert_eq!(held(DOT, 1), held_before, "collateral untouched by repay");
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		let state = branch_state(DOT, PUSD).expect("state");
		assert_eq!(state.dormant_redemption_target, None, "slot released on repay-to-zero");
	});
}

// Closing the last vault used to dead-end: the branch mints aggregate interest
// with per-op ceilings while vaults accrue floors, so a drift residual stayed in
// `minted_interest` forever, read as TCR 0, and `WouldEnterSafetyMode` blocked
// the close. The terminal close must instead sweep the orphan into `bad_debt`
// (it is unbacked circulating pUSD) and settle.
//
// Repaying to zero now only turns each vault into a husk; the sweep happens when
// the *last* husk is closed with `close_vault`. The drift here is tiny — a
// handful of base units (`DRIFT` below), the sum of per-mint ceiling-vs-floor
// rounding across the two pokes — which is exactly why it strands unnoticed
// unless the terminal close sweeps it.
#[test]
fn closing_last_vault_sweeps_interest_drift_to_bad_debt() {
	use frame::traits::fungible::Mutate;
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 400, rate_pct(7, 100)));
		// Distinct accrual timestamps make several ceiling mints land while
		// the vaults only ever accrue floors.
		advance_time(30 * 24 * 3_600 * 1_000);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 1));
		advance_time(24 * 3_600 * 1_000);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(9), DOT, PUSD, 2));
		// Top up both owners so overpay-repays can cover accrued interest.
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&1, 100));
		assert_ok!(<Pusd as Mutate<u64>>::mint_into(&2, 100));

		// Repay both to zero: each becomes a husk, no sweep yet (their stakes keep
		// the branch non-empty).
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(2),
			DOT,
			PUSD,
			2,
			10_000
		));
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			10_000
		));
		assert!(vault_status(DOT, PUSD, 2).is_dormant(), "vault 2 is a husk");
		assert!(vault_status(DOT, PUSD, 1).is_dormant(), "vault 1 is a husk");
		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.debt.bad_debt, 0, "no sweep while husks still hold stake");

		// Closing the first husk still leaves the branch non-empty: no sweep.
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(2), DOT, PUSD, None));
		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.debt.bad_debt, 0, "no sweep while a husk remains");

		// Closing the last husk empties the branch and sweeps the drift. This
		// terminal close was previously rejected with `WouldEnterSafetyMode`.
		// A maximal price would overflow its pre-close TCR; settlement bypasses
		// ratio math entirely and must still complete.
		set_price(DOT, FixedU128::from_inner(u128::MAX));
		assert_ok!(crate::Pallet::<Test>::close_vault(RuntimeOrigin::signed(1), DOT, PUSD, None));
		assert!(Vaults::<Test>::get((DOT, PUSD, 1)).is_none(), "last husk closed");

		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.debt.principal, 0);
		assert_eq!(state.stakes.total, 0);
		assert_eq!(state.debt.minted_interest, 0, "drift swept out of minted_interest");
		assert_eq!(state.ownerless_debt, 0);
		// The drift is exactly this small — a few base units of rounding.
		const DRIFT: Balance = 2;
		assert_eq!(state.debt.bad_debt, DRIFT, "drift recorded as bad debt");
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::BadDebtRecorded {
			collateral_id: DOT,
			stable_id: PUSD,
			amount: state.debt.bad_debt,
		}));

		// The insurance flow can now heal the branch clean.
		let credit = <Assets as frame::traits::fungibles::Balanced<AccountId>>::issue(
			PUSD,
			state.debt.bad_debt,
		);
		let surplus =
			<crate::Pallet<Test> as pusd_primitives::VaultInterface>::heal(&DOT, &PUSD, credit)
				.expect("heal succeeds");
		assert_eq!(surplus.peek(), 0);
		let state = branch_state(DOT, PUSD).expect("branch state");
		assert_eq!(state.debt.bad_debt, 0, "branch fully settled");
	});
}

#[test]
fn redemption_slot_rejects_second_owner() {
	fn park(owner: AccountId) -> DispatchResult {
		redeem_step(DOT, PUSD, owner, 7, |snapshot| {
			Ok(Some(settlement(PUSD, snapshot.debt - 150, (snapshot.debt - 150) / 10)))
		})
	}
	fn parked() -> Option<AccountId> {
		branch_state(DOT, PUSD).expect("branch state").dormant_redemption_target
	}
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		assert_ok!(open(3, DOT, PUSD, 1_000, 500, rate_pct(3, 100)));

		assert_ok!(park(1));
		assert_eq!(parked(), Some(1));
		assert_eq!(park(2).unwrap_err(), crate::Error::<Test>::DormantTargetOccupied.into());
		assert_eq!(parked(), Some(1), "slot still points at the first owner");
		assert!(vault_status(DOT, PUSD, 1).is_dormant(), "first dormant intact");
		assert!(
			vault_status(DOT, PUSD, 2).is_active(),
			"second vault stays Active (step rolled back)"
		);
	});
}
