//! `distribute_yield` / `compound_yield`: the `G` accumulator end to end.
//! First tests where realization is non-trivial — yield is earned through
//! the accumulators rather than seeded by storage writes.

use crate::{mock::*, Error};

/// Deposit `amount` for `who`, minting 10_000 so later top-up deposits in
/// the same test have wallet head-room (unlike [`seed_deposit`]).
fn seed_active(who: AccountId, amount: Balance) {
	mint_stable(PUSD, who, 10_000);
	assert_ok!(deposit(who, DOT, PUSD, amount));
}

#[test]
fn distribute_to_empty_or_unknown_pool_returns_the_credit() {
	build_and_execute(|| {
		// The returned credit is the caller's to route: in the runtime the
		// vault engine's `OnBranchYield` plumbing forwards it to the fee
		// destination (`vault_interest_flows_to_pool_through_the_hook`
		// exercises that split live). Here the test drops it, rescinding the
		// issuance.
		// Unknown branch: nothing to distribute into.
		let leftover = distribute_yield(DOT, PUSD, 100);
		assert_eq!(leftover.peek(), 100);
		drop(leftover);

		// Registered but empty active pool: same.
		register_branch(DOT, PUSD, default_branch_config());
		let leftover = distribute_yield(DOT, PUSD, 100);
		assert_eq!(leftover.peek(), 100);
		drop(leftover);

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_yield_unclaimed, 0);
		let sums = crate::PoolSumsStore::<Test>::get((DOT, PUSD, 0u32, 0u32));
		assert_eq!(sums.g_yield, FixedU128::zero());
	});
}

#[test]
fn distribute_updates_g_exactly() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 1_000);
		activate_all(&[1]);

		let leftover = distribute_yield(DOT, PUSD, 100);
		assert!(leftover.peek().is_zero());
		drop(leftover);

		// delta_G = floor(100 * P / A) = floor(100 * 1e18 / 1000) = 1e17.
		let sums = crate::PoolSumsStore::<Test>::get((DOT, PUSD, 0u32, 0u32));
		assert_eq!(sums.g_yield, FixedU128::from_inner(100_000_000_000_000_000));

		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_yield_unclaimed, 100);
		// Pool holds the 1000 active plus the 100 undistributed yield.
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 1_100);

		System::assert_last_event(
			crate::Event::YieldDistributed { collateral_id: DOT, stable_id: PUSD, amount: 100 }
				.into(),
		);
	});
}

#[test]
fn depositors_realize_yield_proportionally() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 600);
		seed_active(2, 400);
		activate_all(&[1, 2]);

		drop(distribute_yield(DOT, PUSD, 100));

		// G = 0.1; gains = floor(600 * 0.1) = 60 and floor(400 * 0.1) = 40.
		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		assert_ok!(claim_yield(2, DOT, PUSD, 2));
		System::assert_has_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 60,
			}
			.into(),
		);
		System::assert_has_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 2,
				recipient: 2,
				amount: 40,
			}
			.into(),
		);
		// 60 + 40 = 100: no dust on this split.
		assert_eq!(pool_state(DOT, PUSD).total_yield_unclaimed, 0);
	});
}

#[test]
fn flooring_dust_accumulates_until_teardown() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 600);
		seed_active(2, 2_400);
		activate_all(&[1, 2]);

		drop(distribute_yield(DOT, PUSD, 100));

		// delta_G = floor(100 * 1e18 / 3000) = 33_333_333_333_333_333.
		// Gains: floor(600 * inner / 1e18) = 19 (exactly 19.999...98),
		//        floor(2400 * inner / 1e18) = 79 (exactly 79.999...92).
		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		assert_ok!(claim_yield(2, DOT, PUSD, 2));
		System::assert_has_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 19,
			}
			.into(),
		);
		System::assert_has_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 2,
				recipient: 2,
				amount: 79,
			}
			.into(),
		);
		// 100 - 19 - 79 = 2 units of flooring dust stay pool-owned, keeping
		// the balance identity exact.
		assert_eq!(pool_state(DOT, PUSD).total_yield_unclaimed, 2);

		// Live-market interactions do not redistribute the residue: no flow
		// reads it back, so later distributions add their own remainder.
		drop(distribute_yield(DOT, PUSD, 100));
		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		assert_ok!(claim_yield(2, DOT, PUSD, 2));
		assert_eq!(pool_state(DOT, PUSD).total_yield_unclaimed, 4);
		// It backs the pool-balance identity until all depositor rows are gone
		// and branch teardown sweeps it through `StableDustHandler`. The node
		// runtime routes that terminal residue to Treasury; governance.rs pins
		// the sweep and clean re-registration contract.
	});
}

#[test]
fn late_depositor_earns_only_after_their_snapshot() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 600);
		activate_all(&[1]);

		// First distribution belongs to user 1 alone: G = 60/600 = 0.1.
		drop(distribute_yield(DOT, PUSD, 60));

		// User 2 joins at G0 = 0.1.
		seed_active(2, 400);
		advance_time(5_000);
		assert_ok!(poke(2, 2, DOT, PUSD));

		// Second distribution splits over A = 1000: G = 0.1 + 0.1 = 0.2.
		drop(distribute_yield(DOT, PUSD, 100));

		// User 1: floor(600 * (0.2 - 0)) = 120 (all of the first, 60% of
		// the second). User 2: floor(400 * (0.2 - 0.1)) = 40.
		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		assert_ok!(claim_yield(2, DOT, PUSD, 2));
		System::assert_has_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 120,
			}
			.into(),
		);
		System::assert_has_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 2,
				recipient: 2,
				amount: 40,
			}
			.into(),
		);
		assert_eq!(pool_state(DOT, PUSD).total_yield_unclaimed, 0);
	});
}

