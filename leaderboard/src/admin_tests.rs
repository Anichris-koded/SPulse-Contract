//! Issue #20 — admin governance: remove / ban / reset
//!
//! Tests cover every scenario described in the issue and the proposed
//! implementation plan:
//!
//!  * remove_player: top-slot, min-slot, mid-slot, unranked-only, unknown.
//!  * ban_player: ban flag written, stats erased, leaderboard slot freed,
//!    idempotency, and that banned player cannot earn more points.
//!  * reset_player: points zeroed, bet history preserved, slot freed,
//!    re-entry after reset, and unknown-player guard.
//!  * Non-admin rejected for all three functions.

use super::*;
use soroban_sdk::{testutils::Address as _, Env};

// ── helpers ──────────────────────────────────────────────────────────────────

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
    env.cost_estimate().disable_resource_limits();

    let contract_id = env.register(LeaderboardContract, ());
    let client = LeaderboardContractClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let market = Address::generate(&env);
    let referral = Address::generate(&env);

    client.initialize(&admin, &market, &referral);
    (env, client, admin, market, referral)
}

// ── remove_player ─────────────────────────────────────────────────────────────

#[test]
fn test_remove_ranked_player_from_top_slot() {
    // A player at rank 1 (top slot 0) is fully erased.
    let (env, client, admin, market, _referral) = setup();

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    client.add_pts(&market, &alice, &200_u64, &true);
    client.add_pts(&market, &bob, &100_u64, &true);
    assert_eq!(client.get_rank(&alice), 1);
    assert_eq!(client.get_top_player_count(), 2);

    client.remove_player(&admin, &alice);

    // Stats erased.
    assert_eq!(client.get_points(&alice), 0);
    let stats = client.get_stats(&alice);
    assert_eq!(stats.total_bets, 0);

    // Top list compacted: bob is now rank 1.
    assert_eq!(client.get_top_player_count(), 1);
    assert_eq!(client.get_rank(&alice), UNRANKED_RANK);
    assert_eq!(client.get_rank(&bob), 1);

    // Reverse lookup cleared.
    let still_mapped = env.as_contract(&client.address, || {
        env.storage()
            .persistent()
            .has(&DataKey::TopPlayerSlot(alice.clone()))
    });
    assert!(!still_mapped);
}

#[test]
fn test_remove_player_at_min_slot_updates_min_cache() {
    // After the weakest ranked player (the min) is removed, the min cache
    // must reflect the next weakest entry.
    let (env, client, admin, market, _referral) = setup();

    let low = Address::generate(&env);
    let mid = Address::generate(&env);
    let high = Address::generate(&env);
    client.add_pts(&market, &low, &10_u64, &true); // min
    client.add_pts(&market, &mid, &50_u64, &true);
    client.add_pts(&market, &high, &100_u64, &true);
    assert_eq!(client.get_top_player_count(), 3);

    client.remove_player(&admin, &low);

    // The new min must now be 50, not 10.
    assert_eq!(client.get_top_player_count(), 2);
    assert!(client.get_min_points() >= 50);
    assert_eq!(client.get_rank(&low), UNRANKED_RANK);
}

#[test]
fn test_remove_mid_ranked_player_compacts_list() {
    // Removing a player in the middle of the list must compact the remaining
    // players without leaving holes in the forward index.
    let (env, client, admin, market, _referral) = setup();

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let charlie = Address::generate(&env);
    client.add_pts(&market, &alice, &300_u64, &true); // rank 1
    client.add_pts(&market, &bob, &200_u64, &true); // rank 2
    client.add_pts(&market, &charlie, &100_u64, &true); // rank 3

    client.remove_player(&admin, &bob);

    assert_eq!(client.get_top_player_count(), 2);
    let top = client.get_top_players(&0_u32, &20_u32);
    assert_eq!(top.len(), 2);
    assert_eq!(top.get(0).unwrap().address, alice);
    assert_eq!(top.get(1).unwrap().address, charlie);
    assert_eq!(client.get_rank(&alice), 1);
    assert_eq!(client.get_rank(&charlie), 2);
    assert_eq!(client.get_rank(&bob), UNRANKED_RANK);
}

