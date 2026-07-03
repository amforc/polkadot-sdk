# Redemptions Design Doc

**Authors:** Amforc AG - Leonardo Razovic, Luca von Wyttenbach, Raffael Huber  
**Status:** Draft  
**Scope:** Implementable redemption design for a Liquity-v2-style, governance-enabled pUSD protocol built with the Polkadot SDK.

## 1. Purpose and Background

`pallet-redemptions` is the pUSD collateral-exit mechanism. It lets pUSD holders burn pUSD against branch collateral, supports ordinary rate-ordered redemptions, and resolves `FinalRecovery` vaults through deterministic settlement pricing.

Redemptions serve three system roles:

1. **Peg support.** A pUSD holder can exit into collateral when pUSD trades below its target value.
2. **Borrower rate discipline.** Lower-rate vaults receive cheaper debt but sit earlier in the ordinary redemption queue.
3. **Terminal branch settlement.** If a branch contains a `FinalRecovery` vault, redemptions and Stability Pool recovery offsets provide the settlement path for that terminal unsafe-vault state.

The design borrows these Liquity V2 mechanics:

- redemptions against borrower-selected rate ordering;
- lowest-rate vaults redeemed first;
- same-rate LIFO behavior;
- redemption fees based on a base fee plus a decaying dynamic fee;
- redemption path priority over borrower preference during stress.

Intentional differences:

1. **Collateral branches are explicit.** The initial implementation supports single-branch redemption only, while preserving interfaces that can later support multi-branch routing.
2. **Governance exists.** Governance can set fee parameters, settlement parameters, branch enablement, and defensive controls.
3. **`FinalRecovery` settlement is first-class.** The redemption pallet owns the pricing and execution rules for both ordinary redemptions and `FinalRecovery` redemptions.
4. **Stability Pool recovery offsets share redemption settlement math.** The Stability Pool applies pool-specific accumulator updates, but the pricing model is defined here.

## 2. Core Terms

- **Branch:** A collateral-specific borrow market with its own vaults, Stability Pool, risk parameters, collateralization state, and operating mode.
- **Redemption:** Burning pUSD in exchange for branch collateral.
- **Ordinary Redemption:** A redemption against active vaults in ordinary rate-index order.
- **Recovery Redemption:** A redemption against a `FinalRecovery` vault.
- **Recovery Offset:** Stability Pool pUSD cancellation against a `FinalRecovery` vault using the same settlement pricing as recovery redemptions.
- **Redeemer:** Account that burns pUSD and receives collateral.
- **Recipient:** Account that receives collateral output.
- **Rate Index:** Per-branch vault ordering maintained by `pallet-vaults`; ordinary redemptions walk the lowest-rate end first.
- **Dormant Vault:** A vault outside the ordinary rate index. Redemptions may create a debt-bearing Dormant below `MinimumDebt`, or a zero-debt Dormant with residual collateral. Dormants remain in branch accounting and redistribution; only the branch's Dormant redemption target is part of ordinary redemption ordering.
- **`FinalRecovery` Vault:** A vault that is below MCR and cannot be liquidated because it is the last eligible redistribution recipient in the branch.
- **Redemption Fee:** pUSD-denominated protocol fee charged on ordinary redemptions according to a base-fee-plus-dynamic-fee model and routed through `FeeHandler`.
- **Dynamic Fee:** Decaying redemption-fee component that rises with redemption activity and decays over time.
- **Maximum Input:** The maximum pUSD amount the caller is willing to burn.
- **Minimum Collateral Out:** Slippage bound for the minimum collateral the caller must receive.
- **Iteration Limit:** Maximum number of vaults the redemption may touch in one transaction.

## 3. Assumptions

- `pallet-vaults` is the source of truth for vault ownership, status, debt, collateral, rate-index membership, Dormant redemption target, `FinalRecovery` FIFO membership, and branch mode.
- `pallet-vaults` exposes redemption interfaces that return fully accrued vault snapshots and apply redemption effects atomically.
- A redemption reads a fresh oracle price through the vault interface or through the same price snapshot returned by the vault interface.
- Ordinary redemptions skip active vaults with current `CR < 100%`. `FinalRecovery` vaults are exempt from this restriction.
- If a branch contains a `FinalRecovery` vault, recovery redemption takes priority over ordinary redemption ordering.
- Stability Pool recovery offsets use the same `FinalRecovery` settlement pricing defined in this document, but Stability Pool accumulator updates are specified in the Stability Pool document.
- Frozen Mode disables redemptions.
- Rounding favors the system: collateral outputs round down, fees round up where appropriate, and dust remains in protocol accounting until safely swept or settled.

