use super::*;
use soroban_sdk::{
    contract, contractimpl,
    testutils::{storage::Persistent as _, Address as _, Events, Ledger, LedgerInfo},
    token::{Client as TokenClient, StellarAssetClient},
    Address, BytesN, Env, IntoVal, String, Symbol, TryFromVal, Val,
};

use leaderboard::LeaderboardContract;
use pulse_token::PULSETokenContract;
use referral_registry::ReferralRegistryContract;

// ── Test Infrastructure ───────────────────────────────────────────────────────

struct TestSetup {
    env: Env,
    client: PredictionMarketContractClient<'static>,
    admin: Address,
    xlm_admin: StellarAssetClient<'static>,
    xlm: TokenClient<'static>,
    token_client: pulse_token::PULSETokenContractClient<'static>,
    leaderboard_client: leaderboard::LeaderboardContractClient<'static>,
    referral_client: referral_registry::ReferralRegistryContractClient<'static>,
}

fn setup() -> TestSetup {
    let env = Env::default();
    env.mock_all_auths();
    env.cost_estimate().budget().reset_unlimited();

    env.ledger().set(LedgerInfo {
        timestamp: 1_000_000,
        protocol_version: 26,
        sequence_number: 100,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 100,
        min_persistent_entry_ttl: 100,
        max_entry_ttl: 10_000_000,
    });

    let admin = Address::generate(&env);

    let xlm_sac_id = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let xlm_admin = StellarAssetClient::new(&env, &xlm_sac_id);
    let xlm = TokenClient::new(&env, &xlm_sac_id);

    let token_id = env.register(PULSETokenContract, ());
    let token_client = pulse_token::PULSETokenContractClient::new(&env, &token_id);
    token_client.initialize(
        &admin,
        &String::from_str(&env, "PULSE"),
        &String::from_str(&env, "PLSE"),
        &7u32,
    );

    let leaderboard_id = env.register(LeaderboardContract, ());
    let leaderboard_client = leaderboard::LeaderboardContractClient::new(&env, &leaderboard_id);

    let referral_id = env.register(ReferralRegistryContract, ());
    let referral_client =
        referral_registry::ReferralRegistryContractClient::new(&env, &referral_id);

    let market_id = env.register(PredictionMarketContract, ());
    let client = PredictionMarketContractClient::new(&env, &market_id);

    client.initialize(
        &admin,
        &token_id,
        &referral_id,
        &leaderboard_id,
        &xlm_sac_id,
    );
    leaderboard_client.initialize(&admin, &market_id, &referral_id);
    referral_client.initialize(&admin, &market_id, &token_id, &leaderboard_id, &xlm_sac_id);

    // Lever G: the leaderboard now mints PULSE internally (one cross-call from
    // market/referral instead of two). It must know the token AND be authorized
    // as a minter. This mirrors the exact mainnet upgrade sequence.
    leaderboard_client.set_token_contract(&admin, &token_id);
    token_client.set_minter(&leaderboard_id);
    // Legacy minter auths kept harmless (market/referral no longer mint directly).
    token_client.set_minter(&market_id);
    token_client.set_minter(&referral_id);

    TestSetup {
        env,
        client,
        admin,
        xlm_admin,
        xlm,
        token_client,
        leaderboard_client,
        referral_client,
    }
}

fn fund_user(t: &TestSetup, user: &Address, amount: i128) {
    t.xlm_admin.mint(user, &amount);
}

fn create_test_market(t: &TestSetup) -> u64 {
    t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Will BTC hit 100k?"),
        &String::from_str(&t.env, "https://example.com/btc.png"),
        &Category::Crypto,
        &3600_u64,
    )
}

fn advance_time(env: &Env, secs: u64) {
    let current = env.ledger().timestamp();
    env.ledger().set(LedgerInfo {
        timestamp: current + secs,
        protocol_version: 26,
        sequence_number: env.ledger().sequence() + 1,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 100,
        min_persistent_entry_ttl: 100,
        max_entry_ttl: 10_000_000,
    });
}

/// Admin withdraw is capped per call (issue #57); loop until the pot is empty.
fn withdraw_all_admin_fees(t: &TestSetup, recipient: &Address) -> i128 {
    let mut total = 0;
    while t.client.get_accumulated_fees() > 0 {
        total += t.client.withdraw_fees(&t.admin, recipient);
    }
    total
}

fn rewind_time(env: &Env, secs: u64) {
    let current = env.ledger().timestamp();
    env.ledger().set(LedgerInfo {
        timestamp: current - secs,
        protocol_version: 26,
        sequence_number: env.ledger().sequence() + 1,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 100,
        min_persistent_entry_ttl: 100,
        max_entry_ttl: 10_000_000,
    });
}

// ── 1. Initialize ─────────────────────────────────────────────────────────────

#[test]
fn test_initialize() {
    let t = setup();
    assert_eq!(t.client.get_market_count(), 0);
    assert_eq!(t.client.get_accumulated_fees(), 0);
}

// ── 2. Create market ─────────────────────────────────────────────────────────

#[test]
fn test_create_market() {
    let t = setup();
    let id = create_test_market(&t);
    assert_eq!(id, 1);
    assert_eq!(t.client.get_market_count(), 1);

    let market = t.client.get_market(&id);
    assert_eq!(market.total_yes, 0);
    assert_eq!(market.total_no, 0);
    assert!(!market.resolved);
    assert!(!market.cancelled);
    assert_eq!(market.bet_count, 0);
}

#[test]
#[should_panic(expected = "Error(Contract, #27)")]
fn test_reject_zero_market_duration() {
    let t = setup();
    t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Zero duration"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Other,
        &0_u64,
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #27)")]
fn test_reject_market_duration_below_minimum() {
    let t = setup();
    t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Too short"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Other,
        &(MIN_MARKET_DURATION_SECS - 1),
    );
}

#[test]
fn test_market_duration_minimum_is_allowed() {
    let t = setup();
    let id = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Minimum duration"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Other,
        &MIN_MARKET_DURATION_SECS,
    );

    assert_eq!(
        t.client.get_market(&id).end_time,
        t.env.ledger().timestamp() + MIN_MARKET_DURATION_SECS
    );
}

// ── 3. Place YES bet ──────────────────────────────────────────────────────────

#[test]
fn test_place_yes_bet() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let market = t.client.get_market(&id);
    assert_eq!(market.total_yes, 98_0000000);
    assert_eq!(market.total_no, 0);
    assert_eq!(market.bet_count, 1);

    let bet = t.client.get_bet(&id, &user);
    assert_eq!(bet.amount, 98_0000000);
    assert!(bet.is_yes);
    assert!(!bet.claimed);

    // Gross tracked correctly
    assert_eq!(t.client.get_bet_gross(&id, &user), 100_0000000);
}

// ── 4. Place NO bet ───────────────────────────────────────────────────────────

#[test]
fn test_place_no_bet() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    t.client.place_bet(&user, &id, &false, &100_0000000_i128);

    let market = t.client.get_market(&id);
    assert_eq!(market.total_yes, 0);
    assert_eq!(market.total_no, 98_0000000);
}

// ── 5. Fee: full 2% to AccumulatedFees when no referrer ──────────────────────

#[test]
fn test_fee_full_2_percent_no_referrer() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    // Issue #78: only platform_fee is tracked in AccumulatedFees;
    // referral fee stays in referral contract as surplus.
    assert_eq!(t.client.get_accumulated_fees(), 1_5000000);
}

// ── 6. Fee split with referrer ────────────────────────────────────────────────

