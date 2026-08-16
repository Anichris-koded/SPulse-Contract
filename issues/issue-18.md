# [HIGH] `reward` and `add_pts` are both callable by the market contract with divergent semantics — double-counting and token-supply divergence

## Summary

The leaderboard exposes **two** point-adding functions, both gated by `require_market_contract`:

- `reward` (`leaderboard/src/lib.rs`, lines ~152–205): adds points, increments `won_bets`/`lost_bets`, **and mints PULSE**.
- `add_pts` (lines ~208–241): adds points, increments `won_bets`/`lost_bets`, but does **not** mint.

The market contract currently calls `reward` (in `claim`, line ~665). But because `add_pts` is also authorized for the market, **any future or buggy call path** that uses `add_pts` instead of `reward` silently skips token minting — the same user action produces different token supply depending on which function is called. There is no invariant enforcing that `reward` is the only minting path, and no reconciliation between the two.

## Why it's unsolvable by a localized patch

- **Removing `add_pts`** breaks ABI compatibility (the comment says it's kept for ABI compat).
- **Making `add_pts` mint too** would double-mint if both are ever called.
- **The two functions** have no shared state to detect divergence — `won_bets`/`lost_bets` are updated identically, so you can't tell which path was used.

The root problem is **two authorized entry points with different side effects**. A correct fix requires a single canonical reward path and a deprecation/migration of `add_pts` — a cross-contract ABI decision.