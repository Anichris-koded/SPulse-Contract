# [CRITICAL] `resolve_market` lets a resolver sweep the entire pool to fees by resolving to the empty side — griefing / fund theft

## Summary

In `resolve_market` (`prediction_market/src/lib.rs`, lines ~500–518):

```rust
let winning_side = if outcome { market.total_yes } else { market.total_no };
if winning_side == 0 {
    let total_pool = market.total_yes + market.total_no;
    if total_pool > 0 {
        acc += total_pool;   // entire pool swept to AccumulatedFees
    }
}
```

Any resolver (or admin) can resolve a market to the side that has **zero** bets, causing the **entire net pool** to be swept into `AccumulatedFees`. If the resolver is also a fee recipient (or colludes with one), they can immediately `withdraw_fees` and **steal the whole pool** — the bettors on the losing side get nothing back, and the "winners" (who bet on the now-winning side) get nothing either because `winning_side == 0` suppresses the payout in `claim`.

## Attack

1. Attacker becomes a resolver (admin-granted) or colludes with one.
2. A market has `total_yes = 0`, `total_no = 1000 XLM` (all bettors bet "no").
3. Resolver calls `resolve_market(outcome = true)` → `winning_side = total_yes = 0` → the 1000 XLM net pool is swept to `AccumulatedFees`.
4. Resolver (if fee recipient) calls `withdraw_fees` → drains the 1000 XLM.
5. All "no" bettors are now losers; they can only claim `LOSE_TOKENS`/`LOSE_POINTS`, never their stake.

There is **no check** that the chosen outcome is the side with actual liquidity, no minimum winning-side threshold, and no delay/commit-reveal to prevent a resolver from front-running the "true" outcome.

## Why it's unsolvable by a localized patch

- **Requiring `winning_side > 0`** would make markets with a one-sided outcome unresolvable (a legitimate case where everyone bet the same side).
- **Adding a commit-reveal / oracle** changes the entire resolution trust model and adds a new contract.
- **Restricting resolvers** to a trusted set doesn't help — the admin is already trusted, and the issue is the *incentive* to resolve to the empty side, not the identity.
- **Preventing fee-recipient resolvers** from withdrawing swept pools requires tracking the source of `AccumulatedFees` per-market, which the storage model does not support.

The fundamental problem is that `AccumulatedFees` is a **global, fungible** pool with no provenance, so swept market funds and earned platform fees are indistinguishable and both withdrawable by the same actor.

## Suggested direction

A correct fix requires separating **market-owned funds** from **platform fees** in storage, and/or enforcing that resolution outcome matches an externally-verifiable source (oracle/commit-reveal), and/or adding a withdrawal delay + dispute window. All are architectural changes.