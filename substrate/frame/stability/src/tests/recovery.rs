//! Recovery offsets against a real vault in `FinalRecovery`, priced by the real redemptions
//! pallet, so an offset and a recovery redemption cannot be priced differently.
//!
//! The standing figures: the vault holds 1000 collateral against 500 debt, which is 499 borrowed
//! plus a 1 upfront fee. At the parked price of 0.52 its CR is 104%, so the recovery bonus is 3%
//! and cancelling debt `D` pays `floor(floor(D * 1.03) / 0.52)` collateral.
//!
//! The active pool is the 400 that account 1 deposits and matures while the branch is still
//! Normal, which also fixes the standing timestamp and accrued-interest figures.

use crate::{
	mock::*,
	types::{PoolPrecision, RecoveryOffsetSource},
	Error,
};
use frame::prelude::ArithmeticError;
use pallet_vaults::DebtBreakdown;

/// The price that parks the standing vault at a CR of 104%.
const PRICE_AT_104_PERCENT: FixedU128 = FixedU128::from_rational(52, 100);

/// The recovery branch, without the standing vault.
fn recovery_branch() -> ExtBuilder {
	ExtBuilder::default().default_branch().branch_config(recovery_branch_config())
}

/// The recovery branch with the standing vault (owner 5) open.
fn standing_vault() -> ExtBuilder {
	recovery_branch().vault(5, 1_000, 498)
}

/// The standing vault with account 1's 400 active behind it, and 600 in account 1's wallet.
fn standing_head_with_active_pool() -> ExtBuilder {
	standing_vault().stable_balance(1, 600).matured_deposit(1, 400)
}

/// Opens the standing vault (owner 5) and pins its exact debt.
fn open_standing_vault() {
	mint_collateral(DOT, 5, 1_000);
	assert_ok!(open_vault(5, DOT, STABLE, 1_000, 498));
	assert_standing_debt();
}

/// 498 borrowed + 1 upfront fee, plus the unit of interest the next touch rounds up while the
/// pool matures: every literal below derives from 500.
fn assert_standing_debt() {
	assert_eq!(vault_debt(DOT, STABLE, 5), 499);
}

/// Pins the standing vault's debt, then parks it in `FinalRecovery` at `price`.
fn park_standing_head_at(price: FixedU128) {
	assert_standing_debt();
	set_price(DOT, price);
	assert_ok!(enter_final_recovery(DOT, STABLE, 5));
}

/// Asserts that this pallet reported a recovery offset paid by `source`.
fn assert_recovery_source(source: RecoveryOffsetSource) {
	System::assert_has_event(
		crate::Event::RecoveryOffsetApplied { collateral_id: DOT, stable_id: STABLE, source }
			.into(),
	);
}

/// Asserts that the standing vault settled `debt` for `collateral`, paid by `source`.
fn assert_recovery_offset(
	debt: DebtBreakdown<Balance>,
	collateral: Balance,
	source: RecoveryOffsetSource,
) {
	assert_vault_redeemed(5, debt, collateral);
	assert_recovery_source(source);
}

#[test]
fn active_pool_recovery_offset_settles_the_head() {
	standing_head_with_active_pool().build_and_execute(|| {
		park_standing_head_at(PRICE_AT_104_PERCENT);

		// 350 would leave 50 < the 100 floor: clamped to 300, and the settlement burns exactly
		// the clamped amount.
		assert_ok!(offset_recovery(DOT, STABLE, 350));

		// collateral_out = floor(floor(300 * 1.03) / 0.52) = floor(309/0.52)
		//                = 594.
		// The head's two units of interest settle before its principal.
		let debt = DebtBreakdown { principal: 298, interest: 2 };
		assert_recovery_offset(debt, 594, RecoveryOffsetSource::ActivePool);

		// The vault settled 300 of its 500 debt and paid real collateral.
		assert_eq!(vault_debt(DOT, STABLE, 5), 200);
		assert_eq!(pool_stable_held(DOT, STABLE), 100);
		assert_eq!(pool_collateral_held(DOT, STABLE), 594);

		// The standard accumulator update: P = 100/400 = 0.25 and
		// delta_S = floor(594 * 1e18 / 400) = 1.485e18.
		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 100);
		assert_eq!(state.coords.p, FixedU128::from_rational(1, 4));
		assert_eq!(state.total_collateral_gains_unclaimed, 594);
		let sums = active_sums(0, 0);
		assert_eq!(sums.s_collateral, FixedU128::from_rational(1_485, 1_000));

		// The depositor realizes exactly the settled collateral:
		// gain = floor(400 * 1.485) = 594, compounded = floor(400 * 0.25).
		assert_claim_collateral(1, 594);
		let row = deposit_row(DOT, STABLE, 1).expect("row survives");
		assert_eq!(row.active_deposit, 100);
	});
}

