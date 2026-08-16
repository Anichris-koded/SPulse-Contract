# [MEDIUM] `transfer`/`burn` don't extend TTL on balance keys — token balances can expire and be lost

## Summary

`transfer` (`pulse_token/src/lib.rs`, lines ~116–133) and `burn` (lines ~135–156) update `Balance` keys in **persistent storage** but **never call `extend_ttl`**:

```rust
env.storage().persistent().set(&DataKey::Balance(from), &(from_balance - amount));
env.storage().persistent().set(&DataKey::Balance(to), &(to_balance + amount));
// no extend_ttl
```

If a user holds PULSE but is inactive for the TTL period, their `Balance` key **expires and is deleted** — their tokens are **permanently lost** (the `TotalSupply` is not decremented, so the supply and the sum of balances diverge). This is a critical data-integrity issue for a token contract.

## Why it's unsolvable by a localized patch

- **Extending TTL on every transfer** adds gas and doesn't help if the holder is inactive.
- **The token has no keeper** to refresh balances.
- **A correct fix** requires a TTL-refresh strategy (read-bump, keeper, or a storage-rental model) — a systemic storage-lifecycle change (issue #9).

The root problem is **no TTL lifecycle management for token balances**. A correct fix is part of the broader storage-lifecycle redesign.