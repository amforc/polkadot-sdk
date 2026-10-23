use crate::{mock::*, tests::*, types::BranchConfigUpdate};
use frame::traits::fungibles::Inspect as _;
use pallet_linked_list::SortedListInterface;

/// Replacement admins used by the reassignment test.
const NEW_FULL_ADMIN: AccountId = 300;
const NEW_EMERGENCY_ADMIN: AccountId = 301;

fn market_exists(collateral: AssetId, stable: StableId) -> bool {
	crate::pallet::Branches::<Test>::contains_key(collateral, stable)
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

/// Registers the `(DOT, STABLE)` branch from `origin` with the default admins and config.
fn create_dot_branch(origin: RuntimeOrigin) -> DispatchResult {
	Pallet::<Test>::create_branch(
		origin,
		DOT,
		STABLE,
		branch_admins(ADMIN, EMERGENCY_ADMIN),
		default_branch_config(),
		(),
	)
}

/// Removes the empty `(DOT, STABLE)` branch as its full admin and asserts that no creation deposit
/// is left on hold.
fn remove_dot_branch_releasing_deposits() {
	assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, STABLE));

	assert_eq!(creation_deposit_held(STABLECOIN_OWNER), 0, "deposit refunded on removal");
	assert_eq!(creation_deposit_held(ADMIN), 0);
	assert!(!market_exists(DOT, STABLE));
	assert_event(crate::Event::BranchRemoved { collateral_id: DOT, stable_id: STABLE });
}

// A signed asset-owner create locks the refundable deposit and removing the empty branch refunds
// it in full.
#[test]
fn signed_creation_locks_a_deposit_refunded_on_removal() {
	ExtBuilder::default().price(DOT, DEFAULT_PRICE).build_and_execute(|| {
		assert_eq!(creation_deposit_held(STABLECOIN_OWNER), 0);

		assert_ok!(create_dot_branch(RuntimeOrigin::signed(STABLECOIN_OWNER)));

		assert!(market_exists(DOT, STABLE));
		assert_eq!(creation_deposit_held(STABLECOIN_OWNER), MarketDepositBase::get());
		assert_eq!(creation_deposit_held(ADMIN), 0);

		remove_dot_branch_releasing_deposits();
	});
}

// A Root create is deposit-free: no hold is taken, neither from the stablecoin owner nor from the
// full admin that funds the custody seed.
#[test]
fn root_creation_takes_no_deposit() {
	ExtBuilder::default().price(DOT, DEFAULT_PRICE).build_and_execute(|| {
		assert_eq!(creation_deposit_held(STABLECOIN_OWNER), 0);

		assert_ok!(create_dot_branch(RuntimeOrigin::root()));

		assert!(market_exists(DOT, STABLE));
		assert_eq!(creation_deposit_held(STABLECOIN_OWNER), 0);
		assert_eq!(creation_deposit_held(ADMIN), 0);

		remove_dot_branch_releasing_deposits();
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
	});
}

/// Registers an issued-`collateral` branch as Root, checks the custody seed and the custody
/// account's `consumers_while_live` references, then removes the branch and checks that the seed
/// went back to its funder and every reference was released.
fn assert_custody_seed_refunded_on_removal(collateral: AssetId, consumers_while_live: u32) {
	let custody = Pallet::<Test>::redistribution_account(&collateral, &STABLE);
	assert_eq!(System::providers(&custody), 0);
	let funder_before = collateral_balance(collateral.clone(), ADMIN);
	set_price(collateral.clone(), DEFAULT_PRICE);

	assert_ok!(Pallet::<Test>::create_branch(
		RuntimeOrigin::root(),
		collateral.clone(),
		STABLE,
		branch_admins(ADMIN, EMERGENCY_ADMIN),
		default_branch_config(),
		(),
	));

	assert_eq!(
		collateral_balance(collateral.clone(), custody),
		VaultCollateralAssets::minimum_balance(collateral.clone())
	);
	assert_eq!(System::consumers(&custody), consumers_while_live);

	assert_ok!(Pallet::<Test>::remove_branch(
		RuntimeOrigin::signed(ADMIN),
		collateral.clone(),
		STABLE
	));

	assert_eq!(System::consumers(&custody), 0);
	assert_eq!(System::providers(&custody), 0);
	assert_eq!(collateral_balance(collateral, ADMIN), funder_before);
}

