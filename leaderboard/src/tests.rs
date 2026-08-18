use super::*;
use soroban_sdk::{testutils::Address as _, Env};

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
    // The sorted-maintenance bubble (post-#23 design) can rewrite tens of
    // slots in one call, exceeding mainnet invocation limits for the fill-to-
    // capacity tests. Behavior is what these tests prove, so lift the
    // resource limits just like the CPU budget above.
    env.cost_estimate().disable_resource_limits();

    let contract_id = env.register(LeaderboardContract, ());
    let client = LeaderboardContractClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let market = Address::generate(&env);
    let referral = Address::generate(&env);

    client.initialize(&admin, &market, &referral);
    (env, client, admin, market, referral)
}

#[test]
fn test_add_points_and_verify_balance() {
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &100_u64, &true);
    assert_eq!(client.get_points(&user), 100);
}

#[test]
fn test_accumulate_points() {
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &50_u64, &true);
    client.add_pts(&market, &user, &30_u64, &false);
    client.add_pts(&market, &user, &20_u64, &true);
    assert_eq!(client.get_points(&user), 100);
}

#[test]
fn test_bonus_pts_no_won_lost() {
    let (env, client, _admin, market, referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &10_u64, &true);
    client.add_pts(&market, &user, &5_u64, &false);

    let before = client.get_stats(&user);
    assert_eq!(before.won_bets, 1);
    assert_eq!(before.lost_bets, 1);

    client.add_bonus_pts(&referral, &user, &25_u64);

    let after = client.get_stats(&user);
    assert_eq!(after.points, 40);
    assert_eq!(after.total_bets, 3); // won(1) + lost(1) + bonus(1)
    assert_eq!(after.won_bets, 1);
    assert_eq!(after.lost_bets, 1);
}

#[test]
fn test_top_players_sorted() {
    let (env, client, _admin, market, _referral) = setup();

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let charlie = Address::generate(&env);

    client.add_pts(&market, &alice, &50_u64, &true);
    client.add_pts(&market, &bob, &100_u64, &true);
    client.add_pts(&market, &charlie, &75_u64, &true);

    let top = client.get_top_players(&0_u32, &20_u32);
    assert_eq!(top.len(), 3);
    assert_eq!(top.get(0).unwrap().address, bob);
    assert_eq!(top.get(0).unwrap().points, 100);
    assert_eq!(top.get(1).unwrap().address, charlie);
    assert_eq!(top.get(1).unwrap().points, 75);
    assert_eq!(top.get(2).unwrap().address, alice);
    assert_eq!(top.get(2).unwrap().points, 50);
}

#[test]
fn test_top_players_capped_at_50() {
    let (env, client, _admin, market, _referral) = setup();

    for i in 1u64..=55 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &i, &true);
    }

    let page1 = client.get_top_players(&0_u32, &20_u32);
    assert_eq!(page1.len(), 20);
    assert_eq!(page1.get(0).unwrap().points, 55);

    let page2 = client.get_top_players(&20_u32, &20_u32);
    assert_eq!(page2.len(), 20);

    let page3 = client.get_top_players(&40_u32, &20_u32);
    assert_eq!(page3.len(), 10);
    assert_eq!(page3.get(9).unwrap().points, 6);

    assert_eq!(client.get_player_count(), 50);
}

#[test]
fn test_pagination_offset_beyond_count() {
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &100_u64, &true);
    let result = client.get_top_players(&10_u32, &20_u32);
    assert_eq!(result.len(), 0);
}

// OPT: record_bet is now a no-op — total_bets = won_bets + lost_bets + bonus_bets
#[test]
fn test_record_bet_is_noop() {
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    // record_bet is a no-op — should not change any stats
    client.record_bet(&market, &user);
    client.record_bet(&market, &user);
    let stats = client.get_stats(&user);
    assert_eq!(stats.total_bets, 0); // no wins/losses yet
}

