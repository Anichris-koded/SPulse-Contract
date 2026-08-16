# [MEDIUM] `upsert_top` append path uses `<` instead of `<=` for min tracking — equal-points players corrupt the min cache

## Summary

In the append path of `upsert_top` (`leaderboard/src/lib.rs`, lines ~528–538):

```rust
let cur_min: u64 = env.storage().instance().get(&DataKey::MinPoints).unwrap_or(u64::MAX);
if new_count == 1 || new_points < cur_min {
    env.storage().instance().set(&DataKey::MinPoints, &new_points);
    env.storage().instance().set(&DataKey::MinSlot, &s);
}
```

The condition uses **strict `<`**. If a new player's points **equal** the current min, the min cache is **not** updated — but the new player is appended at a **higher slot index** than the existing min. Now there are **two players with the same min points**, but `MinSlot` points to only one of them.

When the list is full and a new player arrives with points **equal to the min**, the full path checks `new_points <= min_pts` (line ~556) and **rejects** the new player — even though they should displace one of the equal-min players. Conversely, if the min player at `MinSlot` is evicted, the other equal-min player is not tracked, and `recompute_min` may pick the wrong slot.

## Why it's unsolvable by a localized patch

- **Changing `<` to `<=`** would make the min slot point to the *last* equal-min player, which is arbitrary and still wrong for tie-breaking.
- **Tie-breaking** requires a deterministic rule (e.g., by address or by slot) that the current code doesn't define.
- **The min cache** (issue #1) is fundamentally a single-slot approximation of a multi-player minimum; ties break it.

The root problem is **the min cache cannot represent ties**. A correct fix requires a tie-aware min tracking or a full recompute on ties — a leaderboard storage redesign.