# [CRITICAL] `set_config` can re-point the market to malicious token/referral/leaderboard contracts with no validation — total fund theft

## Summary

`set_config` (`prediction_market/src/lib.rs`, lines ~190–210) lets the admin replace the `Config` (token, referral, leaderboard, xlm_sac) with **arbitrary addresses**, with **no validation** that they are the expected contracts. Combined with the fact that `place_bet` and `claim` trust these addresses implicitly:

- `place_bet` transfers XLM to `cfg.referral` and invokes `credit` on it (line ~398).
- `claim` invokes `reward` on `cfg.leaderboard` (line ~665) and transfers payouts via `cfg.xlm_sac`.

If `cfg.referral` is pointed to a malicious contract, every bet's referral fee is sent to the attacker. If `cfg.leaderboard` is malicious, `claim`'s reward call can be made to revert (locking claims) or to mint to the attacker. If `cfg.xlm_sac` is malicious, payouts and fee withdrawals are drained.

## Why it's unsolvable by a localized patch

- **Validating addresses** requires a registry of "known good" contract addresses, which itself must be trusted and updatable — a new trust component.
- **The admin is already trusted** (see issue #5), so this is the same root problem: a single admin key controls the entire fund flow.
- **Adding per-call checks** (e.g., verifying the referral contract's code hash before each call) is prohibitively expensive on-chain and still racy.

The fundamental issue is that the market contract holds **no invariant** about its dependencies — it blindly trusts whatever `Config` says. A correct fix requires either a **dependency registry with code-hash pinning**, a **proxy/beacon pattern**, or a **multi-sig governance layer** (issue #5). All are architectural.