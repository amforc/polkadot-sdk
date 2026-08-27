use crate::{mock::*, tests::rate_pct, types::BranchConfigUpdate};
use frame::traits::fungibles::Mutate as FungiblesMutate;

/// Replacement admins used by the reassignment test.
const NEW_FULL_ADMIN: AccountId = 300;
const NEW_EMERGENCY_ADMIN: AccountId = 301;

fn market_exists(collateral: AssetId, stable: StableId) -> bool {
	branch_state(collateral, stable).is_some()
}

/// Repay `owner`'s full `(DOT, PUSD)` debt and close the vault, emptying the
/// market. Mints a pUSD buffer first so any accrued interest beyond the borrowed
/// principal is covered. Repay-to-zero leaves a Dormant husk that still holds the
/// collateral, so an explicit `close_vault` is needed to empty the market.
fn repay_to_close(owner: AccountId) {
	let total = vault(DOT, PUSD, owner).debt.total();
	// The terminal charge can put the payoff one unit past the recorded debt.
	<VaultStableAssets as FungiblesMutate<AccountId>>::mint_into(PUSD, &owner, total + 1)
		.expect("mint repay buffer");
	assert_ok!(Pallet::<Test>::repay_for(RuntimeOrigin::signed(owner), DOT, PUSD, owner, None));
	assert_ok!(Pallet::<Test>::close_vault(RuntimeOrigin::signed(owner), DOT, PUSD, None));
	assert!(!vault_exists(DOT, PUSD, owner), "close removed the vault");
}

// A signed asset-owner create locks the refundable deposit; removing the empty
// market refunds it in full.
#[test]
fn signed_create_takes_deposit_and_remove_refunds() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_eq!(creation_deposit_held(PUSD_OWNER), 0);

		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::signed(PUSD_OWNER),
			DOT,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));
		assert_eq!(creation_deposit_held(PUSD_OWNER), MarketDepositBase::get());

		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert_eq!(creation_deposit_held(PUSD_OWNER), 0, "deposit refunded on removal");
		assert!(!market_exists(DOT, PUSD));
	});
}

// A privileged creation has no depositor, so the market's full administrator
// pays the custody seed. Without the collateral to pay it, registration fails
// outright rather than leaving a market whose first redistributing liquidation
// would revert.
#[test]
fn create_rejects_a_creator_that_cannot_pay_the_custody_seed() {
	build_and_execute(|| {
		const PENNILESS_ADMIN: AccountId = 999;
		set_price(TOKEN_X, FixedU128::from_rational(10u128, 1u128));
		assert_eq!(collateral_balance(TOKEN_X, PENNILESS_ADMIN), 0);
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				TOKEN_X,
				PUSD,
				branch_admins(PENNILESS_ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			Error::<Test>::CustodySeedUnavailable
		);
		assert!(!market_exists(TOKEN_X, PUSD));
	});
}

// Registration takes the provider reference before it
// moves the custody seed, so the seed lands on an account the asset pallet
// would otherwise refuse to create.
#[test]
fn insufficient_collateral_custody_needs_the_provider_reference() {
	build_and_execute(|| {
		let custody = Pallet::<Test>::redistribution_account(&INSUFFICIENT, &PUSD);
		assert_eq!(System::providers(&custody), 0);
		set_price(INSUFFICIENT, FixedU128::from_rational(10u128, 1u128));

		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			INSUFFICIENT,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));
		assert_eq!(collateral_balance(INSUFFICIENT, custody), min_collateral_balance(INSUFFICIENT));
		assert_eq!(System::consumers(&custody), 1, "the asset account holds a consumer reference");

		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), INSUFFICIENT, PUSD));
		assert_eq!(System::consumers(&custody), 0);
		assert_eq!(System::providers(&custody), 0);
	});
}

// A Root create is deposit-free: no hold is taken.
#[test]
fn root_create_takes_no_deposit() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			DOT,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));
		assert_eq!(creation_deposit_held(PUSD_OWNER), 0);
		assert!(market_exists(DOT, PUSD));
	});
}

// The lifecycle hooks fire exactly once each, in order, across a create/remove
// round-trip.
#[test]
fn lifecycle_hooks_fire_on_create_and_remove() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::signed(PUSD_OWNER),
			DOT,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));
		assert_eq!(LifecycleLog::get(), alloc::vec![(DOT, PUSD, true, 1)]);

		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert_eq!(LifecycleLog::get(), alloc::vec![(DOT, PUSD, true, 1), (DOT, PUSD, false, 0)]);
	});
}

