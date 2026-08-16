# [MEDIUM] No cap on referral earnings — unbounded fee accumulation per referrer

## Summary

`credit` (`referral_registry/src/lib.rs`, lines ~206–214) accumulates `ReferralEarnings` with **no cap**:

```rust
let earnings: i128 = env.storage().persistent().get(&DataKey::ReferralEarnings(ref_addr.clone())).unwrap_or(0);
env.storage().persistent().set(&DataKey::ReferralEarnings(ref_addr), &(earnings + referral_fee));
```

A referrer can earn an **unbounded** amount of referral fees. Combined with the sybil vector (issue #29) and the unregistered-referrer vector (issue #26), an attacker can route unlimited referral fees to a single address. There is no:

- Per-referrer earnings cap.
- Referral-count cap.
- Anti-abuse mechanism (e.g., requiring the referred user to actually bet a minimum amount).

## Why it's unsolvable by a localized patch

- **Adding a cap** requires deciding the cap value and enforcing it across the `credit` path — a policy change.
- **The earnings are paid in XLM** at bet time (line ~186), so they're already spent before any cap could be enforced retroactively.
- **A correct fix** requires an anti-abuse policy (caps, minimums, or sybil resistance) — a redesign of the referral incentive model (issue #29).

The root problem is **no economic guardrails on referral rewards**. A correct fix is a policy/design change.