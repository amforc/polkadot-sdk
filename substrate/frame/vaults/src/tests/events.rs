use crate::{mock::*, tests::rate_pct};

fn assert_event(event: crate::Event<Test>) {
	System::assert_has_event(RuntimeEvent::Vaults(event));
}

// Open emits VaultOpened carrying both inputs, plus UpfrontFeeCharged for the
// protocol-favored fee.
#[test]
fn open_vault_emits_canonical_events() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(10, 100)));
		assert_event(crate::Event::VaultOpened {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			collateral: 1_000,
			debt: 2_000,
		});
		// Upfront fee is non-trivial for these inputs.
		let predicted_fee =
			crate::Pallet::<Test>::predict_open_upfront_fee(DOT, PUSD, 2_000, rate_pct(10, 100))
				.expect("registered market");
		assert!(predicted_fee > 0);
		// The charged fee equals the vault's recorded interest; assert the
		// event carries that amount.
		let v = crate::pallet::Vaults::<Test>::get((DOT, PUSD, 1)).unwrap();
		assert_event(crate::Event::UpfrontFeeCharged {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			amount: v.debt.interest,
		});
	});
}

// A third-party deposit emits CollateralDeposited (`from` = caller, `owner` =
// vault owner).
#[test]
fn deposit_collateral_emits_collateral_deposited() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(crate::Pallet::<Test>::deposit_collateral_for(
			RuntimeOrigin::signed(2),
			DOT,
			PUSD,
			1,
			100
		));
		// `from` is the caller (acct 2), `owner` is the vault owner (acct 1).
		assert_event(crate::Event::CollateralDeposited {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			from: 2,
			amount: 100,
		});
	});
}

#[test]
fn borrow_emits_borrowed() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 3_000, 2_000, rate_pct(5, 100)));
		// `None` recipient defaults to the owner; the emitted event confirms it.
		assert_ok!(crate::Pallet::<Test>::borrow(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			500,
			None,
			None,
			Position::endpoints_only()
		));
		assert_event(crate::Event::Borrowed {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			recipient: 1,
			amount: 500,
		});
	});
}

#[test]
fn withdraw_collateral_emits_collateral_withdrawn() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 3_000, 500, rate_pct(5, 100)));
		// `None` recipient defaults to the owner; the emitted event confirms it.
		assert_ok!(crate::Pallet::<Test>::withdraw_collateral(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			100,
			None
		));
		assert_event(crate::Event::CollateralWithdrawn {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			recipient: 1,
			amount: 100,
		});
	});
}

#[test]
fn repay_emits_repaid() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 1_000, rate_pct(5, 100)));
		assert_ok!(crate::Pallet::<Test>::repay_for(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			1,
			Some(200)
		));
		assert_event(crate::Event::Repaid {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			from: 1,
			amount: 200,
		});
	});
}

// A rate change emits BorrowRateChanged (and UpfrontFeeCharged when premature).
#[test]
fn change_rate_emits_borrow_rate_changed() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		// After the cooldown, no fee — only BorrowRateChanged.
		advance_time(24 * 3_600 * 1_000);
		assert_ok!(crate::Pallet::<Test>::change_rate(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			rate_pct(7, 100),
			Position::endpoints_only()
		));
		assert_event(crate::Event::BorrowRateChanged {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			old_rate: rate_pct(5, 100),
			new_rate: rate_pct(7, 100),
		});
	});
}

#[test]
fn premature_change_rate_emits_upfront_fee_charged() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		// Within the cooldown window — fee charged.
		advance_time(12 * 3_600 * 1_000);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(1), DOT, PUSD, 1));
		let predicted =
			crate::Pallet::<Test>::predict_rate_change_upfront_fee(DOT, PUSD, 1, rate_pct(7, 100))
				.expect("registered market and vault");
		assert!(predicted > 0);
		assert_ok!(crate::Pallet::<Test>::change_rate(
			RuntimeOrigin::signed(1),
			DOT,
			PUSD,
			rate_pct(7, 100),
			Position::endpoints_only()
		));
		assert_event(crate::Event::UpfrontFeeCharged {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			amount: predicted,
		});
		assert_event(crate::Event::BorrowRateChanged {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			old_rate: rate_pct(5, 100),
			new_rate: rate_pct(7, 100),
		});
	});
}

