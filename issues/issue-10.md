# [MEDIUM] `create_market` allows `duration_secs = 0` and has no minimum duration — instantly-expired and griefable markets

## Summary

`create_market` (`prediction_market/src/lib.rs`, lines ~278–325) computes `end_time = env.ledger().timestamp() + duration_secs` with **no validation** that `duration_secs > 0`:

```rust
let end_time = env.ledger().timestamp() + duration_secs;
```

If `duration_secs = 0`, the market is **instantly expired** — `place_bet` immediately returns `MarketExpired` (line ~349), so no one can ever bet. The admin can create such markets freely (subject only to the hourly rate limit), and there is no minimum-duration enforcement.

## Why it's unsolvable by a localized patch

- **Adding a `duration_secs > 0` check** is trivial, but it doesn't address the deeper issue: there is **no minimum meaningful duration**, so an admin can create a market that expires in 1 second, front-run a bet, and resolve it — a market-manipulation vector.
- **The rate limit** (`MAX_MARKETS_PER_HOUR`) limits *count*, not *duration*, so an attacker can flood the market list with useless instantly-expired markets, degrading the UI and the `get_market_count`-based indexers.
- **There is no market-creation fee**, so creating garbage markets is free (only gas).

The root problem is that **market creation has no economic or temporal guardrails**. A correct fix requires a minimum duration, a creation fee, and/or a maximum concurrent-market cap — a policy change, not a code fix.

## Related

The same lack of guardrails affects `resolve_market` (issue #3) and the empty-side sweep.