// An issued-asset branch keeps the custody seed in a `pallet-assets` account. Registration takes
// the provider reference before it moves the seed, so the seed lands on an account the asset
// pallet would otherwise refuse to create. Removal sweeps the seed back to the account that funded
// it and releases every reference.
#[test]
fn issued_collateral_custody_seed_is_refunded_on_removal() {
	ExtBuilder::default().build_and_execute(|| {
		assert_custody_seed_refunded_on_removal(TOKEN_X, 0);
	});
}

// Only an insufficient asset's account takes a consumer reference, and removal releases it.
#[test]
fn insufficient_collateral_custody_seed_releases_its_consumer_on_removal() {
	ExtBuilder::default().build_and_execute(|| {
		assert_custody_seed_refunded_on_removal(INSUFFICIENT, 1);
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
	});
}

// The governance envelope and the consistency rules are checked on every write of a config, so an
// update cannot install what creation refuses. A branch with no floor under its vaults can be
// filled with dust: each vault is a storage row, a list node, and a redemption step that the debt
// inside it never pays for. An inverted rate band accepts no rate at all, so the branch takes
// neither a new vault nor a rate change.
#[test]
fn invalid_configs_are_rejected_on_every_write() {
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
		// Envelope floor on MCR is 105%; 104% is outside it.
		assert_noop!(
			create(BranchConfig {
				minimum_collateralization_ratio: percent(104),
				..default_branch_config()
			}),
			Error::<Test>::ConfigOutsideEnvelope(
				BoundViolation::MinimumCollateralizationRatioTooLow,
			)
		);
		// The other two floors, each with MCR at its own floor so the ordering rule, which runs
		// first, does not reject the config before the envelope sees it.
		let bounds = TestBranchConfigBounds::get();
		assert_noop!(
			create(BranchConfig {
				minimum_collateralization_ratio: bounds.min_minimum_collateralization_ratio,
				initial_collateralization_ratio: bounds.min_initial_collateralization_ratio -
					percent(1),
				..default_branch_config()
			}),
			Error::<Test>::ConfigOutsideEnvelope(
				BoundViolation::InitialCollateralizationRatioTooLow,
			)
		);
		assert_noop!(
			create(BranchConfig {
				minimum_collateralization_ratio: bounds.min_minimum_collateralization_ratio,
				safety_collateralization_ratio: bounds.min_safety_collateralization_ratio -
					percent(1),
				..default_branch_config()
			}),
			Error::<Test>::ConfigOutsideEnvelope(
				BoundViolation::SafetyCollateralizationRatioTooLow,
			)
		);
		// A liquidation ratio above the borrow or safety ratio opens vaults that are already
		// liquidatable.
		let defaults = default_branch_config();
		assert_noop!(
			create(BranchConfig {
				minimum_collateralization_ratio: defaults.initial_collateralization_ratio +
					percent(1),
				..default_branch_config()
			}),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::LiquidationRatioAboveInitial)
		);
		assert_noop!(
			create(BranchConfig {
				minimum_collateralization_ratio: defaults.safety_collateralization_ratio +
					percent(1),
				initial_collateralization_ratio: defaults.safety_collateralization_ratio +
					percent(2),
				..default_branch_config()
			}),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::LiquidationRatioAboveSafety)
		);
		assert_noop!(
			create(BranchConfig { minimum_debt: 0, ..default_branch_config() }),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::ZeroMinimumDebt)
		);
		assert_noop!(
			create(BranchConfig { minimum_collateral: 0, ..default_branch_config() }),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::ZeroMinimumCollateral)
		);
		assert_noop!(
			create(BranchConfig {
				minimum_borrow_rate: percent(300),
				maximum_borrow_rate: percent(200),
				..default_branch_config()
			}),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::MinimumBorrowRateAboveMaximum)
		);

		// The debt limit is denominated in the branch's own stablecoin, so no global envelope can
		// judge it: the stablecoin-wide global ceiling caps exposure across branches instead.
		assert_ok!(create(BranchConfig { debt_ceiling: u128::MAX, ..default_branch_config() }));
		assert_eq!(market_config(DOT, STABLE).debt_ceiling, u128::MAX);

		// The band is 0.1%..400%. The emergency admin reaches the inverted band because moving
		// both ends inward is a narrowing by the defensive rule, which compares each end against
		// its own old value and never against the other end.
		assert_noop!(
			set_param(BranchConfigUpdate::MinimumDebt(0)),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::ZeroMinimumDebt)
		);
		assert_noop!(
			set_param(BranchConfigUpdate::MinimumCollateral(0)),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::ZeroMinimumCollateral)
		);
		assert_noop!(
			Pallet::<Test>::set_param(
				RuntimeOrigin::signed(EMERGENCY_ADMIN),
				DOT,
				STABLE,
				BranchConfigUpdate::BorrowRateBounds { min: percent(300), max: percent(200) },
			),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::MinimumBorrowRateAboveMaximum)
		);

		// The ordering rule is inclusive: a liquidation ratio equal to the borrow ratio applies,
		// and lowering the borrow ratio one step under it is refused.
		let initial = defaults.initial_collateralization_ratio;
		assert_ok!(set_param(BranchConfigUpdate::MinimumCollateralizationRatio(initial)));
		assert_noop!(
			set_param(BranchConfigUpdate::InitialCollateralizationRatio(initial - percent(1))),
			Error::<Test>::InvalidBranchConfig(BranchConfigDefect::LiquidationRatioAboveInitial)
		);

		// A narrowing that leaves the ends in order still applies.
		assert_ok!(Pallet::<Test>::set_param(
			RuntimeOrigin::signed(EMERGENCY_ADMIN),
			DOT,
			STABLE,
			BranchConfigUpdate::BorrowRateBounds { min: percent(200), max: percent(300) },
		));

		let config = market_config(DOT, STABLE);
		assert_eq!(config.minimum_borrow_rate, percent(200));
		assert_eq!(config.maximum_borrow_rate, percent(300));
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
		assert_ok!(set_param(BranchConfigUpdate::MinimumCollateralizationRatio(percent(115))));

		crate::tests::assert_event(crate::Event::ParameterUpdated {
			collateral_id: DOT,
			stable_id: STABLE,
			old_value: BranchConfigUpdate::MinimumCollateralizationRatio(percent(110)),
			new_value: BranchConfigUpdate::MinimumCollateralizationRatio(percent(115)),
		});
		assert_eq!(market_config(DOT, STABLE), raised_mcr.clone());

		assert_ok!(set_param(BranchConfigUpdate::DebtCeiling(50_000_000)));

		crate::tests::assert_event(crate::Event::ParameterUpdated {
			collateral_id: DOT,
			stable_id: STABLE,
			old_value: BranchConfigUpdate::DebtCeiling(1_000_000_000_000),
			new_value: BranchConfigUpdate::DebtCeiling(50_000_000),
		});
		assert_eq!(
			market_config(DOT, STABLE),
			BranchConfig { debt_ceiling: 50_000_000, ..raised_mcr }
		);

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

