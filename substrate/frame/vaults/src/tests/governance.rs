use crate::{mock::*, pallet::Vaults, tests::rate_pct, types::BranchConfigUpdate};
use frame::traits::fungibles::Mutate as FungiblesMutate;

/// PUSD's genesis owner — the signer permitted to open a PUSD market with a
/// deposit, per the mock's `EnsureAssetOwner`.
const PUSD_OWNER: AccountId = 1;
/// Replacement admins used by the reassignment test.
const NEW_FULL_ADMIN: AccountId = 200;
const NEW_EMERGENCY_ADMIN: AccountId = 201;
/// One day in milliseconds — the mock guard's `min_ceiling_ttl`.
const DAY_MS: Moment = 24 * 3_600 * 1_000;

fn market_exists(collateral: AssetId, stable: StableId) -> bool {
	branch_state(collateral, stable).is_some()
}

/// Repay `owner`'s full `(DOT, PUSD)` debt and close the vault, emptying the
/// market. Mints a pUSD buffer first so any accrued interest beyond the borrowed
/// principal is covered. Repay-to-zero leaves a Dormant husk that still holds the
/// collateral, so an explicit `close_vault` is needed to empty the market.
fn repay_to_close(owner: AccountId) {
	let total = Vaults::<Test>::get((DOT, PUSD, owner)).expect("vault stored").debt.total();
	<VaultStableAssets as FungiblesMutate<AccountId>>::mint_into(PUSD, &owner, total)
		.expect("mint repay buffer");
	assert_ok!(Pallet::<Test>::repay_for(RuntimeOrigin::signed(owner), DOT, PUSD, owner, total));
	assert_ok!(Pallet::<Test>::close_vault(RuntimeOrigin::signed(owner), DOT, PUSD, None));
	assert!(Vaults::<Test>::get((DOT, PUSD, owner)).is_none(), "close removed the vault");
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
			default_branch_config()
		));
		assert_eq!(creation_deposit_held(PUSD_OWNER), MarketDepositBase::get());

		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert_eq!(creation_deposit_held(PUSD_OWNER), 0, "deposit refunded on removal");
		assert!(!market_exists(DOT, PUSD));
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
			default_branch_config()
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
			default_branch_config()
		));
		assert_eq!(LifecycleLog::get(), alloc::vec![(DOT, PUSD, true)]);

		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert_eq!(LifecycleLog::get(), alloc::vec![(DOT, PUSD, true), (DOT, PUSD, false)]);
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
				config
			),
			Error::<Test>::ConfigOutsideEnvelope
		);
		assert!(!market_exists(DOT, PUSD));
	});
}

// A line above the envelope's `max_branch_line` is rejected at creation.
#[test]
fn create_branch_rejects_line_above_envelope() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		let config = BranchConfig { debt_ceiling: 2_000_000_000, ..default_branch_config() };
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				config
			),
			Error::<Test>::ConfigOutsideEnvelope
		);
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
				default_branch_config()
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
		register_default_branch();
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
			Error::<Test>::ConfigOutsideEnvelope
		);
	});
}

// A non-empty market cannot be removed; once its sole vault closes, removal
// succeeds.
#[test]
fn remove_branch_requires_empty_market() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(open(PUSD_OWNER, DOT, 1_000, 500, rate_pct(5, 100)));
		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD),
			Error::<Test>::MarketNotEmpty
		);

		// Once the sole vault is repaid to zero, the now-empty market is removable.
		repay_to_close(PUSD_OWNER);
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert!(!market_exists(DOT, PUSD));
	});
}

// Reassigning admins moves authority: the old full admin loses it, the new one
// gains it. Only the full admin may reassign.
#[test]
fn set_branch_admins_reassigns_authority() {
	build_and_execute(|| {
		register_default_branch();
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
		let info = crate::pallet::Branches::<Test>::get((DOT, PUSD)).expect("admins stored");
		assert_eq!(info.admins.full_admin, admin_caller(NEW_FULL_ADMIN));
		assert_eq!(info.admins.emergency_admin, admin_caller(NEW_EMERGENCY_ADMIN));

		// The old full admin can no longer act; the new one can.
		assert_noop!(
			Pallet::<Test>::enable_frozen_mode(RuntimeOrigin::signed(ADMIN), DOT, PUSD),
			Error::<Test>::NotBranchAdmin
		);
		assert_ok!(Pallet::<Test>::enable_frozen_mode(
			RuntimeOrigin::signed(NEW_FULL_ADMIN),
			DOT,
			PUSD
		));
		assert!(branch_state(DOT, PUSD).unwrap().is_frozen());
	});
}

