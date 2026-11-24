//! The preconditions every depositor call shares: a registered branch, then an existing row.

use crate::{mock::*, Error};

#[test]
fn every_call_requires_a_pool_then_a_row() {
	ExtBuilder::default().stable_balance(1, 1_000).build_and_execute(|| {
		// Nothing is registered, so every call fails with `PoolNotRegistered` before issuing
		// branch interest or touching vault state.
		assert_noop!(deposit(1, DOT, STABLE, 400), Error::<Test>::PoolNotRegistered);
		assert_noop!(request_withdraw(1, DOT, STABLE, 100), Error::<Test>::PoolNotRegistered);
		assert_noop!(withdraw(1, DOT, STABLE, 100, 1), Error::<Test>::PoolNotRegistered);
		assert_noop!(claim_yield(1, DOT, STABLE, 1), Error::<Test>::PoolNotRegistered);
		assert_noop!(compound(1, DOT, STABLE, 60), Error::<Test>::PoolNotRegistered);
		assert_noop!(claim_collateral(1, DOT, STABLE, 1), Error::<Test>::PoolNotRegistered);
		assert_noop!(settle(7, 1, DOT, STABLE), Error::<Test>::PoolNotRegistered);
		assert_noop!(prune_pool_sums(1, DOT, STABLE, 1), Error::<Test>::PoolNotRegistered);
		assert_noop!(
			Stability::set_stability_pool_config(
				RuntimeOrigin::root(),
				DOT,
				STABLE,
				default_pool_config()
			),
			Error::<Test>::PoolNotRegistered
		);

		// With the pool registered, every call that acts on a row still needs one to exist.
		register_branch(DOT, STABLE, default_branch_config());

		assert_noop!(request_withdraw(1, DOT, STABLE, 100), Error::<Test>::NotInSafetyMode);
		assert_noop!(withdraw(1, DOT, STABLE, 100, 1), Error::<Test>::DepositNotFound);
		assert_noop!(claim_collateral(1, DOT, STABLE, 1), Error::<Test>::DepositNotFound);
		assert_noop!(claim_yield(1, DOT, STABLE, 1), Error::<Test>::DepositNotFound);
		assert_noop!(compound(1, DOT, STABLE, 60), Error::<Test>::DepositNotFound);
		assert_noop!(settle(7, 1, DOT, STABLE), Error::<Test>::DepositNotFound);
	});
}
