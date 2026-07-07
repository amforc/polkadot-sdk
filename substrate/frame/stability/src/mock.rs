//! Test runtime for `pallet-stability`.

use crate as pallet_stability;
use frame::testing_prelude::*;

pub type Block = MockBlock<Test>;

#[frame_construct_runtime]
mod runtime {
	#[runtime::runtime]
	#[runtime::derive(RuntimeCall, RuntimeEvent, RuntimeError, RuntimeOrigin, RuntimeTask)]
	pub struct Test;

	#[runtime::pallet_index(0)]
	pub type System = frame_system;

	#[runtime::pallet_index(1)]
	pub type Stability = pallet_stability;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
}

impl pallet_stability::Config for Test {
	type WeightInfo = ();
}

pub fn new_test_ext() -> TestState {
	let t = RuntimeGenesisConfig::default().build_storage().unwrap();
	let mut ext: TestState = t.into();
	ext.execute_with(|| System::set_block_number(1));
	ext
}
