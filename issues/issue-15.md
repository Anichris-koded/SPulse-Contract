# [LOW] `MIN_BET` is checked on gross, but the staked amount is net — fee-dependent minimum stake inconsistency

## Summary

`place_bet` (`prediction_market/src/lib.rs`, lines ~337–339) checks `amount < MIN_BET` on the **gross** amount, but the actual staked amount is `net = amount * NET_NUMERATOR / BPS_DENOM` (line ~373):

```rust
if amount < MIN_BET { return Err(MarketError::BetTooSmall); }
...
let net = amount * NET_NUMERATOR / BPS_DENOM;
```

So a bet of exactly `MIN_BET` (1 XLM gross) stakes only `0.98 XLM` net. The effective minimum stake is **below** the advertised `MIN_BET`, and the discrepancy is fee-dependent. This is a minor accounting inconsistency, but it compounds with the payout rounding (issue #2) and the fee-reclaim math (issue #1).

## Why it's unsolvable by a localized patch

- **Checking `net < MIN_BET`** instead changes the effective minimum and the fee math, and requires computing `net` before the check (which is already done later).
- **The real issue** is that `MIN_BET`, `net`, `gross`, and the fee constants are **four separate values** with no single source of truth, so any change to one silently affects the others.

The root problem is **fragmented fee/stake constants**. A correct fix requires a single fee model where gross, net, and minimum are derived from one source — a refactor of the fee constants.