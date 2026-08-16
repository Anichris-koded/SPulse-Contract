# [MEDIUM] `credit` refunds the referral fee to the caller (market contract) when there's no referrer — fragile trust assumption

## Summary

In `credit` (`referral_registry/src/lib.rs`, lines ~217–231), when the user has no referrer, the referral fee is transferred **back to the caller**:

```rust
None => {
    if referral_fee > 0 {
        token::Client::new(&env, &xlm_sac).transfer(&env.current_contract_address(), &caller, &referral_fee);
    }
    Ok(false)
}
```

The `caller` is the market contract (verified by `require_market_contract`). The referral registry **trusts** that the market contract will correctly account for the returned fee (add it to `AccumulatedFees`). But:

- If the market contract is upgraded (issue #5) to a version that doesn't add the returned fee to `AccumulatedFees`, the fee is **silently lost** — it stays in the market contract's XLM balance, unreachable.
- The round-trip (market → referral → market) is wasteful and fragile; any change to either contract's accounting breaks the invariant.

## Why it's unsolvable by a localized patch

- **The refund-to-caller** is a cross-contract trust assumption with no on-chain enforcement that the caller accounts for it.
- **Removing the round-trip** requires the market to know the referrer status *before* transferring the fee — which is exactly the `HasReferrer` cache (issue #1/#3) that is itself buggy.
- **A correct fix** requires a single source of truth for referrer status and a fee flow that doesn't round-trip — a redesign of the fee lifecycle.

The root problem is **cross-contract fee accounting with no invariant enforcement**. A correct fix is part of the fee-lifecycle redesign (issue #1/#4).