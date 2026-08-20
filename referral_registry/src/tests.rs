use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events},
    contract, contractimpl,
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Env, String, Symbol, TryFromVal,
};

// Import sibling contracts for inter-contract testing
use leaderboard::LeaderboardContract;
use pulse_token::PULSETokenContract;

// ── Test Helpers ──────────────────────────────────────────────────────────────

struct TestSetup {
    env: Env,
    client: ReferralRegistryContractClient<'static>,
    admin: Address,
    market: Address,
    token_id: Address,
    leaderboard_id: Address,
    xlm_sac_id: Address,
    referral_id: Address,
}

fn setup() -> TestSetup {
    let env = Env::default();
    env.mock_all_auths();
    env.cost_estimate().budget().reset_unlimited();

    let admin = Address::generate(&env);
    let market = Address::generate(&env);

    // Deploy PULSEToken
    let token_id = env.register(PULSETokenContract, ());
    let token_client = pulse_token::PULSETokenContractClient::new(&env, &token_id);
    token_client.initialize(
        &admin,
        &String::from_str(&env, "PULSE"),
        &String::from_str(&env, "PLSE"),
        &7u32,
    );

    // Deploy Leaderboard
    let leaderboard_id = env.register(LeaderboardContract, ());
    let leaderboard_client = leaderboard::LeaderboardContractClient::new(&env, &leaderboard_id);

    // Deploy ReferralRegistry
    let referral_id = env.register(ReferralRegistryContract, ());
    let referral_client = ReferralRegistryContractClient::new(&env, &referral_id);

    // Initialize Leaderboard: market + referral as authorized callers
    leaderboard_client.initialize(&admin, &market, &referral_id);

    // Lever G: leaderboard mints the welcome bonus internally now, so it needs
    // the token address and minter authorization (mirrors mainnet upgrade).
    leaderboard_client.set_token_contract(&admin, &token_id);
    token_client.set_minter(&leaderboard_id);
    // Legacy: referral no longer mints directly, kept harmless.
    token_client.set_minter(&referral_id);

    // Register a SAC for native XLM
    let xlm_sac_id = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    // Initialize referral registry
    referral_client.initialize(&admin, &market, &token_id, &leaderboard_id, &xlm_sac_id);

    TestSetup {
        env,
        client: referral_client,
        admin,
        market,
        token_id,
        leaderboard_id,
        xlm_sac_id,
        referral_id,
    }
}

// ── 1. Register with display name + custom referrer ───────────────────────────

#[test]
fn test_register_with_referrer() {
    let t = setup();
    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    let no_ref: Option<Address> = None;
    t.client
        .register_referral(&referrer, &String::from_str(&t.env, "Referrer"), &no_ref);

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "CryptoKing"),
        &Some(referrer.clone()),
    );

    assert!(t.client.is_registered(&user));
    assert_eq!(t.client.get_referrer(&user), Some(referrer.clone()));
    assert!(t.client.has_referrer(&user));
    assert_eq!(t.client.get_referral_count(&referrer), 1);
}

// ── 2. Register with display name + no referrer ──────────────────────────────

#[test]
fn test_register_no_referrer() {
    let t = setup();
    let user = Address::generate(&t.env);

    let no_ref: Option<Address> = None;
    t.client
        .register_referral(&user, &String::from_str(&t.env, "JustBetting"), &no_ref);

    assert!(t.client.is_registered(&user));
    assert_eq!(t.client.get_referrer(&user), None);
    assert!(!t.client.has_referrer(&user));
}

// ── 3. Welcome bonus: 5 pts + 1 PULSE on registration ─────────────────────

#[test]
fn test_welcome_bonus() {
    let t = setup();
    let user = Address::generate(&t.env);

    let no_ref: Option<Address> = None;
    t.client
        .register_referral(&user, &String::from_str(&t.env, "NewUser"), &no_ref);

    // Leaderboard: 5 welcome points, no win/loss impact
    let lb_client = leaderboard::LeaderboardContractClient::new(&t.env, &t.leaderboard_id);
    assert_eq!(lb_client.get_points(&user), 5);
    let stats = lb_client.get_stats(&user);
    assert_eq!(stats.won_bets, 0);
    assert_eq!(stats.lost_bets, 0);

    // Token: 1 PULSE (7 decimals)
    let tok_client = pulse_token::PULSETokenContractClient::new(&t.env, &t.token_id);
    assert_eq!(tok_client.balance(&user), 1_0000000);
}