#[test]
fn test_fee_split_with_referrer() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    let no_ref: Option<Address> = None;
    t.referral_client.register_referral(
        &referrer,
        &String::from_str(&t.env, "Referrer"),
        &no_ref,
    );
    t.referral_client.register_referral(
        &user,
        &String::from_str(&t.env, "Bettor"),
        &Some(referrer.clone()),
    );

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    assert_eq!(t.client.get_accumulated_fees(), 1_5000000);
    assert_eq!(t.xlm.balance(&referrer), 5000000);
    assert_eq!(t.leaderboard_client.get_points(&referrer), 8);
}

// ── 7. Reject bet on expired market ──────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn test_reject_bet_expired_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    advance_time(&t.env, 3601);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
}

// ── 8. Reject bet on resolved market ─────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_reject_bet_resolved_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    let user2 = Address::generate(&t.env);
    fund_user(&t, &user2, 200_0000000);
    t.client.place_bet(&user2, &id, &false, &50_0000000_i128);
}

// ── 9. Reject bet on cancelled market ────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn test_reject_bet_cancelled_market() {
    let t = setup();
    let id = create_test_market(&t);
    t.client.cancel_market(&t.admin, &id);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
}

// ── 10. Reject bet below minimum ─────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn test_reject_bet_below_minimum() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &5_000_000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn test_reject_gross_minimum_when_net_is_too_small() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    // The gross minimum is not enough to produce the one-XLM net minimum
    // after the two-percent fee.
    t.client.place_bet(&user, &id, &true, &MIN_BET);
}

// ── 11. Increase existing position ───────────────────────────────────────────

#[test]
fn test_increase_position_same_side() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 500_0000000);

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    assert_eq!(t.client.get_bet(&id, &user).amount, 98_0000000);

    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    assert_eq!(t.client.get_bet(&id, &user).amount, 98_0000000 + 49_0000000);

    // Gross tracks full input (both bets)
    assert_eq!(t.client.get_bet_gross(&id, &user), 150_0000000);

    let market = t.client.get_market(&id);
    assert_eq!(market.total_yes, 98_0000000 + 49_0000000);
    assert_eq!(market.bet_count, 1);
}

// ── 12. Reject opposite-side bet ─────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #11)")]
fn test_reject_opposite_side_bet() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 500_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.place_bet(&user, &id, &false, &50_0000000_i128);
}

// ── 13. Resolve market ───────────────────────────────────────────────────────

#[test]
fn test_resolve_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    let market = t.client.get_market(&id);
    assert!(market.resolved);
    assert!(market.outcome);
}

// ── 14. Resolver (non-admin) can resolve ─────────────────────────────────────

#[test]
fn test_resolver_can_resolve() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);

    let resolver = Address::generate(&t.env);
    t.client.add_resolver(&t.admin, &resolver);
    assert!(t.client.is_resolver(&resolver));

    advance_time(&t.env, 3601);
    t.client.resolve_market(&resolver, &id, &true);

    let market = t.client.get_market(&id);
    assert!(market.resolved);
}

// ── 15. Non-resolver cannot resolve ──────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #16)")]
fn test_reject_resolve_market_non_resolver() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    advance_time(&t.env, 3601);
    let rando = Address::generate(&t.env);
    t.client.resolve_market(&rando, &id, &true);
}

// ── 16. Reject double resolution ─────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_reject_double_resolution() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    t.client.resolve_market(&t.admin, &id, &false);
}

// ── 17. Claim-style cancel: admin marks cancelled, bettors pull refunds ───────

#[test]
fn test_cancel_market_claim_style_refund() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);

    let alice_before = t.xlm.balance(&alice);
    let bob_before = t.xlm.balance(&bob);

    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &50_0000000_i128);

    // Admin cancels — O(1) gas, no transfers here
    t.client.cancel_market(&t.admin, &id);
    assert!(t.client.get_market(&id).cancelled);

    // Fees should be zeroed from AccumulatedFees since market is cancelled
    // (fees are returned to bettors via cancel_refund)
    let acc_fees_after_cancel = t.client.get_accumulated_fees();
    assert_eq!(acc_fees_after_cancel, 0);

    // Each bettor pulls their own gross refund
    let alice_refund = t.client.cancel_refund(&alice, &id);
    assert_eq!(alice_refund, 100_0000000); // full gross (100 XLM)
    assert_eq!(t.xlm.balance(&alice), alice_before);
    assert_eq!(t.client.get_bet(&id, &alice).amount, 0);
    assert_eq!(t.client.get_bet_gross(&id, &alice), 0);

    let bob_refund = t.client.cancel_refund(&bob, &id);
    assert_eq!(bob_refund, 50_0000000); // full gross (50 XLM)
    assert_eq!(t.xlm.balance(&bob), bob_before);
    assert_eq!(t.client.get_bet(&id, &bob).amount, 0);
    assert_eq!(t.client.get_bet_gross(&id, &bob), 0);
}

// ── 18. Cancel refund is idempotent — double refund rejected ──────────────────

#[test]
#[should_panic(expected = "Error(Contract, #13)")]
fn test_cancel_refund_double_claim_rejected() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.cancel_market(&t.admin, &id);
    t.client.cancel_refund(&user, &id);
    t.client.cancel_refund(&user, &id); // should fail: NoBetFound (gross zeroed)
}

// ── 19. cancel_refund on non-cancelled market rejected ────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #19)")]
fn test_cancel_refund_non_cancelled_rejected() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    // Market NOT cancelled — should return MarketNotCancelled
    t.client.cancel_refund(&user, &id);
}

// ── 20. Reject cancel on resolved market ─────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_reject_cancel_resolved_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    t.client.cancel_market(&t.admin, &id);
}

// ── 21. Claim as winner ───────────────────────────────────────────────────────

#[test]
fn test_claim_winner() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);

    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128);

    let alice_pre_claim = t.xlm.balance(&alice);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    t.client.claim(&alice, &id);

    let payout = t.xlm.balance(&alice) - alice_pre_claim;
    assert_eq!(payout, 196_0000000);

    let stats = t.leaderboard_client.get_stats(&alice);
    assert_eq!(stats.won_bets, 1);
    assert_eq!(t.token_client.balance(&alice), 10_0000000);
}

// ── 22. Claim as loser ───────────────────────────────────────────────────────

#[test]
fn test_claim_loser() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);

    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128);

    let bob_pre_claim = t.xlm.balance(&bob);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    t.client.claim(&bob, &id);

    assert_eq!(t.xlm.balance(&bob), bob_pre_claim);
    let stats = t.leaderboard_client.get_stats(&bob);
    assert_eq!(stats.lost_bets, 1);
    assert_eq!(t.token_client.balance(&bob), 2_0000000);
}

// ── 23. Reject double claim ───────────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #12)")]
fn test_reject_double_claim() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    t.client.claim(&user, &id);
    t.client.claim(&user, &id);
}

// ── 24. Reject claim on unresolved market ────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn test_reject_claim_unresolved() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.claim(&user, &id);
}

// ── 25. Reject claim on cancelled market ─────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn test_reject_claim_cancelled() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.cancel_market(&t.admin, &id);
    t.client.claim(&user, &id);
}

// ── 26. Admin withdraw fees ──────────────────────────────────────────────────

#[test]
fn test_withdraw_fees() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let fees_before = t.client.get_accumulated_fees();
    assert!(fees_before > 0);
    let cap = fees_before * MAX_WITHDRAWAL_BPS / BPS_DENOM;

    let admin_xlm_before = t.xlm.balance(&t.admin);
    let withdrawn = t.client.withdraw_fees(&t.admin, &t.admin);
    assert_eq!(withdrawn, cap);
    assert_eq!(t.client.get_accumulated_fees(), fees_before - cap);
    assert_eq!(t.xlm.balance(&t.admin), admin_xlm_before + cap);

    let drained = withdraw_all_admin_fees(&t, &t.admin);
    assert_eq!(drained, fees_before - cap);
    assert_eq!(t.client.get_accumulated_fees(), 0);
}