#[test]
fn recovery_offset_can_fully_deplete_the_pool() {
	standing_head_with_active_pool().build_and_execute(|| {
		park_standing_head_at(PRICE_AT_104_PERCENT);

		// capacity = min(500, 1000) = 500, but the pool holds 400: full
		// depletion passes the post-offset floor clamp and bumps the epoch.
		assert_ok!(offset_recovery(DOT, STABLE, 1_000));

		// collateral_out = floor(floor(400 * 1.03) / 0.52) = floor(412/0.52)
		//                = 792.
		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 0);
		assert_eq!(state.coords, Accumulators { p: FixedU128::one(), epoch: 1, scale: 0 });
		assert_eq!(vault_debt(DOT, STABLE, 5), 100);

		// The old-epoch depositor realizes to zero active with the full
		// gain: floor(400 * floor(792e18/400) / 1e18) = 792.
		assert_claim_collateral(1, 792);
		assert!(deposit_row(DOT, STABLE, 1).is_none());
	});
}

#[test]
fn recovery_offset_error_paths() {
	recovery_branch().build_and_execute(|| {
		// No active capital to burn. The pool is sized before the head is looked up, so the empty
		// pool is what rejects the call: once funded, the same call fails on the missing head.
		assert_noop!(offset_recovery(DOT, STABLE, 300), Error::<Test>::NoRecoveryOffsetPerformed);

		mint_stable(STABLE, 1, 600);
		seed_matured_deposit(1, 400);

		// No FinalRecovery vault queued.
		assert_noop!(offset_recovery(DOT, STABLE, 300), Error::<Test>::RecoveryVaultNotFound);

		open_standing_vault();
		park_standing_head_at(PRICE_AT_104_PERCENT);

		// A zero budget resolves to a zero burn.
		assert_noop!(offset_recovery(DOT, STABLE, 0), Error::<Test>::NoRecoveryOffsetPerformed);

		// Frozen halts recovery offsets like every other operation.
		MockOracleAvailable::set(false);

		assert_noop!(offset_recovery(DOT, STABLE, 300), Error::<Test>::BranchFrozen);
		MockOracleAvailable::set(true);
	});
}

#[test]
fn recovery_offset_past_the_supported_scale_range_is_refused() {
	// Recovery sizes its burn by the floor alone, and only the accounting checks `P`. The coarsest
	// precision the pool accepts, `p_min = 1e-3` and `scale_factor = 1e3`, lets `P` fall by at
	// most `1e6` across the two supported crossings before it must stay at or above `p_min`.
	let coarse = StabilityPoolConfig {
		minimum_active_pool_balance: 1,
		precision: PoolPrecision {
			p_min: FixedU128::from_rational(1u128, 1_000u128),
			scale_factor: 1_000,
		},
		..default_pool_config()
	};
	let pool_total: Balance = 2_000_000_000;
	for (survivor, refused) in [(1, true), (2, false)] {
		// The standing vault scaled by 1e7, so the head can absorb the whole pool.
		recovery_branch()
			.pool_config(coarse.clone())
			.vault(5, 10_000_000_000, 4_980_000_000)
			.matured_deposit(1, pool_total)
			.build_and_execute(|| {
				set_price(DOT, PRICE_AT_104_PERCENT);
				assert_ok!(enter_final_recovery(DOT, STABLE, 5));

				// One survivor in 2e9 needs `P = 1e6 / 2e9 = 5e-4`, under `p_min`. Two survivors
				// land on `p_min` exactly, which the pool still represents.
				let burn = pool_total - survivor;
				if refused {
					assert_noop!(
						offset_recovery(DOT, STABLE, burn),
						Error::<Test>::UnsupportedOffsetPrecision
					);
				} else {
					assert_ok!(offset_recovery(DOT, STABLE, burn));
					assert_eq!(pool_state(DOT, STABLE).total_active_deposits, survivor);
				}
			});
	}
}

