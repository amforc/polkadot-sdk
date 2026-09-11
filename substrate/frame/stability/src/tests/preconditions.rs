//! The preconditions every depositor call shares: a registered market, then an existing row.

use crate::{mock::*, Error};

#[test]
fn every_call_requires_a_pool_then_a_row() {
	build_and_execute(|| {
		mint_stable(PUSD, 1, 1_000);

		// Nothing is registered, so nothing can be dispatched. A refused call leaves no trace.
		// The calls that issue market interest first are refused by the vault engine.
		let branch_calls: [fn() -> DispatchResult; 5] = [
			|| deposit(1, DOT, PUSD, 400),
			|| request_withdraw(1, DOT, PUSD, 100),
			|| withdraw(1, DOT, PUSD, 100, 1),
			|| claim_yield(1, DOT, PUSD, 1),
			|| compound(1, DOT, PUSD, 60),
		];
		for call in branch_calls {
			assert_noop!(call(), pallet_vaults::Error::<Test>::BranchNotFound);
		}
		let pool_calls: [fn() -> DispatchResult; 2] =
			[|| claim_collateral(1, DOT, PUSD, 1), || settle(7, 1, DOT, PUSD)];
		for call in pool_calls {
			assert_noop!(call(), Error::<Test>::PoolNotRegistered);
		}
		assert_noop!(
			Stability::set_stability_pool_config(
				RuntimeOrigin::root(),
				DOT,
				PUSD,
				default_pool_config()
			),
			Error::<Test>::PoolNotRegistered
		);

		// With the pool registered, every call that acts on a row still needs one to exist.
		register_branch(DOT, PUSD, default_branch_config());
		let row_calls: [fn() -> DispatchResult; 6] = [
			|| request_withdraw(1, DOT, PUSD, 100),
			|| withdraw(1, DOT, PUSD, 100, 1),
			|| claim_collateral(1, DOT, PUSD, 1),
			|| claim_yield(1, DOT, PUSD, 1),
			|| compound(1, DOT, PUSD, 60),
			|| settle(7, 1, DOT, PUSD),
		];
		for call in row_calls {
			assert_noop!(call(), Error::<Test>::DepositNotFound);
		}
	});
}
