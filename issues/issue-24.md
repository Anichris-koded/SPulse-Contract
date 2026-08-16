# [LOW] Points are monotonically increasing with no decay or penalty — the leaderboard is a permanent, stale ranking

## Summary

Points in the leaderboard **only ever increase** (`s.points += points` in `reward`, `add_pts`, `reward_bonus`, `add_bonus_pts`). There is:

- No point decay over time.
- No penalty for losses (losers still gain `LOSE_POINTS`).
- No way to reduce points.

This means the leaderboard is a **monotonic, permanent ranking** — early adopters who accumulated points long ago can never be overtaken by newer, more active players unless the newer players out-earn them in absolute terms forever. The ranking does not reflect current skill or activity, only cumulative historical points.

## Why it's unsolvable by a localized patch

- **Adding decay** requires a time-based recompute on every read/write — a storage and gas change.
- **Adding penalties** requires tracking loss streaks or time windows — new state.
- **The `upsert_top` logic** (issue #1) assumes points only increase; any decay/penalty breaks the min-cache invariant and would require a full recompute.

The root problem is **the points model is a cumulative counter, not a skill rating**. A correct fix requires a decayed or Elo-style rating system — a fundamental redesign of the leaderboard semantics.