// ── 4. Reject self-referral ──────────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn test_reject_self_referral() {
    let t = setup();
    let user = Address::generate(&t.env);

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "SelfRef"),
        &Some(user.clone()),
    );
}

// ── 5. Reject double registration ────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #4)")]
fn test_reject_double_registration() {
    let t = setup();
    let user = Address::generate(&t.env);

    let no_ref: Option<Address> = None;
    t.client
        .register_referral(&user, &String::from_str(&t.env, "First"), &no_ref);

    // Second registration should fail
    t.client
        .register_referral(&user, &String::from_str(&t.env, "Second"), &no_ref);
}

// ── 6. Display name stored and retrievable ───────────────────────────────────

#[test]
fn test_display_name() {
    let t = setup();
    let user = Address::generate(&t.env);

    let no_ref: Option<Address> = None;
    t.client
        .register_referral(&user, &String::from_str(&t.env, "CryptoKing"), &no_ref);

    assert_eq!(
        t.client.get_display_name(&user),
        String::from_str(&t.env, "CryptoKing"),
    );

    // Unregistered user gets empty string
    let nobody = Address::generate(&t.env);
    assert_eq!(
        t.client.get_display_name(&nobody),
        String::from_str(&t.env, ""),
    );
}

// ── 7. Credit routes fee to referrer + 3 bonus points ────────────────────────

#[test]
fn test_credit_with_referrer() {
    let t = setup();
    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);

    let no_ref: Option<Address> = None;
    t.client
        .register_referral(&referrer, &String::from_str(&t.env, "Referrer"), &no_ref);

    // Register user with referrer
    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "BetFan"),
        &Some(referrer.clone()),
    );

    // Fund the referral contract with XLM so it can pay out
    let sac_admin = StellarAssetClient::new(&t.env, &t.xlm_sac_id);
    sac_admin.mint(&t.referral_id, &100_0000000_i128); // 100 XLM

    let referral_fee: i128 = 5_000_000; // 0.5 XLM

    // Call credit from market contract
    let result = t.client.credit(&t.market, &user, &referral_fee);
    assert!(result);

    // Referrer received the XLM
    let xlm_client = TokenClient::new(&t.env, &t.xlm_sac_id);
    assert_eq!(xlm_client.balance(&referrer), referral_fee);

    // Referrer got 3 leaderboard bonus points
    let lb_client = leaderboard::LeaderboardContractClient::new(&t.env, &t.leaderboard_id);
    assert_eq!(lb_client.get_points(&referrer), 8);

    // Earnings tracked
    assert_eq!(t.client.get_earnings(&referrer), referral_fee);
}

// ── 8. Credit returns false when no custom referrer ──────────────────────────

#[test]
fn test_credit_no_referrer() {
    let t = setup();
    let user = Address::generate(&t.env);

    // Register without referrer
    let no_ref: Option<Address> = None;
    t.client
        .register_referral(&user, &String::from_str(&t.env, "Solo"), &no_ref);

    // In real flow, prediction_market transfers referral_fee to this contract
    // before calling credit. Mirror that here.
    let sac_admin = StellarAssetClient::new(&t.env, &t.xlm_sac_id);
    sac_admin.mint(&t.referral_id, &10_0000000_i128);

    let xlm_client = TokenClient::new(&t.env, &t.xlm_sac_id);
    let market_bal_before = xlm_client.balance(&t.market);

    let result = t.client.credit(&t.market, &user, &5_000_000);
    assert!(!result);

    // Fee returned to caller (market contract)
    assert_eq!(xlm_client.balance(&t.market), market_bal_before + 5_000_000);
}

// ── 8b. Credit returns false for completely unregistered user ─────────────────

#[test]
fn test_credit_unregistered_user() {
    let t = setup();
    let user = Address::generate(&t.env);

    // In real flow, prediction_market transfers referral_fee to this contract first
    let sac_admin = StellarAssetClient::new(&t.env, &t.xlm_sac_id);
    sac_admin.mint(&t.referral_id, &10_0000000_i128);

    let xlm_client = TokenClient::new(&t.env, &t.xlm_sac_id);
    let market_bal_before = xlm_client.balance(&t.market);

    let result = t.client.credit(&t.market, &user, &5_000_000);
    assert!(!result);

    // Fee returned to caller (market contract)
    assert_eq!(xlm_client.balance(&t.market), market_bal_before + 5_000_000);
}

// ── 9. Earnings accumulation across multiple credits ─────────────────────────