/// Opens the sole vault of the `(DOT, STABLE)` branch and asserts that `origin` can remove the
/// branch only after that vault closes.
fn assert_removal_requires_empty_market(origin: RuntimeOrigin) {
	assert_ok!(open(STABLECOIN_OWNER, DOT, STABLE, 1_000, 500, percent(5)));

	assert_noop!(
		Pallet::<Test>::remove_branch(origin.clone(), DOT, STABLE),
		Error::<Test>::BranchNotEmpty
	);

	// Once the sole vault is repaid to zero, the now-empty branch is removable.
	repay_to_close(STABLECOIN_OWNER);
	assert_ok!(Pallet::<Test>::remove_branch(origin, DOT, STABLE));

	assert!(!market_exists(DOT, STABLE));
}

// A non-empty branch cannot be removed; once its sole vault closes, removal
// succeeds.
#[test]
fn remove_branch_requires_empty_market() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_removal_requires_empty_market(RuntimeOrigin::signed(ADMIN));
	});
}

// Governance goes through the same extrinsic, bypassing the admins but not the emptiness rule.
#[test]
fn governance_remove_branch_requires_empty_market() {
	ExtBuilder::default().branch(DOT, STABLE).build_and_execute(|| {
		assert_removal_requires_empty_market(RuntimeOrigin::root());
	});
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

		// Governance can replace an unreachable full admin and restore ordinary per-branch
		// administration. Clearing the freeze needs the full level, so it proves the restored
		// admin holds it.
		assert_ok!(Pallet::<Test>::set_branch_admins(
			RuntimeOrigin::root(),
			DOT,
			STABLE,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
		));

		let info = crate::pallet::Branches::<Test>::get(DOT, STABLE).expect("admins stored");
		assert_eq!(info.admins.full_admin, ADMIN);
		assert_eq!(info.admins.emergency_admin, EMERGENCY_ADMIN);
		assert_ok!(Pallet::<Test>::set_governance_frozen(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			STABLE,
			false
		));
		assert!(!market_state(DOT, STABLE).is_frozen());
	});
}