// OPT: total_bets now = won_bets + lost_bets + bonus_bets (derived at read time)
#[test]
fn test_get_stats_aggregate() {
    let (env, client, _admin, market, referral) = setup();
    let user = Address::generate(&env);

    // 2 wins, 1 loss = 3 total settled bets
    client.add_pts(&market, &user, &20_u64, &true);
    client.add_pts(&market, &user, &30_u64, &true);
    client.add_pts(&market, &user, &5_u64, &false);

    // Bonus points don't affect won/lost counts, but do count toward total_bets
    client.add_bonus_pts(&referral, &user, &10_u64);

    let stats = client.get_stats(&user);
    assert_eq!(stats.points, 65);
    assert_eq!(stats.total_bets, 4); // won_bets(2) + lost_bets(1) + bonus_bets(1)
    assert_eq!(stats.won_bets, 2);
    assert_eq!(stats.lost_bets, 1);
}

// ── Issue #19: bonus-only activity must be reflected in total_bets ──────────

#[test]
fn test_bonus_only_user_has_nonzero_total_bets() {
    // A user who only ever receives referral/welcome bonuses must not read as
    // total_bets == 0. Bonus awards are counted without polluting won/lost.
    let (env, client, _admin, _market, referral) = setup();
    let user = Address::generate(&env);

    // add_bonus_pts: per-referred-bet bonus path.
    client.add_bonus_pts(&referral, &user, &3_u64);
    // reward_bonus: welcome-bonus path (tokens=0 so no mint wiring is needed).
    client.reward_bonus(&referral, &user, &5_u64, &0_i128);

    let stats = client.get_stats(&user);
    assert_eq!(stats.points, 8);
    assert_eq!(stats.total_bets, 2); // 2 bonus awards, 0 settled bets
    assert_eq!(stats.won_bets, 0);
    assert_eq!(stats.lost_bets, 0);
}

#[test]
fn test_rank_calculation() {
    let (env, client, _admin, market, _referral) = setup();

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let charlie = Address::generate(&env);
    let dave = Address::generate(&env);

    client.add_pts(&market, &alice, &50_u64, &true);
    client.add_pts(&market, &bob, &100_u64, &true);
    client.add_pts(&market, &charlie, &75_u64, &true);

    assert_eq!(client.get_rank(&bob), Some(1));
    assert_eq!(client.get_rank(&charlie), Some(2));
    assert_eq!(client.get_rank(&alice), Some(3));
    assert_eq!(client.get_rank(&dave), None);
}

#[test]
fn test_rank_is_none_for_player_outside_top_50() {
    let (env, client, _admin, market, _referral) = setup();

    for points in 1u64..=50 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &points, &true);
    }
    let outside_top_50 = Address::generate(&env);
    client.add_pts(&market, &outside_top_50, &0_u64, &false);

    assert_eq!(client.get_top_player_count(), 50);
    assert_eq!(client.get_rank(&outside_top_50), None);
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_unauthorized_caller_rejected() {
    let (env, client, _admin, _market, _referral) = setup();
    let rando = Address::generate(&env);
    let user = Address::generate(&env);
    client.add_pts(&rando, &user, &10_u64, &true);
}

#[test]
#[should_panic(expected = "Error(Contract, #1)")]
fn test_double_init_rejected() {
    let (_env, client, admin, market, referral) = setup();
    client.initialize(&admin, &market, &referral);
}

#[test]
fn test_player_count() {
    let (env, client, _admin, market, _referral) = setup();
    assert_eq!(client.get_player_count(), 0);

    let u1 = Address::generate(&env);
    let u2 = Address::generate(&env);
    client.add_pts(&market, &u1, &10_u64, &true);
    assert_eq!(client.get_player_count(), 1);
    client.add_pts(&market, &u2, &20_u64, &true);
    assert_eq!(client.get_player_count(), 2);
    client.add_pts(&market, &u1, &5_u64, &false);
    assert_eq!(client.get_player_count(), 2);
}

// ── Lever E: O(1) eviction correctness ────────────────────────────────────────

#[test]
fn test_eviction_replaces_lowest_when_full() {
    // Fill exactly 50 with points 100..149, then add a higher scorer.
    // The new entry must enter and the lowest (100) must be evicted.
    let (env, client, _admin, market, _referral) = setup();
    for i in 0u64..50 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(100 + i), &true);
    }
    assert_eq!(client.get_player_count(), 50);

    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &500_u64, &true);

    // Still capped at 50; newcomer is now #1; the old min (100) is gone.
    assert_eq!(client.get_player_count(), 50);
    let top = client.get_top_players(&0_u32, &20_u32);
    assert_eq!(top.get(0).unwrap().points, 500);

    // Lowest entry is now 101 (the original 100 was evicted).
    let last = client.get_top_players(&40_u32, &20_u32);
    assert_eq!(last.get(9).unwrap().points, 101);
}