## 4. Architecture

`pallet-redemptions` orchestrates redemption flows while delegating vault-state mutation to `pallet-vaults`.

`pallet-redemptions` is responsible for:

1. public redemption extrinsics;
2. ordinary redemption loops;
3. `FinalRecovery` redemption loops;
4. redemption preview functions;
5. redemption fee and dynamic-fee accounting;
6. slippage, partial-fill, and on-chain step-bound enforcement;
7. settlement pricing for `FinalRecovery` redemptions;
8. shared pricing helpers used by Stability Pool recovery offsets;
9. redemption events and user-facing errors.

It is not responsible for:

- storing vault positions;
- maintaining the rate index;
- maintaining the `FinalRecovery` FIFO;
- storing vault collateral holds;
- calculating branch TCR except through vault-provided interfaces;
- updating Stability Pool `P`, `S`, `G`, epoch, or scale accumulators;
- liquidating vaults;
- redistributing liquidation debt or collateral.

### 4.1 Redemption Priority

For a selected branch, the current executable target is selected by this priority:

1. `FinalRecovery` FIFO head, if present;
2. Dormant redemption target, if present;
3. ordinary active vaults from the lowest-rate end of the rate index.

The redemption pallet should request the next target from `pallet-vaults` rather than reconstructing ordering locally.

The Dormant redemption target is the branch's single Dormant continuation slot and the only Dormant vault redemptions can reach. It is a barrier, not just the first candidate. While the slot is occupied, `pallet-vaults` must return it before ordinary rate-index targets, and Redemptions must not reconstruct or walk the ordinary tail directly. If the Dormant target cannot currently cancel debt, including because its `CR < 100%`, the branch loop stops rather than bypassing it.

After each successful step, Redemptions asks `pallet-vaults` for the next target again. Once a step clears the Dormant target, the next lookup can expose the ordinary rate-index tail. A new sub-minimum Dormant can therefore be created only when the slot is empty; no active-target clamp is part of the normal redemption path.

Redistribution can add debt to any Dormant, so a branch can hold debt-bearing Dormants beyond the slot occupant; they never take the slot and are not part of this priority order. This pocket has a bounded source: non-slot Dormants can acquire unredeemable principal only through redistribution assigned to Dormant stakes, and redistribution runs only after active Stability Pool, JIT, and pending-deposit capital are exhausted. That principal can accrue interest while Dormant. Once such a vault's fully accrued debt reaches `MinimumDebt`, permissionless activation returns it to the rate index and to ordinary redemption exposure at its own rate, and an unsafe one is resolved by liquidation. Below `MinimumDebt` the debt is accepted dust.

### 4.2 Branch Routing

The initial implementation is single-branch:

```rust
redeem(
    collateral_id,
    max_pusd_in,
    min_collateral_out,
    recipient,
    max_steps,
)
```

The interface should not prevent later multi-branch routing. A future extension may support a route vector:

```rust
redeem_routed(
    route,
    max_pusd_in,
    min_collateral_out,
    recipient,
    max_steps_per_branch,
)
```

where each route item specifies a collateral branch and maximum branch share.

## 5. Data Model and Storage

### 5.1 Redemption Configuration

```rust
struct RedemptionConfig<Balance, Moment> {
    minimum_redemption_amount: Balance,

    dynamic_fee_decay_period: Moment,
    dynamic_fee_floor: FixedU128,
    dynamic_fee_ceiling: FixedU128,
    base_fee: FixedU128,
    fee_ceiling: FixedU128,
    dynamic_fee_increase_divisor: FixedU128,

    final_recovery_bonus_buffer: FixedU128,
}
```

Valid configs must have a non-zero `minimum_redemption_amount`, non-zero
`dynamic_fee_decay_period`, ordered dynamic-fee and fee ranges, and a non-zero
`dynamic_fee_increase_divisor`.

`final_recovery_bonus_buffer` is used when a `FinalRecovery` vault has `CR >= 100%`. It ensures the recovery bonus does not prevent the vault from improving as settlement proceeds.

### 5.2 Branch Redemption State