// ── 27. Fee recipient withdrawal is capped + timelocked (issue #12) ──────────

#[test]
fn test_fee_recipient_withdraw() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    let treasury = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    t.client.add_fee_recipient(&t.admin, &treasury);

    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    let treasury_before = t.xlm.balance(&treasury);

    // Fee recipient requests a capped withdrawal to the registered treasury.
    t.client.request_withdraw_fees(&recipient, &treasury, &cap);

    // Payout is NOT immediate: timelocked for WITHDRAW_DELAY_SECS.
    let pending = t.client.get_pending_withdrawal(&recipient).unwrap();
    assert_eq!(pending.recipient, treasury);
    assert_eq!(pending.amount, cap);
    assert_eq!(t.xlm.balance(&treasury), treasury_before);

    // After the delay the payout executes.
    advance_time(&t.env, WITHDRAW_DELAY_SECS);
    let withdrawn = t.client.execute_withdraw_fees(&recipient);
    assert_eq!(withdrawn, cap);
    assert_eq!(t.xlm.balance(&treasury), treasury_before + cap);
    assert_eq!(t.client.get_accumulated_fees(), fees - cap);
}

// ── 27b. Fee recipient can no longer withdraw immediately (issue #12) ─────────

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_reject_fee_recipient_immediate_withdraw() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    t.client.withdraw_fees(&recipient, &recipient);
}

// ── 27c. Withdraw to an arbitrary address is rejected (issue #12) ─────────────

#[test]
#[should_panic(expected = "Error(Contract, #21)")]
fn test_reject_withdraw_fees_to_arbitrary_recipient() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let rando = Address::generate(&t.env);
    t.client.withdraw_fees(&t.admin, &rando);
}

#[test]
#[should_panic(expected = "Error(Contract, #21)")]
fn test_reject_fee_recipient_request_arbitrary_recipient() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    let rando = Address::generate(&t.env);
    t.client.request_withdraw_fees(&recipient, &rando, &1_i128);
}

// ── 27d. Cannot drain the whole accumulator in one request (issue #12) ────────

#[test]
#[should_panic(expected = "Error(Contract, #22)")]
fn test_reject_drain_entire_accumulator_in_one_request() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);

    let fees = t.client.get_accumulated_fees();
    t.client.request_withdraw_fees(&recipient, &recipient, &fees);
}

// ── 27e. Payout is locked until the timelock elapses (issue #12) ──────────────

#[test]
#[should_panic(expected = "Error(Contract, #25)")]
fn test_withdrawal_execute_before_delay() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);

    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
    t.client.execute_withdraw_fees(&recipient);
}

// ── 27f. Admin can cancel a pending withdrawal request (issue #12) ────────────

#[test]
fn test_admin_cancel_pending_withdrawal() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);

    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
    assert!(t.client.get_pending_withdrawal(&recipient).is_some());

    t.client.cancel_withdrawal_request(&t.admin, &recipient);
    assert!(t.client.get_pending_withdrawal(&recipient).is_none());
    assert_eq!(t.client.get_accumulated_fees(), fees);
}

// ── 27g. Executing without a pending request is rejected (issue #12) ──────────

#[test]
#[should_panic(expected = "Error(Contract, #24)")]
fn test_execute_without_request() {
    let t = setup();
    let rando = Address::generate(&t.env);
    t.client.execute_withdraw_fees(&rando);
}

// ── 27h. Duplicate withdrawal requests are rejected (issue #12) ───────────────

#[test]
#[should_panic(expected = "Error(Contract, #23)")]
fn test_reject_duplicate_withdrawal_request() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);

    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
}

// ── 28. Non-authorized cannot withdraw fees ───────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_reject_withdraw_fees_non_admin() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    let rando = Address::generate(&t.env);
    t.client.withdraw_fees(&rando, &rando);
}

// ── 29. Bettor index enumeration ─────────────────────────────────────────────

#[test]
fn test_bettor_index_enumeration() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    let charlie = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);
    fund_user(&t, &charlie, 200_0000000);

    t.client.place_bet(&alice, &id, &true, &10_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &20_0000000_i128);
    t.client.place_bet(&charlie, &id, &true, &30_0000000_i128);

    let bettors = t.client.get_market_bettors(&id);
    assert_eq!(bettors.len(), 3);
    assert_eq!(bettors.get(0).unwrap(), alice);
    assert_eq!(bettors.get(1).unwrap(), bob);
    assert_eq!(bettors.get(2).unwrap(), charlie);

    let first_page = t.client.get_market_bettors_page(&id, &0, &2);
    assert_eq!(first_page.len(), 2);
    assert_eq!(first_page.get(0).unwrap(), alice);
    assert_eq!(first_page.get(1).unwrap(), bob);

    let second_page = t.client.get_market_bettors_page(&id, &2, &2);
    assert_eq!(second_page.len(), 1);
    assert_eq!(second_page.get(0).unwrap(), charlie);
}

#[test]
fn test_bettor_index_legacy_read_is_bounded() {
    let t = setup();
    // Simulating a 101-entry legacy index legitimately reads 100 slots in one
    // call, which exceeds the default mainnet-like resource limits — this test
    // proves read boundedness, not gas, so lift the limits like other suites.
    t.env.cost_estimate().disable_resource_limits();
    let id = create_test_market(&t);
    let first = Address::generate(&t.env);
    let beyond_first_page = Address::generate(&t.env);

    // Simulate a large legacy index without spending time creating 101 bets.
    t.env.as_contract(&t.client.address, || {
        t.env.storage().persistent().set(
            &DataKey::BettorCount(id),
            &(MAX_BETTORS_PER_PAGE + 1),
        );
        t.env.storage().persistent().set(
            &DataKey::BettorAt(id, 0),
            &first,
        );
        t.env.storage().persistent().set(
            &DataKey::BettorAt(id, MAX_BETTORS_PER_PAGE),
            &beyond_first_page,
        );
    });

    let legacy_page = t.client.get_market_bettors(&id);
    assert_eq!(legacy_page.len(), 1);
    assert_eq!(legacy_page.get(0).unwrap(), first);

    let later_page = t
        .client
        .get_market_bettors_page(&id, &MAX_BETTORS_PER_PAGE, &1);
    assert_eq!(later_page.len(), 1);
    assert_eq!(later_page.get(0).unwrap(), beyond_first_page);
}

// ── 30. Referrer earns 3 bonus points per referred bet ───────────────────────

#[test]
fn test_referrer_bonus_points_per_bet() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    fund_user(&t, &user, 500_0000000);

    let no_ref: Option<Address> = None;
    t.referral_client.register_referral(
        &referrer,
        &String::from_str(&t.env, "Referrer"),
        &no_ref,
    );
    t.referral_client.register_referral(
        &user,
        &String::from_str(&t.env, "Fan"),
        &Some(referrer.clone()),
    );

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);

    assert_eq!(t.leaderboard_client.get_points(&referrer), 11);
}

// ── 31. Spam guard: TooManyBets ──────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #17)")]
fn test_reject_too_many_bets() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 100_000_000_000);

    // 1.1 XLM gross clears the net minimum (net = 1.078 XLM >= MIN_BET) so the
    // 21st bet actually trips the TooManyBets guard instead of BetTooSmall.
    for _ in 0..=20u32 {
        t.client.place_bet(&user, &id, &true, &11_0000000_i128);
    }
}

// ── 32. Market creation rate limiting ────────────────────────────────────────

#[test]
fn test_market_creation_rate_limit_allows_up_to_max() {
    let t = setup();
    // Should be able to create up to MAX_MARKETS_PER_HOUR (10) in the same window
    for i in 0..10u32 {
        let _ = t.client.create_market(
            &t.admin,
            &String::from_str(&t.env, "Market"),
            &String::from_str(&t.env, "https://x.png"),
            &Category::Crypto,
            &(3600_u64 + i as u64),
        );
    }
    assert_eq!(t.client.get_market_count(), 10);
}

