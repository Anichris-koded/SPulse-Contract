# [MEDIUM] Cross-contract call chains have no gas/budget management — a single expensive call can brick the whole flow

## Summary

The system uses **deep cross-contract call chains** with no gas/budget management:

- `place_bet` → `referral.credit` → `leaderboard.add_bonus_pts` (and `token.mint` in `reward`).
- `claim` → `leaderboard.reward` → `token.mint`.

Each hop consumes gas, and the total cost is **unbounded and unpredictable**. If any hop exceeds the transaction's gas budget (e.g., `leaderboard.reward` triggers the O(n²) `upsert_top`/`recompute_min` on a full list, issue #16), the **entire** `place_bet`/`claim` reverts — the user's bet or claim fails even though the primary action was valid. There is no:

- Gas budget estimation or reservation.
- Fallback if a reward/referral call fails (the whole tx reverts).
- Separation of the "critical" fund movement from the "optional" reward/referral side-effects.

## Why it's unsolvable by a localized patch

- **The reward/referral calls are inline** — there's no way to make them optional without changing the flow.
- **The gas cost** depends on leaderboard state (issue #16), which the market can't predict.
- **A correct fix** requires decoupling the fund movement from the reward/referral side-effects (e.g., a pull-based reward model or a separate reward transaction) — a redesign of the cross-contract flow.

The root problem is **tightly-coupled, gas-unbounded cross-contract calls**. A correct fix is an architectural change to the reward/referral flow.