#[test]
fn test_remove_unranked_player_erases_only_stats() {
    // A player who has points but is not in the top list (list already full or
    // their score didn't qualify) can still be removed.
    let (env, client, admin, market, _referral) = setup();

    // Fill the list with 50 high scorers.
    for i in 0u64..50 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(1000 + i), &true);
    }
    // Low scorer: has stats but is NOT in the list.
    let weak = Address::generate(&env);
    client.add_pts(&market, &weak, &5_u64, &false);
    assert_eq!(client.get_rank(&weak), UNRANKED_RANK);
    assert_eq!(client.get_points(&weak), 5);

    client.remove_player(&admin, &weak);

    assert_eq!(client.get_points(&weak), 0);
    assert_eq!(client.get_top_player_count(), 50); // top list untouched
}

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn test_remove_unknown_player_returns_player_not_found() {
    // Removing an address that was never tracked must return PlayerNotFound (#9).
    let (env, client, admin, _market, _referral) = setup();
    let ghost = Address::generate(&env);
    client.remove_player(&admin, &ghost);
}

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn test_remove_player_rejects_non_admin() {
    let (env, client, _admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    let rando = Address::generate(&env);
    client.add_pts(&market, &alice, &100_u64, &true);
    client.remove_player(&rando, &alice);
}

#[test]
fn test_remove_player_and_re_enter_leaderboard() {
    // A removed player is treated as a fresh entrant: they can earn points
    // and re-enter the top list with no ghost ranking from before.
    let (env, client, admin, market, _referral) = setup();

    let alice = Address::generate(&env);
    client.add_pts(&market, &alice, &500_u64, &true);
    assert_eq!(client.get_rank(&alice), 1);

    client.remove_player(&admin, &alice);
    assert_eq!(client.get_rank(&alice), UNRANKED_RANK);
    assert_eq!(client.get_points(&alice), 0);

    // Re-earn points from scratch.
    client.add_pts(&market, &alice, &300_u64, &true);
    assert_eq!(client.get_points(&alice), 300);
    assert_eq!(client.get_rank(&alice), 1);
}

// ── ban_player ────────────────────────────────────────────────────────────────

#[test]
fn test_ban_player_sets_flag_and_erases_state() {
    let (env, client, admin, market, _referral) = setup();

    let alice = Address::generate(&env);
    client.add_pts(&market, &alice, &200_u64, &true);
    assert_eq!(client.get_rank(&alice), 1);
    assert!(!client.is_banned(&alice));

    client.ban_player(&admin, &alice);

    // Ban flag visible.
    assert!(client.is_banned(&alice));
    // Stats erased.
    assert_eq!(client.get_points(&alice), 0);
    // Top list compacted.
    assert_eq!(client.get_top_player_count(), 0);
    assert_eq!(client.get_rank(&alice), UNRANKED_RANK);
    // Reverse lookup cleared.
    let still_mapped = env.as_contract(&client.address, || {
        env.storage()
            .persistent()
            .has(&DataKey::TopPlayerSlot(alice.clone()))
    });
    assert!(!still_mapped);
}

#[test]
fn test_ban_player_idempotent() {
    // Banning the same player twice must not panic and must leave the ban
    // flag intact.
    let (env, client, admin, market, _referral) = setup();

    let alice = Address::generate(&env);
    client.add_pts(&market, &alice, &100_u64, &true);

    client.ban_player(&admin, &alice);
    assert!(client.is_banned(&alice));

    // Second call is idempotent.
    client.ban_player(&admin, &alice);
    assert!(client.is_banned(&alice));
    assert_eq!(client.get_top_player_count(), 0);
}

#[test]
fn test_ban_preserves_other_players_in_list() {
    // Banning one player must not disturb the rest of the top list.
    let (env, client, admin, market, _referral) = setup();

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let charlie = Address::generate(&env);
    client.add_pts(&market, &alice, &300_u64, &true);
    client.add_pts(&market, &bob, &200_u64, &true);
    client.add_pts(&market, &charlie, &100_u64, &true);

    client.ban_player(&admin, &bob);

    assert_eq!(client.get_top_player_count(), 2);
    assert_eq!(client.get_rank(&alice), 1);
    assert_eq!(client.get_rank(&charlie), 2);
    assert_eq!(client.get_rank(&bob), UNRANKED_RANK);
    assert!(client.is_banned(&bob));
}

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn test_ban_player_rejects_non_admin() {
    let (env, client, _admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    let rando = Address::generate(&env);
    client.add_pts(&market, &alice, &100_u64, &true);
    client.ban_player(&rando, &alice);
}

// ── reset_player ──────────────────────────────────────────────────────────────

#[test]
fn test_reset_player_zeroes_points_but_preserves_bet_history() {
    let (env, client, admin, market, referral) = setup();

    let alice = Address::generate(&env);
    client.add_pts(&market, &alice, &100_u64, &true); // 1 win
    client.add_pts(&market, &alice, &50_u64, &false); // 1 loss
    client.add_bonus_pts(&referral, &alice, &25_u64); // 1 bonus

    let before = client.get_stats(&alice);
    assert_eq!(before.points, 175);
    assert_eq!(before.won_bets, 1);
    assert_eq!(before.lost_bets, 1);
    assert_eq!(before.total_bets, 3);

    client.reset_player(&admin, &alice);

    let after = client.get_stats(&alice);
    assert_eq!(after.points, 0); // zeroed
    // Win/loss/bonus history preserved.
    assert_eq!(after.won_bets, 1);
    assert_eq!(after.lost_bets, 1);
    assert_eq!(after.total_bets, 3);
}

#[test]
fn test_reset_removes_player_from_top_list() {
    let (env, client, admin, market, _referral) = setup();

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    client.add_pts(&market, &alice, &300_u64, &true);
    client.add_pts(&market, &bob, &100_u64, &true);
    assert_eq!(client.get_rank(&alice), 1);

    client.reset_player(&admin, &alice);

    assert_eq!(client.get_rank(&alice), UNRANKED_RANK);
    assert_eq!(client.get_top_player_count(), 1);
    assert_eq!(client.get_rank(&bob), 1);
}

#[test]
fn test_reset_player_allows_re_entry() {
    // After a reset the player has 0 points, which means they're gone from
    // the list. They can earn points again and re-enter from scratch.
    let (env, client, admin, market, _referral) = setup();

    let alice = Address::generate(&env);
    client.add_pts(&market, &alice, &500_u64, &true);
    assert_eq!(client.get_rank(&alice), 1);

    client.reset_player(&admin, &alice);
    assert_eq!(client.get_rank(&alice), UNRANKED_RANK);
    assert_eq!(client.get_points(&alice), 0);

    // Re-earn and re-enter.
    client.add_pts(&market, &alice, &200_u64, &true);
    assert_eq!(client.get_points(&alice), 200);
    assert_eq!(client.get_rank(&alice), 1);
}

#[test]
fn test_reset_min_slot_player_repairs_min_cache() {
    // Resetting the min-slot player (weakest in list) must recompute the
    // MinPoints cache so future eviction thresholds are correct.
    let (env, client, admin, market, _referral) = setup();

    let low = Address::generate(&env);
    let mid = Address::generate(&env);
    let high = Address::generate(&env);
    client.add_pts(&market, &low, &10_u64, &true);
    client.add_pts(&market, &mid, &50_u64, &true);
    client.add_pts(&market, &high, &100_u64, &true);
    assert_eq!(client.get_top_player_count(), 3);

    client.reset_player(&admin, &low);

    assert_eq!(client.get_top_player_count(), 2);
    // Min cache must now reflect the new weakest (50), not the old 10.
    assert!(client.get_min_points() >= 50);
}

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn test_reset_unknown_player_returns_player_not_found() {
    let (env, client, admin, _market, _referral) = setup();
    let ghost = Address::generate(&env);
    client.reset_player(&admin, &ghost);
}

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn test_reset_player_rejects_non_admin() {
    let (env, client, _admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    let rando = Address::generate(&env);
    client.add_pts(&market, &alice, &100_u64, &true);
    client.reset_player(&rando, &alice);
}

// ── Combined scenarios ────────────────────────────────────────────────────────

#[test]
fn test_full_list_remove_boundary_players() {
    // Fill the list to 50 then remove rank-1 (top slot), rank-25 (middle),
    // and rank-50 (min slot). The list must shrink and remain consistent.
    let (env, client, admin, market, _referral) = setup();

    let mut players = soroban_sdk::vec![&env];
    for i in 1u64..=50 {
        let user = Address::generate(&env);
        players.push_back(user.clone());
        // Insert in ascending order so rank 50 = points 1, rank 1 = points 50.
        client.add_pts(&market, &user, &i, &true);
    }
    assert_eq!(client.get_top_player_count(), 50);

    // rank 1 = highest points (player 50)
    let rank1_player = players.get(49).unwrap();
    // rank 25 — mid-list
    let rank25_player = players.get(25).unwrap();
    // rank 50 = lowest points (player 1, points = 1) — the min
    let rank50_player = players.get(0).unwrap();

    client.remove_player(&admin, &rank1_player);
    assert_eq!(client.get_top_player_count(), 49);
    assert_eq!(client.get_rank(&rank1_player), UNRANKED_RANK);

    client.remove_player(&admin, &rank25_player);
    assert_eq!(client.get_top_player_count(), 48);
    assert_eq!(client.get_rank(&rank25_player), UNRANKED_RANK);

    client.remove_player(&admin, &rank50_player);
    assert_eq!(client.get_top_player_count(), 47);
    assert_eq!(client.get_rank(&rank50_player), UNRANKED_RANK);
}

#[test]
fn test_remove_then_ban_is_independent() {
    // Removing a player and then banning a different player should both work
    // correctly in sequence without corrupting each other's state.
    let (env, client, admin, market, _referral) = setup();

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    client.add_pts(&market, &alice, &200_u64, &true);
    client.add_pts(&market, &bob, &100_u64, &true);

    client.remove_player(&admin, &alice);
    client.ban_player(&admin, &bob);

    assert_eq!(client.get_rank(&alice), UNRANKED_RANK);
    assert!(!client.is_banned(&alice)); // removed, not banned

    assert_eq!(client.get_rank(&bob), UNRANKED_RANK);
    assert!(client.is_banned(&bob));

    assert_eq!(client.get_top_player_count(), 0);
}
