# [MEDIUM] `reward_bonus`/`add_bonus_pts` don't increment `won_bets`/`lost_bets` — `total_bets` silently undercounts

## Summary

`reward_bonus` (`leaderboard/src/lib.rs`, lines ~243–288) and `add_bonus_pts` (lines ~291–317) add points but do **not** increment `won_bets`/`lost_bets`:

```rust
s.points += points;
// no won_bets / lost_bets update
```

Meanwhile `get_stats` derives `total_bets = won_bets + lost_bets` (line ~351). So a user who earns points purely through referrals/bonuses has `total_bets = 0` despite having points — the leaderboard's `total_bets` metric is **provably wrong** for bonus-only activity. This also means `get_stats` cannot distinguish "active bettor with 0 points" from "bonus-only user with points".

## Why it's unsolvable by a localized patch

- **Incrementing `won_bets`/`lost_bets`** in the bonus path would mislabel bonus points as bet outcomes.
- **Adding a separate `bonus_bets` field** changes the `UserStats` struct and the ABI of `get_stats`.
- **The `total_bets` derivation** (`won + lost`) is a deliberate optimization (the comment says so) that fundamentally cannot represent bonus-only activity.

The root problem is that **`total_bets` is derived, not stored**, and the derivation excludes bonus activity. A correct fix requires either storing `total_bets` explicitly or adding a bonus counter — a storage/ABI change.