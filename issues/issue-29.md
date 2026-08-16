# [HIGH] No referral chain depth limit — unbounded sybil amplification and fee farming

## Summary

The referral system has **no depth limit** on the referral graph. A user can register with a referrer, who can register with another referrer, and so on, forming an **unbounded chain**. There is no check for:

- Maximum referral depth.
- Cycles (A refers B, B refers A).
- Whether the referrer is already in the user's chain.

Combined with the welcome bonus (`WELCOME_BONUS_TOKENS` + `WELCOME_BONUS_POINTS` minted on every `register_referral`, lines ~155–165) and the per-bet referral fee, an attacker can:

1. Create a chain of N self-controlled addresses.
2. Each registers the previous as referrer, minting `WELCOME_BONUS_TOKENS` each time.
3. Each bets, paying referral fees up the chain to the attacker's addresses.
4. The attacker farms unlimited PULSE and referral fees with no sybil resistance.

## Why it's unsolvable by a localized patch

- **Adding a depth limit** requires traversing the referral graph on registration — an O(depth) scan that can itself be gas-griefed.
- **Detecting cycles** requires graph traversal and storage of the full chain.
- **The welcome bonus** is minted unconditionally on registration, so every sybil address earns it.
- **A correct fix** requires a sybil-resistance mechanism (depth limit, KYC, or a registration fee) — a fundamental redesign of the referral incentive model.

The root problem is **no sybil resistance in the referral incentive**. A correct fix is a policy/design change, not a code patch.