#[test]
#[should_panic(expected = "Error(Contract, #20)")]
fn test_market_creation_rate_limit_exceeded() {
    let t = setup();
    // Create 10 markets (the limit)
    for i in 0..10u32 {
        let _ = t.client.create_market(
            &t.admin,
            &String::from_str(&t.env, "Market"),
            &String::from_str(&t.env, "https://x.png"),
            &Category::Crypto,
            &(3600_u64 + i as u64),
        );
    }
    // 11th should fail
    t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Over limit"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Sports,
        &7200_u64,
    );
}

#[test]
fn test_market_creation_rate_limit_resets_after_window() {
    let t = setup();
    for i in 0..10u32 {
        let _ = t.client.create_market(
            &t.admin,
            &String::from_str(&t.env, "Market"),
            &String::from_str(&t.env, "https://x.png"),
            &Category::Crypto,
            &(3600_u64 + i as u64),
        );
    }
    // Advance past the 1-hour window
    advance_time(&t.env, 3601);
    // Should be able to create again
    let id = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "New window market"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Sports,
        &7200_u64,
    );
    assert_eq!(id, 11);
}

#[test]
#[should_panic(expected = "Error(Contract, #20)")]
fn test_market_creation_rate_limit_rejects_timestamp_regression() {
    let t = setup();
    for i in 0..10u32 {
        let _ = t.client.create_market(
            &t.admin,
            &String::from_str(&t.env, "Market"),
            &String::from_str(&t.env, "https://x.png"),
            &Category::Crypto,
            &(3600_u64 + i as u64),
        );
    }

    // Rewinding the ledger must not reset the active rate-limit window.
    rewind_time(&t.env, 1);
    t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Over limit after rewind"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Sports,
        &7200_u64,
    );
}

// ── 33. Double initialization rejected ───────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #1)")]
fn test_double_init_rejected() {
    let t = setup();
    let tok2 = Address::generate(&t.env);
    let ref2 = Address::generate(&t.env);
    let lb2 = Address::generate(&t.env);
    let xlm2 = Address::generate(&t.env);
    t.client.initialize(&t.admin, &tok2, &ref2, &lb2, &xlm2);
}

// ── 34. Resolve before deadline rejected ─────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #6)")]
fn test_reject_resolve_before_deadline() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    t.client.resolve_market(&t.admin, &id, &true);
}

// ── 35. Withdraw fees when zero ───────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #15)")]
fn test_withdraw_fees_zero() {
    let t = setup();
    t.client.withdraw_fees(&t.admin, &t.admin);
}

// ── 36. Claim with no bet → NoBetFound ───────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #13)")]
fn test_claim_no_bet_found() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    let stranger = Address::generate(&t.env);
    t.client.claim(&stranger, &id);
}

// ── 37. Non-admin create market rejected ─────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_reject_create_market_non_admin() {
    let t = setup();
    let rando = Address::generate(&t.env);
    t.client.create_market(
        &rando,
        &String::from_str(&t.env, "Unauthorized?"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Other,
        &3600_u64,
    );
}

// ── 38. Non-admin cancel rejected ────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_reject_cancel_market_non_admin() {
    let t = setup();
    let id = create_test_market(&t);
    let rando = Address::generate(&t.env);
    t.client.cancel_market(&rando, &id);
}

// ── 39. Market not found ─────────────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #4)")]
fn test_market_not_found() {
    let t = setup();
    t.client.get_market(&999);
}

// ── 40. Multiple markets with categories ─────────────────────────────────────

#[test]
fn test_create_multiple_markets() {
    let t = setup();
    let id1 = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Market A"),
        &String::from_str(&t.env, "https://a.png"),
        &Category::Crypto,
        &3600_u64,
    );
    let id2 = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Market B"),
        &String::from_str(&t.env, "https://b.png"),
        &Category::Sports,
        &7200_u64,
    );
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
    assert_eq!(t.client.get_market_count(), 2);
    assert_eq!(t.client.get_market(&id2).category, Category::Sports);
}

// ── 41. Empty-side resolution: principal stays claimable, fees stay fees (issue #57) ─

#[test]
fn test_empty_side_resolution_pool_to_fees() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);

    // Only YES bets — no one bets NO
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    assert_eq!(t.client.get_accumulated_fees(), 1_5000000);
    assert_eq!(t.client.get_market_fees(&id), 1_5000000);

    // Advance past end_time and resolve NO (empty winning side)
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &false); // total_no == 0

    // Principal in Payout / ForfeitedPool; platform fees locked out of Acc.
    assert_eq!(t.client.get_accumulated_fees(), 0);
    assert_eq!(t.client.get_market_fees(&id), 0);
    assert_eq!(t.client.get_payout(&id, &alice), 98_0000000);
    let fp = t.client.get_forfeited_pool(&id).expect("forfeited pool");
    assert_eq!(fp.amount, 98_0000000);
    assert_eq!(fp.locked_fees, 1_5000000);

    // Immediate withdraw must fail — fees locked for dispute window.
    let treasury = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &treasury);
    assert!(t.client.try_withdraw_fees(&t.admin, &treasury).is_err());

    advance_time(&t.env, DISPUTE_WINDOW_SECS);
    t.client.finalize_zero_side(&id);
    let withdrawn = withdraw_all_admin_fees(&t, &treasury);
    assert_eq!(withdrawn, 1_5000000);
    assert_eq!(t.xlm.balance(&treasury), 1_5000000);

    // Alice claims her net principal plus lose-tier PULSE / points
    let alice_xlm_before = t.xlm.balance(&alice);
    t.client.claim(&alice, &id);
    let bet = t.client.get_bet(&id, &alice);
    assert!(bet.claimed);
    assert_eq!(t.xlm.balance(&alice), alice_xlm_before + 98_0000000);
    assert_eq!(t.token_client.balance(&alice), 2_0000000); // LOSE_TOKENS
    assert_eq!(t.leaderboard_client.get_points(&alice), 10); // LOSE_POINTS
}

// ── 42. Cancel accumulates fees on multiple bets correctly ────────────────────

#[test]
fn test_cancel_fees_zeroed_correctly() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);

    // Two bets accumulate fees (only platform_fee tracked; referral fee goes to surplus)
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128); // 1.5 XLM platform fee
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128); // 1.5 XLM platform fee
    assert_eq!(t.client.get_accumulated_fees(), 3_0000000);

    // Cancel zeroes out those fees
    t.client.cancel_market(&t.admin, &id);
    assert_eq!(t.client.get_accumulated_fees(), 0);

    // Bettors get their gross back
    t.client.cancel_refund(&alice, &id);
    t.client.cancel_refund(&bob, &id);
}