```rust
struct RedemptionState<Moment> {
    dynamic_fee: FixedU128,
    last_fee_operation: Moment,
}
```

The dynamic fee is branch-specific. It decays over time and increases when ordinary redemptions are executed. Recovery redemptions do not increase the ordinary redemption dynamic fee.

### 5.3 Storage Items

| Storage             | Type                                                                          | Purpose                          |
| ------------------- | ----------------------------------------------------------------------------- | -------------------------------- |
| `RedemptionConfigs` | `StorageMap<_, Twox64Concat, T::AssetId, RedemptionConfig<...>, OptionQuery>` | Per-branch redemption parameters |
| `RedemptionStates`  | `StorageMap<_, Twox64Concat, T::AssetId, RedemptionState<...>, ValueQuery>`   | Per-branch dynamic fee state     |

## 6. Interfaces

### 6.1 Public Extrinsics

```rust
redeem(
    origin,
    collateral_id: AssetId,
    max_pusd_in: Balance,
    min_collateral_out: Balance,
    recipient: AccountId,
    max_steps: u32,
)
```

Executes a branch-local redemption. The pallet requests the next redemption target from `pallet-vaults`; the returned target determines whether the step uses recovery-redemption or ordinary-redemption rules.

Rules:

- `max_pusd_in` must be at least `minimum_redemption_amount`.
- The on-chain loop processes at most `min(max_steps, MaxRedemptionSteps)` targets; `max_steps == 0` means the `MaxRedemptionSteps` ceiling.
- Pre-dispatch weight scales with the requested `max_steps` and is refunded to the steps actually touched.
- The caller must hold at least the pUSD amount actually burned.
- The redemption may partially fill if the branch lacks enough redeemable debt or the step bound is reached after making progress.
- The redemption must revert if final `collateral_out < min_collateral_out` (scaled for partial fills).
- The redemption must revert if no debt is canceled.

```rust
preview_redeem(
    collateral_id: AssetId,
    max_pusd_in: Balance,
) -> RedemptionPreview
```

Returns the expected target vaults, aggregate input/output, a `steps` count, and a `truncated` flag, using current state. `steps` counts every target the loop would touch to consume `max_pusd_in`.

Because the preview does not mutate, it cannot drain a Dormant target and cross into the rate index behind it. When redeemable debt remains past a drainable Dormant, the preview stops at the Dormant and so under-reports `steps` (and may leave `truncated` false); the live redemption crosses it. A caller that hits this resubmits with a higher budget. (An exact preview would dry-run the vault-side effects inside the rolled-back transaction.)

### 6.2 Vault Interface Required by Redemptions

```rust
// The redemption-facing subset of `VaultInterface` (pusd-primitives), keyed by
// the `(collateral_id, stable_id)` market.
trait VaultInterface {
    fn next_redemption_target(
        collateral_id: &CollateralId,
        stable_id: &StableId,
        after: Option<&AccountId>,
    ) -> Option<(AccountId, VaultStatus)>;

    // One atomic step: touch the vault, hand the post-touch snapshot to the
    // orchestrator's closure, and apply the allocation it returns. `Ok(None)`
    // persists the touch without redeeming; `Err` rolls the whole step back.
    fn redeem_step(
        collateral_id: &CollateralId,
        stable_id: &StableId,
        owner: &AccountId,
        build_allocation: impl FnOnce(
            RedemptionStepSnapshot<Balance>,
        ) -> Result<Option<RedemptionAllocation<AccountId, Balance>>, DispatchError>,
    ) -> Result<Option<RedemptionAllocation<AccountId, Balance>>, DispatchError>;

    // Terminal FinalRecovery settlement once the market-cancellable debt is
    // exhausted: moves the residual to branch bad debt and removes the vault,
    // returning the residual the orchestrator burns from the Insurance Fund.
    fn settle_recovery_residual(
        collateral_id: &CollateralId,
        stable_id: &StableId,
        owner: &AccountId,
    ) -> Result<Balance, DispatchError>;

    // Fully-accrued branch debt, for the dynamic-fee redeemed-fraction.
    fn branch_debt(collateral_id: &CollateralId, stable_id: &StableId) -> Balance;

    // Burns up to the market's recorded bad debt from `credit`, returning the
    // unconsumed surplus. Carries the Insurance-Fund residual burn.
    fn heal(
        collateral_id: &CollateralId,
        stable_id: &StableId,
        credit: Credit,
    ) -> Result<Credit, DispatchError>;
}
```