#[test]
fn test_low_scorer_rejected_when_full() {
    // Fill 50 with high points, then a low scorer must NOT enter the list.
    let (env, client, _admin, market, _referral) = setup();
    for i in 0u64..50 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(1000 + i), &true);
    }
    let weak = Address::generate(&env);
    client.add_pts(&market, &weak, &5_u64, &false);

    // Weak user has stats/points recorded, but is NOT in the top list.
    assert_eq!(client.get_points(&weak), 5);
    assert_eq!(client.get_rank(&weak), None);
    assert_eq!(client.get_player_count(), 50);
}

#[test]
fn test_bottom_player_rising_updates_min() {
    // When the weakest in-list player gains points, the cached min must update
    // so a later newcomer is compared against the NEW (higher) minimum.
    let (env, client, _admin, market, _referral) = setup();
    let weakest = Address::generate(&env);
    // First entry is the weakest at 100; the rest are 110, 120, … (all higher).
    client.add_pts(&market, &weakest, &100_u64, &true);
    for i in 1u64..50 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(100 + i * 10), &true);
    }
    assert_eq!(client.get_player_count(), 50);

    // Boost the weakest (100 -> 1000) so it is no longer the min.
    client.add_pts(&market, &weakest, &900_u64, &true);
    assert_eq!(client.get_points(&weakest), 1000);

    // The true new minimum is now 110 (second-lowest original). A newcomer with
    // 105 should be REJECTED (105 <= 110), proving the min recomputed correctly
    // rather than staying stale at 100.
    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &105_u64, &true);
    assert_eq!(client.get_rank(&newcomer), None);
    assert_eq!(client.get_player_count(), 50);
}

// ── Lever G: reward() / reward_bonus() ────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_reward_rejects_non_market_caller() {
    // Only the market contract may call reward(). A random caller must be
    // rejected with UnauthorizedCaller (#3) — protects token minting.
    let (env, client, _admin, _market, _referral) = setup();
    let rando = Address::generate(&env);
    let user = Address::generate(&env);
    // tokens=0 so we don't need a token wired; the auth guard must fire first.
    client.reward(&rando, &user, &30_u64, &0_i128, &true);
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_reward_bonus_rejects_non_referral_caller() {
    let (env, client, _admin, _market, _referral) = setup();
    let rando = Address::generate(&env);
    let user = Address::generate(&env);
    client.reward_bonus(&rando, &user, &5_u64, &0_i128);
}

#[test]
fn test_reward_updates_points_and_winloss() {
    // reward() with tokens=0 (no token wired) still updates points + win/loss
    // exactly like add_pts. Proves the points half is independent of minting.
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.reward(&market, &user, &30_u64, &0_i128, &true);
    client.reward(&market, &user, &10_u64, &0_i128, &false);
    let s = client.get_stats(&user);
    assert_eq!(s.points, 40);
    assert_eq!(s.won_bets, 1);
    assert_eq!(s.lost_bets, 1);
    assert_eq!(s.total_bets, 2);
}

// ── Issue #67: TopPlayerSlot reverse-lookup consistency ───────────────────────
//
// The forward index (TopPlayerAt) and the reverse index (TopPlayerSlot) must
// never disagree: an orphaned reverse lookup must not panic on update, must
// not resurrect a player who left the list, and must not return a stale rank.

#[test]
fn test_rank_is_none_for_user_not_in_list() {
    let (env, client, _admin, _market, _referral) = setup();
    let stranger = Address::generate(&env);
    assert_eq!(client.get_rank(&stranger), None);
}

