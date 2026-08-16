# [MEDIUM] `check_rate` underflows on ledger timestamp regression — rate-limit bypass or panic

## Summary

`check_rate` (`prediction_market/src/lib.rs`, lines ~849–870) computes:

```rust
let (new_ws, new_cnt) = if now - ws < 3600 {
```

where `ws` is the stored window start and `now` is `env.ledger().timestamp()`. If `now < ws` (ledger timestamp regression, or a test/network that rewinds time), `now - ws` **underflows** (u64 arithmetic) and wraps to a huge value, which is `>= 3600`, so the code takes the `else` branch and resets the window — **bypassing the rate limit**. In a debug build this could panic; in release it silently resets.

## Why it's unsolvable by a localized patch

- **Adding a `now >= ws` guard** fixes the underflow but not the underlying assumption that ledger time is monotonic — which Soroban does not strictly guarantee across all contexts.
- **The rate limit is per-admin** (it's checked in `create_market` which is admin-only), so the impact is limited to admin abuse — but the same pattern is the *only* spam guard, and its correctness is time-dependent.
- **A robust fix** requires a monotonic counter or a window that tolerates clock skew, which changes the storage layout.

The root problem is **relying on wall-clock arithmetic for a security control**. A correct fix needs a skew-tolerant window design.