// ═══════════════════════════════════════════════════════════════════════════════
// 42. COMPREHENSIVE END-TO-END INTEGRATION TEST
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_e2e_full_inter_contract_flow() {
    let t = setup();

    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    fund_user(&t, &alice, 1000_0000000);
    fund_user(&t, &bob, 1000_0000000);

    let no_ref: Option<Address> = None;
    t.referral_client.register_referral(
        &referrer,
        &String::from_str(&t.env, "Referrer"),
        &no_ref,
    );
    t.referral_client.register_referral(
        &alice,
        &String::from_str(&t.env, "Alice"),
        &Some(referrer.clone()),
    );
    assert_eq!(t.leaderboard_client.get_points(&alice), 5);
    assert_eq!(t.token_client.balance(&alice), 1_0000000);

    let market_id = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Will ETH flip BTC?"),
        &String::from_str(&t.env, "https://eth.png"),
        &Category::Crypto,
        &3600_u64,
    );
    assert_eq!(market_id, 1);

    // Alice bets YES 100 XLM — has referrer
    t.client
        .place_bet(&alice, &market_id, &true, &100_0000000_i128);
    assert_eq!(t.client.get_accumulated_fees(), 1_5000000);
    assert_eq!(t.xlm.balance(&referrer), 5000000);
    assert_eq!(t.leaderboard_client.get_points(&referrer), 8);
    // Alice's welcome bonus counts as activity: won(0) + lost(0) + bonus(1).
    assert_eq!(t.leaderboard_client.get_stats(&alice).total_bets, 1);
    assert_eq!(t.client.get_market(&market_id).total_yes, 98_0000000);
    assert_eq!(t.client.get_bet_gross(&market_id, &alice), 100_0000000);

    // Bob bets NO 200 XLM — no referrer
    t.client
        .place_bet(&bob, &market_id, &false, &200_0000000_i128);
    // Issue #78: only platform_fee tracked per bet; referral fee goes to surplus.
    // Alice: 1.5M, Bob: 3M platform fee → total 4.5M
    assert_eq!(t.client.get_accumulated_fees(), 4_5000000);
    // Bob never registered, so no bonus: total_bets = won(0) + lost(0) + bonus(0).
    assert_eq!(t.leaderboard_client.get_stats(&bob).total_bets, 0);
    assert_eq!(t.client.get_market(&market_id).total_no, 196_0000000);

    // Alice increases YES (+50 XLM)
    t.client
        .place_bet(&alice, &market_id, &true, &50_0000000_i128);
    let alice_bet = t.client.get_bet(&market_id, &alice);
    assert_eq!(alice_bet.amount, 98_0000000 + 49_0000000);
    assert_eq!(t.client.get_bet_gross(&market_id, &alice), 150_0000000);
    assert_eq!(t.client.get_market(&market_id).total_yes, 147_0000000);
    assert_eq!(t.client.get_market(&market_id).bet_count, 2);
    assert_eq!(t.leaderboard_client.get_points(&referrer), 11);

    // Add a resolver and resolve via them
    let resolver = Address::generate(&t.env);
    t.client.add_resolver(&t.admin, &resolver);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&resolver, &market_id, &true);
    assert!(t.client.get_market(&market_id).resolved);

    // Alice claims as winner
    let alice_xlm_before = t.xlm.balance(&alice);
    t.client.claim(&alice, &market_id);
    let alice_payout = t.xlm.balance(&alice) - alice_xlm_before;
    assert_eq!(alice_payout, 343_0000000);
    assert_eq!(t.leaderboard_client.get_points(&alice), 35);
    assert_eq!(t.token_client.balance(&alice), 11_0000000);

    // Bob claims as loser
    let bob_xlm_before = t.xlm.balance(&bob);
    t.client.claim(&bob, &market_id);
    assert_eq!(t.xlm.balance(&bob), bob_xlm_before);
    assert_eq!(t.leaderboard_client.get_points(&bob), 10);
    assert_eq!(t.token_client.balance(&bob), 2_0000000);

    // Fee withdrawal to a registered treasury address
    let treasury = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &treasury);
    let fees_total = t.client.get_accumulated_fees();
    assert!(fees_total > 0);
    let treasury_before = t.xlm.balance(&treasury);
    let withdrawn = withdraw_all_admin_fees(&t, &treasury);
    assert_eq!(withdrawn, fees_total);
    assert_eq!(t.client.get_accumulated_fees(), 0);
    assert_eq!(t.xlm.balance(&treasury), treasury_before + fees_total);

    // Create second market, bet, then cancel — verify claim-style refund
    let market2 = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Will DOGE hit $1?"),
        &String::from_str(&t.env, "https://doge.png"),
        &Category::Crypto,
        &7200_u64,
    );
    let charlie = Address::generate(&t.env);
    fund_user(&t, &charlie, 500_0000000);
    let charlie_before = t.xlm.balance(&charlie);
    t.client
        .place_bet(&charlie, &market2, &true, &100_0000000_i128);
    t.client.cancel_market(&t.admin, &market2);
    // AccumulatedFees from market2 should be zeroed
    assert_eq!(t.client.get_accumulated_fees(), 0);
    // Charlie pulls their own refund (gross = 100 XLM)
    let refunded = t.client.cancel_refund(&charlie, &market2);
    assert_eq!(refunded, 100_0000000);
    assert_eq!(t.xlm.balance(&charlie), charlie_before);
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION SUITE — issue #2 (payout rounding / dust)
// ═══════════════════════════════════════════════════════════════════════════

// ── #2: settlement-time payouts — Σ payouts + dust == pool ──────────────────
#[test]
fn test_many_winners_payouts_exact_and_dust_swept() {
    let t = setup();
    let id = create_test_market(&t);

    let w1 = Address::generate(&t.env);
    let w2 = Address::generate(&t.env);
    let w3 = Address::generate(&t.env);
    let l1 = Address::generate(&t.env);
    fund_user(&t, &w1, 1_000_0000000);
    fund_user(&t, &w2, 1_000_0000000);
    fund_user(&t, &w3, 1_000_0000000);
    fund_user(&t, &l1, 1_000_0000000);

    // Deliberately uneven stakes that do NOT divide the pool evenly.
    t.client.place_bet(&w1, &id, &true, &30_000_001_i128);
    t.client.place_bet(&w2, &id, &true, &40_000_003_i128);
    t.client.place_bet(&w3, &id, &true, &50_000_007_i128);
    t.client.place_bet(&l1, &id, &false, &27_777_779_i128);

    advance_time(&t.env, 3601);
    let fees_before = t.client.get_accumulated_fees();
    t.client.resolve_market(&t.admin, &id, &true);

    let market = t.client.get_market(&id);
    let pool: i128 = market.total_yes + market.total_no;
    let win: i128 = market.total_yes;

    let n1 = t.client.get_bet(&id, &w1).amount;
    let n2 = t.client.get_bet(&id, &w2).amount;
    let n3 = t.client.get_bet(&id, &w3).amount;
    assert_eq!(n1 + n2 + n3, win);

    // Stored payouts must equal the exact integer formula.
    let p1 = (n1 * pool) / win;
    let p2 = (n2 * pool) / win;
    let p3 = (n3 * pool) / win;
    assert_eq!(t.client.get_payout(&id, &w1), p1);
    assert_eq!(t.client.get_payout(&id, &w2), p2);
    assert_eq!(t.client.get_payout(&id, &w3), p3);
    assert_eq!(t.client.get_payout(&id, &l1), 0);

    // The dust is deterministic, bounded, and swept to the fee accumulator.
    let dust: i128 = pool - p1 - p2 - p3;
    assert!(dust >= 0);
    assert!(dust < win);
    assert_eq!(t.client.get_accumulated_fees(), fees_before + dust);

    // No overpay: the sum of payouts never exceeds the pool.
    assert!(p1 + p2 + p3 <= pool);

    // After all claims the market's balance drops by exactly Σ payouts.
    let market_contract = t.client.address.clone();
    let bal_before = t.xlm.balance(&market_contract);
    t.client.claim(&w1, &id);
    t.client.claim(&w2, &id);
    t.client.claim(&w3, &id);
    assert_eq!(
        bal_before - t.xlm.balance(&market_contract),
        p1 + p2 + p3
    );
    assert_eq!(t.xlm.balance(&w1), 1_000_0000000_i128 - 30_000_001_i128 + p1);
}

