# [CRITICAL] Systemic accounting & state-invariant violation across prediction_market ↔ referral_registry ↔ leaderboard — fee theft, leaderboard displacement, and unbounded inflation

## Summary

This is not a single bug but a **family of interacting state-invariant violations** that, taken together, allow an attacker (or even a sequence of ordinary users) to:

1. **Steal platform fees** from `AccumulatedFees` on cancelled markets (the `cancel_market` reclaim math is inconsistent with what is actually held).
2. **Displace a high-points player from the top-50 leaderboard with a lower-points player** (the `MinPoints`/`MinSlot` cache in `upsert_top` goes stale and is never recomputed in the partial-list regime).
3. **Permanently divert referral fees away from a referrer** (the `HasReferrer` cache is never invalidated, so a user who registers a referrer *after* their first bet never pays them).
4. **Mint PULSE without any supply cap** (every `claim` mints `WIN_TOKENS`/`LOSE_TOKENS` with no ceiling, and `reward`/`reward_bonus` are callable by two different trusted callers with divergent accounting).

Each individual defect is subtle; the *combination* is what makes this effectively unsolvable without a coordinated redesign of the storage model, the fee lifecycle, and the cross-contract trust boundaries.

---

## 1. `cancel_market` over-reclaims fees the platform never held (fee theft / accounting inversion)

**File:** `prediction_market/src/lib.rs`, `cancel_market` (lines ~551–566)

```rust
let net_pool = market.total_yes + market.total_no;
let fees_in_pool = net_pool * TOTAL_FEE_BPS / (BPS_DENOM - TOTAL_FEE_BPS);
let mut acc_fees: i128 = env.storage().instance().get(&DataKey::AccumulatedFees).unwrap_or(0);
acc_fees = if fees_in_pool < acc_fees { acc_fees - fees_in_pool } else { 0 };
```

`fees_in_pool` reclaims the **full 200 bps** (`TOTAL_FEE_BPS`) from `AccumulatedFees`. But `AccumulatedFees` only ever holds:

- the **150 bps** `platform_fee` for every bet, **plus**
- the **50 bps** `referral_fee` **only for bettors with no referrer** (see `place_bet`, lines ~391–420).

For any bettor **with a referrer**, the 50 bps referral fee was already transferred out to the referrer at bet time and is **not** in `AccumulatedFees`. Yet `cancel_market` subtracts the full 200 bps worth.

**Consequences:**

- If a cancelled market had any referrer-backed bets, `fees_in_pool > acc_fees`, so the code takes the `else { 0 }` branch and **zeroes the entire accumulator** — including the 150 bps platform fees from *other, unrelated, non-cancelled markets* that were legitimately earned. The platform silently loses those fees.
- Worse: if `withdraw_fees` was already called (accumulator is `0`), the reclaim is a no-op and the platform **keeps** the fees from a market it just cancelled — while `cancel_refund` still refunds the full gross from the contract's XLM balance. The contract's XLM balance and `AccumulatedFees` are now **mutually inconsistent**: the platform is simultaneously "refunding" fees it never returned and "keeping" fees it should have returned.

**Why it's hard:** The correct reclaim depends on per-bet referrer state that is not stored on the market. You cannot reconstruct it from `Market` alone. Fixing it requires either (a) tracking per-market fee composition, (b) clawing back referral fees from referrers (impossible — they're already spent), or (c) changing the fee model so the platform never holds referral fees it can't return. All three are invasive.

---

## 2. `upsert_top` stale `MinPoints`/`MinSlot` cache → low-points player displaces a high-points player

**File:** `leaderboard/src/lib.rs`, `upsert_top` (lines ~478–586)

The top-50 list maintains a cached `MinPoints`/`MinSlot` (instance storage) to avoid a full O(n) scan on every update. The cache is only recomputed in two places:

- the **append path** when `new_points < cur_min` (lines ~528–538), and
- the **in-place path** only when `count >= MAX_TOP_PLAYERS && s == cached_min_slot` (lines ~499–502).

**The bug:** In the **partial-list regime** (`count < MAX_TOP_PLAYERS`), if a player *already in the list* increases their points and they happen to be the current min, the cache is **not** recomputed (the `count >= MAX_TOP_PLAYERS` guard is false). `MinPoints`/`MinSlot` now point to a player who is no longer the minimum.

When the list later fills to 50 and a **new** player arrives, the full path reads the stale `min_pts`:

```rust
if new_points <= min_pts { return; }   // stale min is too LOW → new player passes
...
// overwrite the slot at stale min_slot — which now holds a HIGH-points player
```

**Concrete exploit:**

1. Fill the list to 49 players. Player A is the min with 100 pts (slot 0).
2. Player A wins more bets → 500 pts. In-place update. `count (49) >= 50` is false → **no recompute**. `MinPoints` stays `100`, `MinSlot` stays `0`.
3. Player B (new) arrives with 200 pts → appended (count = 50, full). `200 < 100`? No → min cache still stale (`100`/`0`).
4. Player C (new) arrives with **120 pts**. Full path: `120 <= 100`? No → C is admitted and **overwrites slot 0**, evicting A (500 pts). A's `TopPlayerSlot` is removed; A is gone from the top-50 despite having 500 pts, while C (120 pts) is in.

`get_top_players` and `get_rank` now return a leaderboard that is **provably wrong** — a 120-pt player ranks above a 500-pt player, and the 500-pt player's rank is `0`.

**Why it's hard:** The cache is an optimization that trades correctness for gas. Any fix that recomputes the min on every in-place update reintroduces the O(n) scan the cache was designed to avoid. A correct fix needs a different data structure (e.g., a sorted index, a heap, or per-slot dirty flags) — a non-trivial redesign of the storage model. The existing tests (`test_top_players_capped_at_50`) only cover monotonically-increasing *new* players and never exercise the in-place-update-then-fill sequence, so the bug is invisible to the current suite.