#[test]
fn active_recovery_rolls_back_when_pool_accounting_fails_after_settlement() {
	standing_head_with_active_pool().build_and_execute(|| {
		park_standing_head_at(PRICE_AT_104_PERCENT);

		hypothetically!({
			mutate_pool_state(DOT, STABLE, |state| {
				state.total_collateral_gains_unclaimed = Balance::MAX;
			});

			// `assert_noop!` compares the whole storage root, so it also proves the vault
			// settlement and the pool burn rolled back with the failed plan.
			assert_noop!(offset_recovery(DOT, STABLE, 300), ArithmeticError::Overflow);
		});
	});
}

#[test]
fn incoming_recovery_rolls_back_when_deposit_accounting_fails_after_settlement() {
	standing_vault().stable_balance(2, 200).deposit(2, 100).build_and_execute(|| {
		park_standing_head_at(PRICE_AT_104_PERCENT);

		hypothetically!({
			crate::Deposits::<Test>::mutate((DOT, STABLE, 2), |row| {
				row.as_mut().expect("deposit row exists").claimable_collateral = Balance::MAX;
			});

			// `assert_noop!` compares the whole storage root, so it also proves the depositor
			// burn and the vault settlement rolled back with the failed claimable update.
			assert_noop!(deposit(2, DOT, STABLE, 200), ArithmeticError::Overflow);
		});
	});
}

#[test]
fn par_band_head_settles_at_face_value() {
	standing_head_with_active_pool().build_and_execute(|| {
		// CR = 1000 * 0.5025 / 500 = 100.5%: at or above par, but inside the
		// 1% bonus buffer, where the raw excess CR - 100% - buffer would be
		// negative (-0.5%). The max(0, ·) clamp in `recovery_bonus` settles
		// at exactly face value instead of ever discounting.
		park_standing_head_at(FixedU128::from_rational(5_025u128, 10_000u128));

		// collateral_out = floor(200 / 0.5025) = 398 — no bonus, no haircut.
		assert_ok!(offset_recovery(DOT, STABLE, 200));

		let debt = DebtBreakdown { principal: 198, interest: 2 };
		assert_recovery_offset(debt, 398, RecoveryOffsetSource::ActivePool);
		assert_eq!(vault_debt(DOT, STABLE, 5), 300);

		// Full settlement cancels the remaining 300: floor(300 / 0.5025) = 597.
		seed_deposit(2, 300);

		let debt = DebtBreakdown { principal: 300, interest: 0 };
		assert_recovery_offset(debt, 597, RecoveryOffsetSource::IncomingDeposit);
		assert_eq!(vault_debt(DOT, STABLE, 5), 0);
		let row = deposit_row(DOT, STABLE, 2).expect("row created");
		assert_eq!(row.claimable_collateral, 597);
		assert!(row.pending_deposit.is_none());
	});
}

#[test]
fn below_par_head_rejects_offsets_and_deposits() {
	standing_head_with_active_pool().stable_balance(2, 300).build_and_execute(|| {
		// CR = 400/500 = 80%: below par. The (empty) Insurance Fund plays
		// no role — any split prices as BelowPar for the pool.
		park_standing_head_at(FixedU128::from_rational(4u128, 10u128));

		assert_noop!(offset_recovery(DOT, STABLE, 300), Error::<Test>::RecoveryOffsetBelowPar);
		// Incoming deposits are rejected wholesale rather than settled at
		// a discount.
		assert_noop!(deposit(2, DOT, STABLE, 300), Error::<Test>::RecoveryOffsetBelowPar);
		assert!(deposit_row(DOT, STABLE, 2).is_none());
		assert_eq!(stable_balance(STABLE, 2), 300);
	});
}