// ForceOrigin acts as a full administrator so governance can recover a branch with unavailable
// administrators. The runtime configuration limits still apply. A signer who is neither a branch
// admin nor the force origin can neither update nor remove the branch.
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
			Pallet::<Test>::remove_branch(RuntimeOrigin::signed(9), DOT, STABLE),
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

// The oracle check runs first, so every other asset is priced before it reaches the guard it
// names.
#[test]
fn create_branch_rejects_unpriced_or_unknown_assets_and_role_collision() {
	ExtBuilder::default().build_and_execute(|| {
		let create = |collateral, stable| {
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				collateral,
				stable,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config(),
				(),
			)
		};

		clear_price(DOT);
		assert_noop!(create(DOT, STABLE), Error::<Test>::OraclePriceNotAvailable);

		set_price(AssetId::WithId(999_999), DEFAULT_PRICE);
		assert_noop!(create(AssetId::WithId(999_999), STABLE), Error::<Test>::UnknownCollateral);

		set_price(DOT, DEFAULT_PRICE);
		assert_noop!(create(DOT, 9_999), Error::<Test>::UnknownStable);

		set_price(AssetId::WithId(STABLE), DEFAULT_PRICE);
		assert_noop!(
			create(AssetId::WithId(STABLE), STABLE),
			Error::<Test>::StableCollateralCollision
		);
	});
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
		assert!(<LinkedList as SortedListInterface<VaultList, AccountId>>::contains(
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
		assert_noop!(liquidate(9, DOT, STABLE, 1, 0, 0), Error::<Test>::VaultNotLiquidatable);
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
		assert_noop!(liquidate(9, DOT, STABLE, 1, 0, 0), Error::<Test>::VaultNotLiquidatable);
		// The liquidation ratio may not exceed ICR.
		assert_ok!(set_param(BranchConfigUpdate::InitialCollateralizationRatio(percent(130))));

		assert_ok!(set_param(BranchConfigUpdate::MinimumCollateralizationRatio(percent(130))));

		assert!(vault_status(DOT, STABLE, 1).is_active());
		assert_ok!(liquidate(9, DOT, STABLE, 1, 0, 0));
		assert!(!vault_exists(DOT, STABLE, 1));
	});
}
