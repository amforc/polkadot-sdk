//! Permissionless discovery of redistribution dust through the single Dormant slot.

use crate::{
	mock::*,
	tests::{assert_event, percent, vault_status},
	Error, Event,
};
use frame::prelude::One;
use stablecoin_primitives::VaultInterface;

/// Two debt-free stake bearers receive a liquidation lazily, outside the redemption indices.
fn redistribute_to_husks(debt: Balance) {
	let config = crate::BranchConfig { upfront_fee_period: 0, ..default_branch_config() };
	register_market_with(DOT, STABLE, DEFAULT_PRICE, config);
	for owner in [1, 2] {
		assert_ok!(open(owner, DOT, STABLE, 1_000, 500, percent(5)));
		assert_ok!(repay(owner, DOT, STABLE, owner, None));
	}
	assert_ok!(open(3, DOT, STABLE, 200, debt, percent(5)));
	set_price(DOT, FixedU128::one());
	assert_ok!(liquidate(99, DOT, STABLE, 3, 0, 0));
}

fn park_vault(remaining: Balance) {
	let config = crate::BranchConfig { upfront_fee_period: 0, ..default_branch_config() };
	register_market_with(DOT, STABLE, DEFAULT_PRICE, config);
	assert_ok!(open(1, DOT, STABLE, 1_040, 500, percent(50)));
	assert_ok!(redeem(DOT, STABLE, 3, 500 - remaining));
}

#[test]
fn nomination_settles_multiple_redistribution_recipients_serially() {
	build_and_execute(|| {
		redistribute_to_husks(200);
		assert_eq!(Vaults::next_redemption_target(&DOT, &STABLE, None), None);
		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 0);
		assert_noop!(activate_dormant(9, DOT, STABLE, 1), Error::<Test>::DebtBelowMinimum);

		// No poke is needed: nomination realizes the debt and collateral before checking them.
		assert_ok!(nominate_dormant(9, DOT, STABLE, 1));
		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 100);
		assert_eq!(vault(DOT, STABLE, 1).collateral, 1_095);
		assert_eq!(held(DOT, 1), 1_095);
		assert!(vault_status(DOT, STABLE, 1).is_dormant());
		assert_eq!(LinkedList::node(rate_list(DOT, STABLE), 1), None);
		assert_eq!(Vaults::redemption_queue(DOT, STABLE, 10), vec![1]);
		assert_event(Event::DormantTargetNominated {
			collateral_id: DOT,
			stable_id: STABLE,
			owner: 1,
		});

		// Re-nomination is allowed, but nobody can displace this holder. The failed nomination
		// must also roll back the second recipient's pending redistribution and collateral hold.
		assert_ok!(nominate_dormant(8, DOT, STABLE, 1));
		assert_noop!(nominate_dormant(9, DOT, STABLE, 2), Error::<Test>::DormantTargetOccupied);
		assert_eq!(vault(DOT, STABLE, 2).debt.total(), 0);
		assert_eq!(held(DOT, 2), 1_000);

		assert_eq!(redeem(DOT, STABLE, 4, 100).expect("nominated dust settles"), 1);
		assert_eq!(Vaults::next_redemption_target(&DOT, &STABLE, None), None);
		assert_ok!(nominate_dormant(9, DOT, STABLE, 2));
		assert_eq!(redeem(DOT, STABLE, 4, 100).expect("next recipient settles"), 2);
		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 0);
		assert_eq!(vault(DOT, STABLE, 2).debt.total(), 0);
		assert_eq!(Vaults::next_redemption_target(&DOT, &STABLE, None), None);
	});
}

#[test]
fn nomination_rejects_non_dormant_non_dust_and_underwater_vaults() {
	build_and_execute(|| {
		park_vault(100);
		assert_noop!(nominate_dormant(9, DOT, STABLE, 2), Error::<Test>::VaultNotFound);
		// Below par the dust needs liquidation or recovery, not the slot.
		set_price(DOT, FixedU128::from_rational(99, 1_000));
		assert_noop!(
			nominate_dormant(9, DOT, STABLE, 1),
			Error::<Test>::UnsafeCollateralizationRatio
		);
		set_price(DOT, DEFAULT_PRICE);
		assert_ok!(open(2, DOT, STABLE, 1_000, 500, percent(5)));
		assert_noop!(nominate_dormant(9, DOT, STABLE, 2), Error::<Test>::InvalidVaultStatus);
		assert_ok!(repay(1, DOT, STABLE, 1, None));
		assert_noop!(nominate_dormant(9, DOT, STABLE, 1), Error::<Test>::DebtNotDust);
	});
	// Each husk holds half the stake, so it owes 200 of the 400 redistributed: exactly the
	// minimum. Debt that size is not dust, and the vault returns through activation instead.
	build_and_execute(|| {
		redistribute_to_husks(400);
		assert_noop!(nominate_dormant(9, DOT, STABLE, 1), Error::<Test>::DebtNotDust);
		assert_ok!(activate_dormant(9, DOT, STABLE, 1));
		assert_eq!(vault(DOT, STABLE, 1).debt.total(), 200);
		assert!(vault_status(DOT, STABLE, 1).is_active());
	});
}