#[test]
fn incoming_deposit_recovers_first_and_queues_the_rest() {
	standing_head_with_active_pool().build_and_execute(|| {
		park_standing_head_at(PRICE_AT_104_PERCENT);
		let state_before = pool_state(DOT, STABLE);
		let sums_before = active_sums(0, 0);

		// Full settlement burns 500 of the depositor payment:
		// collateral_out = floor(floor(500 * 1.03) / 0.52) = floor(515/0.52) = 990.
		seed_deposit(2, 800);

		let debt = DebtBreakdown { principal: 498, interest: 2 };
		assert_recovery_offset(debt, 990, RecoveryOffsetSource::IncomingDeposit);
		System::assert_has_event(market_event!(DepositReceived {
			depositor: 2,
			amount: 800,
			pending_amount: 300,
		}));

		// The head is fully drained (Dormant, out of the FIFO).
		assert_eq!(vault_debt(DOT, STABLE, 5), 0);

		let row = deposit_row(DOT, STABLE, 2).expect("row created");
		assert_eq!(row.claimable_collateral, 990);
		assert_eq!(row.pending_deposit.expect("leftover queued").amount, 300);
		assert_eq!(stable_balance(STABLE, 2), 0);

		// The part spent on the recovery never entered the pool balance and never touched
		// the accumulators.
		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 400);
		assert_eq!(state.total_pending_deposits, 300);
		assert_eq!(state.total_collateral_gains_unclaimed, 990);
		assert_eq!(state.coords, state_before.coords);
		assert_eq!(active_sums(0, 0), sums_before);
		assert_eq!(pool_stable_held(DOT, STABLE), 700);
		assert_eq!(pool_collateral_held(DOT, STABLE), 990);

		// With the head gone, a follow-up deposit queues normally.
		seed_deposit(2, 100);

		let row = deposit_row(DOT, STABLE, 2).expect("row kept");
		assert_eq!(row.pending_deposit.expect("merged").amount, 400);

		// The direct credit is claimable through the normal path.
		assert_claim_collateral(2, 990);
	});
}

#[test]
fn incoming_deposit_fully_used_leaves_no_pending() {
	standing_head_with_active_pool().build_and_execute(|| {
		park_standing_head_at(PRICE_AT_104_PERCENT);

		// used = min(200, capacity 500) = 200 — nothing left to queue;
		// collateral_out = floor(floor(200 * 1.03) / 0.52) = floor(206/0.52)
		//                = 396.
		seed_deposit(2, 200);

		System::assert_has_event(market_event!(DepositReceived {
			depositor: 2,
			amount: 200,
			pending_amount: 0,
		}));
		let row = deposit_row(DOT, STABLE, 2).expect("row created");
		assert_eq!(row.claimable_collateral, 396);
		assert!(row.pending_deposit.is_none());
		assert_eq!(stable_balance(STABLE, 2), 0);

		// The head keeps its remaining 300 debt and stays in the FIFO.
		assert_eq!(vault_debt(DOT, STABLE, 5), 300);
		assert_eq!(pool_state(DOT, STABLE).total_pending_deposits, 0);
	});
}

/// The standing head with account 1's 400 active behind it, in a pool that takes every minted
/// unit, so the unit of interest the head's touch rounds up inside the recovery settlement reaches
/// the pool as yield instead of the fee account.
fn standing_head_taking_all_yield() -> ExtBuilder {
	recovery_branch()
		.pool_config(StabilityPoolConfig {
			yield_share: Permill::from_percent(100),
			..default_pool_config()
		})
		.vault(5, 1_000, 498)
		.stable_balance(1, 600)
		.matured_deposit(1, 400)
}

/// Parks the head and lets interest run past the unit its last touch charged ahead, so the touch
/// inside the settlement rounds up a unit the aggregate accrual did not cover.
fn park_with_unminted_vault_interest() {
	park_standing_head_at(PRICE_AT_104_PERCENT);
	advance_time(30 * 24 * 3_600 * 1_000);
	// Only the events of the operation under test are compared.
	stability_events_since_last_call();
}

