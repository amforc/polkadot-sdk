use crate::{mock::*, tests::*, types::BranchConfigUpdate};
use frame::traits::fungibles::Inspect as _;
use pallet_linked_list::SortedListInterface;

/// Replacement admins used by the reassignment test.
const NEW_FULL_ADMIN: AccountId = 300;
const NEW_EMERGENCY_ADMIN: AccountId = 301;

fn market_exists(collateral: AssetId, stable: StableId) -> bool {
	branch_state(collateral, stable).is_some()
}

/// Repay `owner`'s full `(DOT, STABLE)` debt and close the vault, emptying the
/// branch. Mints a stablecoin buffer first so any accrued interest beyond the borrowed
/// principal is covered. Repay-to-zero leaves a Dormant husk that still holds the
/// collateral, so an explicit `close_vault` is needed to empty the branch.
fn repay_to_close(owner: AccountId) {
	let total = vault(DOT, STABLE, owner).debt.total();
	mint_stable(STABLE, owner, total);
	assert_ok!(repay(owner, DOT, STABLE, owner, None));
	assert_ok!(close_vault(owner, DOT, STABLE, None));
	assert!(!vault_exists(DOT, STABLE, owner), "close removed the vault");
}

// A signed asset-owner create locks the refundable deposit; removing the empty
// branch refunds it in full.
#[test]
fn signed_create_takes_deposit_and_remove_refunds() {
	ExtBuilder::default().price(DOT, DEFAULT_PRICE).build_and_execute(|| {
		assert_eq!(creation_deposit_held(STABLECOIN_OWNER), 0);

		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::signed(STABLECOIN_OWNER),
			DOT,
			STABLE,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));

		assert_eq!(creation_deposit_held(STABLECOIN_OWNER), MarketDepositBase::get());

		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, STABLE));

		assert_eq!(creation_deposit_held(STABLECOIN_OWNER), 0, "deposit refunded on removal");
		assert!(!market_exists(DOT, STABLE));
		assert_event(crate::Event::BranchRemoved { collateral_id: DOT, stable_id: STABLE });
	});
}

// A privileged creation has no depositor, so the branch's full administrator
// pays the custody seed. Without the collateral to pay it, registration fails
// outright rather than leaving a branch whose first redistributing liquidation
// would revert.
#[test]
fn create_rejects_a_creator_that_cannot_pay_the_custody_seed() {
	ExtBuilder::default().build_and_execute(|| {
		const PENNILESS_ADMIN: AccountId = 999;
		set_price(TOKEN_X, DEFAULT_PRICE);
		assert_eq!(collateral_balance(TOKEN_X, PENNILESS_ADMIN), 0);

		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				TOKEN_X,
				STABLE,
				branch_admins(PENNILESS_ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			Error::<Test>::CustodySeedUnavailable
		);
		assert!(!market_exists(TOKEN_X, STABLE));
	});
}

// Registration takes the provider reference before it
// moves the custody seed, so the seed lands on an account the asset pallet
// would otherwise refuse to create.
#[test]
fn insufficient_collateral_custody_needs_the_provider_reference() {
	ExtBuilder::default().build_and_execute(|| {
		let custody = Pallet::<Test>::redistribution_account(&INSUFFICIENT, &STABLE);
		assert_eq!(System::providers(&custody), 0);
		set_price(INSUFFICIENT, DEFAULT_PRICE);

		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			INSUFFICIENT,
			STABLE,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));

		assert_eq!(
			collateral_balance(INSUFFICIENT, custody),
			VaultCollateralAssets::minimum_balance(INSUFFICIENT)
		);
		assert_eq!(System::consumers(&custody), 1, "the asset account holds a consumer reference");

		assert_ok!(Pallet::<Test>::remove_branch(
			RuntimeOrigin::signed(ADMIN),
			INSUFFICIENT,
			STABLE
		));

		assert_eq!(System::consumers(&custody), 0);
		assert_eq!(System::providers(&custody), 0);
	});
}