// The emergency admin can pull the freeze, not just the full admin.
#[test]
fn emergency_admin_can_freeze() {
	build_and_execute(|| {
		register_default_branch();
		assert_ok!(Pallet::<Test>::enable_frozen_mode(
			RuntimeOrigin::signed(EMERGENCY_ADMIN),
			DOT,
			PUSD
		));
		assert!(branch_state(DOT, PUSD).unwrap().is_frozen());
	});
}

// The global manager origin freezes any market through the same extrinsic the
// admins use, bypassing them.
#[test]
fn governance_can_freeze_bypassing_admins() {
	build_and_execute(|| {
		register_default_branch();
		// Root is not a branch admin, yet the kill switch passes.
		assert_ok!(Pallet::<Test>::enable_frozen_mode(RuntimeOrigin::root(), DOT, PUSD));
		assert!(branch_state(DOT, PUSD).unwrap().is_frozen());
	});
}

// A signer who is neither a branch admin nor the global manager can neither
// freeze nor remove.
#[test]
fn freeze_and_remove_reject_unauthorized_signers() {
	build_and_execute(|| {
		register_default_branch();
		const NOBODY: AccountId = 999;
		assert_noop!(
			Pallet::<Test>::enable_frozen_mode(RuntimeOrigin::signed(NOBODY), DOT, PUSD),
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
		register_default_branch();
		assert_ok!(open(PUSD_OWNER, DOT, 1_000, 500, rate_pct(5, 100)));
		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::root(), DOT, PUSD),
			Error::<Test>::MarketNotEmpty
		);

		repay_to_close(PUSD_OWNER);
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::root(), DOT, PUSD));
		assert!(!market_exists(DOT, PUSD));
	});
}

// Filling the registry blocks further creation; removing a market frees its
// `MaxBranches` slot for the next create.
#[test]
fn remove_branch_frees_max_branches_slot() {
	build_and_execute(|| {
		// Four collaterals against two stablecoins is exactly `MaxBranches`.
		for collateral in [DOT, TOKEN_X, ETH, COLL_C] {
			set_price(collateral.clone(), FixedU128::from_rational(10u128, 1u128));
			for stable in [PUSD, EUSD] {
				assert_ok!(Pallet::<Test>::create_branch(
					RuntimeOrigin::root(),
					collateral.clone(),
					stable,
					branch_admins(ADMIN, EMERGENCY_ADMIN),
					default_branch_config()
				));
			}
		}
		assert_eq!(
			u32::try_from(crate::pallet::Branches::<Test>::iter_keys().count()).unwrap(),
			MaxBranches::get()
		);

		// The registry is full: a market on the spare collateral is rejected.
		set_price(COLL_D, FixedU128::from_rational(10u128, 1u128));
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				COLL_D,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config()
			),
			Error::<Test>::TooManyBranches
		);

		// Removing one market frees a slot; the previously-rejected create lands.
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert_eq!(
			u32::try_from(crate::pallet::Branches::<Test>::iter_keys().count()).unwrap(),
			MaxBranches::get() - 1
		);
		assert_ok!(Pallet::<Test>::create_branch(
			RuntimeOrigin::root(),
			COLL_D,
			PUSD,
			branch_admins(ADMIN, EMERGENCY_ADMIN),
			default_branch_config()
		));
		assert_eq!(
			u32::try_from(crate::pallet::Branches::<Test>::iter_keys().count()).unwrap(),
			MaxBranches::get()
		);
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
				default_branch_config()
			),
			Error::<Test>::UnknownStable
		);
	});
}

// A market whose stablecoin is also trusted as collateral is rejected — as its
// own collateral (self), or as a sibling market's collateral (cross-market).
// Otherwise the freely-minted coin could be posted as backing.
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
				default_branch_config()
			),
			Error::<Test>::StableCollateralCollision
		);

		// Cross-market: EUSD is a live market's stablecoin, so a new market may not
		// trust the EUSD asset as its collateral.
		register_market(DOT, EUSD);
		set_price(AssetId::WithId(EUSD), FixedU128::from_rational(10u128, 1u128));
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				AssetId::WithId(EUSD),
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				default_branch_config()
			),
			Error::<Test>::StableCollateralCollision
		);
	});
}

