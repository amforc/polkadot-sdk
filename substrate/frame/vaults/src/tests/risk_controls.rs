//! Shared-collateral risk controls: the per-collateral global debt ceiling and
//! the per-market autoline.

use crate::{mock::*, tests::rate_pct};
use frame::traits::fungibles::Mutate;

const DAY_MS: Moment = 24 * 3_600 * 1_000;

fn effective_ceiling(collateral: AssetId, stable: StableId) -> Balance {
	branch_state(collateral, stable).unwrap().effective_ceiling
}

/// Register a market with the autoline enabled (`gap`/`ttl`), priced at 10.
/// `line_max` is the static `debt_ceiling` cap.
fn register_autoline_market(
	collateral: AssetId,
	stable: StableId,
	line_max: Balance,
	gap: Balance,
	ttl: Moment,
) {
	register_market_with(
		collateral,
		stable,
		FixedU128::from_rational(10u128, 1u128),
		BranchConfig {
			debt_ceiling: line_max,
			ceiling_gap: gap,
			ceiling_ttl: ttl,
			..default_branch_config()
		},
	);
}

// A collateral whose global ceiling is the default `0` cannot be borrowed
// against, even though a market on it can be registered.
#[test]
fn collateral_with_zero_ceiling_cannot_be_borrowed() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Governance pins the global ceiling back to 0 (allow-list off).
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), DOT, 0));
		assert_noop!(
			open(1, DOT, PUSD, 10_000, 2_000, rate_pct(5, 100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
	});
}

// A borrow that fits the per-branch ceiling but pushes the collateral's summed
// debt past `GlobalDebtCeiling[C]` is rejected; a smaller one that fits succeeds.
#[test]
fn borrow_breaching_global_ceiling_is_rejected() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// DOT priced at 10, ceiling 250 DOT => 2_500 PUSD of total principal.
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), DOT, 250));

		// 2_000 PUSD == 200 DOT, fits.
		assert_ok!(open(1, DOT, PUSD, 100_000, 2_000, rate_pct(5, 100)));
		// Another 1_000 PUSD == 100 DOT => 300 DOT total, breaches.
		assert_noop!(
			open(2, DOT, PUSD, 100_000, 1_000, rate_pct(5, 100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
		// A 400 PUSD borrow == 40 DOT keeps the total at 240 DOT, fits.
		assert_ok!(open(2, DOT, PUSD, 100_000, 400, rate_pct(5, 100)));
	});
}

// Markets on the same collateral share its ceiling: the second market's borrow
// is capped by the first market's debt.
#[test]
fn global_ceiling_is_shared_across_a_collaterals_markets() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		register_market(DOT, EUSD);
		// The ceiling caps every market's total stable debt (principal plus accrued
		// interest), valued in DOT; 260 leaves headroom above the ~200 DOT first
		// borrow for its upfront-fee interest.
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), DOT, 260));

		// ~200 DOT of debt on the first market (2_000 PUSD plus a small upfront fee).
		assert_ok!(open(1, DOT, PUSD, 100_000, 2_000, rate_pct(5, 100)));
		// 1_000 EUSD == 100 DOT on the second would take the shared total past 260 DOT.
		assert_noop!(
			open(2, DOT, EUSD, 100_000, 1_000, rate_pct(5, 100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
		// 500 EUSD == 50 DOT fits the remaining shared headroom.
		assert_ok!(open(2, DOT, EUSD, 100_000, 500, rate_pct(5, 100)));
	});
}

// The global ceiling caps a market's total outstanding stable debt — principal
// plus minted interest, pending redistribution, and socialized bad debt — not
// principal alone.
#[test]
fn global_ceiling_counts_all_outstanding_debt() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		// Ceiling 200 DOT; 1_500 PUSD (150 DOT) of principal fits with headroom.
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), DOT, 200));
		// Seed 600 PUSD (== 60 DOT) of non-principal debt on the branch.
		mutate_branch_state(DOT, PUSD, |state| {
			state.debt.minted_interest = 200;
			state.debt.pending_redistribution_principal = 200;
			state.debt.bad_debt = 200;
		});
		// 150 DOT principal + 60 DOT non-principal debt == 210 DOT > the 200 DOT cap.
		assert_noop!(
			open(1, DOT, PUSD, 100_000, 1_500, rate_pct(1, 1_000)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
		// Clearing the non-principal debt frees the headroom; the same borrow lands.
		mutate_branch_state(DOT, PUSD, |state| {
			state.debt.minted_interest = 0;
			state.debt.pending_redistribution_principal = 0;
			state.debt.bad_debt = 0;
		});
		assert_ok!(open(1, DOT, PUSD, 100_000, 1_500, rate_pct(1, 1_000)));
	});
}

// Repaying lowers the derived collateral debt, freeing global-ceiling headroom.
#[test]
fn repaying_frees_global_ceiling_headroom() {
	build_and_execute(|| {
		register_market(DOT, PUSD);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), DOT, 250));
		assert_ok!(open(1, DOT, PUSD, 100_000, 2_000, rate_pct(5, 100))); // 200 DOT
		assert_noop!(
			open(2, DOT, PUSD, 100_000, 1_000, rate_pct(5, 100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);

		// Repay most of owner 1's debt, dropping the collateral debt well below the cap.
		<VaultStableAssets as Mutate<AccountId>>::mint_into(PUSD, &1, 10_000).unwrap();
		assert_ok!(Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, 1_500));

		// The freed headroom now admits the previously-rejected borrow.
		assert_ok!(open(2, DOT, PUSD, 100_000, 1_000, rate_pct(5, 100)));
	});
}