// The custody seed of a privileged branch refunds its current full admin. An insufficient
// collateral cannot open an account for an admin with no provider, so removal fails whole
// rather than burning the seed or leaving a half-removed branch.
#[test]
fn remove_rejects_a_seed_refund_the_full_admin_cannot_receive() {
	ExtBuilder::default().price(INSUFFICIENT, DEFAULT_PRICE).build_and_execute(|| {
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			INSUFFICIENT,
			STABLE,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));
		assert_ok!(Pallet::<Test>::set_branch_admins(
			RuntimeOrigin::signed(ADMIN),
			INSUFFICIENT,
			STABLE,
			branch_admins(NEW_FULL_ADMIN, NEW_EMERGENCY_ADMIN),
		));
		assert_eq!(System::providers(&NEW_FULL_ADMIN), 0);

		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::root(), INSUFFICIENT, STABLE),
			Error::<Test>::CollateralPayoutFailed
		);
		assert!(market_exists(INSUFFICIENT, STABLE));
	});
}

// A Root create is deposit-free: no hold is taken, neither from the stablecoin owner nor from the
// full admin that funds the custody seed.
#[test]
fn root_create_takes_no_deposit() {
	ExtBuilder::default().price(DOT, DEFAULT_PRICE).build_and_execute(|| {
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			DOT,
			STABLE,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));

		assert_eq!(creation_deposit_held(STABLECOIN_OWNER), 0);
		assert_eq!(creation_deposit_held(ADMIN), 0);
		assert!(market_exists(DOT, STABLE));
	});
}

// A config that breaches the governance envelope is rejected at creation.
#[test]
fn create_branch_rejects_config_outside_envelope() {
	ExtBuilder::default().price(DOT, DEFAULT_PRICE).build_and_execute(|| {
		// Envelope floor on MCR is 105%; 104% is outside it.
		let config = BranchConfig {
			minimum_collateralization_ratio: percent(104),
			..default_branch_config()
		};

		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				STABLE,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				config,
				(),
			),
			Error::<Test>::ConfigOutsideEnvelope(
				BoundViolation::MinimumCollateralizationRatioTooLow
			)
		);
		assert!(!market_exists(DOT, STABLE));
	});
}

// A branch with no floor under its vaults can be filled with dust: each vault is
// a storage row, a list node, and a redemption step that the debt inside it never
// pays for. Every write of a config is held to the rule, so an update cannot
// install what creation refuses.
#[test]
fn zero_vault_floors_are_rejected_on_every_write() {
	ExtBuilder::default().price(DOT, DEFAULT_PRICE).build_and_execute(|| {
		let create = |config| {
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				STABLE,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				config,
				(),
			)
		};

		assert_noop!(
			create(BranchConfig { minimum_debt: 0, ..default_branch_config() }),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::ZeroMinimumDebt)
		);
		assert_noop!(
			create(BranchConfig { minimum_collateral: 0, ..default_branch_config() }),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::ZeroMinimumCollateral)
		);

		register_market(DOT, STABLE);
		let set_param =
			|update| Pallet::<Test>::set_param(RuntimeOrigin::signed(ADMIN), DOT, STABLE, update);

		assert_noop!(
			set_param(BranchConfigUpdate::MinimumDebt(0)),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::ZeroMinimumDebt)
		);
		assert_noop!(
			set_param(BranchConfigUpdate::MinimumCollateral(0)),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::ZeroMinimumCollateral)
		);
	});
}

