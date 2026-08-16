# [MEDIUM] `register_referral` doesn't validate that the referrer is registered — unregistered referrers get paid

## Summary

`register_referral` (`referral_registry/src/lib.rs`, lines ~112–167) accepts any `referrer: Option<Address>` with **no check** that the referrer is a registered user:

```rust
if let Some(ref ref_addr) = referrer {
    if *ref_addr == user { return Err(ReferralError::SelfReferral); }
}
// no check that ref_addr is registered
```

A user can register with an **unregistered** referrer. When that user bets, `credit` (lines ~169–233) pays the referral fee to the unregistered referrer and increments their `ReferralCount`/`ReferralEarnings` — even though they never registered. This:

- Lets an attacker create a "referrer" address that is actually a contract they control, and route referral fees to it.
- Inflates `ReferralCount` for non-participants.
- Breaks the assumption that referrers are real, registered users.

## Why it's unsolvable by a localized patch

- **Requiring the referrer to be registered** requires a lookup that may not exist yet (the referrer registers *after* the user, or never).
- **The referral graph** has no depth limit (issue #29), so even registered referrers can be sybil.
- **The `credit` path** trusts the referrer address blindly and transfers XLM to it — a reentrancy vector (issue #27).

The root problem is **no validation of the referral relationship**. A correct fix requires referrer-registration enforcement and referral-graph validation — a design change to the referral model.