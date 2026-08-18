//! The metered `on_idle` walk: cursorized branch reconciliation,
//  the flat vault refresh, and the weight accounting for
//! both plus the cursor overhead.

use crate::{
	mock::*,
	pallet::{BranchIdleCursor, Branches, IdleCursor, Vaults},
	tests::rate_pct,
	weights::WeightInfo,
	Config,
};
use frame::prelude::Weight;

fn base() -> Weight {
	<Test as Config>::WeightInfo::on_idle_base()
}

fn per_branch() -> Weight {
	<Test as Config>::WeightInfo::on_idle_one_branch()
}

fn per_vault() -> Weight {
	<Test as Config>::WeightInfo::on_idle_one_vault()
}

fn frozen_branch_count() -> usize {
	Branches::<Test>::iter()
		.filter(|(_, _, branch)| branch.state.is_frozen())
		.count()
}

// The full walk: pass one charges the base, drains the one-branch registry
// (clearing its cursor), refreshes four of five vaults, and parks the vault
// cursor; a pass too small for a vault still reconciles the branch and leaves
// the parked cursor untouched; the final pass finishes the map and wraps by
// clearing the cursor. Every reported weight is the exact sum of base, branch,
// and vault attempts.
#[test]
fn on_idle_walk_budgets_touches_and_wraps_the_cursor() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		for owner in 1..=5u64 {
			assert_ok!(open(owner, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		}
		advance_time(365 * 24 * 3_600 * 1_000);

		// Base + one branch + four of the five vaults; the branch walk's half
		// share ((44 − 1) / 2 = 21) comfortably covers the single branch.
		let budget = base()
			.saturating_add(per_branch())
			.saturating_add(per_vault().saturating_mul(4));
		let spent = crate::Pallet::<Test>::on_idle_walk(budget);
		assert_eq!(spent, budget, "first pass consumes the whole meter");
		assert!(
			BranchIdleCursor::<Test>::get().is_none(),
			"draining the one-branch registry cleared its cursor"
		);
		assert!(
			IdleCursor::<Test>::get().is_some(),
			"meter drained mid-map parks the vault cursor"
		);

		// Base + twice the branch weight: the half share affords exactly one
		// branch refresh, and the leftover cannot fit a vault.
		let starved = base().saturating_add(per_branch().saturating_mul(2));
		let spent = crate::Pallet::<Test>::on_idle_walk(starved);
		assert_eq!(
			spent,
			base().saturating_add(per_branch()),
			"starved pass still reconciles the branch"
		);
		assert!(
			IdleCursor::<Test>::get().is_some(),
			"starved pass leaves the parked vault cursor untouched"
		);

		let spent = crate::Pallet::<Test>::on_idle_walk(budget);
		assert_eq!(
			spent,
			base().saturating_add(per_branch()).saturating_add(per_vault()),
			"final pass: base, one branch, the one remaining vault"
		);
		assert!(
			IdleCursor::<Test>::get().is_none(),
			"draining the map wraps by clearing the cursor"
		);
		for owner in 1..=5u64 {
			let vault = Vaults::<Test>::get((DOT, PUSD, owner)).expect("vault stored");
			assert!(vault.debt.interest > 0, "every vault was touched across the two passes");
		}
	});
}

// Branch spam cannot starve vault maintenance: with ten registered markets and
// a budget whose half share covers only four branch refreshes, the vault walk
// still runs on the reserved remainder.
#[test]
fn branch_walk_cannot_starve_the_vault_walk() {
	build_and_execute(|| {
		register_ten_markets();
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(
			RuntimeOrigin::root(),
			PUSD,
			GLOBAL_CEILING
		));
		assert_ok!(open(1, DOT, PUSD, 1_000, 500, rate_pct(5, 100)));
		advance_time(365 * 24 * 3_600 * 1_000);

		// Base + eight branch weights: the branch walk's half share (8 / 2 =
		// 4 branch weights) affords exactly four refreshes, and the reserved
		// remainder (4 branch weights = 12) still fits the one vault at 10.
		let budget = base().saturating_add(per_branch().saturating_mul(8));
		assert!(
			per_vault().all_lte(per_branch().saturating_mul(4)),
			"the reserved vault share must fit one vault refresh"
		);
		let spent = crate::Pallet::<Test>::on_idle_walk(budget);
		assert_eq!(
			spent,
			base()
				.saturating_add(per_branch().saturating_mul(4))
				.saturating_add(per_vault()),
			"four branch attempts, then the vault on the reserved share"
		);
		assert!(
			BranchIdleCursor::<Test>::get().is_some(),
			"branch walk parked mid-registry at its half share"
		);
		let vault = Vaults::<Test>::get((DOT, PUSD, 1)).expect("vault stored");
		assert!(vault.debt.interest > 0, "the vault walk was not starved");
	});
}

// The branch cursor advances across passes, visits every market exactly once
// per revolution, and wraps by clearing itself when the registry drains.
// Observability: with the oracle down, each visited branch freezes.
#[test]
fn branch_cursor_advances_drains_and_wraps() {
	build_and_execute(|| {
		register_ten_markets();
		MockOracleAvailable::set(false);

		// Budget 25: half of (25 − 1) affords four branch refreshes per pass.
		let budget = base().saturating_add(per_branch().saturating_mul(8));
		crate::Pallet::<Test>::on_idle_walk(budget);
		assert_eq!(frozen_branch_count(), 4);
		assert!(BranchIdleCursor::<Test>::get().is_some());

		crate::Pallet::<Test>::on_idle_walk(budget);
		assert_eq!(frozen_branch_count(), 8, "second pass resumes without revisiting");
		assert!(BranchIdleCursor::<Test>::get().is_some());

		let spent = crate::Pallet::<Test>::on_idle_walk(budget);
		assert_eq!(frozen_branch_count(), 10, "third pass finishes the registry");
		assert!(
			BranchIdleCursor::<Test>::get().is_none(),
			"draining the registry wraps by clearing the cursor"
		);
		assert_eq!(
			spent,
			base().saturating_add(per_branch().saturating_mul(2)),
			"the tail pass charges only its two attempts plus the base"
		);
	});
}

// A budget below the walk's base cost performs no work and reports zero: no
// storage is read or written uncharged.
#[test]
fn insufficient_weight_performs_no_uncharged_work() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		MockOracleAvailable::set(false);

		assert_eq!(crate::Pallet::<Test>::on_idle_walk(Weight::zero()), Weight::zero());
		assert_eq!(frozen_branch_count(), 0, "no branch was reconciled");
		assert!(BranchIdleCursor::<Test>::get().is_none());
		assert!(IdleCursor::<Test>::get().is_none());
	});
}
