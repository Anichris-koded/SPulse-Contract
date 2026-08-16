# [MEDIUM] `get_rank` returns 0 for any player outside the top-50 — misleading and inconsistent with `get_top_players`

## Summary

`get_rank` (`leaderboard/src/lib.rs`, lines ~434–469) returns `0` if the user is not in the top list (`TopPlayerSlot` is `None`):

```rust
let slot: Option<u32> = env.storage().persistent().get(&DataKey::TopPlayerSlot(user.clone()));
if slot.is_none() { return 0; }
```

So a player ranked #51 (or #500) gets rank `0`, which is indistinguishable from "not ranked" and is **lower** than the #50 player's rank of `50`. The rank is also computed **only against the top-50** (line ~457 loops `0..count`), so a player's rank is relative to the top-50, not the whole population — a player with the 60th-highest points gets `0`, while the 50th gets `50`.

## Why it's unsolvable by a localized patch

- **Returning the true rank** requires knowing the total player population and their points — the contract only stores the top-50, not all players.
- **Storing all players' ranks** is unbounded storage.
- **The `TopPlayerSlot`** only exists for top-50 members, so there's no way to look up a non-member's rank without a full scan.

The root problem is that **the leaderboard only tracks the top-50**, so any rank query outside that set is undefined. A correct fix requires either a full player index (unbounded) or an explicit "not in top-50" semantic distinct from rank `0` — a design decision, not a code fix.