//! Maturity cohorts: activation that no row touch has to trigger.
//!
//! Deposits gather in at most two deadline-ordered cohorts, and the first pool
//! operation past a deadline moves the cohort's surviving capital into the active total in one
//! step. Member rows realize lazily through the checkpoint that advancement leaves behind. These
//! tests drive the cohort lifecycle, the two-phase settlement, and the governance edges.

use crate::{mock::*, Error};
use stablecoin_primitives::StabilityPoolInspect;

#[test]
fn yield_distribution_activates_matured_capital_without_a_touch() {
	ExtBuilder::default().default_branch().deposit(1, 400).build_and_execute(|| {
		// Before the deadline the capital is pending: the pool takes no yield for it.
		assert_yield_declined(DOT, STABLE, 40);

		// Past the deadline, the distribution itself advances the cohort first: the 400 enters
		// the denominator without anyone touching the row.
		advance_time(9_000);
		assert_eq!(distribute_yield_leftover(DOT, STABLE, 40), 0);

		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 400);
		assert_eq!(state.total_pending_deposits, 0);
		assert!(state.open_cohorts.is_empty());
		System::assert_has_event(market_event!(CohortActivated {
			cohort: CohortId(0),
			deadline: 10_000,
			amount: 400,
		}));

		// The row itself is untouched; it still shows the tranche as pending.
		let row = deposit_row(DOT, STABLE, 1).expect("row exists");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.pending_deposit.expect("not yet settled").amount, 400);

		// The claim settles the row through the checkpoint and pays the yield earned since
		// activation: delta_G = 40 / 400 = 0.1, so floor(400 * 0.1) = 40.
		assert_claim_yield(1, 40);
		let row = deposit_row(DOT, STABLE, 1).expect("row exists");
		assert_eq!(row.active_deposit, 400);
		assert!(row.pending_deposit.is_none());
	});
}

#[test]
fn offsets_size_against_matured_capital_without_a_touch() {
	ExtBuilder::default().default_branch().deposit(1, 400).build_and_execute(|| {
		// Deposited at t = 1_000: the cohort boundary lands at 10_000, and t = 9_999 is one
		// millisecond short. The read-only sizing simulates the advancement and still finds
		// no active capital, so an offset reaches the 400 only through the pending leg.
		advance_time(8_999);

		assert_storage_noop!({
			let quote = market_quote(&DOT, &STABLE);
			assert_eq!(Stability::quote_active(&quote, 400), 0);
			assert_eq!(Stability::quote_pending(&quote, 100, 0), 100);
		});
		// The activation view agrees: one millisecond short, the 400 is still pending.
		let view = Stability::pool_after_activation(DOT, STABLE).expect("pool registered");
		assert_eq!((view.total_active_deposits, view.total_pending_deposits), (0, 400));
		assert_eq!(view.open_cohorts.len(), 1);

		// Exactly at t = 10_000 the same call sees the 400 as active, with the row and the pool
		// untouched.
		advance_time(1);

		assert_storage_noop!({
			let quote = market_quote(&DOT, &STABLE);
			assert_eq!(Stability::quote_active(&quote, 400), 400);
			assert_eq!(Stability::quote_pending(&quote, 400, 400), 0);
		});
		// The view reports the classification the quote used, while the stored totals still
		// count the capital as pending until an operation commits the activation.
		let view = storage_noop(|| Stability::pool_after_activation(DOT, STABLE))
			.expect("pool registered");
		assert_eq!((view.total_active_deposits, view.total_pending_deposits), (400, 0));
		assert!(view.open_cohorts.is_empty());
		let stored = pool_state(DOT, STABLE);
		assert_eq!((stored.total_active_deposits, stored.total_pending_deposits), (0, 400));

		// The transactional offset commits the same advancement and settles against it:
		// P = 300/400 = 0.75, delta_S = 80 * (1/400) = 0.2.
		let offset = simulate_offset(DOT, STABLE, 100, 80);

		assert_eq!(offset, SimulatedOffset { active: 100, pending: 0, leftover: 0 });
		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 300);
		assert_eq!(state.total_pending_deposits, 0);
		System::assert_has_event(market_event!(CohortActivated {
			cohort: CohortId(0),
			deadline: 10_000,
			amount: 400,
		}));
		// Aggregate activation did not touch the depositor row.
		let row = deposit_row(DOT, STABLE, 1).expect("row exists");
		assert_eq!(row.active_deposit, 0);
		assert_eq!(row.pending_deposit.expect("not yet settled").amount, 400);

		// The row settles lazily: phase one carries the full 400 across the checkpoint, phase
		// two prices the offset it lived through as active capital.
		assert_ok!(settle(7, 1, DOT, STABLE));

		let row = deposit_row(DOT, STABLE, 1).expect("row exists");
		assert_eq!(row.active_deposit, 300);
		assert_eq!(row.claimable_collateral, 80);
		System::assert_has_event(market_event!(PendingDepositActivated {
			depositor: 1,
			amount: 400,
		}));
	});
}