// The settlement re-enters this pallet through the vault's yield hook. The yield it distributes
// must survive the commit of the pool row the operation held in memory while it ran: the
// post-test `try_state` then proves the pool balance still matches its tracked totals.
#[test]
fn incoming_deposit_keeps_yield_distributed_during_settlement() {
	standing_head_taking_all_yield().build_and_execute(|| {
		park_with_unminted_vault_interest();
		let yield_before = pool_state(DOT, STABLE).total_yield_unclaimed;

		seed_deposit(2, 800);

		assert_eq!(
			stability_events_since_last_call(),
			vec![
				crate::Event::YieldDistributed { collateral_id: DOT, stable_id: STABLE, amount: 1 },
				crate::Event::YieldDistributed { collateral_id: DOT, stable_id: STABLE, amount: 1 },
				crate::Event::RecoveryOffsetApplied {
					collateral_id: DOT,
					stable_id: STABLE,
					source: RecoveryOffsetSource::IncomingDeposit,
				},
				crate::Event::DepositReceived {
					collateral_id: DOT,
					stable_id: STABLE,
					depositor: 2,
					amount: 800,
					pending_amount: 298,
				},
			]
		);
		assert_eq!(pool_state(DOT, STABLE).total_yield_unclaimed, yield_before + 2);
	});
}

#[test]
fn active_pool_offset_keeps_yield_distributed_during_settlement() {
	standing_head_taking_all_yield().build_and_execute(|| {
		park_with_unminted_vault_interest();
		let yield_before = pool_state(DOT, STABLE).total_yield_unclaimed;

		assert_ok!(offset_recovery(DOT, STABLE, 300));

		assert_eq!(
			stability_events_since_last_call(),
			vec![
				crate::Event::YieldDistributed { collateral_id: DOT, stable_id: STABLE, amount: 2 },
				crate::Event::RecoveryOffsetApplied {
					collateral_id: DOT,
					stable_id: STABLE,
					source: RecoveryOffsetSource::ActivePool,
				},
			]
		);
		assert_eq!(pool_state(DOT, STABLE).total_yield_unclaimed, yield_before + 2);
	});
}

// The deposit is charged for a settlement that closes the head, so only a deposit that found no
// head may report the smaller weight.
#[test]
fn deposit_refunds_to_the_no_recovery_weight_only_without_a_head() {
	standing_head_with_active_pool().stable_balance(2, 900).build_and_execute(|| {
		park_standing_head_at(PRICE_AT_104_PERCENT);

		let settled = Stability::deposit(RuntimeOrigin::signed(2), DOT, STABLE, 800)
			.expect("the deposit settles the head");

		assert_vault_redeemed(5, DebtBreakdown { principal: 498, interest: 2 }, 990);
		assert_eq!(settled.actual_weight, None);

		let queued = Stability::deposit(RuntimeOrigin::signed(2), DOT, STABLE, 100)
			.expect("the deposit queues");

		assert_eq!(queued.actual_weight, Some(<() as crate::WeightInfo>::deposit_no_recovery()));
	});
}

// A head that cannot be offset is still read and priced, so the deposit that meets it keeps the
// full charge rather than the weight of an empty queue.
#[test]
fn deposit_keeps_the_full_weight_for_a_head_it_cannot_offset() {
	standing_head_with_active_pool().stable_balance(2, 900).build_and_execute(|| {
		park_standing_head_at(PRICE_AT_104_PERCENT);
		// The registration price lifts the head above the exit threshold.
		set_price(DOT, REGISTRATION_PRICE);
		stability_events_since_last_call();

		let queued = Stability::deposit(RuntimeOrigin::signed(2), DOT, STABLE, 100)
			.expect("the deposit queues past the head");

		assert_eq!(queued.actual_weight, None);
		// The deposit activates the matured cohort and queues: no recovery offset is among its
		// events.
		assert_eq!(
			stability_events_since_last_call(),
			vec![
				crate::Event::CohortActivated {
					collateral_id: DOT,
					stable_id: STABLE,
					cohort: CohortId(0),
					deadline: 10_000,
					amount: 400,
				},
				crate::Event::DepositReceived {
					collateral_id: DOT,
					stable_id: STABLE,
					depositor: 2,
					amount: 100,
					pending_amount: 100,
				},
			]
		);
	});
}
