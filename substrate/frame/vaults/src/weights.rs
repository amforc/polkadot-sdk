//! Placeholder weight info. Real benchmarks land in a follow-up.

use frame::prelude::Weight;

pub trait WeightInfo {
	fn open_vault() -> Weight;
	fn deposit_collateral_for() -> Weight;
	fn withdraw_collateral() -> Weight;
	fn borrow() -> Weight;
	fn repay_for() -> Weight;
	fn change_rate() -> Weight;
	fn close_vault() -> Weight;
	fn poke() -> Weight;
	fn enter_final_recovery() -> Weight;
	fn exit_final_recovery() -> Weight;
	fn activate_dormant() -> Weight;
	fn create_branch() -> Weight;
	fn remove_branch() -> Weight;
	fn set_param() -> Weight;
	fn set_branch_admins() -> Weight;
	fn set_global_debt_ceiling() -> Weight;
	fn set_governance_frozen() -> Weight;
	fn refresh_branch() -> Weight;
	fn on_idle_base() -> Weight;
	fn on_idle_one_branch() -> Weight;
	fn on_idle_one_vault() -> Weight;
}

impl WeightInfo for () {
	fn open_vault() -> Weight {
		Weight::zero()
	}
	fn deposit_collateral_for() -> Weight {
		Weight::zero()
	}
	fn withdraw_collateral() -> Weight {
		Weight::zero()
	}
	fn borrow() -> Weight {
		Weight::zero()
	}
	fn repay_for() -> Weight {
		Weight::zero()
	}
	fn change_rate() -> Weight {
		Weight::zero()
	}
	fn close_vault() -> Weight {
		Weight::zero()
	}
	fn poke() -> Weight {
		Weight::zero()
	}
	fn enter_final_recovery() -> Weight {
		Weight::zero()
	}
	fn exit_final_recovery() -> Weight {
		Weight::zero()
	}
	fn activate_dormant() -> Weight {
		Weight::zero()
	}
	fn create_branch() -> Weight {
		Weight::zero()
	}
	fn remove_branch() -> Weight {
		Weight::zero()
	}
	fn set_param() -> Weight {
		Weight::zero()
	}
	fn set_branch_admins() -> Weight {
		Weight::zero()
	}
	fn set_global_debt_ceiling() -> Weight {
		Weight::zero()
	}
	fn set_governance_frozen() -> Weight {
		Weight::zero()
	}
	fn refresh_branch() -> Weight {
		Weight::zero()
	}
	fn on_idle_base() -> Weight {
		Weight::from_parts(1, 1)
	}
	fn on_idle_one_branch() -> Weight {
		Weight::from_parts(3, 3)
	}
	fn on_idle_one_vault() -> Weight {
		Weight::from_parts(10, 10)
	}
}
