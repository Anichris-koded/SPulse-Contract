# [MEDIUM] `ReferralCount`/`ReferralEarnings` TTLs are never extended — referral history can silently vanish

## Summary

`register_referral` writes `ReferralCount` (line ~146) and `credit` writes `ReferralEarnings` (line ~211) to **persistent storage**, but **neither is ever TTL-extended**:

```rust
env.storage().persistent().set(&DataKey::ReferralCount(ref_addr.clone()), &(count + 1));
// no extend_ttl
...
env.storage().persistent().set(&DataKey::ReferralEarnings(ref_addr), &(earnings + referral_fee));
// no extend_ttl
```

If a referrer is inactive for the TTL period, their `ReferralCount` and `ReferralEarnings` **expire and are deleted**. The referrer's historical referral record is permanently lost — `get_referral_count` and `get_earnings` return `0` even though they referred many users. This is a data-integrity issue (related to issue #9).

## Why it's unsolvable by a localized patch

- **Extending TTL on every write** adds gas and doesn't help if the referrer is inactive.
- **The counters are per-referrer** and there's no keeper to refresh them.
- **A correct fix** requires a TTL-refresh strategy (read-bump, keeper, or storage-rental) — a systemic storage-lifecycle change (issue #9).

The root problem is **no TTL lifecycle management for referral counters**. A correct fix is part of the broader storage-lifecycle redesign.