#[test]
fn test_stale_slot_self_heals_after_entry_expired() {
    // Simulate a TTL expiry that removes the forward TopPlayerAt entry while
    // the reverse TopPlayerSlot lookup survives. Previously the next update
    // panicked on the missing entry; now it must self-heal and re-enter the
    // player without duplicating them.
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &100_u64, &true);
    assert_eq!(client.get_rank(&user), Some(1));

    // Expire only the forward entry — the reverse mapping stays behind.
    env.as_contract(&client.address, || {
        env.storage().persistent().remove(&DataKey::TopPlayerAt(0));
    });

    // The next update must not panic and must not treat the stale lookup as
    // the player being in the list.
    client.add_pts(&market, &user, &50_u64, &true);
    assert_eq!(client.get_points(&user), 150);
    assert_eq!(client.get_rank(&user), Some(1)); // re-entered the list

    // Exactly one entry for the user after re-insertion — no duplicate.
    let top = client.get_top_players(&0_u32, &20_u32);
    let matches = top.iter().filter(|e| e.address == user).count();
    assert_eq!(matches, 1);
}

#[test]
fn test_eviction_repairs_expired_min_entry() {
    // Fill the board and let the weakest entry's TopPlayerAt "expire" while
    // the MinPoints/MinSlot cache still points at its slot. A new high scorer
    // must trigger reconciliation (repair), not a panic, and must enter #1.
    let (env, client, _admin, market, _referral) = setup();
    let mut weakest = None;
    for i in 0u64..50 {
        let user = Address::generate(&env);
        if i == 0 {
            weakest = Some(user.clone()); // 100 pts — the weakest of the set
        }
        client.add_pts(&market, &user, &(100 + i), &true);
    }
    assert_eq!(client.get_player_count(), 50);
    let weakest = weakest.unwrap();

    // Expire the current minimum (100 pts, slot 49) — the cache is now stale.
    env.as_contract(&client.address, || {
        env.storage().persistent().remove(&DataKey::TopPlayerAt(49));
    });

    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &500_u64, &true);

    // The list was reconciled: newcomer entered as #1, board is full again.
    assert_eq!(client.get_player_count(), 50);
    let top = client.get_top_players(&0_u32, &20_u32);
    assert_eq!(top.get(0).unwrap().address, newcomer);
    assert_eq!(top.get(0).unwrap().points, 500);
    assert_eq!(client.get_rank(&newcomer), Some(1));
    // The expired player (100) is gone; even though their orphaned
    // TopPlayerSlot survives, get_rank must not report a stale rank.
    assert_eq!(client.get_rank(&weakest), None);
    // 101 is the new minimum — the repaired min cache agrees.
    assert_eq!(client.get_min_points(), 101);
}

#[test]
fn test_eviction_clears_reverse_mapping() {
    // Fill the board, then let a newcomer displace the weakest entry. The
    // evicted player's TopPlayerSlot must be removed so get_rank reads 0.
    let (env, client, _admin, market, _referral) = setup();
    let mut weakest = None;
    for i in 0u64..50 {
        let user = Address::generate(&env);
        if i == 0 {
            weakest = Some(user.clone());
        }
        client.add_pts(&market, &user, &(100 + i), &true);
    }
    let weakest = weakest.unwrap();
    assert_eq!(client.get_rank(&weakest), Some(50));

    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &1000_u64, &true);
    assert_eq!(client.get_rank(&newcomer), Some(1));

    // Displaced player: no rank and no lingering reverse mapping.
    assert_eq!(client.get_rank(&weakest), None);
    let still_mapped = env.as_contract(&client.address, || {
        env.storage().persistent().has(&DataKey::TopPlayerSlot(weakest.clone()))
    });
    assert!(!still_mapped);
}

#[test]
fn test_stale_min_rejected_before_eviction() {
    // The min cache must be validated on the eviction path: if the entry it
    // points at has expired, a newcomer must be admitted into the freed slot
    // even when their points are lower than the stale cached minimum.
    let (env, client, _admin, market, _referral) = setup();
    for i in 0u64..50 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(100 + i), &true);
    }
    // Expire the minimum (100 pts) and leave the MinPoints cache stale at 100.
    env.as_contract(&client.address, || {
        env.storage().persistent().remove(&DataKey::TopPlayerAt(49));
    });

    // 50 < stale min (100): a naive cache check would wrongly reject this
    // newcomer — the repaired list has room, so they must enter.
    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &50_u64, &true);
    assert_eq!(client.get_rank(&newcomer), Some(50));
    assert_eq!(client.get_player_count(), 50);
    let last = client.get_top_players(&40_u32, &20_u32);
    assert_eq!(last.get(9).unwrap().points, 50);
}
