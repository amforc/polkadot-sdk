//! Verifies `try_state` invariants under `try-runtime` and after each test.

use crate::pallet::{Config, Stablecoins};
use frame::{deps::sp_runtime::TryRuntimeError, prelude::ensure, traits::Time};
use stablecoin_primitives::VaultInterface;

pub fn do_try_state<T: Config>() -> Result<(), TryRuntimeError> {
	let now = T::TimeProvider::now();
	let mut rows: usize = 0;
	for (stable_id, info) in Stablecoins::<T>::iter() {
		// Every write path validates before inserting. A stored invalid config means a path
		// skipped the shared validation.
		ensure!(info.config.is_valid(), "stored redemption config fails `is_valid`");
		// The row is seeded by the coin's first branch and cleared with its last, so it must
		// not outlive the vault engine's count of the branches it governs.
		ensure!(
			T::Vaults::stablecoin_markets(&stable_id) > 0,
			"redemption row for a stablecoin with no live branch"
		);
		// Config updates can move the bounds past the last redemption's fee. Reads apply the
		// current bounds after decay; the historical fee remains the decay anchor.
		ensure!(info.fee_state.last_fee_operation <= now, "`last_fee_operation` is ahead of now");
		rows += 1;
	}
	// Each row above is a distinct issued stablecoin, so equal counts leave no issued stablecoin
	// without a row, which would be a branch redemptions cannot price. The vault engine's own
	// `try_state` owns the count's agreement with its branch registry.
	ensure!(
		rows == T::Vaults::issued_stablecoin_count(),
		"redemption rows diverge from the vault engine's issued stablecoins"
	);
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::do_try_state;
	use crate::{mock::*, RedemptionInfo, Stablecoins};
	use frame::{
		arithmetic::{FixedU128, Zero},
		deps::sp_runtime::DispatchError,
		testing_prelude::hypothetically,
	};

	// Each corruption that no write path can produce must trip its own check, with the message
	// that check reports.

	/// Corrupts a sound default branch and asserts that `try_state` reports `message`.
	///
	/// The corruption is rolled back so the post-test invariant check still passes.
	fn assert_detects(message: &'static str, corrupt: impl FnOnce()) {
		ExtBuilder::default().default_branch().build_and_execute(|| {
			assert_eq!(do_try_state::<Test>(), Ok(()));
			hypothetically!({
				corrupt();

				assert_eq!(do_try_state::<Test>(), Err(DispatchError::Other(message)));
			});
		});
	}

	#[test]
	fn detects_a_row_for_a_coin_no_branch_issues() {
		assert_detects("redemption row for a stablecoin with no live branch", || {
			Stablecoins::<Test>::insert(
				USDX,
				RedemptionInfo {
					config: default_redemption_config(),
					fee_state: Default::default(),
				},
			)
		});
	}

	#[test]
	fn detects_an_issued_coin_without_its_row() {
		assert_detects(
			"redemption rows diverge from the vault engine's issued stablecoins",
			|| Stablecoins::<Test>::remove(STABLE),
		);
	}

	/// A stored config that `set_redemption_config` would have refused.
	#[test]
	fn detects_a_stored_invalid_config() {
		assert_detects("stored redemption config fails `is_valid`", || {
			Stablecoins::<Test>::mutate(STABLE, |info| {
				info.as_mut().expect("STABLE is registered").config.dynamic_fee_ceiling =
					FixedU128::zero();
				info.as_mut().expect("STABLE is registered").config.dynamic_fee_floor =
					FixedU128::from_rational(1, 100);
			})
		});
	}

	#[test]
	fn detects_a_fee_operation_stamped_after_now() {
		assert_detects("`last_fee_operation` is ahead of now", || {
			Stablecoins::<Test>::mutate(STABLE, |info| {
				info.as_mut().expect("STABLE is registered").fee_state.last_fee_operation =
					Timestamp::get() + 1;
			})
		});
	}
}
