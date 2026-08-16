# [HIGH] `get_market_bettors` iterates an unbounded bettor index — gas DoS on large markets

## Summary

`get_market_bettors` (`prediction_market/src/lib.rs`, lines ~740–758) reads `BettorCount(market_id)` and then loops `0..count`, reading every `BettorAt(market_id, i)`:

```rust
let count: u32 = env.storage().persistent().get(&DataKey::BettorCount(market_id)).unwrap_or(0);
let mut result: Vec<Address> = Vec::new(&env);
for i in 0..count {
    if let Some(addr) = env.storage().persistent().get::<DataKey, Address>(&DataKey::BettorAt(market_id, i)) {
        result.push_back(addr);
    }
}
```

There is **no upper bound** on `count`. A market with thousands of bettors makes this function exceed the Soroban gas/CPU budget, so the call **always reverts** — the market's bettor list becomes permanently unreadable. Because `BettorCount` is incremented on every first bet (line ~457), an attacker can spam many small bets (up to `MAX_BETS_PER_USER` each) to inflate `count` and brick the read path for everyone.

## Why it's unsolvable by a localized patch

- **Adding a page limit** to the loop still requires reading `count` entries to skip to the offset — the O(n) scan remains.
- **Capping `BettorCount`** would silently drop legitimate bettors from the index, breaking `get_market_bettors` correctness.
- **The index is append-only** with no compaction, so the cost grows monotonically with market size.

The root problem is an **unbounded linear scan** in a read function. A correct fix requires a paginated, indexed, or chunked storage layout for the bettor list — a storage-model change.

## Related

The same unbounded-scan pattern exists in `leaderboard::get_top_players` (issue #16) and `get_rank` (issue #17).