// A config that breaches the governance envelope is rejected at creation.
#[test]
fn create_branch_rejects_config_outside_envelope() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		// Envelope floor on MCR is 105%; 104% is outside it.
		let config = BranchConfig {
			minimum_collateralization_ratio: rate_pct(104, 100),
			..default_branch_config()
		};
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				config,
				(),
			),
			Error::<Test>::ConfigOutsideEnvelope(
				BoundViolation::MinimumCollateralizationRatioTooLow
			)
		);
		assert!(!market_exists(DOT, PUSD));
	});
}

// A market with no floor under its vaults can be filled with dust: each vault is
// a storage row, a list node, and a redemption step that the debt inside it never
// pays for. Every write of a config is held to the rule, so an update cannot
// install what creation refuses.
#[test]
fn zero_vault_floors_are_rejected_on_every_write() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		let create = |config| {
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
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

		register_market(DOT, PUSD);
		let set_param =
			|update| Pallet::<Test>::set_param(RuntimeOrigin::signed(ADMIN), DOT, PUSD, update);
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

// An inverted rate band accepts no rate at all, so the market takes neither a new
// vault nor a rate change. The emergency admin can reach it: moving both ends
// inward is a narrowing by the defensive rule, which compares each end against its
// own old value and never against the other end.
#[test]
fn inverted_borrow_rate_band_is_rejected_on_every_write() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		let inverted = BranchConfig {
			minimum_borrow_rate: rate_pct(300, 100),
			maximum_borrow_rate: rate_pct(200, 100),
			..default_branch_config()
		};
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				inverted,
				(),
			),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::MinimumBorrowRateAboveMaximum)
		);

		register_market(DOT, PUSD); // Band is 0.1%..400%.
		let narrow = |min, max| {
			Pallet::<Test>::set_param(
				RuntimeOrigin::signed(EMERGENCY_ADMIN),
				DOT,
				PUSD,
				BranchConfigUpdate::BorrowRateBounds { min, max },
			)
		};
		assert_noop!(
			narrow(rate_pct(300, 100), rate_pct(200, 100)),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::MinimumBorrowRateAboveMaximum)
		);
		// A narrowing that leaves the ends in order still applies.
		assert_ok!(narrow(rate_pct(200, 100), rate_pct(300, 100)));
		let config = branch_config(DOT, PUSD).expect("config");
		assert_eq!(config.minimum_borrow_rate, rate_pct(200, 100));
		assert_eq!(config.maximum_borrow_rate, rate_pct(300, 100));
	});
}

// The debt limit is denominated in the market's own stablecoin, so no global
// envelope can judge it: the creator picks it, while the stablecoin-wide global
// ceiling caps total exposure across all markets that issue that stablecoin.
#[test]
fn create_branch_accepts_any_debt_ceiling() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		let config = BranchConfig { debt_ceiling: u128::MAX, ..default_branch_config() };
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			DOT,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			config,
			(),
		));
		assert_eq!(branch_config(DOT, PUSD).expect("config").debt_ceiling, u128::MAX);
	});
}

// A market the oracle cannot price cannot be created.
#[test]
fn create_branch_rejects_unpriced_collateral() {
	build_and_execute(|| {
		// No `set_price(DOT, ..)` here.
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
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
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// 110% -> 106% is a loosening the full admin may apply (floor is 105%).
		assert_ok!(Pallet::<Test>::set_param(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			BranchConfigUpdate::MinimumCollateralizationRatio(rate_pct(106, 100))
		));
		assert_eq!(
			branch_config(DOT, PUSD).unwrap().minimum_collateralization_ratio,
			rate_pct(106, 100)
		);
		// 104% is below the envelope floor — even the full admin cannot go there.
		assert_noop!(
			Pallet::<Test>::set_param(
				RuntimeOrigin::signed(ADMIN),
				DOT,
				PUSD,
				BranchConfigUpdate::MinimumCollateralizationRatio(rate_pct(104, 100))
			),
			Error::<Test>::ConfigOutsideEnvelope(
				BoundViolation::MinimumCollateralizationRatioTooLow
			)
		);
	});
}

// A non-empty market cannot be removed; once its sole vault closes, removal
// succeeds.
#[test]
fn remove_branch_requires_empty_market() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(PUSD_OWNER, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD),
			Error::<Test>::BranchNotEmpty
		);

		// Once the sole vault is repaid to zero, the now-empty market is removable.
		repay_to_close(PUSD_OWNER);
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert!(!market_exists(DOT, PUSD));
	});
}