#[test]
fn test_earnings_accumulation() {
    let t = setup();
    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);

    let no_ref: Option<Address> = None;
    t.client
        .register_referral(&referrer, &String::from_str(&t.env, "Referrer"), &no_ref);

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "Bettor"),
        &Some(referrer.clone()),
    );

    // Fund the referral contract with XLM
    let sac_admin = StellarAssetClient::new(&t.env, &t.xlm_sac_id);
    sac_admin.mint(&t.referral_id, &1000_0000000_i128); // 1000 XLM

    // Multiple credits
    t.client.credit(&t.market, &user, &5_000_000_i128); // 0.5 XLM
    t.client.credit(&t.market, &user, &3_000_000_i128); // 0.3 XLM
    t.client.credit(&t.market, &user, &2_000_000_i128); // 0.2 XLM

    assert_eq!(t.client.get_earnings(&referrer), 10_000_000_i128); // 1.0 XLM total
}

// ── 10. Referrer bonus points accumulate (3 per referred bet) ────────────────

#[test]
fn test_referrer_bonus_points_accumulate() {
    let t = setup();
    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);

    let no_ref: Option<Address> = None;
    t.client
        .register_referral(&referrer, &String::from_str(&t.env, "Referrer"), &no_ref);

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "Bettor"),
        &Some(referrer.clone()),
    );

    // Fund
    let sac_admin = StellarAssetClient::new(&t.env, &t.xlm_sac_id);
    sac_admin.mint(&t.referral_id, &1000_0000000_i128);

    // 3 credits → 3 × 3 = 9 bonus pts for referrer
    t.client.credit(&t.market, &user, &5_000_000_i128);
    t.client.credit(&t.market, &user, &5_000_000_i128);
    t.client.credit(&t.market, &user, &5_000_000_i128);

    let lb_client = leaderboard::LeaderboardContractClient::new(&t.env, &t.leaderboard_id);
    assert_eq!(lb_client.get_points(&referrer), 14); // welcome bonus + 3 × 3 pts
}

// ── 11. Referral count tracking ──────────────────────────────────────────────

#[test]
fn test_referral_count_tracking() {
    let t = setup();
    let referrer = Address::generate(&t.env);
    let no_ref: Option<Address> = None;
    t.client
        .register_referral(&referrer, &String::from_str(&t.env, "Referrer"), &no_ref);

    // 3 users register with the same referrer
    for _ in 0..3 {
        let user = Address::generate(&t.env);
        t.client.register_referral(
            &user,
            &String::from_str(&t.env, "Buddy"),
            &Some(referrer.clone()),
        );
    }

    assert_eq!(t.client.get_referral_count(&referrer), 3);
}

// ── 12. Unregistered referrers are rejected ─────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn test_reject_unregistered_referrer() {
    let t = setup();
    let user = Address::generate(&t.env);
    let unregistered_referrer = Address::generate(&t.env);

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "Bettor"),
        &Some(unregistered_referrer),
    );
}

// ── 13. Double initialization rejected ───────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #1)")]
fn test_double_init_rejected() {
    let t = setup();

    // Second init should fail
    let market2 = Address::generate(&t.env);
    t.client.initialize(
        &t.admin,
        &market2,
        &t.token_id,
        &t.leaderboard_id,
        &t.xlm_sac_id,
    );
}

// ── Lever A: lazy migration — a user stored under the OLD key layout must still
//    be fully readable after the upgrade (Registered + DisplayName + Referrer). ──
#[test]
fn test_legacy_user_still_readable() {
    let s = setup();
    let legacy_user = Address::generate(&s.env);
    let legacy_ref = Address::generate(&s.env);

    // Simulate a pre-upgrade registration by writing the OLD keys directly.
    s.env.as_contract(&s.referral_id, || {
        s.env
            .storage()
            .persistent()
            .set(&DataKey::Registered(legacy_user.clone()), &true);
        s.env.storage().persistent().set(
            &DataKey::DisplayName(legacy_user.clone()),
            &String::from_str(&s.env, "OldTimer"),
        );
        s.env
            .storage()
            .persistent()
            .set(&DataKey::Referrer(legacy_user.clone()), &legacy_ref);
    });

    // All read paths must resolve via the legacy fallback.
    assert!(s.client.is_registered(&legacy_user));
    assert_eq!(
        s.client.get_display_name(&legacy_user),
        String::from_str(&s.env, "OldTimer")
    );
    assert_eq!(
        s.client.get_referrer(&legacy_user),
        Some(legacy_ref.clone())
    );
    assert!(s.client.has_referrer(&legacy_user));

    // And a legacy user must NOT be able to double-register under the new scheme.
    let res =
        s.client
            .try_register_referral(&legacy_user, &String::from_str(&s.env, "OldTimer"), &None);
    assert!(res.is_err());
}

