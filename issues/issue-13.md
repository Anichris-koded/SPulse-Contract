# [MEDIUM] `cancel_refund` zeroes `gross` but leaves `net` intact — inconsistent bet state after refund

## Summary

`cancel_refund` (`prediction_market/src/lib.rs`, lines ~571–603) refunds the gross and sets `entry.gross = 0` for idempotency:

```rust
let gross = entry.gross;
entry.gross = 0; // idempotency guard
env.storage().persistent().set(&bet_key, &entry);
```

But `entry.net` is **not** zeroed. After a refund, `get_bet` (line ~720) still reports `amount: e.net` and `get_user_bet_count` still reports `e.count`, while `get_bet_gross` reports `0`. The bet appears **partially refunded** — the net is still "staked" in the market's `total_yes`/`total_no` (which are never decremented on cancel), so the market's totals and the user's bet record are **mutually inconsistent**.

## Why it's unsolvable by a localized patch

- **Zeroing `net` too** would break `get_bet`'s ABI contract (the frontend reads `amount`).
- **Decrementing `market.total_yes/total_no`** on refund requires iterating all refunded bets to recompute totals, or tracking per-bet side contributions — a storage change.
- **The `Market` struct** stores only aggregate `total_yes`/`total_no`, not per-bet breakdowns, so you cannot know how much to decrement without the bet's side and net.

The root problem is that **refund and market-total accounting are decoupled**. A correct fix requires either decrementing totals per-refund (needs per-bet side tracking) or a separate refund ledger — a storage-model change.