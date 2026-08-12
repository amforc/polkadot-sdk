# Lazy redistribution accounting

Liquidation must have a bounded cost. Therefore, a market records each residual without a walk of
all vaults. Each eligible vault receives its share at its next touch.

## Ownership and custody

The market records all value that does not yet have a vault owner:

- `pending_redistribution_principal` is part of market and stablecoin debt.
- `pending_redistribution_collateral` stays in the market redistribution account.
- `pending_redistribution_weight` accrues interest at recipient rates from the liquidation time.

These pending pools are liabilities and custody. They are not rounding error. `try_state` checks
them against vault rows, market totals, debt indexes, and the redistribution hold.

## Allocation invariants

A recipient accrues interest at its own rate from the liquidation time. A delayed touch must not
change this interest.

Snapshot-corrected stake prevents collateral touch order from changing the next allocation. A
vault opened after a liquidation cannot claim that liquidation's normal per-stake share.

A debt-bearing vault has at least one stake unit. This keeps the vault eligible for redistribution
and liquidation when the corrected stake calculation floors to zero.

The last stake bearer receives the exact pending-pool complement. This drains all pending
principal, collateral, weight, and interest-time value.

## Precision policy

A multi-recipient allocation can leave a share below accumulator resolution. The pending pools
retain this value until it becomes distributable or only one stake bearer remains.

The final bearer can receive residue from a liquidation that occurred before the vault opened.
This policy conserves value but does not preserve historical beneficiary ownership for that residue.

Only this residue can cause a difference between a stateless multi-step quote and execution.
Execution can consolidate stake and assign the final complement.

Single-vault projection and execution use the same pending-touch calculation. Execution must stay
within the caller's stablecoin budget.

The required properties are conservation, bounded liquidation, recipient-rate interest,
late-vault exclusion, and material touch-order independence.
