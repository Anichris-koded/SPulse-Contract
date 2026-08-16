# [HIGH] `withdraw_fees` lets a fee recipient drain the entire accumulator to an arbitrary address with no cap or provenance check

## Summary

`withdraw_fees` (`prediction_market/src/lib.rs`, lines ~683–711) allows any admin or fee recipient to withdraw the **entire** `AccumulatedFees` to **any** `recipient` address:

```rust
pub fn withdraw_fees(env: Env, caller: Address, recipient: Address) -> Result<i128, MarketError> {
    caller.require_auth();
    Self::require_admin_or_fee_recipient(&env, &caller)?;
    let fees: i128 = env.storage().instance().get(&DataKey::AccumulatedFees).unwrap_or(0);
    ...
    token::Client::new(&env, &cfg.xlm_sac).transfer(&env.current_contract_address(), &recipient, &fees);
    env.storage().instance().set(&DataKey::AccumulatedFees, &0_i128);
}
```

There is **no cap**, **no per-market attribution** (issue #4), **no delay**, and **no check** that `recipient` is a legitimate fee recipient. A single fee recipient can drain the entire global accumulator — including swept user principal (issue #3) — to any address in one call.

## Why it's unsolvable by a localized patch

- **Restricting `recipient`** to a whitelist requires a registry and doesn't stop a *compromised* fee recipient.
- **Capping the withdrawal** requires knowing the per-market fee composition, which doesn't exist (issue #4).
- **Adding a delay** requires a withdrawal-request state machine and changes the UX.
- **The global accumulator** (issue #4) is the root cause — you cannot safely withdraw "fees" when the bucket also holds user principal.

The root problem is **unbounded, unprovenanced, immediate withdrawal** of a fungible pool. A correct fix requires per-market fee accounting, a withdrawal cap/delay, and recipient validation — an architectural change.