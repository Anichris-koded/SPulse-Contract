// ── Issue #21: instance-storage TTL extension for MinPoints/MinSlot ──────────
//
// MinPoints, MinSlot, TopPlayerCount, Admin, MarketContract, ReferralContract
// and TokenContract all live in *instance* storage, which carries its own TTL
// separate from the per-key TTL on persistent entries (Stats, TopPlayerAt).
// These tests prove that instance TTL is actively extended so the leaderboard
// cache cannot silently expire on an idle-but-still-used contract.

use super::*;
use soroban_sdk::{
    testutils::{storage::Instance as _, Address as _, Ledger as _},
    Env,
};

fn setup() -> (
    Env,
    LeaderboardContractClient<'static>,
    Address,
    Address,
    Address,
) {
    let env = Env::default();
    env.mock_all_auths();
    env.cost_estimate().budget().reset_unlimited();

    let contract_id = env.register(LeaderboardContract, ());
    let client = LeaderboardContractClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let market = Address::generate(&env);
    let referral = Address::generate(&env);

    client.initialize(&admin, &market, &referral);
    (env, client, admin, market, referral)
}

fn instance_ttl(env: &Env, contract_id: &Address) -> u32 {
    env.as_contract(contract_id, || env.storage().instance().get_ttl())
}

#[test]
fn test_initialize_extends_instance_ttl() {
    // initialize() writes Admin/MarketContract/ReferralContract/TopPlayerCount/
    // MinPoints/MinSlot into instance storage — the TTL on that instance entry
    // must be bumped to TTL_HIGH, not left at the ledger's default minimum.
    let (env, client, _admin, _market, _referral) = setup();
    let ttl = instance_ttl(&env, &client.address);
    assert!(
        ttl >= TTL_BUMP,
        "instance TTL ({ttl}) was not extended past TTL_BUMP after initialize"
    );
}

#[test]
fn test_add_pts_refreshes_instance_ttl_when_below_threshold() {
    // Simulate the contract going idle for long enough that instance TTL
    // (which covers MinPoints/MinSlot) drops below the refresh threshold,
    // then prove the next write (add_pts) bumps it back up to TTL_HIGH —
    // this is the exact mechanism that prevents the cached min from expiring.
    let (env, client, _admin, market, _referral) = setup();

    let ttl_after_init = instance_ttl(&env, &client.address);
    assert!(ttl_after_init >= TTL_BUMP);

    // Advance the ledger far enough that the remaining TTL falls below
    // TTL_BUMP (the extend_ttl threshold) but the entry has not yet expired.
    let advance = ttl_after_init - TTL_BUMP + 1;
    env.ledger().set_sequence_number(advance);

    let ttl_before_write = instance_ttl(&env, &client.address);
    assert!(
        ttl_before_write < TTL_BUMP,
        "test setup invariant broken: ttl_before_write ({ttl_before_write}) should be below TTL_BUMP"
    );

    let user = Address::generate(&env);
    client.add_pts(&market, &user, &10_u64, &true);

    let ttl_after_write = instance_ttl(&env, &client.address);
    assert_eq!(
        ttl_after_write, TTL_HIGH,
        "add_pts must refresh instance TTL back to TTL_HIGH once it drops below TTL_BUMP"
    );
}

#[test]
fn test_add_bonus_pts_refreshes_instance_ttl_when_below_threshold() {
    let (env, client, _admin, _market, referral) = setup();

    let ttl_after_init = instance_ttl(&env, &client.address);
    let advance = ttl_after_init - TTL_BUMP + 1;
    env.ledger().set_sequence_number(advance);
    assert!(instance_ttl(&env, &client.address) < TTL_BUMP);

    let user = Address::generate(&env);
    client.add_bonus_pts(&referral, &user, &10_u64);

    assert_eq!(instance_ttl(&env, &client.address), TTL_HIGH);
}

#[test]
fn test_set_token_contract_refreshes_instance_ttl_when_below_threshold() {
    let (env, client, admin, _market, _referral) = setup();

    let ttl_after_init = instance_ttl(&env, &client.address);
    let advance = ttl_after_init - TTL_BUMP + 1;
    env.ledger().set_sequence_number(advance);
    assert!(instance_ttl(&env, &client.address) < TTL_BUMP);

    let token = Address::generate(&env);
    client.set_token_contract(&admin, &token);

    assert_eq!(instance_ttl(&env, &client.address), TTL_HIGH);
}

#[test]
fn test_min_points_and_min_slot_survive_ttl_refresh_cycle() {
    // Fill the top list to capacity, let instance TTL decay close to its
    // refresh threshold, then keep writing. MinPoints/MinSlot must remain
    // correct throughout — the TTL refresh must never reset or corrupt them.
    //
    // Points are inserted in descending order so each add_pts is an O(1)
    // append (new entry is always the new minimum, no bubble-up swaps) —
    // this keeps each call's write footprint within network resource limits,
    // independent of the TTL behavior under test.
    let (env, client, _admin, market, _referral) = setup();

    for i in 0u64..MAX_TOP_PLAYERS as u64 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(1000 - i), &true);
    }
    assert_eq!(client.get_top_player_count(), MAX_TOP_PLAYERS);
    let min_before = client.get_min_points();
    assert_eq!(min_before, 1000 - (MAX_TOP_PLAYERS as u64 - 1));

    let ttl_after_fill = instance_ttl(&env, &client.address);
    let advance = ttl_after_fill - TTL_BUMP + 1;
    env.ledger().set_sequence_number(advance);
    assert!(instance_ttl(&env, &client.address) < TTL_BUMP);

    // A newcomer beating the current min should still correctly evict it,
    // and the min cache should still be intact and correct afterward.
    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &(min_before + 1), &true);

    assert_eq!(instance_ttl(&env, &client.address), TTL_HIGH);
    assert_eq!(client.get_top_player_count(), MAX_TOP_PLAYERS);
    assert_eq!(client.get_min_points(), min_before + 1); // old min evicted
}

#[test]
fn test_instance_ttl_not_bumped_again_while_above_threshold() {
    // extend_ttl is a threshold-gated no-op when the TTL is already high
    // enough — verifies we're not silently masking a missing extend_ttl call
    // by asserting on a value that would also pass with none at all.
    let (env, client, _admin, market, _referral) = setup();

    let ttl_after_init = instance_ttl(&env, &client.address);
    assert_eq!(ttl_after_init, TTL_HIGH);

    // Advance a small amount that keeps remaining TTL above TTL_BUMP.
    env.ledger().set_sequence_number(10);
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &10_u64, &true);

    // live_until_ledger is unchanged, so ttl is simply reduced by the
    // sequence advance rather than reset to TTL_HIGH again.
    assert_eq!(instance_ttl(&env, &client.address), TTL_HIGH - 10);
}

// ── Cross-contract interface versioning (issue #84) ───────────────────────────

#[test]
fn test_interface_version_reported() {
    let (_env, client, _admin, _market, _referral) = setup();
    assert_eq!(client.interface_version(), 1);
}