// ── #2: single winner receives the whole pool (no dust) ─────────────────────
#[test]
fn test_single_winner_gets_whole_net_pool() {
    let t = setup();
    let id = create_test_market(&t);
    let winner = Address::generate(&t.env);
    let loser = Address::generate(&t.env);
    fund_user(&t, &winner, 1_000_0000000);
    fund_user(&t, &loser, 1_000_0000000);

    t.client.place_bet(&winner, &id, &true, &60_0000000_i128);
    t.client.place_bet(&loser, &id, &false, &60_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    let market = t.client.get_market(&id);
    let total = market.total_yes + market.total_no;
    let before = t.xlm.balance(&winner);
    t.client.claim(&winner, &id);
    // Whole pool (both nets) goes to the single winner.
    assert_eq!(t.xlm.balance(&winner) - before, total);
    assert_eq!(t.client.get_payout(&id, &winner), total);
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION SUITE — issue #9 (persistent storage TTL)
// ═══════════════════════════════════════════════════════════════════════════

fn advance_ledgers(env: &Env, n: u32) {
    let current_seq = env.ledger().sequence();
    env.ledger().set(LedgerInfo {
        timestamp: env.ledger().timestamp() + 1,
        protocol_version: 26,
        sequence_number: current_seq + n,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 100,
        min_persistent_entry_ttl: 100,
        max_entry_ttl: 10_000_000,
    });
}

// ── #9: claims/refunds keep recoverable storage alive (TTL re-bump) ──────────
#[test]
fn test_claim_rebumps_ttl_entries() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    // Fast-forward deep into the TTL window (but not past it).
    advance_ledgers(&t.env, 6_000_000);

    let market_contract = t.client.address.clone();
    let bet_key = DataKey::Bet(id, user.clone());
    let market_key = DataKey::Market(id);
    let ttl = |key: &DataKey| -> u32 {
        t.env
            .as_contract(&market_contract, || t.env.storage().persistent().get_ttl(key))
    };
    let before_bet = ttl(&bet_key);
    let before_market = ttl(&market_key);

    t.client.claim(&user, &id);

    let after_bet = ttl(&bet_key);
    let after_market = ttl(&market_key);
    assert!(after_bet > before_bet);
    assert!(after_market > before_market);
}

#[test]
fn test_cancel_refund_rebumps_ttl_entries() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.cancel_market(&t.admin, &id);

    advance_ledgers(&t.env, 6_000_000);

    let market_contract = t.client.address.clone();
    let bet_key = DataKey::Bet(id, user.clone());
    let market_key = DataKey::Market(id);
    let ttl = |key: &DataKey| -> u32 {
        t.env
            .as_contract(&market_contract, || t.env.storage().persistent().get_ttl(key))
    };
    let bet_before = ttl(&bet_key);
    let market_before = ttl(&market_key);

    t.client.cancel_refund(&user, &id);

    assert!(ttl(&bet_key) > bet_before);
    assert!(ttl(&market_key) > market_before);
}

// ── #54: permissionless refresh + per-market expiry tracking + migration ─────

#[test]
fn test_get_market_ttl_tracks_live_entry() {
    let t = setup();
    assert_eq!(t.client.get_market_ttl(&99_u64), 0);
    let id = create_test_market(&t);
    assert!(t.client.get_market_ttl(&id) >= TTL_BUMP);
}

#[test]
fn test_refresh_market_ttl_rebumps_bet_and_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    advance_ledgers(&t.env, 6_000_000);

    let market_contract = t.client.address.clone();
    let bet_key = DataKey::Bet(id, user.clone());
    let market_key = DataKey::Market(id);
    let ttl = |key: &DataKey| -> u32 {
        t.env
            .as_contract(&market_contract, || t.env.storage().persistent().get_ttl(key))
    };
    let bet_before = ttl(&bet_key);
    let market_before = ttl(&market_key);

    // Anyone can pay to keep the keys alive — no auth required.
    assert_eq!(t.client.refresh_market_ttl(&id), 1);
    assert!(ttl(&bet_key) > bet_before);
    assert!(ttl(&market_key) > market_before);
    assert!(t.client.get_market_ttl(&id) > market_before);
}

#[test]
fn test_refresh_markets_migrates_existing_entries() {
    let t = setup();
    let a = create_test_market(&t);
    let b = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 400_0000000);
    t.client.place_bet(&user, &a, &true, &100_0000000_i128);
    t.client.place_bet(&user, &b, &true, &100_0000000_i128);

    advance_ledgers(&t.env, 6_000_000);
    let before_a = t.client.get_market_ttl(&a);
    let bumped = t.client.refresh_markets(&1_u64, &20_u32);
    assert_eq!(bumped, 2);
    assert!(t.client.get_market_ttl(&a) > before_a);
    assert!(t.client.get_market_ttl(&b) >= TTL_BUMP);
}

#[test]
fn test_resolve_market_rebumps_payout_entry() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);

    advance_ledgers(&t.env, 6_000_000);
    t.client.resolve_market(&t.admin, &id, &true);

    let market_contract = t.client.address.clone();
    let payout_ttl = t.env.as_contract(&market_contract, || {
        t.env.storage()
            .persistent()
            .get_ttl(&DataKey::Payout(id, user.clone()))
    });
    assert!(payout_ttl >= TTL_BUMP);
}

// ── Cross-contract interface versioning (issue #84) ───────────────────────────

// Stands in for a referral_registry/leaderboard deployment upgraded to an
// incompatible ABI: it only implements interface_version(), reporting a
// version this prediction_market build does not expect.
#[contract]
struct MockIncompatibleDependency;

#[contractimpl]
impl MockIncompatibleDependency {
    pub fn interface_version(_env: Env) -> u32 {
        99
    }
}

fn activate_config(
    t: &TestSetup,
    token: &Address,
    referral: &Address,
    leaderboard: &Address,
    xlm_sac: &Address,
) {
    t.client
        .set_config(&t.admin, token, referral, leaderboard, xlm_sac);
    advance_time(&t.env, CONFIG_DELAY_SECS);
    t.client.execute_set_config(&t.admin);
}

#[test]
fn test_interface_version_reported() {
    let t = setup();
    assert_eq!(t.client.interface_version(), 1);
}

#[test]
#[should_panic(expected = "Error(Contract, #36)")]
fn test_place_bet_rejects_incompatible_referral() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    let fake_referral = t.env.register(MockIncompatibleDependency, ());
    let cfg = t.client.get_config();
    activate_config(
        &t,
        &cfg.token,
        &fake_referral,
        &cfg.leaderboard,
        &cfg.xlm_sac,
    );

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #36)")]
fn test_claim_rejects_incompatible_leaderboard() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    let fake_leaderboard = t.env.register(MockIncompatibleDependency, ());
    let cfg = t.client.get_config();
    activate_config(
        &t,
        &cfg.token,
        &cfg.referral,
        &fake_leaderboard,
        &cfg.xlm_sac,
    );

    t.client.claim(&user, &id);
}

// Stands in for a leaderboard deployment that reports the version this
// prediction_market build expects, but is missing the actual reward()
// function it's about to call. Proves the known limitation of the version
// check: a matching u32 alone does not prove ABI compatibility, only that
// the callee's author intended it to be compatible. If a breaking change to
// reward()'s signature ever ships without bumping INTERFACE_VERSION, this is
// exactly the failure mode that results, just past the version check instead
// of at it.
#[contract]
struct MockLeaderboardMissingReward;

#[contractimpl]
impl MockLeaderboardMissingReward {
    pub fn interface_version(_env: Env) -> u32 {
        1
    }
    // No reward() here on purpose.
}

#[test]
#[should_panic]
fn test_matching_version_does_not_guarantee_claim_succeeds() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    let fake_leaderboard = t.env.register(MockLeaderboardMissingReward, ());
    let cfg = t.client.get_config();
    activate_config(
        &t,
        &cfg.token,
        &cfg.referral,
        &fake_leaderboard,
        &cfg.xlm_sac,
    );

    // require_compatible_leaderboard passes (version 1 == version 1), then
    // claim() panics inside the real reward() invoke_contract call because
    // the function doesn't exist on the callee.
    t.client.claim(&user, &id);
}

