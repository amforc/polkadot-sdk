use crate::{
	mock::*,
	tests::{percent, vault_status, ONE_DAY_MS},
};

/// In the registered `(DOT, STABLE)` branch, open one vault, then drop the oracle price so the
/// branch enters Safety mode (TCR ≈ 125.87% — between ICR 120% and Safety 130%).
///
/// pre-state: state.total_collateral=1000 DOT, state.debt.principal=5000 STABLE, price=6.30.
/// Vault (acct 1) starts at CR=199.6% before the price drop and stays the
/// only vault on the branch.
fn enter_safety_mode_single_vault() {
	assert_ok!(open(1, DOT, STABLE, 1_000, 5_000, percent(5)));
	set_price(DOT, FixedU128::from_rational(63u128, 10u128));
	// Sanity: `state.frozen` must remain `None`; mode is *derived* from TCR.
	assert!(!market_state(DOT, STABLE).is_frozen());
	// Verify the branch really entered Safety mode rather than trusting the
	// hand-picked price — the mode is derived from live TCR.
	assert_eq!(
		branch_mode(DOT, STABLE),
		Some(BranchMode::Safety),
		"price drop must put the branch in Safety mode"
	);
}

/// Returns the live TCR of the `(DOT, STABLE)` branch.
fn safety_branch_tcr() -> stablecoin_primitives::CollateralRatio {
	crate::Pallet::<Test>::branch_tcr(DOT, STABLE).expect("tcr")
}

// All four positions clear ICR but worsen the already stressed branch's TCR.
#[test]
fn safety_mode_rejects_tcr_worsening_operations() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_safety_mode_single_vault();

		assert_noop!(
			open(2, DOT, STABLE, 100, 510, percent(5)),
			crate::Error::<Test>::SafetyModeTcrWorsening
		);
		assert_noop!(
			borrow(1, DOT, STABLE, 200, None),
			crate::Error::<Test>::SafetyModeTcrWorsening
		);
		assert_noop!(
			withdraw_collateral(1, DOT, STABLE, 1, None),
			crate::Error::<Test>::SafetyModeTcrWorsening
		);
		assert_noop!(
			change_rate(1, DOT, STABLE, percent(7)),
			crate::Error::<Test>::SafetyModeTcrWorsening
		);
	});
}

// The Safety-branch rule is "post_tcr ≥ pre_tcr" (non-worsening), with no upper bound, so it must
// not over-block. Opening a large healthy vault that drives TCR up is allowed, even when it
// implicitly exits Safety mode.
#[test]
fn safety_mode_allows_opening_a_vault_that_raises_tcr() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_safety_mode_single_vault();
		let tcr_before = safety_branch_tcr();

		// New vault B with CR = 630% — improves TCR substantially.
		assert_ok!(open(2, DOT, STABLE, 1_000, 1_000, percent(5)));

		assert!(safety_branch_tcr() > tcr_before, "the operation strictly improves branch TCR");
	});
}

// Repayment always raises TCR, so it needs no mode gate; this is also why repaying to zero leaves a
// husk rather than closing it.
#[test]
fn safety_mode_allows_repayment() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_safety_mode_single_vault();
		let tcr_before = safety_branch_tcr();

		assert_ok!(repay(1, DOT, STABLE, 1, Some(2_000)));

		assert!(safety_branch_tcr() > tcr_before, "the operation strictly improves branch TCR");
	});
}

// Redemption always raises TCR, so it needs no mode gate.
#[test]
fn safety_mode_allows_redemption() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_safety_mode_single_vault();
		let tcr_before = safety_branch_tcr();

		let target = redeem(DOT, STABLE, 3, 1_000);

		assert_eq!(target, Ok(1));
		// The vault keeps enough debt to stay Active.
		assert!(vault_status(DOT, STABLE, 1).is_active());
		assert!(safety_branch_tcr() > tcr_before, "the operation strictly improves branch TCR");
	});
}

// A rate change that carries no upfront fee leaves TCR unchanged, so the non-worsening rule allows
// it.
#[test]
fn safety_mode_allows_a_fee_free_rate_change() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		enter_safety_mode_single_vault();
		// Wait out the default one-day cooldown so the rate change carries no upfront fee.
		advance_time(ONE_DAY_MS);

		assert_ok!(change_rate(1, DOT, STABLE, percent(7)));
	});
}