`next_redemption_target` returns the target `owner` and its `VaultStatus`
(`Active`, `Dormant`, or `FinalRecovery`). `RedemptionStepSnapshot` carries the same
`status` plus the post-touch `debt`, held `collateral`, and the branch
`redistribution_penalty` (the recovery-bonus bound; only consulted by `FinalRecovery`
pricing), so the orchestrator selects a pricing regime without a second classifying call.

`redeem_step` fully accrues aggregate interest, touches the target vault, validates its
current status, and prices and applies the step against that post-touch snapshot inside
one atomic call.

`next_redemption_target` is the authoritative current-target interface. Both forms re-apply the
`FinalRecovery`/Dormant barrier first, so slot clearance, creation, liquidation, activation, or
close since the previous step is reflected before any ordinary target is selected; targets behind
an occupied Dormant slot are never exposed. With `after == None` it returns the `FinalRecovery`
FIFO head, then the Dormant redemption target while set, then the ordinary rate-index tail. With
`after == Some(owner)`, when no barrier gates, it returns the next ordinary target after `owner`
(its head-ward rate-index neighbor).

The orchestrator carries a cursor instead of restarting from the head after each redeem: it
advances the cursor to a skipped (underwater) vault and leaves it there across subsequent redeems.
Because a skipped vault stays live, its head-ward neighbor advances past vaults removed by those
redeems, so an underwater prefix is skipped once rather than re-walked on every redeem — which
matters under a sharp price drop, when underwater vaults cluster at the low-rate tail exactly as
redemption demand spikes.

## 7. Ordinary Redemptions

An ordinary redemption burns pUSD against vaults in branch redemption order. It does not apply to `FinalRecovery` vaults; if a `FinalRecovery` vault exists, recovery redemption has priority.

### 7.1 Ordinary Redemption Loop

For each bounded on-chain step:

1. ask `pallet-vaults` for the next redemption target;
2. if the target is a `FinalRecovery` vault, switch to recovery-redemption rules;
3. prepare a fully accrued redemption snapshot;
4. if the target is the Dormant redemption target and no debt can be cancelled, including because its `CR < 100%`, stop this branch loop;
5. skip the target if it is an active rate-index vault with `CR < 100%`;
6. calculate debt cancellation and collateral output at face value;
7. apply ordinary redemption fee;
8. withdraw `total_pusd_in` from the redeemer as a `StableCredit`;
9. split the credit into `debt_credit` and any `fee_credit`, then route `fee_credit` through `FeeHandler`;
10. call `pallet-vaults` with the per-vault allocation and debt-cancellation credit so Vaults consumes the credit, transfers collateral, and updates or removes the vault;
11. continue until `max_pusd_in` is exhausted, no redeemable target remains, or the step bound (`min(max_steps, MaxRedemptionSteps)`) is reached.

### 7.2 Face-Value Exchange

Before fees, ordinary redemption exchanges pUSD for collateral at face value:

```text
collateral_out_before_fee = floor(debt_cancelled / price)
```

Ordinary redemption fees are specified in Section 8.

## 8. Redemption Fees

Ordinary redemptions charge a dynamic pUSD fee. The fee has two purposes:

1. reduce unnecessary churn when pUSD is near peg;
2. make large redemptions temporarily increase the cost of subsequent redemptions.

The redeemed debt is burned through the linear credit passed into Vaults. The redemption fee is not burned; Redemptions splits it from the total withdrawn credit and routes it through `FeeHandler`.

### 8.1 Dynamic-Fee Decay

Before applying a new ordinary redemption, the branch dynamic fee is decayed from `last_fee_operation` to the current time.

```text
elapsed_half_lives = time_elapsed / dynamic_fee_decay_period
decay_factor = 2^(-elapsed_half_lives)
decayed_dynamic_fee = floor(dynamic_fee * decay_factor)
```

`dynamic_fee_decay_period` is the redemption-fee half-life. The suggested initial value is 6 hours. The implementation may approximate `2^(-elapsed_half_lives)` with a fixed-point helper, but it must be deterministic and monotonic: more elapsed time must never produce a higher decayed dynamic fee.

### 8.2 Dynamic-Fee Increase