// poke emits InterestAccrued when there is pending interest to materialise.
#[test]
fn poke_emits_interest_accrued() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 3_000, 2_000, rate_pct(50, 100)));
		advance_time(7 * 24 * 3_600 * 1_000);
		assert_ok!(crate::Pallet::<Test>::poke(RuntimeOrigin::signed(2), DOT, PUSD, 1));
		// Exact magnitude: 7 days at 50% on 2_000 principal accrues
		// floor(2_000 * 0.5 * 7days / year) = 19 (interest is on principal, not the fee).
		System::assert_has_event(RuntimeEvent::Vaults(crate::Event::InterestAccrued {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			amount: 19,
		}));
	});
}

// A redemption emits VaultRedeemed (one per redeemed vault).
#[test]
fn redemption_emits_vault_redeemed() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(1, 100)));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(2, 100)));
		let target = redeem(DOT, PUSD, 3, 200).expect("redeem ok");
		assert_eq!(target, 1);
		// VaultRedeemed event: don't pin the exact magnitudes (collateral
		// rounding depends on price), just confirm the event landed.
		let saw = System::events().into_iter().any(|e| {
			matches!(
				e.event,
				RuntimeEvent::Vaults(crate::Event::VaultRedeemed {
					collateral_id, owner, recipient, debt_cancelled, ..
				}) if collateral_id == DOT
					&& owner == 1
					&& recipient == 3
					&& debt_cancelled == 200
			)
		});
		assert!(saw, "expected a VaultRedeemed event");
	});
}

// register_branch emits BranchRegistered.
#[test]
fn register_branch_emits_branch_registered() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_event(crate::Event::BranchRegistered { collateral_id: DOT, stable_id: PUSD });
	});
}

// A governance freeze emits ModeChanged.
#[test]
fn set_governance_frozen_emits_mode_changed() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(crate::Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			true
		));
		// Branch starts in Normal mode (no debt yet, TCR is treated as
		// infinity); after the governance freeze it transitions to Frozen.
		let saw = System::events().into_iter().any(|e| {
			matches!(
				e.event,
				RuntimeEvent::Vaults(crate::Event::ModeChanged { collateral_id, new_mode, .. })
					if collateral_id == DOT
						&& matches!(new_mode, crate::BranchMode::Frozen { .. })
			)
		});
		assert!(saw, "expected a ModeChanged → Frozen event");
	});
}

// Governance setters emit ParameterUpdated carrying the changed field and value.
#[test]
fn set_parameter_emits_parameter_updated() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(crate::Pallet::<Test>::set_param(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			crate::types::BranchConfigUpdate::MinimumCollateralizationRatio(
				FixedU128::from_rational(115u128, 100u128)
			)
		));
		assert_event(crate::Event::ParameterUpdated {
			collateral_id: DOT,
			stable_id: PUSD,
			update: crate::types::BranchConfigUpdate::MinimumCollateralizationRatio(
				FixedU128::from_rational(115u128, 100u128),
			),
		});
	});
}

// A DebtCeiling update goes through the same shared parameter machinery and
// emits ParameterUpdated.
#[test]
fn debt_ceiling_update_emits_parameter_updated() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(crate::Pallet::<Test>::set_param(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			crate::types::BranchConfigUpdate::DebtCeiling(50_000_000)
		));
		assert_event(crate::Event::ParameterUpdated {
			collateral_id: DOT,
			stable_id: PUSD,
			update: crate::types::BranchConfigUpdate::DebtCeiling(50_000_000),
		});
		assert_eq!(branch_config(DOT, PUSD).expect("config").debt_ceiling, 50_000_000);
	});
}

// enter_final_recovery emits VaultStatusChanged.
#[test]
fn enter_final_recovery_emits_status_change() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Single vault that we'll push into FinalRecovery via a price drop.
		assert_ok!(open(1, DOT, PUSD, 1_000, 2_000, rate_pct(5, 100)));
		set_price(DOT, FixedU128::from_rational(2u128, 100u128));
		assert_ok!(crate::Pallet::<Test>::enter_final_recovery(
			RuntimeOrigin::signed(2),
			DOT,
			PUSD,
			1
		));
		assert_event(crate::Event::VaultStatusChanged {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
			old_status: crate::types::VaultStatus::Active,
			new_status: crate::types::VaultStatus::FinalRecovery,
		});
	});
}