// An inverted rate band accepts no rate at all, so the branch takes neither a new
// vault nor a rate change. The emergency admin can reach it: moving both ends
// inward is a narrowing by the defensive rule, which compares each end against its
// own old value and never against the other end.
#[test]
fn inverted_borrow_rate_band_is_rejected_on_every_write() {
	ExtBuilder::default().price(DOT, DEFAULT_PRICE).build_and_execute(|| {
		let inverted = BranchConfig {
			minimum_borrow_rate: percent(300),
			maximum_borrow_rate: percent(200),
			..default_branch_config()
		};

		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				STABLE,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				inverted,
				(),
			),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::MinimumBorrowRateAboveMaximum)
		);

		register_market(DOT, STABLE); // Band is 0.1%..400%.
		let narrow = |min, max| {
			Pallet::<Test>::set_param(
				RuntimeOrigin::signed(EMERGENCY_ADMIN),
				DOT,
				STABLE,
				BranchConfigUpdate::BorrowRateBounds { min, max },
			)
		};

		assert_noop!(
			narrow(percent(300), percent(200)),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::MinimumBorrowRateAboveMaximum)
		);

		// A narrowing that leaves the ends in order still applies.
		assert_ok!(narrow(percent(200), percent(300)));

		let config = market_config(DOT, STABLE);
		assert_eq!(config.minimum_borrow_rate, percent(200));
		assert_eq!(config.maximum_borrow_rate, percent(300));
	});
}

// The debt limit is denominated in the branch's own stablecoin, so no global
// envelope can judge it: the creator picks it, while the stablecoin-wide global
// ceiling caps total exposure across all branches that issue that stablecoin.
#[test]
fn create_branch_accepts_any_debt_ceiling() {
	ExtBuilder::default().price(DOT, DEFAULT_PRICE).build_and_execute(|| {
		let config = BranchConfig { debt_ceiling: u128::MAX, ..default_branch_config() };

		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			DOT,
			STABLE,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			config,
			(),
		));

		assert_eq!(market_config(DOT, STABLE).debt_ceiling, u128::MAX);
	});
}

// A branch the oracle cannot price cannot be created.
#[test]
fn create_branch_rejects_unpriced_collateral() {
	ExtBuilder::default().build_and_execute(|| {
		// DOT has no oracle price.
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				STABLE,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			Error::<Test>::OraclePriceNotAvailable
		);
	});
}

// The full admin can loosen a parameter within the envelope, but not past its
// floor.
#[test]
fn full_admin_loosens_within_envelope_but_not_past_floor() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// Successful updates preserve the complete config and emit the field and value.
		let raised_mcr = BranchConfig {
			minimum_collateralization_ratio: percent(115),
			..default_branch_config()
		};
		let lowered_ceiling = BranchConfig { debt_ceiling: 50_000_000, ..raised_mcr.clone() };
		for (old_value, update, expected) in [
			(
				BranchConfigUpdate::MinimumCollateralizationRatio(percent(110)),
				BranchConfigUpdate::MinimumCollateralizationRatio(percent(115)),
				raised_mcr,
			),
			(
				BranchConfigUpdate::DebtCeiling(1_000_000_000_000),
				BranchConfigUpdate::DebtCeiling(50_000_000),
				lowered_ceiling,
			),
		] {
			assert_ok!(Pallet::<Test>::set_param(
				RuntimeOrigin::signed(ADMIN),
				DOT,
				STABLE,
				update,
			));

			crate::tests::assert_event(crate::Event::ParameterUpdated {
				collateral_id: DOT,
				stable_id: STABLE,
				old_value,
				new_value: update,
			});
			assert_eq!(branch_config(DOT, STABLE), Some(expected));
		}

		// 115% -> 106% is a loosening the full admin may apply (floor is 105%).
		assert_ok!(Pallet::<Test>::set_param(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			STABLE,
			BranchConfigUpdate::MinimumCollateralizationRatio(percent(106))
		));

		assert_eq!(market_config(DOT, STABLE).minimum_collateralization_ratio, percent(106));

		// 104% is below the envelope floor — even the full admin cannot go there.
		assert_noop!(
			Pallet::<Test>::set_param(
				RuntimeOrigin::signed(ADMIN),
				DOT,
				STABLE,
				BranchConfigUpdate::MinimumCollateralizationRatio(percent(104))
			),
			Error::<Test>::ConfigOutsideEnvelope(
				BoundViolation::MinimumCollateralizationRatioTooLow
			)
		);
	});
}

