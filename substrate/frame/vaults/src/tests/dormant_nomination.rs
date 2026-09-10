//! Permissionless discovery of redistribution dust through the single Dormant slot.

use crate::{
	mock::*,
	tests::{assert_event, rate_pct, vault_status},
	Error, Event,
};
use frame::prelude::One;
use pusd_primitives::VaultInterface;

/// Two debt-free stake bearers receive a liquidation lazily, outside the redemption indices.
fn redistribute_to_husks(debt: Balance) {
	let config = crate::BranchConfig { upfront_fee_period: 0, ..default_branch_config() };
	register_market_with(DOT, PUSD, FixedU128::from_u32(10), config);
	for owner in [1, 2] {
		assert_ok!(open(owner, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_ok!(repay(owner, DOT, PUSD, owner, None));
	}
	assert_ok!(open(3, DOT, PUSD, 200, debt, rate_pct(5, 100)));
	set_price(DOT, FixedU128::one());
	assert_ok!(liquidate(99, DOT, PUSD, 3, 0, 0));
}

fn park_vault(remaining: Balance) {
	let config = crate::BranchConfig { upfront_fee_period: 0, ..default_branch_config() };
	register_market_with(DOT, PUSD, FixedU128::from_u32(10), config);
	assert_ok!(open(1, DOT, PUSD, 1_040, 500, rate_pct(50, 100)));
	assert_ok!(redeem(DOT, PUSD, 3, 500 - remaining));
}

#[test]
fn nomination_settles_multiple_redistribution_recipients_serially() {
	build_and_execute(|| {
		redistribute_to_husks(200);
		assert_eq!(Vaults::next_redemption_target(&DOT, &PUSD, None), None);
		assert_eq!(vault(DOT, PUSD, 1).debt.total(), 0);
		assert_noop!(activate_dormant(9, DOT, PUSD, 1), Error::<Test>::DebtBelowMinimum);

		// No poke is needed: nomination realizes the debt and collateral before checking them.
		assert_ok!(nominate_dormant(9, DOT, PUSD, 1));
		assert_eq!(vault(DOT, PUSD, 1).debt.total(), 100);
		assert_eq!(vault(DOT, PUSD, 1).collateral, 1_095);
		assert_eq!(held(DOT, 1), 1_095);
		assert!(vault_status(DOT, PUSD, 1).is_dormant());
		assert_eq!(LinkedList::neighbors(rate_list(DOT, PUSD), 1), None);
		assert_eq!(Vaults::redemption_queue(DOT, PUSD, 10), vec![1]);
		assert_event(Event::DormantTargetNominated {
			collateral_id: DOT,
			stable_id: PUSD,
			owner: 1,
		});

		// Re-nomination is allowed, but nobody can displace this holder. The failed nomination
		// must also roll back the second recipient's pending redistribution and collateral hold.
		assert_ok!(nominate_dormant(8, DOT, PUSD, 1));
		assert_noop!(nominate_dormant(9, DOT, PUSD, 2), Error::<Test>::DormantTargetOccupied);
		assert_eq!(vault(DOT, PUSD, 2).debt.total(), 0);
		assert_eq!(held(DOT, 2), 1_000);

		assert_eq!(redeem(DOT, PUSD, 4, 100).expect("nominated dust settles"), 1);
		assert_eq!(Vaults::next_redemption_target(&DOT, &PUSD, None), None);
		assert_ok!(nominate_dormant(9, DOT, PUSD, 2));
		assert_eq!(redeem(DOT, PUSD, 4, 100).expect("next recipient settles"), 2);
		assert_eq!(vault(DOT, PUSD, 1).debt.total(), 0);
		assert_eq!(vault(DOT, PUSD, 2).debt.total(), 0);
		assert_eq!(Vaults::next_redemption_target(&DOT, &PUSD, None), None);
	});
}

#[test]
fn nomination_rejects_non_dormant_non_dust_and_underwater_vaults() {
	build_and_execute(|| {
		park_vault(100);
		assert_noop!(nominate_dormant(9, DOT, PUSD, 2), Error::<Test>::VaultNotFound);
		// Below par the dust needs liquidation or recovery, not the slot.
		set_price(DOT, FixedU128::from_rational(99, 1_000));
		assert_noop!(
			nominate_dormant(9, DOT, PUSD, 1),
			Error::<Test>::UnsafeCollateralizationRatio
		);
		set_price(DOT, FixedU128::from_u32(10));
		assert_ok!(open(2, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		assert_noop!(nominate_dormant(9, DOT, PUSD, 2), Error::<Test>::InvalidVaultStatus);
		assert_ok!(repay(1, DOT, PUSD, 1, None));
		assert_noop!(nominate_dormant(9, DOT, PUSD, 1), Error::<Test>::DebtNotDust);
	});
}