After an ordinary redemption, the dynamic fee increases according to the redeemed fraction of branch debt.

```text
redeemed_fraction = redeemed_debt / branch_debt_before_redemption
dynamic_fee = clamp(
    decayed_dynamic_fee + redeemed_fraction / dynamic_fee_increase_divisor,
    dynamic_fee_floor,
    dynamic_fee_ceiling,
)
```

`dynamic_fee_increase_divisor` and `dynamic_fee_ceiling` are configurable per branch.

### 8.3 Fee Calculation

```text
redemption_fee_rate = clamp(
    dynamic_fee + base_fee,
    base_fee,
    fee_ceiling,
)
```

The ordinary redemption fee is taken in pUSD and routed through `FeeHandler` by Redemptions. It is split out of the total withdrawn `StableCredit` and is not included in the `debt_credit` passed to Vaults for debt cancellation.

```text
fee_pusd = ceil(debt_cancelled * redemption_fee_rate)
total_pusd_in = debt_cancelled + fee_pusd
collateral_out = collateral_out_before_fee
```

The redemption must ensure `total_pusd_in <= max_pusd_in`. For partial fills, the maximum cancellable debt is bounded by the caller's pUSD budget after accounting for the fee.

Recovery redemptions do not charge ordinary redemption fees. The recovery bonus or discounted recovery rate already defines the settlement incentive. After a recovery redemption or recovery offset fully settles the current `FinalRecovery` vault, the transaction stops; any later ordinary redemption or next recovery-vault settlement must be submitted as a separate transaction.

## 9. `FinalRecovery` Redemptions and Settlement Pricing

A `FinalRecovery` vault is redeemed before ordinary vaults. Immediately before each recovery redemption, the redemption pallet must obtain a fully accrued snapshot and fresh price through the vault interface.

`FinalRecovery` has two pricing regimes.

### 9.1 Recovery Bonus when `CR >= 100%`

When the recovery vault has at least 100% CR, pUSD is canceled at face value and the redeemer receives collateral plus a bounded recovery bonus.

```text
recovery_bonus = min(
    max(0, CR - 100% - final_recovery_bonus_buffer),
    redistribution_penalty
)

collateral_out_value = debt_cancelled * (1 + recovery_bonus)
collateral_out       = floor(collateral_out_value / price)
```

The bonus must not make the recovery vault's CR worse than its pre-redemption CR.

### 9.2 Insurance-Adjusted Settlement when `CR < 100%`

When the recovery vault is below 100% CR, redemptions use insurance-adjusted recovery pricing.

Each stablecoin has its own Insurance Fund account (the runtime maps `stable_id` to an account), so cover held for one stablecoin never settles another stablecoin's bad debt.

Let:

```text
D = fully accrued recovery vault debt
C = recovery vault collateral value at current oracle price
IF = pUSD balance available in the Insurance Fund
current_shortfall = max(D - C, 0)
effective_cover = min(IF, current_shortfall)
market_cancel_debt = D - effective_cover
```

If `market_cancel_debt == 0`, the Insurance Fund can cover the remaining debt. The final settlement burns pUSD from the Insurance Fund and removes the vault.

Otherwise:

```text
recovery_rate = C / market_cancel_debt
```

A redeemer or offsetter burning `x` pUSD receives:

```text
collateral_out_value = floor(x * recovery_rate)
collateral_out       = floor(collateral_out_value / price)
```

The maximum externally cancellable debt is `market_cancel_debt`. Once that amount has been canceled, all recovery-vault collateral has been paid out and the remaining debt equals `effective_cover`. The vault pallet records the residual as bad debt and atomically calls the Insurance Fund burn path for the same amount.

If the Insurance Fund is empty, `recovery_rate = C / D`. pUSD holders absorb the full shortfall through the discounted recovery rate.

When multiple `FinalRecovery` vaults exist, only the FIFO head is redeemable. The Insurance Fund is read at the time the current head is settled. If the first recovery vault consumes the available Insurance Fund, the next recovery vault is settled under the same rules with the then-current Insurance Fund balance, which may be zero.

### 9.3 Stability Pool Recovery Offsets

A Stability Pool recovery offset uses the same pricing rules as recovery redemption. The distinction is the funding source and accounting effect:

- redemption burns pUSD from the redeemer and transfers collateral directly to the recipient;
- recovery offset burns active Stability Pool deposits and updates pool accumulators;
- no JIT path exists for recovery offsets.

