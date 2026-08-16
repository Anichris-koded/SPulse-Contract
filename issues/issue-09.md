# [CRITICAL] Persistent storage TTLs can expire before claims/refunds — user funds permanently locked

## Summary

All market, bet, and bettor-index entries are stored in **persistent storage** with a TTL bump of `TTL_BUMP = 3_153_600` (~1 year) and `TTL_HIGH = 6_307_200` (~2 years) (`prediction_market/src/lib.rs`, lines ~27–28). The TTL is extended **only at write time** (e.g., `place_bet` line ~444, `create_market` line ~318).

There is **no mechanism to extend TTL on read**, and no keeper/refresher. If a market is resolved but a winner does not claim within the TTL window, the `BetEntry` and `Market` keys **expire and are deleted** by the ledger. The user's payout is then **permanently unrecoverable** — `claim` returns `MarketNotFound`/`NoBetFound`, and the funds remain stranded in the contract.

## Why it's unsolvable by a localized patch

- **Extending TTL on read** (`claim`, `cancel_refund`) helps only if the user claims *before* expiry — it cannot resurrect an already-expired key.
- **A keeper/refresher** is an off-chain component that must be trusted and funded; it is not part of the contract and can fail or be abandoned.
- **The TTL is a fixed constant** — there is no per-market or per-bet expiry policy, and no way to know when a key will expire without tracking its last-bump time.
- **Soroban's TTL model** means the contract cannot "own" data indefinitely without paying for it; the design must decide who pays for storage longevity.

The root problem is that **user funds depend on storage that can silently vanish**. A correct fix requires a TTL-refresh strategy (read-bump, keeper, or a storage-rental model) and a migration of existing entries — a systemic storage-lifecycle redesign.

## Related

The same TTL-expiry risk applies to `leaderboard` stats/top-list entries (issue #21) and `referral_registry` counters (issue #28).