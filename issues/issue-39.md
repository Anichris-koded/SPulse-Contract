# [HIGH] No upgrade coordination between contracts — independent upgrades can break the cross-contract ABI

## Summary

Each of the four contracts can be upgraded **independently** via its own `upgrade()` (issue #5). There is **no coordination** or versioning between them. The contracts are tightly coupled through cross-contract calls:

- `prediction_market` calls `referral.credit` and `leaderboard.reward`.
- `referral_registry` calls `leaderboard.reward_bonus` and `leaderboard.add_bonus_pts`.
- `leaderboard` calls `token.mint`.

If any contract is upgraded to a version with a **different function signature, argument order, or return type**, the calling contract's `env.invoke_contract` will **fail or misbehave** — e.g., `credit` returning a different type, `reward` changing its parameter order, or `mint` changing its ABI. Because there's no version check or interface contract, a single unilateral upgrade can **brick the entire system** (all bets, claims, and referrals fail).

## Why it's unsolvable by a localized patch

- **Adding version checks** requires each contract to verify the caller's interface version before invoking — new cross-contract calls and storage.
- **Coordinating upgrades** requires a deployment orchestration layer and a migration plan — an operational/architectural change.
- **The ABI is implicit** — there's no interface definition or versioning mechanism in the code.
- **A correct fix** requires an interface-versioning scheme and coordinated upgrade tooling — a cross-contract architectural change.

The root problem is **no interface contract or upgrade coordination**. A correct fix is a systemic governance/deployment redesign.