// The autoline knobs must sit inside the envelope: the gap at or below
// `max_ceiling_gap`, and — when the autoline is enabled — the ttl at or above
// `min_ceiling_ttl`. This stops a creator defeating the gradual-increase control.
#[test]
fn create_branch_rejects_autoline_knobs_outside_envelope() {
	build_and_execute(|| {
		set_price(DOT, FixedU128::from_rational(10u128, 1u128));
		// A gap above `max_ceiling_gap` is rejected.
		let wide_gap = BranchConfig {
			ceiling_gap: 2_000_000_000,
			ceiling_ttl: DAY_MS,
			..default_branch_config()
		};
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				wide_gap
			),
			Error::<Test>::ConfigOutsideEnvelope
		);
		// With the autoline enabled, a ttl below `min_ceiling_ttl` is rejected.
		let fast_ttl =
			BranchConfig { ceiling_gap: 1_000, ceiling_ttl: DAY_MS - 1, ..default_branch_config() };
		assert_noop!(
			Pallet::<Test>::create_branch(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				branch_admins(ADMIN, EMERGENCY_ADMIN),
				fast_ttl
			),
			Error::<Test>::ConfigOutsideEnvelope
		);
		assert!(!market_exists(DOT, PUSD));
	});
}

// The autoline knobs can be tuned after creation, but only within the envelope.
#[test]
fn set_ceiling_knobs_apply_within_envelope() {
	build_and_execute(|| {
		register_default_branch(); // autoline off: gap == 0, ttl == 0
							 // Raise the ttl first (valid while the autoline is still disabled), then the gap.
		assert_ok!(Pallet::<Test>::set_param(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			BranchConfigUpdate::CeilingTtl(DAY_MS)
		));
		assert_ok!(Pallet::<Test>::set_param(
			RuntimeOrigin::signed(ADMIN),
			DOT,
			PUSD,
			BranchConfigUpdate::CeilingGap(1_000)
		));
		let config = branch_config(DOT, PUSD).expect("config");
		assert_eq!(config.ceiling_gap, 1_000);
		assert_eq!(config.ceiling_ttl, DAY_MS);
		// A ttl below the floor (autoline now enabled) is rejected.
		assert_noop!(
			Pallet::<Test>::set_param(
				RuntimeOrigin::signed(ADMIN),
				DOT,
				PUSD,
				BranchConfigUpdate::CeilingTtl(DAY_MS - 1)
			),
			Error::<Test>::ConfigOutsideEnvelope
		);
		// A gap above the cap is rejected.
		assert_noop!(
			Pallet::<Test>::set_param(
				RuntimeOrigin::signed(ADMIN),
				DOT,
				PUSD,
				BranchConfigUpdate::CeilingGap(2_000_000_000)
			),
			Error::<Test>::ConfigOutsideEnvelope
		);
	});
}

// A market with residual bad debt cannot be removed, even with no vaults, stake,
// or principal left — the bad debt is still an unbacked liability.
#[test]
fn remove_branch_rejected_while_bad_debt_remains() {
	build_and_execute(|| {
		register_default_branch();
		mutate_branch_state(DOT, PUSD, |state| {
			state.debt.bad_debt = 1;
		});
		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD),
			Error::<Test>::MarketNotEmpty
		);
		// Once the bad debt is cleared, the empty market is removable.
		mutate_branch_state(DOT, PUSD, |state| {
			state.debt.bad_debt = 0;
		});
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert!(!market_exists(DOT, PUSD));
	});
}

// A market still holding collateral in its redistribution account cannot be
// removed — the collateral would be stranded with no path left to reach it.
#[test]
fn remove_branch_rejected_while_collateral_remains() {
	build_and_execute(|| {
		register_default_branch();
		mutate_branch_state(DOT, PUSD, |state| {
			state.total_collateral = 1;
		});
		assert_noop!(
			Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD),
			Error::<Test>::MarketNotEmpty
		);
		mutate_branch_state(DOT, PUSD, |state| {
			state.total_collateral = 0;
		});
		assert_ok!(Pallet::<Test>::remove_branch(RuntimeOrigin::signed(ADMIN), DOT, PUSD));
		assert!(!market_exists(DOT, PUSD));
	});
}