// ── Emergency Pause (issue #83) ───────────────────────────────────────────────

#[test]
fn test_pause_unpause_admin_only() {
    let t = setup();
    assert!(!t.client.is_paused());

    t.client.pause(&t.admin);
    assert!(t.client.is_paused());

    t.client.unpause(&t.admin);
    assert!(!t.client.is_paused());
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_pause_rejects_non_admin() {
    let t = setup();
    let not_admin = Address::generate(&t.env);
    t.client.pause(&not_admin);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_create_market() {
    let t = setup();
    t.client.pause(&t.admin);
    create_test_market(&t);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_place_bet() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    t.client.pause(&t.admin);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_resolve_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);

    t.client.pause(&t.admin);
    t.client.resolve_market(&t.admin, &id, &true);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_claim() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    t.client.pause(&t.admin);
    t.client.claim(&user, &id);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_withdraw_fees() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    t.client.pause(&t.admin);
    t.client.withdraw_fees(&t.admin, &t.admin);
}

// Refunds remain the users' emergency exit even while paused.
#[test]
fn test_cancel_refund_still_works_while_paused() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.cancel_market(&t.admin, &id);

    t.client.pause(&t.admin);
    let refunded = t.client.cancel_refund(&user, &id);
    assert_eq!(refunded, 100_0000000);
}

// View functions must keep working while paused.
#[test]
fn test_view_functions_work_while_paused() {
    let t = setup();
    let id = create_test_market(&t);
    t.client.pause(&t.admin);

    assert_eq!(t.client.get_market_count(), 1);
    let market = t.client.get_market(&id);
    assert_eq!(market.id, id);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_cancel_market() {
    let t = setup();
    let id = create_test_market(&t);

    t.client.pause(&t.admin);
    t.client.cancel_market(&t.admin, &id);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_request_withdraw_fees() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;

    t.client.pause(&t.admin);
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_execute_withdraw_fees() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;

    // Request while unpaused, then let the timelock mature — the pause check
    // in execute_withdraw_fees must still block payout even on a matured request.
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
    advance_time(&t.env, WITHDRAW_DELAY_SECS);

    t.client.pause(&t.admin);
    t.client.execute_withdraw_fees(&recipient);
}

// The admin's ability to kill a compromised/stuck withdrawal request must
// remain available mid-pause, same as the users' cancel_refund exit path.
#[test]
fn test_cancel_withdrawal_request_still_works_while_paused() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);

    t.client.pause(&t.admin);
    t.client.cancel_withdrawal_request(&t.admin, &recipient);

    assert!(t.client.get_pending_withdrawal(&recipient).is_none());
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION — issue #51 (set_config pinning / governance)
// ═══════════════════════════════════════════════════════════════════════════

fn second_leaderboard(t: &TestSetup) -> Address {
    let id = t.env.register(LeaderboardContract, ());
    let client = leaderboard::LeaderboardContractClient::new(&t.env, &id);
    client.initialize(&t.admin, &t.client.address, &t.referral_client.address);
    id
}

#[test]
fn test_set_config_does_not_apply_immediately() {
    let t = setup();
    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);

    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );

    // Live config is unchanged until execute_set_config after the delay.
    assert_eq!(t.client.get_config().leaderboard, cfg.leaderboard);
    let pending = t.client.get_pending_config().expect("pending change");
    assert_eq!(pending.cfg.leaderboard, new_lb);
    assert_eq!(pending.approvers.len(), 1);
}

#[test]
#[should_panic(expected = "Error(Contract, #28)")]
fn test_set_config_rejects_arbitrary_address() {
    let t = setup();
    let cfg = t.client.get_config();
    let attacker = Address::generate(&t.env);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &attacker,
        &cfg.leaderboard,
        &cfg.xlm_sac,
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #28)")]
fn test_set_config_rejects_wasm_as_xlm_sac() {
    let t = setup();
    let cfg = t.client.get_config();
    // A WASM/native contract must not be installable as the XLM SAC.
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &cfg.leaderboard,
        &cfg.token,
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #32)")]
fn test_set_config_execute_before_delay() {
    let t = setup();
    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    t.client.execute_set_config(&t.admin);
}

#[test]
fn test_set_config_execute_after_delay_and_pin() {
    let t = setup();
    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    advance_time(&t.env, CONFIG_DELAY_SECS);
    t.client.execute_set_config(&t.admin);

    assert_eq!(t.client.get_config().leaderboard, new_lb);
    assert!(t.client.get_pending_config().is_none());
    let pins = t.client.get_pinned_hashes().expect("pins");
    assert_eq!(pins.xlm_sac, BytesN::from_array(&t.env, &[0u8; 32]));
}

#[test]
fn test_cancel_set_config_during_dispute_window() {
    let t = setup();
    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    t.client.cancel_set_config(&t.admin);
    assert!(t.client.get_pending_config().is_none());
    assert_eq!(t.client.get_config().leaderboard, cfg.leaderboard);
}

#[test]
#[should_panic(expected = "Error(Contract, #33)")]
fn test_set_config_multisig_requires_threshold() {
    let t = setup();
    let g2 = Address::generate(&t.env);
    t.client.add_governor(&t.admin, &g2);
    t.client.set_governor_threshold(&t.admin, &2_u32);

    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    advance_time(&t.env, CONFIG_DELAY_SECS);
    // Only the proposer approved (1 of 2).
    t.client.execute_set_config(&t.admin);
}

#[test]
fn test_set_config_multisig_execute_with_second_approval() {
    let t = setup();
    let g2 = Address::generate(&t.env);
    t.client.add_governor(&t.admin, &g2);
    t.client.set_governor_threshold(&t.admin, &2_u32);

    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    t.client.approve_set_config(&g2);
    advance_time(&t.env, CONFIG_DELAY_SECS);
    t.client.execute_set_config(&g2);

    assert_eq!(t.client.get_config().leaderboard, new_lb);
}

#[test]
#[should_panic(expected = "Error(Contract, #18)")]
fn test_set_config_non_governor_rejected() {
    let t = setup();
    let cfg = t.client.get_config();
    let stranger = Address::generate(&t.env);
    t.client.set_config(
        &stranger,
        &cfg.token,
        &cfg.referral,
        &cfg.leaderboard,
        &cfg.xlm_sac,
    );
}

fn last_event_name(env: &Env) -> Symbol {
    let events = env.events().all();
    let last = events.get(events.len() - 1).unwrap();
    let topic0: Val = last.1.get_unchecked(0);
    Symbol::try_from_val(env, &topic0).unwrap()
}

#[test]
fn test_create_market_emits_event() {
    let t = setup();
    let _id = create_test_market(&t);
    assert_eq!(last_event_name(&t.env), Symbol::new(&t.env, "market_created"));
}

#[test]
fn test_place_bet_emits_event() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    assert_eq!(last_event_name(&t.env), Symbol::new(&t.env, "bet_placed"));
}

#[test]
fn test_resolve_market_emits_event() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    assert_eq!(last_event_name(&t.env), Symbol::new(&t.env, "market_resolved"));
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION — issue #57 (withdraw_fees provenance / drain)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_cancel_does_not_wipe_other_market_fees() {
    let t = setup();
    let id1 = create_test_market(&t);
    let id2 = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Market 2"),
        &String::from_str(&t.env, "https://m2.png"),
        &Category::Other,
        &3600_u64,
    );

    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);

    t.client.place_bet(&alice, &id1, &true, &100_0000000_i128); // 1.5 XLM
    t.client.place_bet(&bob, &id2, &true, &100_0000000_i128); // 1.5 XLM
    assert_eq!(t.client.get_accumulated_fees(), 3_0000000);
    assert_eq!(t.client.get_market_fees(&id1), 1_5000000);
    assert_eq!(t.client.get_market_fees(&id2), 1_5000000);

    t.client.cancel_market(&t.admin, &id1);

    assert_eq!(t.client.get_market_fees(&id1), 0);
    assert_eq!(t.client.get_market_fees(&id2), 1_5000000);
    assert_eq!(t.client.get_accumulated_fees(), 1_5000000);
}

