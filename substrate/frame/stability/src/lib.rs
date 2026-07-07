//! # Stability Pool Pallet

#![cfg_attr(not(feature = "std"), no_std)]

pub mod weights;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub use pallet::*;
pub use weights::WeightInfo;

#[frame::pallet]
pub mod pallet {
	use super::*;
	use frame::prelude::*;

	pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type WeightInfo: WeightInfo;
	}

	#[pallet::event]
	pub enum Event<T: Config> {}
}
