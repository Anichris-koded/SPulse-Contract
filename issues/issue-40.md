# [HIGH] Trust model has no revocation — a compromised or malicious trusted contract can never be removed

## Summary

The contracts establish **one-way trust** at initialization:

- `prediction_market` trusts `cfg.referral` and `cfg.leaderboard` (via `Config`).
- `leaderboard` trusts `MarketContract` and `ReferralContract` (via `set_contracts`).
- `referral_registry` trusts `MarketContract`, `LeaderboardContract`, `TokenContract`, `XlmSacContract`.

There is **no revocation mechanism** — once a contract is trusted, it can only be re-pointed by the admin (issue #5/#6), and there's no way to **remove** a trusted contract entirely (only replace it). If a trusted contract is compromised or found malicious, the system cannot sever the trust link; it can only swap in a new address, which requires the admin to know the replacement and coordinate the upgrade (issue #39).

## Why it's unsolvable by a localized patch

- **Adding a revocation function** requires each contract to support "no trusted contract" state and handle calls to it gracefully — a cross-cutting change.
- **The trust is one-way and implicit** — there's no capability/role model, just hardcoded address checks.
- **A correct fix** requires a capability-based or role-based trust model with revocation — a fundamental redesign of the cross-contract auth.

The root problem is **static, one-way, non-revocable trust**. A correct fix is a systemic auth redesign.