// A non-empty branch cannot be removed; once its sole vault closes, removal
// succeeds. Governance goes through the same extrinsic, bypassing the admins but
// not the emptiness rule.
#[test]
fn remove_branch_requires_empty_market() {
	for origin in [RuntimeOrigin::signed(ADMIN), RuntimeOrigin::root()] {
		ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
			assert_ok!(open(STABLECOIN_OWNER, DOT, STABLE, 1_000, 500, percent(5)));

			assert_noop!(
				Pallet::<Test>::remove_branch(origin.clone(), DOT, STABLE),
				Error::<Test>::BranchNotEmpty
			);

			// Once the sole vault is repaid to zero, the now-empty branch is removable.
			repay_to_close(STABLECOIN_OWNER);
			assert_ok!(Pallet::<Test>::remove_branch(origin.clone(), DOT, STABLE));

			assert!(!market_exists(DOT, STABLE));
		});
	}
}

// Reassigning admins moves authority: the old full admin loses it, the new one
// gains it. The emergency admin may not reassign.
#[test]
fn set_branch_admins_reassigns_authority() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		// The emergency admin cannot reassign.
		assert_noop!(
			Pallet::<Test>::set_branch_admins(
				RuntimeOrigin::signed(EMERGENCY_ADMIN),
				DOT,
				STABLE,
				branch_admins(NEW_FULL_ADMIN, NEW_EMERGENCY_ADMIN),
			),
			Error::<Test>::NotBranchAdmin
		);

		assert_ok!(Pallet::<Test>::set_branch_admins(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			STABLE,
			branch_admins(NEW_FULL_ADMIN, NEW_EMERGENCY_ADMIN),
		));

		let info = crate::pallet::Branches::<Test>::get(DOT, STABLE).expect("admins stored");
		assert_eq!(info.admins.full_admin, NEW_FULL_ADMIN);
		assert_eq!(info.admins.emergency_admin, NEW_EMERGENCY_ADMIN);
		assert_event(crate::Event::BranchAdminsChanged {
			collateral_id: DOT,
			stable_id: STABLE,
			old_admins: branch_admins(ADMIN, EMERGENCY_ADMIN),
			new_admins: branch_admins(NEW_FULL_ADMIN, NEW_EMERGENCY_ADMIN),
		});

		// The old full admin can no longer act; the new one can.
		assert_noop!(
			Pallet::<Test>::set_governance_frozen(RuntimeOrigin::signed(ADMIN), DOT, STABLE, true),
			Error::<Test>::NotBranchAdmin
		);
		assert_ok!(Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(NEW_FULL_ADMIN),
			DOT,
			STABLE,
			true
		));
		assert!(market_state(DOT, STABLE).is_frozen());
	});
}

// Governance can replace an unreachable full admin and restore ordinary
// per-branch administration.
#[test]
fn force_origin_can_reassign_branch_admins() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(Pallet::<Test>::set_branch_admins(
			RuntimeOrigin::root(),
			DOT,
			STABLE,
			branch_admins(NEW_FULL_ADMIN, NEW_EMERGENCY_ADMIN),
		));

		let info = crate::pallet::Branches::<Test>::get(DOT, STABLE).expect("admins stored");
		assert_eq!(info.admins.full_admin, NEW_FULL_ADMIN);
		assert_eq!(info.admins.emergency_admin, NEW_EMERGENCY_ADMIN);
		assert_ok!(Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(NEW_FULL_ADMIN),
			DOT,
			STABLE,
			true,
		));
	});
}

// The emergency admin can pull the freeze, not just the full admin.
#[test]
fn emergency_admin_can_freeze() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(EMERGENCY_ADMIN),
			DOT,
			STABLE,
			true
		));

		assert!(market_state(DOT, STABLE).is_frozen());
	});
}

