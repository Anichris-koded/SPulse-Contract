# [HIGH] `get_top_players` uses O(n²) selection sort with full Vec rebuilds — gas bomb that reverts on real leaderboards

## Summary

`get_top_players` (`leaderboard/src/lib.rs`, lines ~361–432) sorts the top list with a **selection sort** where each swap rebuilds the entire `Vec`:

```rust
for i in 0..n {
    let mut max_idx = i;
    for j in (i + 1)..n { ... }
    if max_idx != i {
        // rebuild the whole Vec to swap two elements
        let mut rebuilt: Vec<PlayerEntry> = Vec::new(&env);
        for k in 0..n { ... }
        all = rebuilt;
    }
}
```

With `MAX_TOP_PLAYERS = 50`, the worst case is ~1225 comparisons and up to ~1225 **full Vec rebuilds** of 50 elements each — tens of thousands of allocations in Soroban's linear memory. This **exceeds the gas/CPU budget** on a full leaderboard, so `get_top_players` **always reverts** for real users. The comment even acknowledges it's O(n²) but the "optimization" (fewer rebuilds when already sorted) does not help the common case where points are interleaved.

## Why it's unsolvable by a localized patch

- **The sort is on-read** — every call re-sorts from scratch; there is no cached sorted order.
- **Soroban `Vec` is immutable** — "swapping" requires a full rebuild, so any comparison-sort is O(n²) allocations.
- **Reducing `MAX_TOP_PLAYERS`** changes the product and still leaves the O(n²) cost.
- **A correct fix** requires maintaining a **sorted index at write time** (in `upsert_top`) or a heap/ordered structure, so reads are O(k) — a storage-model redesign.

The root problem is **sorting on read with an immutable data structure**. A correct fix needs a write-time-ordered index, which is a fundamental change to the leaderboard storage.