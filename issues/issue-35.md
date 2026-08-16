# [LOW] `set_minter`/`remove_minter` have no idempotency or authorization check on the minter — redundant and confusing minter state

## Summary

`set_minter` (`pulse_token/src/lib.rs`, lines ~70–77) and `remove_minter` (lines ~79–86) are admin-only, but:

- `set_minter` doesn't check if the minter is **already** authorized — calling it twice is a no-op that still costs gas.
- `remove_minter` doesn't check if the minter **was** authorized — removing a non-minter is a silent no-op.
- There's no way to **list** authorized minters, so the admin can't audit who can mint.

More importantly, the test setup (`prediction_market/src/tests.rs` lines ~82–85) authorizes **three** minters (leaderboard, market, referral) even though only the leaderboard actually mints (via `reward`/`reward_bonus`). The market and referral are authorized as minters but never use it — a **latent privilege** that becomes a minting vector if either contract is compromised or upgraded (issue #5).

## Why it's unsolvable by a localized patch

- **Adding idempotency checks** is trivial but doesn't address the redundant-minter-privilege problem.
- **Removing the unused minter authorizations** requires a coordinated change to the deployment scripts and tests.
- **A correct fix** requires a minter-audit function and a least-privilege review of who actually needs minting rights — a governance/security review.

The root problem is **over-privileged and un-auditable minter state**. A correct fix needs a minter registry and least-privilege enforcement.