// ForceOrigin acts as a full administrator so governance can recover a branch with unavailable
// administrators. The runtime configuration limits still apply.
#[test]
fn force_origin_acts_as_full_branch_admin() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(Pallet::<Test>::set_param(
			RuntimeOrigin::root(),
			DOT,
			STABLE,
			BranchConfigUpdate::MinimumDebt(250)
		));

		assert_eq!(market_config(DOT, STABLE).minimum_debt, 250);

		assert_noop!(
			Pallet::<Test>::set_param(
				RuntimeOrigin::signed(9),
				DOT,
				STABLE,
				BranchConfigUpdate::MinimumDebt(300)
			),
			Error::<Test>::NotBranchAdmin
		);
		assert_noop!(
			Pallet::<Test>::set_param(
				RuntimeOrigin::root(),
				DOT,
				STABLE,
				BranchConfigUpdate::BorrowRateBounds {
					min: FixedU128::from_rational(1u128, 1_000u128),
					max: FixedU128::from_rational(500u128, 100u128),
				}
			),
			Error::<Test>::ConfigOutsideEnvelope(BoundViolation::BorrowRateTooHigh)
		);

		// Root is not a branch admin, yet the kill switch passes both ways.
		assert_ok!(Pallet::<Test>::set_governance_frozen(RuntimeOrigin::root(), DOT, STABLE, true));
		assert!(market_state(DOT, STABLE).is_frozen());
		assert_ok!(Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::root(),
			DOT,
			STABLE,
			false
		));
		assert!(!market_state(DOT, STABLE).is_frozen());
	});
}

// Registration claims exactly one provider reference on the branch's
// redistribution account and removal releases exactly that one — a reference
// someone else planted (e.g. by pre-funding the address) is not stolen.
#[test]
fn redistribution_account_provider_reference_is_paired() {
	ExtBuilder::default().build_and_execute(|| {
		let account = Pallet::<Test>::redistribution_account(&DOT, &STABLE);

		register_market(DOT, STABLE);

		assert_eq!(System::providers(&account), 2);

		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, STABLE));

		assert_eq!(System::providers(&account), 0);

		// A third party provided for the address before the branch existed.
		System::inc_providers(&account);

		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			DOT,
			STABLE,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));

		assert_eq!(System::providers(&account), 3);

		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, STABLE));

		assert_eq!(System::providers(&account), 1);
	});
}

// An issued-asset branch keeps the custody seed in a `pallet-assets` account. Removal sweeps the
// seed back to the account that funded it and releases the branch's provider reference. The
// asset is sufficient, so its account takes no consumer reference: the insufficient case lives in
// `insufficient_collateral_custody_needs_the_provider_reference`.
#[test]
fn issued_collateral_removal_refunds_the_custody_seed() {
	ExtBuilder::default().build_and_execute(|| {
		let account = Pallet::<Test>::redistribution_account(&TOKEN_X, &STABLE);
		let funder_before = collateral_balance(TOKEN_X, ADMIN);
		register_market(TOKEN_X, STABLE);
		assert_eq!(
			collateral_balance(TOKEN_X, account),
			VaultCollateralAssets::minimum_balance(TOKEN_X)
		);

		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), TOKEN_X, STABLE));

		assert_eq!(System::providers(&account), 0);
		assert_eq!(collateral_balance(TOKEN_X, ADMIN), funder_before);
	});
}

// A signer who is neither a branch admin nor the force origin can neither
// freeze nor remove.
#[test]
fn freeze_and_remove_reject_unauthorized_signers() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		const NOBODY: AccountId = 999;

		assert_noop!(
			Pallet::<Test>::set_governance_frozen(RuntimeOrigin::signed(NOBODY), DOT, STABLE, true),
			Error::<Test>::NotBranchAdmin
		);
		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::signed(NOBODY), DOT, STABLE),
			Error::<Test>::NotBranchAdmin
		);
	});
}

