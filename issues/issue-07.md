# [HIGH] No event emission anywhere — off-chain indexers, UIs, and auditors cannot track state transitions

## Summary

None of the four contracts emit a single Soroban event (`env.events().publish(...)`). Every state transition — market creation, bets, resolution, cancellation, claims, fee withdrawals, referrals, mints, leaderboard updates — is **silent**.

This makes the system **un-auditable off-chain**:

- The frontend (`frontend/script.js`, `frontend/soroban.js`) must poll `get_market`/`get_bet`/`get_stats` and infer transitions, which is racy and expensive.
- There is no way to reconstruct the fee ledger, the payout history, or the referral graph from events.
- A malicious or buggy transition (e.g., a wrongful `resolve_market`) leaves **no on-chain trace** for users or auditors to detect.

## Why it's unsolvable by a localized patch

- **Adding events** is easy per-function, but doing it *correctly* requires deciding the event schema, the topics, and the data for **every** function across **all four** contracts — and retrofitting events onto already-deployed contracts requires an upgrade (issue #5).
- **The absence of events** is a systemic observability gap: even if you add events going forward, historical state is unrecoverable.
- **Indexers** (the frontend) would need a coordinated rewrite to consume the new event stream.

This is a cross-cutting observability/design flaw. A correct fix requires a coordinated event-schema design across all contracts plus an indexer migration — not a single-file patch.