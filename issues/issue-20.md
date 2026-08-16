# [MEDIUM] No way to remove or ban a player from the top list — malicious or fraudulent players persist forever

## Summary

The leaderboard has **no function** to remove a player from the top-50, reset their points, or ban them. Once a player enters the top list, they remain there until displaced by a higher-points player (which, given the stale-min bug in issue #1/#2, may never happen correctly). There is no admin function to:

- Remove a fraudulent/sybil player.
- Reset a player's points.
- Correct a wrongly-awarded bonus.

This means a player who exploits the referral sybil vector (issue #29) or the stale-min displacement (issue #1) is **permanently** entrenched in the leaderboard, and the admin has no recourse short of a full contract upgrade.

## Why it's unsolvable by a localized patch

- **Adding a `remove_player` function** requires recomputing the top list, the `MinPoints`/`MinSlot` cache, and all `TopPlayerSlot` reverse lookups — a non-trivial maintenance operation that the current storage model doesn't support cleanly.
- **The `upsert_top` logic** assumes points only increase; a removal/reset breaks that invariant and would require a full recompute.
- **There is no audit trail** (issue #7) to even identify which player to remove.

The root problem is that **the leaderboard is append-only and ungovernable**. A correct fix requires admin governance functions and a recompute mechanism — a feature/design addition, not a bug fix.