#[test]
fn two_open_cohorts_roll_together_after_an_idle_stretch() {
	ExtBuilder::default()
		.default_branch()
		.stable_balance(1, 300)
		.stable_balance(2, 500)
		.build_and_execute(|| {
			// Window one fills cohort 0 (deadline 10_000). Window two needs a later deadline, so
			// cohort 1 opens at 15_000.
			assert_ok!(deposit(1, DOT, STABLE, 300));
			advance_time(5_000);
			assert_ok!(deposit(2, DOT, STABLE, 500));

			let state = pool_state(DOT, STABLE);
			assert_eq!(state.open_cohorts.len(), 2);
			let older = &state.open_cohorts[0];
			let newer = &state.open_cohorts[1];
			assert_eq!((older.id.0, older.deadline, older.members), (0, 10_000, 1));
			assert_eq!((newer.id.0, newer.deadline, newer.members), (1, 15_000, 1));

			// A long idle stretch passes both deadlines; the backlog is bounded by the two cohorts.
			// One distribution advances both against the same instant and splits over 800:
			// delta_G = 80 / 800 = 0.1.
			advance_time(20_000);
			assert_eq!(distribute_yield_leftover(DOT, STABLE, 80), 0);

			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_active_deposits, 800);
			assert_eq!(state.total_pending_deposits, 0);
			for (cohort, deadline, amount) in [(0, 10_000, 300), (1, 15_000, 500)] {
				System::assert_has_event(market_event!(CohortActivated {
					cohort: CohortId(cohort),
					deadline,
					amount,
				}));
			}

			// Both members realize their post-activation yield, and the last settlement of each
			// cohort prunes its checkpoint.
			assert_ok!(claim_yield(1, DOT, STABLE, 1));
			assert_ok!(claim_yield(2, DOT, STABLE, 2));

			assert_eq!(stable_balance(STABLE, 1), 30);
			assert_eq!(stable_balance(STABLE, 2), 50);
			assert_eq!(crate::CohortCheckpoints::<Test>::iter().count(), 0);
		});
}

#[test]
fn settlement_splits_losses_and_gains_at_the_checkpoint() {
	ExtBuilder::default().default_branch().deposit(1, 400).build_and_execute(|| {
		// Two backstop offsets while pending compose through the accumulators:
		// P_pending = 300/400 = 0.75, delta_S_pending = 50 * (1/400) = 0.125; then
		// P_pending = 0.75 * 240/300 = 0.6, delta_S_pending = 30 * (0.75/300) = 0.075.
		let offset = simulate_offset(DOT, STABLE, 100, 50);
		assert_eq!(offset.debt(), 100);
		let offset = simulate_offset(DOT, STABLE, 60, 30);
		assert_eq!(offset.debt(), 60);

		// Advancement resolves the cohort's units at maturity: floor(400 * 0.6) = 240 moves
		// into the active pool, and the yield distributes over exactly that:
		// delta_G = 60 / 240 = 0.25.
		advance_time(9_000);
		distribute_all_yield(60);
		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 240);
		assert_eq!(state.total_pending_deposits, 0);

		// An active offset after activation: P = 120/240 = 0.5, delta_S = 90 * (1/240) = 0.375.
		assert_eq!(simulate_offset(DOT, STABLE, 120, 90).debt(), 120);

		// One call settles both phases: the pending leg up to the checkpoint (240 survive,
		// 400 * 0.2 = 80 collateral), then the survivor as active capital (120 survive, 90
		// collateral, 60 yield).
		assert_ok!(settle(7, 1, DOT, STABLE));

		let row = deposit_row(DOT, STABLE, 1).expect("row exists");
		assert_eq!(row.active_deposit, 120);
		assert_eq!(row.claimable_collateral, 80 + 90);
		assert_eq!(row.claimable_yield, 60);
		System::assert_has_event(market_event!(PendingDepositActivated {
			depositor: 1,
			amount: 240,
		}));

		// Every tracked total reconciles against the claims.
		assert_claim_collateral(1, 170);
		assert_ok!(claim_yield(1, DOT, STABLE, 1));
		assert_ok!(withdraw(1, DOT, STABLE, 1_000, 1));
		assert_eq!(stable_balance(STABLE, 1), 60 + 120);
		assert!(deposit_row(DOT, STABLE, 1).is_none());
		assert_eq!(crate::CohortCheckpoints::<Test>::iter().count(), 0);
	});
}