// Autoline: the effective ceiling starts at `min(gap, line_max)` and rises by a
// gap only once `ttl` has elapsed.
#[test]
fn autoline_rises_by_gap_only_after_ttl() {
	build_and_execute(|| {
		register_autoline_market(DOT, PUSD, 10_000, 1_000, DAY_MS);
		// Initial line is min(gap, line_max) == 1_000.
		assert_eq!(effective_ceiling(DOT, PUSD), 1_000);
		assert_ok!(open(1, DOT, PUSD, 100_000, 1_000, rate_pct(5, 100)));

		// The line binds: no more can be borrowed yet.
		assert_noop!(
			open(2, DOT, PUSD, 100_000, 500, rate_pct(5, 100)),
			Error::<Test>::DebtCeilingExceeded
		);
		// Poking before `ttl` does nothing.
		assert_ok!(Pallet::<Test>::poke_ceiling(RuntimeOrigin::signed(9), DOT, PUSD));
		assert_eq!(effective_ceiling(DOT, PUSD), 1_000);

		// After `ttl`, a poke raises it by one gap to min(debt + gap, line_max) == 2_000.
		advance_time(DAY_MS);
		assert_ok!(Pallet::<Test>::poke_ceiling(RuntimeOrigin::signed(9), DOT, PUSD));
		assert_eq!(effective_ceiling(DOT, PUSD), 2_000);
		// The headroom is now usable.
		assert_ok!(open(2, DOT, PUSD, 100_000, 1_000, rate_pct(5, 100)));
	});
}

// Autoline decreases apply immediately — no `ttl` wait when debt falls.
#[test]
fn autoline_falls_instantly() {
	build_and_execute(|| {
		register_autoline_market(DOT, PUSD, 10_000, 1_000, DAY_MS);
		assert_ok!(open(1, DOT, PUSD, 100_000, 1_000, rate_pct(5, 100)));
		advance_time(DAY_MS);
		assert_ok!(Pallet::<Test>::poke_ceiling(RuntimeOrigin::signed(9), DOT, PUSD));
		assert_eq!(effective_ceiling(DOT, PUSD), 2_000);

		// Repay down to ~300 principal, then poke immediately (no time passes).
		<VaultStableAssets as Mutate<AccountId>>::mint_into(PUSD, &1, 10_000).unwrap();
		assert_ok!(Pallet::<Test>::repay_for(RuntimeOrigin::signed(1), DOT, PUSD, 1, 700));
		assert_ok!(Pallet::<Test>::poke_ceiling(RuntimeOrigin::signed(9), DOT, PUSD));

		// The decrease landed instantly (no `ttl` wait): min(post-repay principal 301 +
		// gap 1_000, line_max 10_000) = 1_301, down from 2_000.
		assert_eq!(effective_ceiling(DOT, PUSD), 1_301, "ceiling fell instantly on repay");
		// The lowered line now binds: a new vault whose debt would push branch principal
		// (301) past 1_301 is rejected — though it would have fit under the old 2_000.
		assert_noop!(
			open(2, DOT, PUSD, 100_000, 1_100, rate_pct(5, 100)),
			Error::<Test>::DebtCeilingExceeded
		);
	});
}

// A frozen market's autoline ceiling never rises, even after `ttl`.
#[test]
fn autoline_does_not_rise_while_frozen() {
	build_and_execute(|| {
		register_autoline_market(DOT, PUSD, 10_000, 1_000, DAY_MS);
		assert_ok!(open(1, DOT, PUSD, 100_000, 1_000, rate_pct(5, 100)));
		assert_ok!(Pallet::<Test>::enable_frozen_mode(RuntimeOrigin::signed(ADMIN), DOT, PUSD));

		advance_time(DAY_MS);
		assert_ok!(Pallet::<Test>::poke_ceiling(RuntimeOrigin::signed(9), DOT, PUSD));
		assert_eq!(effective_ceiling(DOT, PUSD), 1_000, "frozen market's ceiling stayed pinned");
	});
}

// No autoline expansion can let the collateral's summed debt breach its global
// ceiling: a borrow that the (huge) per-branch line admits is still rejected.
#[test]
fn autoline_expansion_cannot_exceed_global_ceiling() {
	build_and_execute(|| {
		// A line and gap far above the global ceiling.
		register_autoline_market(DOT, PUSD, 1_000_000, 1_000_000, DAY_MS);
		assert_ok!(Pallet::<Test>::set_global_debt_ceiling(RuntimeOrigin::root(), DOT, 250));

		// 2_000 PUSD == 200 DOT fits.
		assert_ok!(open(1, DOT, PUSD, 100_000, 2_000, rate_pct(5, 100)));
		// The autoline line (1_000_000) clears this, but the global cap (250 DOT) does not.
		assert_noop!(
			open(2, DOT, PUSD, 100_000, 1_000, rate_pct(5, 100)),
			Error::<Test>::GlobalDebtCeilingExceeded
		);
	});
}
