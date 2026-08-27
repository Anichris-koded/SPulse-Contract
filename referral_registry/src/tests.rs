use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env, String,
};

use leaderboard::LeaderboardContract;
use pulse_token::PULSETokenContract;

struct TestSetup {
    env: Env,
    client: ReferralRegistryContractClient<'static>,
    admin: Address,
    market: Address,
    token_client: pulse_token::PULSETokenContractClient<'static>,
    leaderboard_client: leaderboard::LeaderboardContractClient<'static>,
    xlm: TokenClient<'static>,
    xlm_admin: StellarAssetClient<'static>,
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
    let market = Address::generate(&env);

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
    leaderboard_client.initialize(&admin, &market, &token_id);

    let referral_id = env.register(ReferralRegistryContract, ());
    let client = ReferralRegistryContractClient::new(&env, &referral_id);
    client.initialize(&admin, &market, &token_id, &leaderboard_id, &xlm_sac_id);

    leaderboard_client.set_token_contract(&admin, &token_id);
    token_client.set_minter(&leaderboard_id);
    token_client.set_minter(&referral_id);
    token_client.set_minter(&market);

    xlm_admin.mint(&referral_id, &1000_0000000);

    TestSetup {
        env,
        client,
        admin,
        market,
        token_client,
        leaderboard_client,
        xlm,
        xlm_admin,
    }
}

#[test]
fn test_register_referral_without_referrer() {
    let t = setup();
    let user = Address::generate(&t.env);

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Option::<Address>::None,
    );

    assert_eq!(t.client.get_referrer(&user), None);
    assert_eq!(t.client.get_display_name(&user), Some(String::from_str(&t.env, "User")));
}

#[test]
fn test_register_referral_with_referrer() {
    let t = setup();
    let referrer = Address::generate(&t.env);
    let user = Address::generate(&t.env);

    t.client.register_referral(
        &referrer,
        &String::from_str(&t.env, "Referrer"),
        &Option::<Address>::None,
    );

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Some(referrer.clone()),
    );

    assert_eq!(t.client.get_referrer(&user), Some(referrer.clone()));
    assert_eq!(t.client.get_referrer_count(&referrer), 1);
}

#[test]
fn test_credit_with_referrer_pays_referrer() {
    let t = setup();
    let referrer = Address::generate(&t.env);
    let user = Address::generate(&t.env);

    t.client.register_referral(
        &referrer,
        &String::from_str(&t.env, "Referrer"),
        &Option::<Address>::None,
    );
    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Some(referrer.clone()),
    );

    let referral_fee = 5_0000000_i128;
    let referrer_balance_before = t.xlm.balance(&referrer);

    let paid = t.client.credit(&t.market, &user, &referral_fee);
    assert_eq!(paid, true);

    assert_eq!(t.xlm.balance(&referrer), referrer_balance_before + referral_fee);
    assert_eq!(t.client.get_earnings(&referrer), referral_fee);
}

#[test]
fn test_credit_without_referrer_returns_to_bettor() {
    let t = setup();
    let user = Address::generate(&t.env);

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Option::<Address>::None,
    );

    let referral_fee = 5_0000000_i128;
    let user_balance_before = t.xlm.balance(&user);

    let paid = t.client.credit(&t.market, &user, &referral_fee);
    assert_eq!(paid, false);

    assert_eq!(t.xlm.balance(&user), user_balance_before + referral_fee);
}

#[test]
fn test_credit_after_late_referrer_registration() {
    let t = setup();
    let referrer = Address::generate(&t.env);
    let user = Address::generate(&t.env);

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Option::<Address>::None,
    );

    let referral_fee = 5_0000000_i128;
    let user_balance_before = t.xlm.balance(&user);

    let paid = t.client.credit(&t.market, &user, &referral_fee);
    assert_eq!(paid, false);
    assert_eq!(t.xlm.balance(&user), user_balance_before + referral_fee);

    t.client.register_referral(
        &referrer,
        &String::from_str(&t.env, "Referrer"),
        &Option::<Address>::None,
    );

    let referrer_balance_before = t.xlm.balance(&referrer);
    let paid = t.client.credit(&t.market, &user, &referral_fee);
    assert_eq!(paid, true);
    assert_eq!(t.xlm.balance(&referrer), referrer_balance_before + referral_fee);
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_credit_rejects_non_market_caller() {
    let t = setup();
    let user = Address::generate(&t.env);
    let rando = Address::generate(&t.env);

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Option::<Address>::None,
    );

    t.client.credit(&rando, &user, &5_0000000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #4)")]
fn test_register_referral_rejects_double_registration() {
    let t = setup();
    let user = Address::generate(&t.env);

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Option::<Address>::None,
    );
    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Option::<Address>::None,
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_register_referral_rejects_self_referral() {
    let t = setup();
    let user = Address::generate(&t.env);

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Some(user.clone()),
    );
}

#[test]
fn test_referral_depth_limit() {
    let t = setup();
    let max_depth = 5;

    let mut users: Vec<Address> = Vec::new(&t.env);
    for _ in 0..(max_depth + 2) {
        users.push_back(Address::generate(&t.env));
    }

    t.client.register_referral(
        &users.get(0).unwrap(),
        &String::from_str(&t.env, "U0"),
        &Option::<Address>::None,
    );

    for i in 1..=max_depth {
        let referrer = users.get((i - 1) as u32).unwrap();
        let user = users.get(i as u32).unwrap();
        let result = if i == max_depth {
            t.client.try_register_referral(
                &user,
                &String::from_str(&t.env, "U"),
                &Some(referrer.clone()),
            )
        } else {
            t.client.register_referral(
                &user,
                &String::from_str(&t.env, "U"),
                &Some(referrer.clone()),
            );
            Ok(Ok(()))
        };
        if i == max_depth {
            assert!(result.is_err(), "should fail at depth limit");
        }
    }
}

#[test]
fn test_pause_unpause() {
    let t = setup();
    let user = Address::generate(&t.env);

    t.client.pause(&t.admin);
    assert!(t.client.is_paused());

    let result = t.client.try_register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Option::<Address>::None,
    );
    assert!(result.is_err());

    t.client.unpause(&t.admin);
    assert!(!t.client.is_paused());

    t.client.register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Option::<Address>::None,
    );
    assert_eq!(t.client.get_referrer(&user), None);
}