#[test]
fn depleted_cohort_activates_nothing_but_keeps_gains_claimable() {
	ExtBuilder::default().default_branch().deposit(1, 400).build_and_execute(|| {
		// The backstop consumes the whole pending stock before maturity:
		// delta_S_pending = 100 * (1/400) = 0.25, then a pending epoch bump.
		let offset = simulate_offset(DOT, STABLE, 400, 100);
		assert_eq!(offset.debt(), 400);
		assert_eq!(pool_state(DOT, STABLE).pending_coords.epoch, 1);

		// The cohort still advances at its deadline — with nothing surviving — so its members
		// can realize the gains earned before the depletion.
		advance_time(9_000);
		assert_ok!(settle(7, 1, DOT, STABLE));

		System::assert_has_event(market_event!(CohortActivated {
			cohort: CohortId(0),
			deadline: 10_000,
			amount: 0,
		}));
		let row = deposit_row(DOT, STABLE, 1).expect("row carries the gain");
		assert_eq!(row.active_deposit, 0);
		assert!(row.pending_deposit.is_none());
		assert_eq!(row.claimable_collateral, 100);

		assert_claim_collateral(1, 100);
		assert!(deposit_row(DOT, STABLE, 1).is_none());
		assert_eq!(crate::CohortCheckpoints::<Test>::iter().count(), 0);
	});
}

#[test]
fn zero_entry_delay_credits_the_active_pool_at_once() {
	ExtBuilder::default()
		.default_branch()
		.pool_config(StabilityPoolConfig { entry_delay: 0, ..default_pool_config() })
		.build_and_execute(|| {
			seed_deposit(1, 400);

			let row = deposit_row(DOT, STABLE, 1).expect("row exists");
			assert_eq!(row.active_deposit, 400);
			assert!(row.pending_deposit.is_none());
			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_active_deposits, 400);
			assert_eq!(state.total_pending_deposits, 0);
			assert!(state.open_cohorts.is_empty());
		});
}

#[test]
fn raising_the_delay_stretches_the_latest_cohort() {
	ExtBuilder::default()
		.default_branch()
		.stable_balance(1, 200)
		.stable_balance(2, 300)
		.stable_balance(3, 400)
		.build_and_execute(|| {
			// Two windows occupy both slots: cohort 0 matures at 10_000, cohort 1 fills at 15_000.
			assert_ok!(deposit(1, DOT, STABLE, 200));
			advance_time(5_000);
			assert_ok!(deposit(2, DOT, STABLE, 300));
			assert_eq!(pending_deadline(DOT, STABLE, 2), Some(15_000));

			// Governance quadruples the delay. The next deposit needs a deadline of 40_000; with
			// the the older cohort still open, the latest cohort stretches rather than
			// opening a third. Its earlier member waits longer, which is the safe
			// direction.
			set_pool_config(StabilityPoolConfig { entry_delay: 20_000, ..default_pool_config() });

			assert_ok!(deposit(3, DOT, STABLE, 400));

			assert_eq!(pending_deadline(DOT, STABLE, 2), Some(40_000));
			assert_eq!(pending_deadline(DOT, STABLE, 3), Some(40_000));
			assert_eq!(pending_deadline(DOT, STABLE, 1), Some(10_000));

			// Cohort 0 advances alone at its own deadline; the stretched cohort follows at its.
			advance_time(4_000);
			assert_ok!(settle(7, 1, DOT, STABLE));
			assert_eq!(pool_state(DOT, STABLE).total_active_deposits, 200);
			advance_time(30_000);
			assert_ok!(settle(7, 2, DOT, STABLE));
			assert_ok!(settle(7, 3, DOT, STABLE));
			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_active_deposits, 900);
			assert_eq!(state.total_pending_deposits, 0);
		});
}