// The registry has no global cap: every collateral/stablecoin combination the
// role rules admit can be registered — here more branches than the old registry
// cap ever allowed — and a removed branch's pair can be re-created.
#[test]
fn registry_has_no_global_cap() {
	ExtBuilder::default().build_and_execute(|| {
		register_ten_markets();

		assert_eq!(crate::pallet::Branches::<Test>::iter_keys().count(), 10);

		// Removing a branch and re-creating the same pair round-trips.
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, STABLE));

		assert_eq!(crate::pallet::Branches::<Test>::iter_keys().count(), 9);

		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			DOT,
			STABLE,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));

		assert_eq!(crate::pallet::Branches::<Test>::iter_keys().count(), 10);
	});
}

#[test]
fn create_branch_rejects_unknown_assets_and_role_collision() {
	let cases = [
		(AssetId::WithId(999_999), STABLE, Error::<Test>::UnknownCollateral),
		(DOT, 9_999, Error::<Test>::UnknownStable),
		(AssetId::WithId(STABLE), STABLE, Error::<Test>::StableCollateralCollision),
	];
	for (collateral, stable, error) in cases {
		ExtBuilder::default().build_and_execute(|| {
			set_price(collateral.clone(), DEFAULT_PRICE);

			assert_noop!(
				Pallet::<Test>::create_branch(
					RuntimeOrigin::root(),
					collateral.clone(),
					stable,
					branch_admins(ADMIN, EMERGENCY_ADMIN),
					default_branch_config(),
					(),
				),
				error
			);
		});
	}
}

#[test]
fn ensure_branch_full_admin_authorizes_only_the_full_admin() {
	use crate::EnsureBranchFullAdmin;
	use frame::traits::EnsureOriginWithArg;

	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		let market = (DOT, STABLE);

		assert_ok!(EnsureBranchFullAdmin::<Test>::try_origin(
			RuntimeOrigin::signed(ADMIN),
			&market
		));
		// The full admin authorizes exactly one branch; everything below is rejected.
		assert!(EnsureBranchFullAdmin::<Test>::try_origin(
			RuntimeOrigin::signed(EMERGENCY_ADMIN),
			&market
		)
		.is_err());
		assert!(
			EnsureBranchFullAdmin::<Test>::try_origin(RuntimeOrigin::signed(1), &market).is_err()
		);
		assert!(EnsureBranchFullAdmin::<Test>::try_origin(RuntimeOrigin::root(), &market).is_err());
		// An unregistered branch has no admin, so even the admin account fails.
		assert!(EnsureBranchFullAdmin::<Test>::try_origin(
			RuntimeOrigin::signed(ADMIN),
			&(ETH, STABLE)
		)
		.is_err());
	});
}

/// Sets one branch parameter as the full admin.
fn set_param(update: BranchConfigUpdate<Balance>) -> DispatchResult {
	Pallet::<Test>::set_param(RuntimeOrigin::signed(ADMIN), DOT, STABLE, update)
}

/// Opens vault 1 at a collateralization ratio just under 1.25 next to a vault large enough to keep
/// the branch well clear of safety mode, so only vault 1's own ratio decides what it may do.
fn open_vault_near_one_and_a_quarter() {
	assert_ok!(open(2, DOT, STABLE, 100_000, 1_000, percent(5)));
	assert_ok!(open(1, DOT, STABLE, 1_000, 8_000, percent(5)));
	assert_eq!(branch_mode(DOT, STABLE), Some(BranchMode::Normal));
}