// A legacy user with NO referrer (only Registered + DisplayName) reads correctly.
#[test]
fn test_legacy_user_without_referrer() {
    let s = setup();
    let legacy_user = Address::generate(&s.env);
    s.env.as_contract(&s.referral_id, || {
        s.env
            .storage()
            .persistent()
            .set(&DataKey::Registered(legacy_user.clone()), &true);
        s.env.storage().persistent().set(
            &DataKey::DisplayName(legacy_user.clone()),
            &String::from_str(&s.env, "Solo"),
        );
    });
    assert!(s.client.is_registered(&legacy_user));
    assert_eq!(s.client.get_referrer(&legacy_user), None);
    assert!(!s.client.has_referrer(&legacy_user));
}

// ── Cross-contract interface versioning (issue #84) ───────────────────────────

// A stand-in for a leaderboard deployment that was upgraded to an
// incompatible ABI: it only implements interface_version(), reporting a
// version this referral_registry build does not expect. Used to prove that
// register_referral/credit refuse to call into it rather than failing deep
// inside argument decoding (or, worse, silently misinterpreting arguments).
#[contract]
struct MockIncompatibleLeaderboard;

#[contractimpl]
impl MockIncompatibleLeaderboard {
    pub fn interface_version(_env: Env) -> u32 {
        99
    }
}

fn setup_with_incompatible_leaderboard() -> TestSetup {
    let env = Env::default();
    env.mock_all_auths();
    env.cost_estimate().budget().reset_unlimited();

    let admin = Address::generate(&env);
    let market = Address::generate(&env);

    let token_id = env.register(PULSETokenContract, ());
    pulse_token::PULSETokenContractClient::new(&env, &token_id).initialize(
        &admin,
        &String::from_str(&env, "PULSE"),
        &String::from_str(&env, "PLSE"),
        &7u32,
    );

    let leaderboard_id = env.register(MockIncompatibleLeaderboard, ());

    let referral_id = env.register(ReferralRegistryContract, ());
    let referral_client = ReferralRegistryContractClient::new(&env, &referral_id);

    let xlm_sac_id = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    referral_client.initialize(&admin, &market, &token_id, &leaderboard_id, &xlm_sac_id);

    TestSetup {
        env,
        client: referral_client,
        admin,
        market,
        token_id,
        leaderboard_id,
        xlm_sac_id,
        referral_id,
    }
}

#[test]
fn test_interface_version_reported() {
    let t = setup();
    assert_eq!(t.client.interface_version(), 1);
}

#[test]
fn test_register_referral_rejects_incompatible_leaderboard() {
    let t = setup_with_incompatible_leaderboard();

    let user = Address::generate(&t.env);
    let result =
        t.client
            .try_register_referral(&user, &String::from_str(&t.env, "Someone"), &None);
    assert_eq!(result.unwrap_err().unwrap(), ReferralError::IncompatibleInterface);
    // The registration itself must not have partially applied.
    assert!(!t.client.is_registered(&user));
}

#[test]
fn test_credit_rejects_incompatible_leaderboard() {
    let t = setup_with_incompatible_leaderboard();

    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    // Write the profile directly — register_referral itself would already
    // fail against the incompatible leaderboard, and this test is only
    // concerned with credit()'s own version check.
    t.env.as_contract(&t.referral_id, || {
        t.env.storage().persistent().set(
            &DataKey::Profile(user.clone()),
            &UserProfile {
                display_name: String::from_str(&t.env, "Bettor"),
                referrer: Some(referrer),
            },
        );
    });

    // Fund the referral contract so the fee transfer preceding the
    // leaderboard call succeeds and the version check is what's exercised.
    let sac_admin = StellarAssetClient::new(&t.env, &t.xlm_sac_id);
    sac_admin.mint(&t.referral_id, &100_0000000_i128);

    let result = t.client.try_credit(&t.market, &user, &1_0000000_i128);
    assert_eq!(result.unwrap_err().unwrap(), ReferralError::IncompatibleInterface);
}

