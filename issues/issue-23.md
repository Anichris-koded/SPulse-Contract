# [MEDIUM] `get_top_players` pagination re-scans the entire list on every page — O(n) per page, no index

## Summary

`get_top_players` (`leaderboard/src/lib.rs`, lines ~361–432) reads **all** `TopPlayerAt(0..count)` entries, sorts them, and then slices `[offset..offset+page_size]`. Every page request re-reads and re-sorts the **entire** top list:

```rust
let mut all: Vec<PlayerEntry> = Vec::new(&env);
for i in 0..count { all.push_back(...); }
// sort all
// slice [offset..end]
```

There is no persistent sorted index, so pagination is O(n log n) per page (and O(n²) with the selection sort, issue #16). A frontend paging through the leaderboard triggers a full re-sort on every page — a gas/CPU cost that grows with `count` and reverts on large lists.

## Why it's unsolvable by a localized patch

- **The sort is on-read** (issue #16) — there's no cached order to page through.
- **Adding a persistent sorted index** requires maintaining it on every `upsert_top` write — a storage-model change.
- **The `offset`-based pagination** is inherently O(n) to skip to the offset without an index.

The root problem is **no write-time ordering**. A correct fix requires a persistent ordered index (issue #16) so pagination is O(page_size), not O(n) — a leaderboard storage redesign.