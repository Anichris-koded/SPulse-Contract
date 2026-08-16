# [MEDIUM] `OppositeSideBet` permanently locks a user to one side of a market — no hedging, no rebalancing

## Summary

`place_bet` (`prediction_market/src/lib.rs`, lines ~358–365) rejects any bet on the opposite side of a user's existing bet:

```rust
if e.is_yes != is_yes {
    return Err(MarketError::OppositeSideBet);
}
```

Once a user bets "yes" on a market, they can **never** bet "no" on that same market — even to hedge, rebalance, or correct a mistake. There is no way to close or reverse a position. This is a **design limitation** that:

- Prevents legitimate hedging strategies.
- Locks users into a position they cannot exit, even if the market moves against them.
- Is inconsistent with the `cancel_refund` path (which only works on cancelled markets, not resolved ones).

## Why it's unsolvable by a localized patch

- **Removing the check** would allow a user to bet both sides, which breaks the payout math: a user could bet `X` on yes and `Y` on no, and the `claim` logic (`is_winner = entry.is_yes == market.outcome`) would only pay one side, but the user's `net` on the losing side is still counted in `total_pool`, inflating the pool and diluting other winners.
- **Adding a position-close function** requires tracking per-bet side and net to refund the losing side — a new state machine and storage.
- **The `BetEntry`** stores a single `is_yes` and a single `net`; it cannot represent a two-sided position.

The root problem is that **the bet model is one-sided and irreversible**. A correct fix requires a position model (open/close, two-sided) — a fundamental redesign of the betting state.