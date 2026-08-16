# [MEDIUM] `TopPlayerSlot` reverse lookups are not cleaned up on eviction in all paths — orphaned entries and stale ranks

## Summary

In `upsert_top` (`leaderboard/src/lib.rs`, lines ~478–586), when a player is evicted from the full list, the code removes their `TopPlayerSlot`:

```rust
if let Some(old) = env.storage().persistent().get::<DataKey, PlayerEntry>(&DataKey::TopPlayerAt(min_slot)) {
    env.storage().persistent().remove(&DataKey::TopPlayerSlot(old.address));
}
```

But this cleanup only happens in the **full-list eviction path**. In the **in-place update path** (lines ~489–504), if a player's points change and they remain in the list, their `TopPlayerSlot` is correctly retained. However, there is **no cleanup** for:

- Players whose `TopPlayerAt` entry expires via TTL (issue #9/#21) — their `TopPlayerSlot` remains, so `get_rank` returns a stale slot pointing to a deleted entry.
- The stale-min displacement bug (issue #1) — when a low-points player overwrites a high-points player's slot, the high-points player's `TopPlayerSlot` is removed, but if the overwrite is wrong (issue #1), the wrong player is evicted and their slot is orphaned.

The result is **orphaned `TopPlayerSlot` entries** that make `get_rank` return incorrect (stale) ranks, and `upsert_top` treat a player as "in the list" when their entry is gone.

## Why it's unsolvable by a localized patch

- **The cleanup is conditional** on the eviction path; there's no global invariant enforcing `TopPlayerSlot` ↔ `TopPlayerAt` consistency.
- **TTL expiry** (issue #9) deletes `TopPlayerAt` without any hook to clean `TopPlayerSlot`.
- **A correct fix** requires a consistency invariant and a reconciliation/repair function — a storage-integrity redesign.

The root problem is **no invariant linking the forward index and reverse index**. A correct fix needs atomic index maintenance and a repair mechanism.