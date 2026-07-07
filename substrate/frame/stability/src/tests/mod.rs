mod activation;
mod claims;
mod deposit;
mod epoch_scale;
mod governance;
mod mode;
mod offsets;
mod pending_offsets;
mod recovery;
mod withdrawals;
mod yield_distribution;

use crate::{mock::*, pallet::STORAGE_VERSION};
use frame::traits::GetStorageVersion;

#[test]
fn genesis_writes_declared_storage_version() {
	new_test_ext().execute_with(|| {
		assert_eq!(Stability::in_code_storage_version(), STORAGE_VERSION);
		assert_eq!(Stability::on_chain_storage_version(), STORAGE_VERSION);
	});
}