// Reassigning admins moves authority: the old full admin loses it, the new one
// gains it. The emergency admin may not reassign.
#[test]
fn set_branch_admins_reassigns_authority() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// The emergency admin cannot reassign.
		assert_noop!(
			Pallet::<Test>::set_branch_admins(
				RuntimeOrigin::signed(EMERGENCY_ADMIN),
				DOT,
				PUSD,
				branch_admins(NEW_FULL_ADMIN, NEW_EMERGENCY_ADMIN),
			),
			Error::<Test>::NotBranchAdmin
		);

		assert_ok!(Pallet::<Test>::set_branch_admins(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			branch_admins(NEW_FULL_ADMIN, NEW_EMERGENCY_ADMIN),
		));
		let info = crate::pallet::Branches::<Test>::get(DOT, PUSD).expect("admins stored");
		assert_eq!(info.admins.full_admin, NEW_FULL_ADMIN);
		assert_eq!(info.admins.emergency_admin, NEW_EMERGENCY_ADMIN);

		// The old full admin can no longer act; the new one can.
		assert_noop!(
			Pallet::<Test>::set_governance_frozen(RuntimeOrigin::signed(ADMIN), DOT, PUSD, true),
			Error::<Test>::NotBranchAdmin
		);
		assert_ok!(Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(NEW_FULL_ADMIN),
			DOT,
			PUSD,
			true
		));
		assert!(branch_state(DOT, PUSD).unwrap().is_frozen());
	});
}

// Governance can replace an unreachable full admin and restore ordinary
// per-market administration.
#[test]
fn force_origin_can_reassign_branch_admins() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(Pallet::<Test>::set_branch_admins(
			RuntimeOrigin::root(),
			DOT,
			PUSD,
			branch_admins(NEW_FULL_ADMIN, NEW_EMERGENCY_ADMIN),
		));

		let info = crate::pallet::Branches::<Test>::get(DOT, PUSD).expect("admins stored");
		assert_eq!(info.admins.full_admin, NEW_FULL_ADMIN);
		assert_eq!(info.admins.emergency_admin, NEW_EMERGENCY_ADMIN);
		assert_ok!(Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(NEW_FULL_ADMIN),
			DOT,
			PUSD,
			true,
		));
	});
}

// The emergency admin can pull the freeze, not just the full admin.
#[test]
fn emergency_admin_can_freeze() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(EMERGENCY_ADMIN),
			DOT,
			PUSD,
			true
		));
		assert!(branch_state(DOT, PUSD).unwrap().is_frozen());
	});
}

// The force origin freezes any market through the same extrinsic the
// admins use, bypassing them.
#[test]
fn governance_can_freeze_bypassing_admins() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Root is not a branch admin, yet the kill switch passes.
		assert_ok!(Pallet::<Test>::set_governance_frozen(RuntimeOrigin::root(), DOT, PUSD, true));
		assert!(branch_state(DOT, PUSD).unwrap().is_frozen());
	});
}

// ForceOrigin acts as a full administrator so governance can recover a market with unavailable
// administrators. The runtime configuration limits still apply.
#[test]
fn force_origin_acts_as_full_branch_admin() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(Pallet::<Test>::set_param(
			RuntimeOrigin::root(),
			DOT,
			PUSD,
			BranchConfigUpdate::MinimumDebt(250)
		));
		assert_eq!(branch_config(DOT, PUSD).expect("config").minimum_debt, 250);
		assert_noop!(
			Pallet::<Test>::set_param(
				RuntimeOrigin::signed(9),
				DOT,
				PUSD,
				BranchConfigUpdate::MinimumDebt(300)
			),
			Error::<Test>::NotBranchAdmin
		);
		assert_noop!(
			Pallet::<Test>::set_param(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				BranchConfigUpdate::BorrowRateBounds {
					min: FixedU128::from_rational(1u128, 1_000u128),
					max: FixedU128::from_rational(500u128, 100u128),
				}
			),
			Error::<Test>::ConfigOutsideEnvelope(BoundViolation::BorrowRateTooHigh)
		);

		assert_ok!(Pallet::<Test>::set_governance_frozen(RuntimeOrigin::root(), DOT, PUSD, true));
		assert_ok!(Pallet::<Test>::set_governance_frozen(RuntimeOrigin::root(), DOT, PUSD, false));
		assert!(!branch_state(DOT, PUSD).unwrap().is_frozen());
	});
}

// Registration claims exactly one provider reference on the market's
// redistribution account and removal releases exactly that one — a reference
// someone else planted (e.g. by pre-funding the address) is not stolen.
#[test]
fn redistribution_account_provider_reference_is_paired() {
	build_and_execute(|| {
		let account = Pallet::<Test>::redistribution_account(&DOT, &PUSD);
		register_market(DOT, PUSD);
		assert_eq!(System::providers(&account), 2);
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert_eq!(System::providers(&account), 0);

		// A third party provided for the address before the market existed.
		System::inc_providers(&account);
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			DOT,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));
		assert_eq!(System::providers(&account), 3);
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert_eq!(System::providers(&account), 1);
	});
}