#[test]
fn compound_moves_yield_into_active_and_it_earns() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 600);
		activate_all(&[1]);
		drop(distribute_yield(DOT, PUSD, 60));

		assert_ok!(compound(1, DOT, PUSD, 60));

		let row = deposit_row(DOT, PUSD, 1).expect("row exists");
		assert_eq!(row.active_deposit, 660);
		assert_eq!(row.claimable_yield, 0);
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_active_deposits, 660);
		assert_eq!(state.total_yield_unclaimed, 0);
		// Pure accounting: the pool balance did not move.
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 660);
		System::assert_last_event(
			crate::Event::YieldCompounded {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				amount: 60,
			}
			.into(),
		);

		// The compounded amount earns from here on: G moves by 66/660 = 0.1
		// and the whole 660 realizes floor(660 * 0.1) = 66.
		drop(distribute_yield(DOT, PUSD, 66));
		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		System::assert_has_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 66,
			}
			.into(),
		);
	});
}

#[test]
fn compound_clamps_to_claimable() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 600);
		activate_all(&[1]);
		drop(distribute_yield(DOT, PUSD, 60));

		// Asking for more than the 60 claimable compounds exactly the 60.
		assert_ok!(compound(1, DOT, PUSD, 1_000));
		System::assert_last_event(
			crate::Event::YieldCompounded {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				amount: 60,
			}
			.into(),
		);
	});
}

#[test]
fn compound_with_nothing_claimable_reverts() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 600);
		activate_all(&[1]);

		assert_noop!(compound(1, DOT, PUSD, 60), Error::<Test>::NoYieldToCompound);
		// A zero request is equally nothing to compound.
		drop(distribute_yield(DOT, PUSD, 60));
		assert_noop!(compound(1, DOT, PUSD, 0), Error::<Test>::NoYieldToCompound);
	});
}

#[test]
fn compound_without_row_or_branch_reverts() {
	build_and_execute(|| {
		assert_noop!(compound(1, DOT, PUSD, 60), Error::<Test>::PoolNotRegistered);
		register_branch(DOT, PUSD, default_branch_config());
		assert_noop!(compound(1, DOT, PUSD, 60), Error::<Test>::DepositNotFound);
	});
}

#[test]
fn vault_interest_flows_to_pool_through_the_hook() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 400);
		activate_all(&[1]);

		// A vault accruing 5%/yr on exactly 500 debt (499 borrowed + 1
		// upfront fee; the fee's 75% share floors to zero, so the whole
		// unit routes to the fee destination).
		mint_collateral(DOT, 5, 2_000);
		assert_ok!(open_vault(5, DOT, PUSD, 1_000, 499));
		assert_eq!(vault_debt(DOT, PUSD, 5), 500);
		assert_eq!(stable_balance(PUSD, FEE_DEST), 1);

		// One exact year of interest, minted on the next branch touch
		// (opening a second vault, whose own upfront fee is another unit
		// for the fee destination). Interest accrues on the 499 principal
		// at the floor-derived weighted rate — weighted sum
		// floor(499 * 0.05) = 24 — so exactly 24 mints.
		advance_time(pusd_primitives::MILLIS_PER_YEAR);
		mint_collateral(DOT, 6, 2_000);
		assert_ok!(open_vault(6, DOT, PUSD, 1_000, 499));

		// The hook split the 24: floor(75% * 24) = 18 to the pool; the
		// 6 remainder plus the two upfront fees (1 + 1) go to the fee
		// destination.
		let state = pool_state(DOT, PUSD);
		assert_eq!(state.total_yield_unclaimed, 18);
		let sums = crate::PoolSumsStore::<Test>::get((DOT, PUSD, 0u32, 0u32));
		// delta_G = floor(18 * 1e18 / 400) = 4.5e16.
		assert_eq!(sums.g_yield, FixedU128::from_inner(45_000_000_000_000_000));
		let pool = Stability::pool_account(&DOT, &PUSD);
		assert_eq!(stable_balance(PUSD, pool), 418);
		assert_eq!(stable_balance(PUSD, FEE_DEST), 8);
		System::assert_has_event(
			crate::Event::YieldDistributed { collateral_id: DOT, stable_id: PUSD, amount: 18 }
				.into(),
		);

		// The depositor realizes the full pool share: floor(400 * 0.045).
		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		System::assert_has_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 18,
			}
			.into(),
		);
	});
}

#[test]
fn pending_deposits_earn_no_yield() {
	build_and_execute(|| {
		register_branch(DOT, PUSD, default_branch_config());
		seed_active(1, 600);
		activate_all(&[1]);
		// User 2's deposit is still pending when the yield arrives.
		seed_active(2, 400);

		drop(distribute_yield(DOT, PUSD, 60));

		// The whole distribution went to the 600 active: G = 0.1, and user
		// 2 snapshots G at deposit time, so nothing accrues to them even
		// after activation.
		advance_time(5_000);
		assert_ok!(poke(2, 2, DOT, PUSD));
		assert_noop!(claim_yield(2, DOT, PUSD, 2), Error::<Test>::NoClaimableYield);

		assert_ok!(claim_yield(1, DOT, PUSD, 1));
		System::assert_has_event(
			crate::Event::YieldClaimed {
				collateral_id: DOT,
				stable_id: PUSD,
				depositor: 1,
				recipient: 1,
				amount: 60,
			}
			.into(),
		);
	});
}