#[test]
fn lowering_the_delay_joins_the_standing_cohort() {
	ExtBuilder::default()
		.default_branch()
		.deposit(1, 200)
		.stable_balance(2, 300)
		.build_and_execute(|| {
			// With the delay lowered, the new deposit's requirement (4_000) is already covered by
			// the standing cohort: it joins and waits until 10_000 — longer than newly required,
			// never shorter.
			set_pool_config(StabilityPoolConfig { entry_delay: 2_000, ..default_pool_config() });

			assert_ok!(deposit(2, DOT, STABLE, 300));

			assert_eq!(pending_deadline(DOT, STABLE, 1), Some(10_000));
			assert_eq!(pending_deadline(DOT, STABLE, 2), Some(10_000));

			advance_time(9_000);
			distribute_all_yield(50);

			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_active_deposits, 500);
			assert_eq!(state.total_pending_deposits, 0);
		});
}

#[test]
fn freeze_halts_advancement_but_settles_committed_checkpoints() {
	ExtBuilder::branch_with_debt().deposit(2, 300).build_and_execute(|| {
		// The cohort advances while the branch is healthy; the row stays unsettled.
		advance_time(9_000);
		distribute_all_yield(70);
		assert_eq!(pool_state(DOT, STABLE).total_active_deposits, 700);

		// A later cohort matures during the freeze and stays pending until the branch thaws.
		seed_deposit(3, 200);
		assert_eq!(pending_deadline(DOT, STABLE, 3), Some(25_000));

		// A frozen branch cannot advance cohorts, but settling a row through a checkpoint that
		// was committed before the freeze is pure bookkeeping and stays available.
		MockOracleAvailable::set(&false);
		advance_time(10_000);
		assert_ok!(settle(7, 2, DOT, STABLE));

		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 700);
		assert_eq!(state.total_pending_deposits, 200);
		assert_eq!(pending_deadline(DOT, STABLE, 3), Some(25_000));
		let row = deposit_row(DOT, STABLE, 2).expect("row exists");
		assert_eq!(row.active_deposit, 300);
		assert!(row.pending_deposit.is_none());
		// Yield distributed after this deposit's activation: floor(300 * (70 * (1/700))) = 30.
		assert_eq!(row.claimable_yield, 30);
		// Paying it out stays frozen.
		assert_noop!(claim_yield(2, DOT, STABLE, 2), Error::<Test>::BranchFrozen);
		MockOracleAvailable::set(&true);
		assert_ok!(claim_yield(2, DOT, STABLE, 2));

		// The thaw lets the first operation activate the overdue cohort.
		let state = pool_state(DOT, STABLE);
		assert_eq!(state.total_active_deposits, 900);
		assert_eq!(state.total_pending_deposits, 0);
	});
}

