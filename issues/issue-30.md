# [MEDIUM] `register_referral` has no `display_name` length limit — storage DoS via oversized strings

## Summary

`register_referral` (`referral_registry/src/lib.rs`, lines ~112–167) accepts a `display_name: String` with **no length validation**:

```rust
env.storage().persistent().set(
    &DataKey::Profile(user.clone()),
    &UserProfile { display_name, referrer: referrer.clone() },
);
```

A user can register with an arbitrarily large `display_name` (up to Soroban's max string size), consuming a large amount of persistent storage per registration. An attacker can register many accounts with huge names, inflating storage costs and potentially exhausting the contract's storage budget or making reads (`get_display_name`, `load_profile`) expensive.

## Why it's unsolvable by a localized patch

- **Adding a length cap** is trivial, but the deeper issue is that **storage cost is not charged to the user** — the contract pays for the storage, so an attacker can grief the contract's storage budget.
- **The `UserProfile`** is read on every `credit` call (via `load_profile`), so oversized profiles make every bet more expensive.
- **A correct fix** requires a length cap AND a storage-cost model (charging users for their storage footprint) — a policy change.

The root problem is **unbounded user-controlled storage with no cost recovery**. A correct fix needs input validation and a storage-rental model.