// A stand-in for a leaderboard deployment that reports the EXPECTED version
// (so require_compatible_leaderboard's check passes) but is missing the
// actual function the caller is about to invoke. This is the limitation the
// version check does not cover: a matching u32 alone does not prove the
// callee's real function shape still matches, only that its author intended
// it to. If someone bumps INTERFACE_VERSION without actually shipping the
// signature that number is supposed to promise, callers still break, just
// past the version check instead of at it.
#[contract]
struct MockLeaderboardMissingRewardBonus;

#[contractimpl]
impl MockLeaderboardMissingRewardBonus {
    pub fn interface_version(_env: Env) -> u32 {
        1
    }
    // No reward_bonus() here on purpose.
}

fn setup_with_version_matched_but_incompatible_leaderboard() -> TestSetup {
    let env = Env::default();
    env.mock_all_auths();
    env.cost_estimate().budget().reset_unlimited();

    let admin = Address::generate(&env);
    let market = Address::generate(&env);

    let token_id = env.register(PULSETokenContract, ());
    pulse_token::PULSETokenContractClient::new(&env, &token_id).initialize(
        &admin,
        &String::from_str(&env, "PULSE"),
        &String::from_str(&env, "PLSE"),
        &7u32,
    );

    let leaderboard_id = env.register(MockLeaderboardMissingRewardBonus, ());

    let referral_id = env.register(ReferralRegistryContract, ());
    let referral_client = ReferralRegistryContractClient::new(&env, &referral_id);

    let xlm_sac_id = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    referral_client.initialize(&admin, &market, &token_id, &leaderboard_id, &xlm_sac_id);

    TestSetup {
        env,
        client: referral_client,
        admin,
        market,
        token_id,
        leaderboard_id,
        xlm_sac_id,
        referral_id,
    }
}

// Documents the known gap in issue #84's fix: a version match is not proof
// of ABI compatibility. require_compatible_leaderboard happily passes here
// (both sides report version 1), but the subsequent reward_bonus() call
// still traps because the mock never implements it. The version check is a
// best-effort signal, not a guarantee, unless every breaking ABI change is
// disciplined about bumping INTERFACE_VERSION at the same time.
#[test]
#[should_panic]
fn test_matching_version_does_not_guarantee_call_succeeds() {
    let t = setup_with_version_matched_but_incompatible_leaderboard();
    let user = Address::generate(&t.env);
    // Passes require_compatible_leaderboard (version 1 == version 1), then
    // panics inside the real reward_bonus() invoke_contract call because the
    // function doesn't exist on the callee.
    t.client
        .register_referral(&user, &String::from_str(&t.env, "Someone"), &None);
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
#[should_panic(expected = "Error(Contract, #6)")]
fn test_pause_rejects_non_admin() {
    let t = setup();
    let not_admin = Address::generate(&t.env);
    t.client.pause(&not_admin);
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_paused_rejects_register_referral() {
    let t = setup();
    t.client.pause(&t.admin);
    let user = Address::generate(&t.env);
    t.client
        .register_referral(&user, &String::from_str(&t.env, "Someone"), &None);
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_paused_rejects_credit() {
    let t = setup();
    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    // Write the profile directly rather than via register_referral, so this
    // test only exercises the pause gate on credit() (register_referral's
    // leaderboard cross-call has an unrelated, pre-existing ABI mismatch).
    t.env.as_contract(&t.referral_id, || {
        t.env.storage().persistent().set(
            &DataKey::Profile(user.clone()),
            &UserProfile {
                display_name: String::from_str(&t.env, "Bettor"),
                referrer: Some(referrer.clone()),
            },
        );
    });

    t.client.pause(&t.admin);
    t.client.credit(&t.market, &user, &1_0000000_i128);
}

#[test]
fn test_view_functions_work_while_paused() {
    let t = setup();
    let user = Address::generate(&t.env);
    t.env.as_contract(&t.referral_id, || {
        t.env.storage().persistent().set(
            &DataKey::Profile(user.clone()),
            &UserProfile {
                display_name: String::from_str(&t.env, "Someone"),
                referrer: None,
            },
        );
    });

    t.client.pause(&t.admin);
    assert!(t.client.is_registered(&user));
}

#[test]
fn test_register_referral_emits_event() {
    let t = setup();
    let user = Address::generate(&t.env);
    let no_ref: Option<Address> = None;
    t.client
        .register_referral(&user, &String::from_str(&t.env, "Alice"), &no_ref);
    let events = t.env.events().all();
    let last = events.get(events.len() - 1).unwrap();
    let name = Symbol::try_from_val(&t.env, &last.1.get_unchecked(0)).unwrap();
    assert_eq!(name, Symbol::new(&t.env, "referral_registered"));
}