#[test]
fn aggregate_tracks_an_epoch_bump_inside_one_cohort() {
	ExtBuilder::default()
		.default_branch()
		.deposit(1, 300)
		.stable_balance(2, 500)
		.build_and_execute(|| {
			// User 1 joins at pending epoch 0; the backstop then consumes the whole stock, bumping
			// the pending epoch: delta_S_pending = 90 * (1/300) = 0.3 before the bump.
			let offset = simulate_offset(DOT, STABLE, 300, 90);
			assert_eq!(offset.debt(), 300);

			// User 2 joins the same cohort in the same window, at epoch 1. The join revalues the
			// aggregate first: user 1's consumed capital compounds to zero across the epoch, so the
			// cohort carries the new joiner alone.
			assert_ok!(deposit(2, DOT, STABLE, 500));

			let state = pool_state(DOT, STABLE);
			let cohort = state.open_cohorts.first().expect("cohort open");
			assert_eq!(cohort.members, 2);
			assert_eq!(cohort.amount, 500);
			assert_eq!(cohort.coords.epoch, 1);

			// At maturity the revalued aggregate activates as it stands.
			advance_time(9_000);
			assert_ok!(settle(7, 2, DOT, STABLE));

			System::assert_has_event(market_event!(CohortActivated {
				cohort: CohortId(0),
				deadline: 10_000,
				amount: 500,
			}));
			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_active_deposits, 500);
			assert_eq!(state.total_pending_deposits, 0);
			assert_eq!(deposit_row(DOT, STABLE, 2).expect("row exists").active_deposit, 500);

			// User 1's capital is gone with the depletion, but the collateral it earned before the
			// bump survives: floor(300 * 0.3) = 90. Paying it out prunes the row and, as the last
			// member, the checkpoint.
			assert_ok!(settle(7, 1, DOT, STABLE));

			let row = deposit_row(DOT, STABLE, 1).expect("row carries the gain");
			assert_eq!(row.active_deposit, 0);
			assert!(row.pending_deposit.is_none());
			assert_eq!(row.claimable_collateral, 90);
			assert_claim_collateral(1, 90);
			assert!(deposit_row(DOT, STABLE, 1).is_none());
			assert_eq!(crate::CohortCheckpoints::<Test>::iter().count(), 0);
		});
}

#[test]
fn pending_scale_crossing_reprices_the_aggregate_through_the_divisor() {
	let unit: Balance = 10_000_000_000_000; // 1e13
	ExtBuilder::default()
		.default_branch()
		.min_active_pool(10)
		.deposit(1, unit)
		.stable_balance(2, 200)
		.build_and_execute(|| {
			// The backstop burns all but 100 of the 1e13 pending stock: the survival ratio 1e-11
			// forces one rescale, P_pending = 0.01 at scale 1 — as on the active side. The
			// collateral lands in the row of the pre-crossing scale: delta_S_pending = 1e12 / 1e13.
			let offset = simulate_offset(DOT, STABLE, unit - 100, 1_000_000_000_000);
			assert_eq!(offset.debt(), unit - 100);
			let state = pool_state(DOT, STABLE);
			assert_eq!(state.pending_coords.scale, 1);
			assert_eq!(state.pending_coords.p, FixedU128::from_rational(1, 100));
			let pending_sums = |scale| pending_sums(0, scale);
			assert_eq!(pending_sums(0).s_collateral, FixedU128::from_rational(1, 10));
			assert_eq!(pending_sums(1).s_collateral, FixedU128::zero());

			// User 2 joins the same cohort one scale later: the join revalues the aggregate through
			// the crossing's divisor — ceil(1e13 * 0.01 / 1e9) = 100 — and adds the 200 on top.
			assert_ok!(deposit(2, DOT, STABLE, 200));

			let state = pool_state(DOT, STABLE);
			let cohort = state.open_cohorts.first().expect("cohort open");
			assert_eq!(cohort.amount, 300);
			assert_eq!(cohort.coords.scale, 1);

			// Advancement activates the revalued aggregate; the member floors settle to the same
			// values: floor(1e13 * 0.01 / 1e9) = 100 and floor(200 * 0.01 / 0.01) = 200.
			advance_time(9_000);
			assert_ok!(settle(7, 1, DOT, STABLE));
			assert_ok!(settle(7, 2, DOT, STABLE));

			let state = pool_state(DOT, STABLE);
			assert_eq!(state.total_active_deposits, 300);
			assert_eq!(state.total_pending_deposits, 0);
			let row = deposit_row(DOT, STABLE, 1).expect("row exists");
			assert_eq!(row.active_deposit, 100);
			// User 1 alone held the stock when the collateral was paid: floor(1e13 * 0.1) = 1e12.
			assert_eq!(row.claimable_collateral, 1_000_000_000_000);
			let row = deposit_row(DOT, STABLE, 2).expect("row exists");
			assert_eq!(row.active_deposit, 200);
			assert_eq!(row.claimable_collateral, 0);
		});
}
