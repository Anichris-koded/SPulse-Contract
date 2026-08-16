# [HIGH] No pause/circuit-breaker mechanism anywhere — cannot halt the system in an emergency

## Summary

None of the four contracts has a **pause** or **circuit-breaker** function. In the event of a critical vulnerability (e.g., issue #1, #3, #27), there is **no way to halt betting, claims, or withdrawals** while a fix is deployed. The only recourse is a full `upgrade` (issue #5), which itself is irreversible and requires a trusted admin.

This means:

- A discovered exploit **cannot be contained** — funds continue to flow until the upgrade is deployed.
- There is no "kill switch" to stop a malicious resolver (issue #3) or a reentrancy attack (issue #27) in progress.
- Users have no way to know the system is compromised (no events, issue #7).

## Why it's unsolvable by a localized patch

- **Adding a pause flag** to each contract requires a coordinated change across all four contracts and their callers.
- **A pause must be respected by every state-changing function** — a cross-cutting change.
- **The pause authority** is the admin, which is the same single point of failure (issue #5).
- **A correct fix** requires a pause mechanism, an emergency-withdrawal path, and a governance layer — an architectural addition.

The root problem is **no emergency response capability**. A correct fix is a systemic safety feature, not a bug fix.