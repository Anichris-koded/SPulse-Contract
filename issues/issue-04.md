# [CRITICAL] `AccumulatedFees` is a global fungible pool with no per-market provenance — cross-market contamination on cancel

## Summary

`AccumulatedFees` is a single global `i128` in instance storage (`prediction_market/src/lib.rs`). It accumulates platform fees from **all** markets, plus swept pools from `resolve_market` (empty-side) and `cancel_market` reclaims. There is **no per-market attribution**.

This makes the accounting **non-composable and provably incorrect**:

1. `cancel_market` subtracts `fees_in_pool` from the **global** accumulator, but that accumulator also holds fees from *other, unrelated, non-cancelled markets*. Cancelling one market can zero out (or reduce) fees legitimately earned by other markets.
2. `withdraw_fees` drains the **entire** global accumulator to a single recipient, with no way to know which market's fees (or swept pools) are being withdrawn.
3. A resolver sweeping an empty-side pool (see issue #3) mixes **user principal** with **platform fees** in the same fungible bucket, so a fee withdrawal can pay out user principal.

## The invariant that is violated

The system implicitly assumes:

```
Σ (per-market earned fees) + Σ (swept pools) == AccumulatedFees
```

But because `AccumulatedFees` is a single scalar, the moment any market is cancelled or any pool is swept, the mapping between the scalar and the per-market reality is **lost**. There is no way to answer: "how much of the current accumulator belongs to market X?"

## Why it's unsolvable by a localized patch

- **Adding per-market fee tracking** requires a new storage key per market and a migration of the existing global accumulator — a storage-model change.
- **Restricting `withdraw_fees`** to per-market amounts requires the per-market ledger that doesn't exist.
- **The `cancel_market` reclaim** (issue #1) and the **empty-side sweep** (issue #3) both write to this global scalar, so fixing them independently still leaves the fungibility problem.

The global accumulator is the root cause of three separate critical issues (#1, #3, #4). A correct fix requires replacing it with a **per-market fee ledger** plus a **global settlement view**, which is an architectural redesign of the entire fee lifecycle.