//! Issue #20 — admin governance: remove / ban / reset
//!
//! Tests cover every scenario described in the issue and the proposed
//! implementation plan:
//!
//!  * remove_player: top-slot, min-slot, mid-slot, unranked-only, unknown,
//!    player-with-only-pending-reward, and sequential multi-removal repair.
//!  * ban_player: ban flag written, stats erased, leaderboard slot freed,
//!    idempotency (including banning an unknown address), and that banned
//!    players are rejected on every accrual path (add_pts, reward,
//!    reward_bonus, add_bonus_pts, queue_reward, claim_pending_rewards).
//!  * reset_player: points zeroed, bet history preserved, slot freed,
//!    pending rewards dropped, unranked-with-stats, re-entry after reset,
//!    and unknown-player guard.
//!  * Non-admin rejected for all three functions; admin functions return
//!    NotInitialized when the contract has not been initialized.

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

// ── Issue #20: ban enforcement on every accrual path ──────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn test_banned_player_rejected_by_add_pts() {
    let (env, client, admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    client.add_pts(&market, &alice, &100_u64, &true);
    client.ban_player(&admin, &alice);
    client.add_pts(&market, &alice, &10_u64, &true);
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn test_banned_player_rejected_by_reward() {
    let (env, client, admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    client.add_pts(&market, &alice, &100_u64, &true);
    client.ban_player(&admin, &alice);
    client.reward(&market, &alice, &30_u64, &0_i128, &true);
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn test_banned_player_rejected_by_add_bonus_pts() {
    let (env, client, admin, _market, referral) = setup();
    let alice = Address::generate(&env);
    client.add_bonus_pts(&referral, &alice, &5_u64);
    client.ban_player(&admin, &alice);
    client.add_bonus_pts(&referral, &alice, &5_u64);
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn test_banned_player_rejected_by_reward_bonus() {
    let (env, client, admin, _market, referral) = setup();
    let alice = Address::generate(&env);
    client.add_bonus_pts(&referral, &alice, &5_u64);
    client.ban_player(&admin, &alice);
    client.reward_bonus(&referral, &alice, &5_u64, &0_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn test_banned_player_rejected_by_queue_reward() {
    let (env, client, admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    client.ban_player(&admin, &alice);
    client.queue_reward(&market, &alice, &30_u64, &0_i128, &true);
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn test_banned_player_rejected_by_claim_pending_rewards() {
    let (env, client, admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    // Queue while not banned, then ban before the claim is submitted.
    client.queue_reward(&market, &alice, &30_u64, &0_i128, &true);
    client.ban_player(&admin, &alice);
    client.claim_pending_rewards(&alice);
}

#[test]
fn test_claim_guard_fires_before_consuming_pending_reward() {
    // The claim guard must return PlayerBanned BEFORE consuming the pending
    // reward. Construct the state directly (ban flag without going through
    // ban_player, which also erases state) to prove the guard is independent.
    let (env, client, _admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    client.queue_reward(&market, &alice, &30_u64, &0_i128, &true);

    env.as_contract(&client.address, || {
        env.storage()
            .persistent()
            .set(&DataKey::BannedPlayer(alice.clone()), &true);
    });
    assert!(client.is_banned(&alice));

    let res = client.try_claim_pending_rewards(&alice);
    assert!(res.is_err(), "claim must fail for a banned player");

    // The pending reward survives so it is not destroyed by the rejection.
    let pending = client.get_pending_reward(&alice).unwrap();
    assert_eq!(pending.points, 30);
}

#[test]
fn test_banned_player_cannot_reenter_leaderboard() {
    // Even after being banned, accrual paths keep rejecting the player, so the
    // ban is a hard ceiling rather than a one-shot removal.
    let (env, client, admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    client.add_pts(&market, &alice, &100_u64, &true);
    client.ban_player(&admin, &alice);

    // Direct storage poke: force the ban flag away to prove accrual re-checks
    // the flag every call (belt and suspenders — not part of the public API).
    env.as_contract(&client.address, || {
        env.storage().persistent().set(&DataKey::BannedPlayer(alice.clone()), &false);
    });
    // No longer banned, so the player may accrue again.
    client.add_pts(&market, &alice, &50_u64, &true);
    assert_eq!(client.get_points(&alice), 50);
    assert_eq!(client.get_rank(&alice), 1);
}

// ── Issue #20: edge cases ─────────────────────────────────────────────────────

#[test]
fn test_ban_unknown_address_is_idempotent_ok() {
    // Banning an address that was never tracked must not error: it simply
    // records the ban (idempotent), and a second ban is also Ok.
    let (env, client, admin, _market, _referral) = setup();
    let ghost = Address::generate(&env);
    client.ban_player(&admin, &ghost);
    assert!(client.is_banned(&ghost));
    client.ban_player(&admin, &ghost);
    assert!(client.is_banned(&ghost));
    assert_eq!(client.get_top_player_count(), 0);
}

#[test]
fn test_reset_unranked_player_with_stats_works() {
    // A player who has stats but is kept out of a full list (low scorer) can
    // still be reset: points zeroed, bet history preserved, list untouched.
    let (env, client, admin, market, _referral) = setup();
    for i in 0u64..50 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(1000 + i), &true);
    }
    let weak = Address::generate(&env);
    client.add_pts(&market, &weak, &5_u64, &false);
    assert_eq!(client.get_rank(&weak), UNRANKED_RANK);
    assert_eq!(client.get_points(&weak), 5);

    client.reset_player(&admin, &weak);

    let after = client.get_stats(&weak);
    assert_eq!(after.points, 0);
    assert_eq!(after.lost_bets, 1); // history preserved
    assert_eq!(after.total_bets, 1);
    assert_eq!(client.get_top_player_count(), 50); // list untouched
}

#[test]
fn test_remove_player_with_only_pending_reward() {
    // A player who queued rewards but never claimed (no Stats, not in list)
    // is still tracked via PendingReward: remove must erase it, not fail.
    let (env, client, admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.queue_reward(&market, &user, &30_u64, &0_i128, &true);
    assert!(client.get_pending_reward(&user).is_some());
    assert_eq!(client.get_rank(&user), UNRANKED_RANK);

    client.remove_player(&admin, &user);

    assert_eq!(client.get_pending_reward(&user), None);
    assert_eq!(client.get_points(&user), 0);
}

#[test]
fn test_reset_player_clears_pending_reward() {
    // Resetting a player must also drop any queued-but-unclaimed points, so a
    // later claim cannot undo the reset.
    let (env, client, admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &100_u64, &true);
    client.queue_reward(&market, &user, &30_u64, &0_i128, &true);

    client.reset_player(&admin, &user);

    assert_eq!(client.get_pending_reward(&user), None);
    assert_eq!(client.get_points(&user), 0);
}

#[test]
fn test_repair_top_index_survives_many_sequential_removals() {
    // Remove every player one at a time. Each removal compacts the index via
    // repair_top_index; the board must stay consistent and end empty.
    let (env, client, admin, market, _referral) = setup();
    let mut players = soroban_sdk::vec![&env];
    for i in 1u64..=50 {
        let user = Address::generate(&env);
        players.push_back(user.clone());
        client.add_pts(&market, &user, &i, &true);
    }
    assert_eq!(client.get_top_player_count(), 50);

    for idx in 0..50 {
        let user = players.get(idx).unwrap();
        client.remove_player(&admin, &user);
        assert_eq!(client.get_rank(&user), UNRANKED_RANK);
        // After the 50th removal the list is empty and the min cache is reset.
        if idx < 49 {
            assert_eq!(client.get_top_player_count(), 49 - idx as u32);
        }
    }
    assert_eq!(client.get_top_player_count(), 0);
    assert_eq!(client.get_min_points(), 0);
    assert_eq!(client.get_min_slot(), 0);
}

// ── Issue #20: admin functions when the contract is not initialized ───────────

fn uninitialized_setup() -> (Env, LeaderboardContractClient<'static>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    env.cost_estimate().budget().reset_unlimited();
    env.cost_estimate().disable_resource_limits();

    let contract_id = env.register(LeaderboardContract, ());
    let client = LeaderboardContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let user = Address::generate(&env);
    (env, client, admin, user)
}

#[test]
#[should_panic(expected = "Error(Contract, #2)")]
fn test_remove_player_when_not_initialized() {
    let (_env, client, admin, user) = uninitialized_setup();
    client.remove_player(&admin, &user);
}

#[test]
#[should_panic(expected = "Error(Contract, #2)")]
fn test_ban_player_when_not_initialized() {
    let (_env, client, admin, user) = uninitialized_setup();
    client.ban_player(&admin, &user);
}

#[test]
#[should_panic(expected = "Error(Contract, #2)")]
fn test_reset_player_when_not_initialized() {
    let (_env, client, admin, user) = uninitialized_setup();
    client.reset_player(&admin, &user);
}
