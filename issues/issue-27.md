# [CRITICAL] `credit` transfers XLM to an arbitrary referrer address — reentrancy and fund-drain vector

## Summary

`credit` (`referral_registry/src/lib.rs`, lines ~169–233) transfers the referral fee to the referrer, which is an **arbitrary user-supplied address**:

```rust
Some(ref_addr) => {
    token::Client::new(&env, &xlm_sac).transfer(&env.current_contract_address(), &ref_addr, &referral_fee);
    let _: Val = env.invoke_contract(&leaderboard, &Symbol::new(&env, "add_bonus_pts"), ...);
    ...
}
```

If the referrer is a **contract**, the `transfer` triggers its `receive`/`invoke` logic, which can **reenter** the market contract (`place_bet`, `claim`, `cancel_refund`) or the referral registry **before** the current operation's state is finalized. Because `place_bet` (issue #1/#4) performs external calls *before* writing the `BetEntry` and market totals, a reentrant call observes **partially-updated state** — the classic check-effects-interaction violation.

## Attack

1. Attacker deploys a malicious referrer contract.
2. Attacker registers it as a referrer (issue #26 allows unregistered referrers).
3. Attacker bets; `place_bet` transfers the referral fee to the malicious referrer, which reenters `claim`/`cancel_refund` on a market whose state is mid-update.
4. The reentrant call can claim/refund based on stale or inconsistent totals, draining funds.

## Why it's unsolvable by a localized patch

- **Soroban's auth model** authenticates the initiating call but does **not** prevent cross-contract reentrancy.
- **Moving state writes before external calls** (the standard fix) conflicts with the fee flow, which *requires* the `credit` call to determine `paid_referrer` before finalizing `AccumulatedFees` (issue #1/#4).
- **Restricting referrers to EOAs** is not possible on Soroban — any address can be a contract.

The root problem is **external calls to untrusted addresses before state finalization**. A correct fix requires a reentrancy guard and a check-effects-interaction reordering — a redesign of the bet/fee flow.