The Stability Pool document owns `P`, `S`, `G`, epoch, scale, depositor realization, and withdrawal-delay effects.

## 10. Mode Rules

| Operation                   | Normal         | Safety         | Frozen         |
| --------------------------- | -------------- | -------------- | -------------- |
| Ordinary redemption         | Yes            | Yes            | No             |
| Recovery redemption         | Yes            | Yes            | No             |
| Redemption preview          | Yes            | Yes            | No             |
| Dynamic-fee update            | Yes            | Yes            | No             |
| Config parameter update     | Branch admin   | Branch admin   | Branch admin   |

Config parameter updates are authorized per market by the runtime's `UpdateOrigin`, keyed by the `(collateral, stable)` pair. Runtimes point it at the market's full admin (the authority that created the branch) and compose a governance override with `EitherOf`.

Safety Mode permits redemptions because they burn pUSD and improve branch TCR. Recovery redemptions are explicit settlement operations.

Frozen Mode means the branch cannot safely price or process ordinary risk-changing actions.

## 11. Events

```rust
event OrdinaryRedemptionExecuted {
    collateral_id,
    redeemer,
    recipient,
    pusd_burned,
    collateral_out,
    fee_pusd,
    steps,
}

event RecoveryRedemptionExecuted {
    collateral_id,
    redeemer,
    recipient,
    vault_owner,
    pusd_burned,
    collateral_out,
    regime,
}

event RedemptionDynamicFeeUpdated {
    collateral_id,
    old_dynamic_fee,
    new_dynamic_fee,
}

event RedemptionConfigUpdated {
    collateral_id,
}
```

## 12. Errors

```rust
BelowMinimumRedemptionAmount
NoRedeemableVault
SlippageExceeded
InsufficientPusdBalance
BranchFrozen
InvalidBranch
OracleUnavailable
RecoverySettlementFailed
InsuranceFundBurnFailed
InvalidRedemptionConfig
```

## 13. Invariants

- Redemptions never create pUSD.
- For each redemption step, canceled debt equals the pUSD the orchestrator burns (withdrawn from the redeemer, split from the fee, and rescinded), except for the explicitly atomic Insurance Fund burn during final recovery settlement.
- Ordinary redemptions never redeem an ordinary vault with `CR < 100%`.
- If a `FinalRecovery` vault exists, it is served before ordinary redemption ordering.
- Recovery offsets and recovery redemptions use the same settlement pricing.
- Redemption output must satisfy the caller's `min_collateral_out` or revert.
- Rate-index ordering is read from `pallet-vaults`; redemptions do not maintain a separate ordering structure.
- The branch registry is read from `pallet-vaults` through `BranchModeProvider` and `RedemptionConfigs` rows exist for exactly the registered branches.
- The Dormant redemption target is a barrier before ordinary rate-index traversal; ordinary active targets are not exposed while the slot is occupied, so redemptions structurally cannot create a second debt-bearing Dormant of their own making.
- The Insurance Fund is not reserved per vault; settlement reads available Insurance Fund balance at execution time.
- Only the `FinalRecovery` FIFO head is redeemable.
- Rounding must not overpay collateral relative to the applicable pricing formula.

## 14. Suggested Initial Parameters

| Parameter                     | Suggested initial value | Notes                                                    |
| ----------------------------- | ----------------------: | -------------------------------------------------------- |
| `minimum_redemption_amount`   |                100 pUSD | Should prevent dust redemptions.                         |
| `MaxRedemptionSteps`          |                     TBD | Runtime-constant ceiling for caller-supplied `max_steps` |
| `dynamic_fee_decay_period`      |                 6 hours | Impacts expected redemption demand.                      |
| `dynamic_fee_floor`             |                       0 | The dynamic fee may decay to zero.                     |
| `dynamic_fee_ceiling`           |                    100% | Caps the dynamic-fee state.                        |
| `base_fee`        |                    0.5% | Minimum ordinary redemption fee.                         |
| `fee_ceiling`      |                    100% | Maximum ordinary redemption fee.                         |
| `dynamic_fee_increase_divisor`  |                       2 | Divides redeemed branch-debt fraction before rate bump.  |
| `final_recovery_bonus_buffer` |                      1% | Should be reviewed with the recovery-bonus formula.      |
