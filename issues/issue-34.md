# [CRITICAL] PULSE token has no supply cap — unbounded inflation via rewards and referrals

## Summary

`mint` (`pulse_token/src/lib.rs`, lines ~88–114) has **no supply ceiling**:

```rust
pub fn mint(env: Env, minter: Address, to: Address, amount: i128) -> Result<(), TokenError> {
    ...
    let supply: i128 = env.storage().instance().get(&DataKey::TotalSupply).unwrap_or(0);
    env.storage().instance().set(&DataKey::TotalSupply, &(supply + amount));
}
```

Every `claim` mints `WIN_TOKENS` (10 PULSE) or `LOSE_TOKENS` (2 PULSE) per participant (`leaderboard::reward`), and every `register_referral` mints `WELCOME_BONUS_TOKENS` (1 PULSE) (`leaderboard::reward_bonus`). With no cap, `total_supply` grows **without bound**. This:

- Dilutes all existing holders indefinitely.
- Makes the token's value unpredictable and non-scarce.
- Combined with the sybil vector (issue #29), lets an attacker mint unlimited PULSE by creating endless referral chains and claiming rewards.

## Why it's unsolvable by a localized patch

- **Adding a cap** requires deciding who enforces it (token vs. leaderboard) and migrating the mint authority model.
- **The two minting paths** (`reward` and `reward_bonus`, issue #18) are independent and uncoordinated, so a cap in one doesn't bound the other.
- **A correct fix** requires a supply cap, a unified reward-accounting path, and a migration — a cross-contract redesign.

The root problem is **no monetary policy**. A correct fix is an economic/design change affecting the token and both reward paths.