// An issued-asset market keeps the seed in a `pallet-assets` account, which
// takes consumer references rather than a provider one. Removal has to sweep
// the seed before releasing the provider reference, or the consumers block it.
#[test]
fn issued_collateral_registration_leaves_no_references_behind() {
	build_and_execute(|| {
		let account = Pallet::<Test>::redistribution_account(&TOKEN_X, &PUSD);
		let funder_before = collateral_balance(TOKEN_X, ADMIN);
		register_market(TOKEN_X, PUSD);
		assert_eq!(collateral_balance(TOKEN_X, account), min_collateral_balance(TOKEN_X));

		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), TOKEN_X, PUSD));
		assert_eq!(System::providers(&account), 0);
		assert_eq!(System::consumers(&account), 0);
		assert_eq!(collateral_balance(TOKEN_X, ADMIN), funder_before);
	});
}

// A signer who is neither a branch admin nor the force origin can neither
// freeze nor remove.
#[test]
fn freeze_and_remove_reject_unauthorized_signers() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		const NOBODY: AccountId = 999;
		assert_noop!(
			Pallet::<Test>::set_governance_frozen(RuntimeOrigin::signed(NOBODY), DOT, PUSD, true),
			Error::<Test>::NotBranchAdmin
		);
		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::signed(NOBODY), DOT, PUSD),
			Error::<Test>::NotBranchAdmin
		);
	});
}

// Governance removal goes through `remove_branch` and still requires the
// market to be empty.
#[test]
fn governance_remove_bypasses_admins_and_requires_empty() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(open(PUSD_OWNER, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::root(), DOT, PUSD),
			Error::<Test>::BranchNotEmpty
		);

		repay_to_close(PUSD_OWNER);
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::root(), DOT, PUSD));
		assert!(!market_exists(DOT, PUSD));
	});
}

// The registry has no global cap: every collateral/stablecoin combination the
// role rules admit can be registered — here more markets than the old registry
// cap ever allowed — and a removed market's pair can be re-created.
#[test]
fn registry_has_no_global_cap() {
	build_and_execute(|| {
		register_ten_markets();
		assert_eq!(crate::pallet::Branches::<Test>::iter_keys().count(), 10);

		// Removing a market and re-creating the same pair round-trips.
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert_eq!(crate::pallet::Branches::<Test>::iter_keys().count(), 9);
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			DOT,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config(),
			(),
		));
		assert_eq!(crate::pallet::Branches::<Test>::iter_keys().count(), 10);
	});
}

// A market whose stablecoin asset does not exist is rejected.
#[test]
fn create_branch_rejects_unknown_stable() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		// Asset id 9_999 was never created.
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				9_999,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			Error::<Test>::UnknownStable
		);
	});
}

// A market whose stablecoin is also its own collateral is rejected — otherwise
// the freely-minted coin could be posted as backing. The cross-market
// directions live in `tests::asset_roles::cross_role_reuse_rejected_in_both_directions`.
#[test]
fn create_branch_rejects_stable_collateral_collision() {
	build_and_execute(|| {
		// Self-collision: the collateral asset id equals the stablecoin asset id.
		set_price(AssetId::WithId(PUSD), FixedU128::from_rational(10u128, 1u128));
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				AssetId::WithId(PUSD),
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			),
			Error::<Test>::StableCollateralCollision
		);
	});
}

// A market still holding collateral in its redistribution account cannot be
// removed — the collateral would be stranded with no path left to reach it.
#[test]
fn remove_branch_rejected_while_collateral_remains() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		mutate_branch_state(DOT, PUSD, |state| {
			state.total_collateral = 1;
		});
		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD),
			Error::<Test>::BranchNotEmpty
		);
		mutate_branch_state(DOT, PUSD, |state| {
			state.total_collateral = 0;
		});
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert!(!market_exists(DOT, PUSD));
	});
}

#[test]
fn ensure_branch_full_admin_authorizes_only_the_full_admin() {
	use crate::EnsureBranchFullAdmin;
	use frame::traits::EnsureOriginWithArg;

	build_and_execute(|| {
		register_market(DOT, PUSD);
		let market = (DOT, PUSD);
		assert_ok!(EnsureBranchFullAdmin::<Test>::try_origin(
			RuntimeOrigin::signed(ADMIN),
			&market
		));
		// The full admin authorizes exactly one market; everything below is rejected.
		assert!(EnsureBranchFullAdmin::<Test>::try_origin(
			RuntimeOrigin::signed(EMERGENCY_ADMIN),
			&market
		)
		.is_err());
		assert!(
			EnsureBranchFullAdmin::<Test>::try_origin(RuntimeOrigin::signed(1), &market).is_err()
		);
		assert!(EnsureBranchFullAdmin::<Test>::try_origin(RuntimeOrigin::root(), &market).is_err());
		// An unregistered market has no admin, so even the admin account fails.
		assert!(EnsureBranchFullAdmin::<Test>::try_origin(
			RuntimeOrigin::signed(ADMIN),
			&(ETH, PUSD)
		)
		.is_err());
	});
}
