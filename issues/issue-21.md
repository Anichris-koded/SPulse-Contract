# [MEDIUM] `MinPoints`/`MinSlot` are stored in instance storage with no TTL extension — the leaderboard's core cache can expire

## Summary

The leaderboard's `MinPoints` and `MinSlot` are stored in **instance storage** (`leaderboard/src/lib.rs`, lines ~499–605). Instance storage in Soroban has a **TTL that must be extended** just like persistent storage, but the code **never calls `extend_ttl`** on `MinPoints`/`MinSlot` (or on `TopPlayerCount`, `Admin`, `MarketContract`, etc.).

If the instance-storage TTL expires (which happens if the contract is idle for the TTL period), the entire leaderboard's cached min is lost. On the next `upsert_top`, `min_pts` reads `u64::MAX` (line ~543) and triggers `recompute_min` — but if the list is full and the recompute is skipped or the cache is partially stale, the stale-min displacement bug (issue #1) is triggered. The instance-storage TTL expiry is a **latent trigger** for the leaderboard corruption.

## Why it's unsolvable by a localized patch

- **Extending TTL on every write** adds gas and doesn't help if the contract is idle.
- **Instance storage TTL** is a Soroban platform constraint — the contract must actively bump it, and there's no keeper.
- **The cache design** (issue #1) is already fragile; TTL expiry makes it worse by silently resetting `MinPoints` to `u64::MAX`.

The root problem is **no TTL lifecycle management for instance storage**. A correct fix requires a TTL-refresh strategy across all instance keys — a systemic storage-lifecycle change (related to issue #9).