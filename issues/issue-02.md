# [CRITICAL] Payout rounding leaves dust permanently trapped in the contract — sum of payouts never equals the pool

## Summary

In `claim` (`prediction_market/src/lib.rs`, line ~652):

```rust
let payout = (entry.net * total_pool) / winning_side;
```

Each winner's payout is computed with **independent integer division**. The sum of all payouts is **provably less than or equal to** `total_pool`, and in general **strictly less** — the remainder (dust) is never distributed and never accounted for. It accumulates in the contract's XLM balance forever, unreachable by anyone.

## Root cause

For a market with `n` winners each staking `net_i` on the winning side `W = Σ net_i`, and total pool `P = W + L`:

```
payout_i = floor(net_i * P / W)
Σ payout_i = floor(P * Σ net_i / W) = floor(P * W / W) = P  (only if all divisions are exact)
```

In general `Σ floor(net_i * P / W) ≤ P`, with the deficit `P - Σ payout_i` being the accumulated rounding error. This dust is **not** added to `AccumulatedFees`, **not** refunded, and **not** tracked anywhere. It is simply stranded in the contract's XLM balance.

## Why it's unsolvable by a localized patch

- **Distributing the remainder** requires knowing the full set of winners and their exact stakes at claim time — but claims are **per-user and asynchronous**. You cannot know the final remainder until the *last* winner claims, and there is no mechanism to trigger a final settlement.
- **Adding the remainder to fees** requires a post-claim reconciliation pass that the contract has no trigger for.
- **Changing the payout formula** (e.g., rounding up the last claimer) requires knowing who claims last — impossible to determine in advance.
- **Pre-computing exact payouts at resolve time** would require iterating all winners and storing per-user payouts, which reintroduces the unbounded-loop gas problem and changes the storage model.

The dust is a permanent, growing, unaccounted liability that breaks the core invariant `contract_balance == AccumulatedFees + Σ unclaimed_payouts + Σ refundable_gross`.

## Suggested direction

A correct fix requires a **settlement model**: either (a) compute and store exact per-winner payouts at `resolve_market` time (with a bounded winner iteration), or (b) sweep the remainder to `AccumulatedFees` via a finalization function callable only after all claims, or (c) adopt a different payout scheme (e.g., proportional with a designated dust recipient). All options change the storage layout and the claim lifecycle.