// Raising the debt floor does not demote an open vault below it: the vault stays Active and in the
// rate list, but a borrow must lift its principal to the new floor and only a full repayment may
// leave less than it.
#[test]
fn raised_minimum_debt_keeps_open_vaults_active_and_floors_their_next_change() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));
		let raised_minimum_debt = 1_000;
		let principal = vault(DOT, STABLE, 1).debt.principal;
		assert!(principal < raised_minimum_debt);

		assert_ok!(set_param(BranchConfigUpdate::MinimumDebt(raised_minimum_debt)));
		assert_ok!(poke(9, DOT, STABLE, 1));

		assert!(vault_status(DOT, STABLE, 1).is_active());
		assert!(<LinkedList as SortedListInterface<VaultList, u64>>::contains(
			&VaultList::Rate(DOT, STABLE),
			&1
		));
		let shortfall = raised_minimum_debt - principal;
		assert_noop!(borrow(1, DOT, STABLE, shortfall - 1, None), Error::<Test>::DebtBelowMinimum);
		hypothetically!({
			assert_ok!(borrow(1, DOT, STABLE, shortfall, None));
			assert_eq!(vault(DOT, STABLE, 1).debt.principal, raised_minimum_debt);
		});
		assert_noop!(repay(1, DOT, STABLE, 1, Some(1)), Error::<Test>::DebtWouldBecomeDust);
		mint_stable(STABLE, 1, vault(DOT, STABLE, 1).debt.total());
		assert_ok!(repay(1, DOT, STABLE, 1, None));
		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 0);
	});
}

// Raising the collateral floor reaches only new vaults: an open vault under it keeps withdrawing
// down to what its ratio allows.
#[test]
fn raised_minimum_collateral_binds_only_new_vaults() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_ok!(open(1, DOT, STABLE, 1_000, 500, percent(5)));

		assert_ok!(set_param(BranchConfigUpdate::MinimumCollateral(2_000)));

		assert!(vault_status(DOT, STABLE, 1).is_active());
		assert_ok!(withdraw_collateral(1, DOT, STABLE, 100, None));
		assert_eq!(vault(DOT, STABLE, 1).collateral, 900);
		assert_noop!(
			open(2, DOT, STABLE, 1_999, 500, percent(5)),
			Error::<Test>::InsufficientCollateral
		);
		assert_ok!(open(2, DOT, STABLE, 2_000, 500, percent(5)));
	});
}

// Raising ICR leaves an open vault under it Active and out of liquidation's reach,
// but blocks the borrow and withdrawal that would otherwise have passed.
#[test]
fn raised_icr_keeps_open_vaults_but_blocks_their_borrow_and_withdrawal() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		open_vault_near_one_and_a_quarter();
		hypothetically!({
			assert_ok!(borrow(1, DOT, STABLE, 1, None));
		});
		hypothetically!({
			assert_ok!(withdraw_collateral(1, DOT, STABLE, 1, None));
		});

		assert_ok!(set_param(BranchConfigUpdate::InitialCollateralizationRatio(percent(130))));

		assert!(vault_status(DOT, STABLE, 1).is_active());
		assert_noop!(liquidate(DOT, STABLE, 1), Error::<Test>::VaultNotLiquidatable);
		assert_noop!(borrow(1, DOT, STABLE, 1, None), Error::<Test>::UnsafeCollateralizationRatio);
		assert_noop!(
			withdraw_collateral(1, DOT, STABLE, 1, None),
			Error::<Test>::UnsafeCollateralizationRatio
		);
		assert_ok!(deposit_collateral(1, DOT, STABLE, 1, 1));
		assert_ok!(repay(1, DOT, STABLE, 1, Some(1)));
	});
}

// Raising the liquidation ratio applies to open vaults at once: a vault the raise leaves under it
// becomes liquidatable with no price move.
#[test]
fn raised_minimum_ratio_makes_open_vaults_under_it_liquidatable() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		open_vault_near_one_and_a_quarter();
		assert_noop!(liquidate(DOT, STABLE, 1), Error::<Test>::VaultNotLiquidatable);
		// The liquidation ratio may not exceed ICR.
		assert_ok!(set_param(BranchConfigUpdate::InitialCollateralizationRatio(percent(130))));

		assert_ok!(set_param(BranchConfigUpdate::MinimumCollateralizationRatio(percent(130))));

		assert!(vault_status(DOT, STABLE, 1).is_active());
		assert_ok!(liquidate(DOT, STABLE, 1));
		assert!(!vault_exists(DOT, STABLE, 1));
	});
}