// In Normal mode, a premature rate change that would push TCR below the
// safety threshold reverts. The upfront fee bumps
// `state.debt.minted_interest` and lowers post-TCR; if pre-TCR is
// just above Safety, post-TCR can land below it.
#[test]
fn normal_mode_blocks_premature_rate_change_pulling_into_safety() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 5_000, percent(5)));
		// Drop price to 6.55 — TCR ≈ 1000*6.55/5005 ≈ 130.87% (just above
		// Safety 130%). The upfront fee on a premature rate hike bumps
		// `debt.minted_interest` enough to land post-TCR below
		// Safety, tripping the Normal-branch rule in `enforce_mode_rules`.
		set_price(DOT, FixedU128::from_rational(655u128, 100u128));

		assert_noop!(
			change_rate(1, DOT, STABLE, percent(50)),
			crate::Error::<Test>::WouldEnterSafetyMode
		);
	});
}

// Repaying to zero is always allowed (it improves TCR) and just leaves a husk; the TCR gate is on
// releasing its collateral. Withdrawing the last collateral from a debt-free vault auto-closes it,
// so that withdraw hits the same gate as an explicit close.
#[test]
fn emptying_withdraw_is_tcr_gated_and_auto_closes() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 5_000, percent(5)));
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(5)));
		// Top up acct 2's stablecoin so the repay can cover principal + upfront fee.
		let v = vault(DOT, STABLE, 2);
		let total = v.debt.principal + v.debt.interest;
		assert_ok!(<Stable as frame::traits::fungible::Mutate<AccountId>>::transfer(
			&1,
			&2,
			v.debt.interest,
			frame::traits::tokens::Preservation::Expendable,
		));
		// Repay to zero — leaves a Dormant husk still holding its 1000 DOT.
		assert_ok!(repay(2, DOT, STABLE, 2, Some(total)));
		assert!(vault_status(DOT, STABLE, 2).is_dormant(), "husk survives the repay");

		// The branch is still in Normal mode at price 6.30 (TCR ≈ 252%), but releasing the
		// husk's 1000 DOT would push TCR into Safety, so the emptying withdraw is blocked exactly
		// like the explicit close.
		set_price(DOT, FixedU128::from_rational(63u128, 10u128));
		assert_eq!(branch_mode(DOT, STABLE), Some(BranchMode::Normal),);
		assert_noop!(close_vault(2, DOT, STABLE, None), crate::Error::<Test>::WouldEnterSafetyMode);
		assert_noop!(
			withdraw_collateral(2, DOT, STABLE, 1_000, None),
			crate::Error::<Test>::WouldEnterSafetyMode
		);

		// Back at price 10 the release keeps the branch in Normal mode: the withdraw
		// empties the vault and auto-closes it, paying out the collateral.
		set_price(DOT, DEFAULT_PRICE);
		let collateral_before = collateral_balance(DOT, 2);
		assert_ok!(withdraw_collateral(2, DOT, STABLE, 1_000, None));

		assert!(!vault_exists(DOT, STABLE, 2), "zero/zero vault auto-closed");
		assert_eq!(held(DOT, 2), 0, "hold fully released");
		assert_eq!(collateral_balance(DOT, 2), collateral_before + 1_000 + VAULT_DEPOSIT);
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::VaultClosed {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 2,
			recipient: 2,
			collateral: 1_000,
		}));

		// The row is gone, so an explicit close has nothing to act on.
		assert_noop!(close_vault(2, DOT, STABLE, None), crate::Error::<Test>::VaultNotFound);
	});
}

// The ICR gate protects each vault in both branch modes, against both a borrow and a withdraw. A
// change to another vault can change the branch mode, but it cannot make this vault safe.
#[test]
fn borrow_and_withdraw_below_icr_blocked_in_both_modes() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 5_000, percent(5)));
		assert_ok!(open(2, DOT, STABLE, 100, 200, percent(5)));
		// This price puts the vault below ICR and the branch in Safety mode.
		set_price(DOT, FixedU128::from_rational(21u128, 10u128));
		assert_eq!(branch_mode(DOT, STABLE), Some(BranchMode::Safety));

		let borrow_one = || borrow(2, DOT, STABLE, 1, None);
		let withdraw_one = || withdraw_collateral(2, DOT, STABLE, 1, None);
		assert_noop!(borrow_one(), crate::Error::<Test>::UnsafeCollateralizationRatio);
		assert_noop!(withdraw_one(), crate::Error::<Test>::UnsafeCollateralizationRatio);

		// Improve only the branch ratio to isolate the per-vault gate.
		assert_ok!(deposit_collateral(1, DOT, STABLE, 1, 99_000));
		assert_eq!(branch_mode(DOT, STABLE), Some(BranchMode::Normal));
		assert_noop!(borrow_one(), crate::Error::<Test>::UnsafeCollateralizationRatio);
		assert_noop!(withdraw_one(), crate::Error::<Test>::UnsafeCollateralizationRatio);

		// The same calls succeed after this vault clears ICR.
		assert_ok!(deposit_collateral(2, DOT, STABLE, 2, 100));
		assert_ok!(borrow_one());
		assert_ok!(withdraw_one());
	});
}
