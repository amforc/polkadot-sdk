//! Branch-mode types.

use crate::Millis;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::TypeInfo;

/// Branch operating mode. `Normal` and `Safety` are derived from live TCR;
/// `Frozen` is the only persisted mode.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
)]
pub enum BranchMode {
	Normal,
	Safety,
	Frozen,
}

/// Reason the branch was put into `Frozen` mode.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
)]
pub enum FrozenReason {
	OracleFailure,
	Governance,
}

/// Stored `Frozen` state attached to `BranchState` while frozen.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Debug,
)]
pub struct FrozenState {
	pub reason: FrozenReason,
	pub entered_at: Millis,
}
