# [MEDIUM] Legacy key migration creates inconsistent state between pre-upgrade and post-upgrade users

## Summary

The referral registry supports **two storage layouts** for user profiles:

- **Legacy** (pre-upgrade): separate `Registered`, `DisplayName`, `Referrer` keys.
- **New** (post-upgrade): a single packed `Profile` key.

`load_profile` (`referral_registry/src/lib.rs`, lines ~235–265) reads the new `Profile` first, then falls back to the legacy keys. But the two layouts are **not reconciled**:

- A legacy user's `Referrer`/`DisplayName` are **never migrated** to the new `Profile` key.
- `is_registered` returns `true` for both, but `get_referrer`/`get_display_name` read different keys depending on which layout the user was created under.
- If a legacy user's `Registered` key expires via TTL (issue #9/#28) but their `Profile` was never written, they become **unregistered** — their referral relationship is lost.

## Why it's unsolvable by a localized patch

- **Migrating legacy users** requires an admin migration function that iterates all legacy users — unbounded and gas-heavy.
- **The dual-layout read** (`load_profile`) is a permanent compatibility shim that can never be removed without breaking legacy users.
- **TTL expiry** (issue #9) deletes legacy keys with no migration hook.

The root problem is **a storage-layout migration with no migration path**. A correct fix requires a one-time migration and a single canonical layout — a storage-migration project.