#[test]
fn test_cancel_reclaims_pool_fees_not_inflated_ledger() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    assert_eq!(t.client.get_market_fees(&id), 1_5000000);

    // Simulate an inflated per-market ledger (10 XLM recorded vs 1.5 earned).
    t.env.as_contract(&t.client.address, || {
        t.env
            .storage()
            .persistent()
            .set(&DataKey::MarketFees(id), &10_0000000_i128);
        t.env
            .storage()
            .instance()
            .set(&DataKey::AccumulatedFees, &10_0000000_i128);
    });

    t.client.cancel_market(&t.admin, &id);
    // Only pool-attributable fees (1.5 XLM) leave the ledger on cancel.
    assert_eq!(t.client.get_market_fees(&id), 8_5000000);
    assert_eq!(t.client.get_accumulated_fees(), 8_5000000);
}

#[test]
fn test_withdraw_fees_cannot_take_empty_side_principal() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);

    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &false);

    let treasury = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &treasury);
    // Fees locked in ForfeitedPool — nothing withdrawable yet.
    assert_eq!(t.client.get_accumulated_fees(), 0);
    assert!(t.client.try_withdraw_fees(&t.admin, &treasury).is_err());

    advance_time(&t.env, DISPUTE_WINDOW_SECS);
    t.client.finalize_zero_side(&id);
    let withdrawn = withdraw_all_admin_fees(&t, &treasury);
    assert_eq!(withdrawn, 1_5000000);

    let alice_before = t.xlm.balance(&alice);
    t.client.claim(&alice, &id);
    assert_eq!(t.xlm.balance(&alice), alice_before + 98_0000000);
}

#[test]
#[should_panic(expected = "Error(Contract, #21)")]
fn test_fee_recipient_two_step_cannot_target_arbitrary_address() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    let stranger = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);

    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    t.client
        .request_withdraw_fees(&recipient, &stranger, &cap);
}

#[test]
fn test_legacy_fees_start_empty_on_fresh_deploy() {
    let t = setup();
    t.client.migrate_fee_ledger();
    assert_eq!(t.client.get_legacy_fees(), 0);
    assert_eq!(t.client.get_accumulated_fees(), 0);
}

#[test]
fn test_migrate_fee_ledger_snapshots_legacy_balance() {
    let t = setup();
    let legacy_amount: i128 = 7_5000000;
    t.env.as_contract(&t.client.address, || {
        t.env
            .storage()
            .instance()
            .set(&DataKey::AccumulatedFees, &legacy_amount);
        t.env.storage().instance().remove(&DataKey::FeeLedgerMigrated);
    });

    t.client.migrate_fee_ledger();
    assert_eq!(t.client.get_legacy_fees(), legacy_amount);
    assert_eq!(t.client.get_accumulated_fees(), legacy_amount);

    // New bets after migration land on per-market ledgers, not LegacyFees.
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    assert_eq!(t.client.get_legacy_fees(), legacy_amount);
    assert_eq!(t.client.get_market_fees(&id), 1_5000000);
    assert_eq!(t.client.get_accumulated_fees(), legacy_amount + 1_5000000);
}

#[test]
fn test_admin_withdraw_respects_cap() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 500_0000000);
    // Build a fee pot large enough that 20% is visibly less than 100%.
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    let fees = t.client.get_accumulated_fees();
    assert_eq!(fees, 4_5000000);

    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    let withdrawn = t.client.withdraw_fees(&t.admin, &t.admin);
    assert_eq!(withdrawn, cap);
    assert_eq!(t.client.get_accumulated_fees(), fees - cap);
    assert!(withdrawn < fees);
}

#[test]
fn test_two_step_withdraw_debits_market_ledger() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    assert_eq!(t.client.get_market_fees(&id), 1_5000000);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
    advance_time(&t.env, WITHDRAW_DELAY_SECS);
    let withdrawn = t.client.execute_withdraw_fees(&recipient);
    assert_eq!(withdrawn, cap);
    assert_eq!(t.client.get_market_fees(&id), fees - cap);
    assert_eq!(t.client.get_accumulated_fees(), fees - cap);
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION SUITE — issue #3 (zero-side / resolver collusion)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
#[should_panic(expected = "Error(Contract, #37)")]
fn test_zero_side_claim_panics_during_dispute_window() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &false);
    t.client.claim(&alice, &id);
}

#[test]
#[should_panic(expected = "Error(Contract, #40)")]
fn test_resolver_cannot_bet_on_any_market() {
    let t = setup();
    let id = create_test_market(&t);
    let resolver = Address::generate(&t.env);
    t.client.add_resolver(&t.admin, &resolver);
    fund_user(&t, &resolver, 200_0000000);
    t.client.place_bet(&resolver, &id, &true, &100_0000000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #40)")]
fn test_reject_add_resolver_who_holds_open_stake() {
    let t = setup();
    let id = create_test_market(&t);
    let colluder = Address::generate(&t.env);
    fund_user(&t, &colluder, 200_0000000);
    t.client.place_bet(&colluder, &id, &true, &100_0000000_i128);
    t.client.add_resolver(&t.admin, &colluder);
}

#[test]
#[should_panic(expected = "Error(Contract, #40)")]
fn test_admin_cannot_resolve_market_they_staked() {
    let t = setup();
    let id = create_test_market(&t);
    fund_user(&t, &t.admin, 200_0000000);
    t.client.place_bet(&t.admin, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &false);
}

#[test]
fn test_two_sided_loser_does_not_receive_xlm_on_claim() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    let bob_before = t.xlm.balance(&bob);
    t.client.claim(&bob, &id);
    assert_eq!(t.xlm.balance(&bob), bob_before, "loser must not receive XLM");
    assert_eq!(t.token_client.balance(&bob), 2_0000000);
}

#[test]
fn test_freeze_zero_side_during_dispute_refunds_gross() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    let alice_before = t.xlm.balance(&alice);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &false);

    t.client.freeze_market(&t.admin, &id);
    let fp = t.client.get_forfeited_pool(&id).unwrap();
    assert!(fp.frozen);
    assert!(t.client.get_market(&id).cancelled);
    assert_eq!(t.client.get_payout(&id, &alice), 0);

    assert!(t.client.try_claim(&alice, &id).is_err());
    let refunded = t.client.cancel_refund(&alice, &id);
    assert_eq!(refunded, 100_0000000);
    assert_eq!(t.xlm.balance(&alice), alice_before);
    assert_eq!(t.client.get_accumulated_fees(), 0);
}

#[test]
#[should_panic(expected = "Error(Contract, #39)")]
fn test_reject_freeze_after_dispute_window() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &false);
    advance_time(&t.env, DISPUTE_WINDOW_SECS);
    t.client.freeze_market(&t.admin, &id);
}

#[test]
#[should_panic(expected = "Error(Contract, #38)")]
fn test_reject_freeze_two_sided_market() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    t.client.freeze_market(&t.admin, &id);
}

#[test]
#[should_panic(expected = "Error(Contract, #15)")]
fn test_resolver_cannot_drain_principal_immediately_after_zero_side() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &false);

    let treasury = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &treasury);
    t.client.withdraw_fees(&t.admin, &treasury);
}