---

## 3. `HasReferrer` cache is never invalidated → referral fees permanently diverted

**File:** `prediction_market/src/lib.rs`, `place_bet` (lines ~391–416)

```rust
let cached: Option<bool> = env.storage().persistent().get(&hr_key);
let paid_referrer = if cached == Some(false) {
    false
} else {
    xlm.transfer(&this, &cfg.referral, &referral_fee);
    let result: bool = env.invoke_contract(...);   // referral.credit(...)
    if cached.is_none() {
        env.storage().persistent().set(&hr_key, &result);  // cached forever
    }
    result
};
if !paid_referrer { acc_fees += referral_fee; }
```

Once `HasReferrer(user)` is cached as `false` (first bet with no referrer), it is **never updated**. If the user later calls `referral_registry.register_referral` (which is allowed — nothing prevents it), every subsequent bet:

- skips the `credit` call entirely,
- diverts the 50 bps referral fee into `AccumulatedFees` instead of to the referrer.

The referrer is silently defrauded, and the platform's `AccumulatedFees` is inflated by fees it has no right to. There is no `set_referrer`/`invalidate_referrer` path anywhere in the system.

**Why it's hard:** The cache exists to save a cross-contract call on every bet. Invalidating it requires either (a) the referral registry to notify the market on registration (new cross-contract call + new trust surface), (b) removing the cache and paying the gas on every bet, or (c) a versioned cache keyed by a registration epoch. All change the gas profile and the trust model.

---

## 4. Cross-contract reentrancy & state-ordering in `place_bet` / `credit`

**File:** `prediction_market/src/lib.rs` `place_bet` (lines ~378–441) and `referral_registry/src/lib.rs` `credit` (lines ~169–233)

`place_bet` performs, in order:

1. `xlm.transfer(user → this)` — external call
2. `xlm.transfer(this → referral, referral_fee)` — external call
3. `env.invoke_contract(referral, "credit", ...)` — external call, which itself:
   - `xlm.transfer(this → referrer, referral_fee)` — **external call to an arbitrary referrer address**
   - `env.invoke_contract(leaderboard, "add_bonus_pts", ...)` — external call
4. **only then** writes the `BetEntry` and updates market totals.

The referrer in step 3 is an **arbitrary user-supplied address** (from `register_referral`). If the referrer is a contract, it can reenter `place_bet`, `claim`, or `cancel_refund` **before** the current bet's `BetEntry` is written and before `market.total_yes/total_no` are updated. The contract's XLM balance is already credited (step 1) and partially debited (step 2), so a reentrant `claim`/`cancel_refund` observes a **partially-updated state** — the classic check-effects-interaction violation. Soroban's auth model does not prevent reentrancy between *different* contracts; it only authenticates the initiating call.

**Why it's hard:** Fixing requires moving all state writes (BetEntry, market totals, AccumulatedFees) *before* any external call, and/or adding a reentrancy guard. But the fee flow *requires* the external `credit` call to determine `paid_referrer` before `AccumulatedFees` can be finalized — a circular dependency that forces a redesign of the fee accounting.

---

## 5. Unbounded PULSE minting (no supply cap)

**File:** `pulse_token/src/lib.rs` `mint` (lines ~88–114) and `leaderboard/src/lib.rs` `reward`/`reward_bonus` (lines ~152–288)

`mint` has **no supply ceiling**. Every `claim` mints `WIN_TOKENS` (10 PULSE) or `LOSE_TOKENS` (2 PULSE) per participant, and every `register_referral` mints `WELCOME_BONUS_TOKENS` (1 PULSE). With no cap, `total_supply` grows without bound. Additionally, `reward` (market-trusted) and `reward_bonus` (referral-trusted) are two independent minting entry points with **different** accounting semantics (`reward` increments `won_bets`/`lost_bets`; `reward_bonus` does not) — so the same user can be rewarded through both paths with no reconciliation, and `total_bets` (derived as `won + lost`) silently undercounts bonus-only activity.

**Why it's hard:** Adding a cap requires deciding who enforces it (token vs. leaderboard), migrating the mint authority model, and reconciling the two reward paths — all while preserving the existing ABI and deployed storage.

---

## Reproduction sketch (single test harness)

A single Soroban test that chains all four defects:

1. Create a market; user A bets with a referrer R (referral fee paid to R).
2. Cancel the market → `AccumulatedFees` is zeroed even though it held A's 150 bps platform fee (defect 1).
3. Fill the leaderboard to 49; make the min player win more; add a 120-pt player that evicts the 500-pt min (defect 2).
4. User B bets with no referrer (cache `false`), then registers referrer R, then bets again → R gets nothing, fee diverted (defect 3).
5. Observe `total_supply` growing with no bound across repeated claims (defect 5).

No single fix addresses more than one defect, and several fixes are mutually exclusive (e.g., removing the `HasReferrer` cache to fix defect 3 increases gas, which the `upsert_top` cache in defect 2 was itself designed to avoid).

---

## Suggested triage

This should be treated as a **systemic redesign** rather than a patch queue. A correct resolution likely requires:

- A per-market fee-composition ledger (or a fee-clawback mechanism) for `cancel_market`.
- A sorted/self-healing top-list data structure that never relies on a stale min cache.
- A versioned or event-driven referrer cache.
- A strict check-effects-interaction ordering in `place_bet` with a reentrancy guard.
- A supply cap and a unified reward-accounting path.

Because the defects are interdependent and touch the storage model, the trust boundaries, and the gas profile simultaneously, this issue is **not resolvable by a localized patch** — it demands a coordinated, cross-contract refactor with new invariants and a new test suite.