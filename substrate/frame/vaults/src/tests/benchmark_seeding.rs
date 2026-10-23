//! The [`BenchmarkVaults`] contract the Stability Pool and redemptions benchmarks seed through.

use crate::{
	benchmarking::fixtures::redistribution_held, mock::*, pallet::Vaults, types::VaultStatus,
};
use frame::{arithmetic::One, traits::fungibles::Balanced};
use stablecoin_primitives::{
	BenchmarkVaults, CollateralRatio, RedemptionSettlement, VaultInterface,
};

type Seeder = Pallet<Test>;

fn register() -> (AssetId, StableId) {
	let market = Seeder::register_market().expect("benchmark branch registers");
	assert_eq!(Seeder::register_market(), Ok(market.clone()), "registration is idempotent");
	market
}

#[test]
fn redemption_targets_are_walked_in_seed_order_with_pending_shares() {
	ExtBuilder::default().build_and_execute(|| {
		let (collateral, stable) = register();

		let seed = Seeder::seed_redemption_targets(3, 3_600_000).expect("targets seed");

		assert_eq!(seed.owners.len(), 3);
		let mut walked = Vec::new();
		let mut cursor = None;
		while let Some((owner, status)) =
			Seeder::next_redemption_target(&collateral, &stable, cursor.as_ref())
		{
			assert_eq!(status, VaultStatus::Active);
			walked.push(owner);
			cursor = walked.last().cloned();
		}
		assert_eq!(walked, seed.owners);
		let snapshot_debt: Balance = seed
			.owners
			.iter()
			.map(|owner| {
				let stored = Vaults::<Test>::get((&collateral, stable, owner)).unwrap().vault;
				let projected =
					Seeder::project_redemption_snapshot(&collateral, &stable, owner).unwrap();
				// The share is still pending: projection adds it, the stored row does not.
				assert!(projected.debt > stored.debt.total());
				projected.debt
			})
			.sum();
		assert_eq!(seed.total_debt, snapshot_debt);
		assert!(redistribution_held::<Test>() > 0);
	});
}

#[test]
fn recovery_head_settles_in_full_and_closes() {
	ExtBuilder::default().build_and_execute(|| {
		let (collateral, stable) = register();

		let head = Seeder::stage_recovery_head(3_600_000).expect("head stages");

		let target = Seeder::next_redemption_target(&collateral, &stable, None);
		assert_eq!(target, Some((head.owner, VaultStatus::FinalRecovery)));
		let snapshot =
			Seeder::project_redemption_snapshot(&collateral, &stable, &head.owner).unwrap();
		assert_eq!(snapshot.debt, head.debt);
		assert_eq!(snapshot.collateral, head.collateral);
		assert_eq!(
			stablecoin_primitives::collateralization_ratio(&snapshot.position(), head.price),
			Ok(CollateralRatio::Ratio(FixedU128::one()))
		);

		let payment = <VaultStableAssets as Balanced<AccountId>>::issue(stable, head.debt);
		assert_ok!(Seeder::redeem_step(
			&collateral,
			&stable,
			&head.owner,
			&SP_ACCOUNT,
			RedemptionSettlement {
				debt_payment: payment,
				collateral_to_recipient: head.collateral
			},
		));

		assert!(!Vaults::<Test>::contains_key((&collateral, stable, head.owner)));
	});
}
