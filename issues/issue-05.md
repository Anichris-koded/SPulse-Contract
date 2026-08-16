# [CRITICAL] `upgrade()` allows arbitrary WASM replacement with no timelock, delay, or multi-sig — single compromised admin key = total loss

## Summary

Every contract exposes an `upgrade(admin, new_wasm_hash)` that calls `env.deployer().update_current_contract_wasm(new_wasm_hash)` with **no delay, no timelock, no quorum, and no validation** of the new WASM:

- `prediction_market/src/lib.rs` line ~184
- `leaderboard/src/lib.rs` line ~116
- `referral_registry/src/lib.rs` line ~98
- `pulse_token/src/lib.rs` line ~66

A single compromised admin key (or a malicious admin) can atomically replace any contract's bytecode with arbitrary logic — e.g., a version that drains all XLM, mints unlimited PULSE, or rewrites `AccumulatedFees` — in **one transaction**, with **no warning to users** and **no way to revert**.

## Why it's unsolvable by a localized patch

- **Adding a timelock** requires a new contract (or a delay state machine) and changes the upgrade UX; it also doesn't stop a *compromised* admin, only a *rushed* one.
- **Multi-sig** requires replacing the single `Address` admin model with a threshold-signature scheme — a fundamental change to the auth model across all four contracts.
- **Validating the new WASM** is impossible in general — you cannot prove arbitrary bytecode is safe.
- **The upgrade is irreversible** — `update_current_contract_wasm` replaces the code in place; there is no rollback unless the new code itself provides one.

The deeper problem is that **admin == absolute power** with no separation of duties, no upgrade delay, and no emergency circuit-breaker. This is a systemic trust-model flaw, not a code bug.

## Suggested direction

A correct fix requires a **governance layer**: a timelock + multi-sig + upgrade-announcement mechanism, and ideally a proxy/beacon pattern so upgrades are opt-in and reversible. This is a cross-contract architectural change affecting all four contracts and their deployment scripts.