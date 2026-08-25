use super::*;
use soroban_sdk::{testutils::{Address as _, Events}, Env, Symbol, TryFromVal, Val};

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
fn test_pending_rewards_accumulate_until_claimed() {
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);

    client.queue_reward(&market, &user, &30_u64, &0_i128, &true);
    client.queue_reward(&market, &user, &10_u64, &0_i128, &false);

    assert_eq!(client.get_points(&user), 0);
    let pending = client.get_pending_reward(&user).unwrap();
    assert_eq!(pending.points, 40);
    assert_eq!(pending.won_delta, 1);
    assert_eq!(pending.lost_delta, 1);
    assert_eq!(pending.bet_delta, 2);

    client.claim_pending_rewards(&user);
    assert_eq!(client.get_points(&user), 40);
    let stats = client.get_stats(&user);
    assert_eq!(stats.won_bets, 1);
    assert_eq!(stats.lost_bets, 1);
    assert_eq!(client.get_pending_reward(&user), None);
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

    assert_eq!(client.get_top_player_count(), 50);
}

#[test]
fn test_pagination_offset_beyond_count() {
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &100_u64, &true);
    let result = client.get_top_players(&10_u32, &20_u32);
    assert_eq!(result.len(), 0);
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

    client.add_bonus_pts(&referral, &user, &3_u64);
    client.add_bonus_pts(&referral, &user, &5_u64);

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

    assert_eq!(client.get_rank(&bob), 1);
    assert_eq!(client.get_rank(&charlie), 2);
    assert_eq!(client.get_rank(&alice), 3);
    assert_eq!(client.get_rank(&dave), UNRANKED_RANK);
}

#[test]
fn test_rank_unranked_for_player_outside_top_50() {
    let (env, client, _admin, market, _referral) = setup();

    for points in 1u64..=50 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &points, &true);
    }
    let outside_top_50 = Address::generate(&env);
    // 0 points: stats are recorded but the player never enters the top list.
    client.add_pts(&market, &outside_top_50, &0_u64, &false);

    assert_eq!(client.get_top_player_count(), 50);
    assert_eq!(client.get_rank(&outside_top_50), UNRANKED_RANK);
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
    assert_eq!(client.get_top_player_count(), 0);

    let u1 = Address::generate(&env);
    let u2 = Address::generate(&env);
    client.add_pts(&market, &u1, &10_u64, &true);
    assert_eq!(client.get_top_player_count(), 1);
    client.add_pts(&market, &u2, &20_u64, &true);
    assert_eq!(client.get_top_player_count(), 2);
    client.add_pts(&market, &u1, &5_u64, &false);
    assert_eq!(client.get_top_player_count(), 2);
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
    assert_eq!(client.get_top_player_count(), 50);

    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &500_u64, &true);

    // Still capped at 50; newcomer is now #1; the old min (100) is gone.
    assert_eq!(client.get_top_player_count(), 50);
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
    assert_eq!(client.get_rank(&weak), UNRANKED_RANK);
    assert_eq!(client.get_player_count(), 50);
    assert_eq!(client.get_top_player_count(), 50);
}

#[test]
fn test_bottom_player_rising_updates_min() {
    // When the weakest in-list player gains points, the cached min must update
    // so a later newcomer is compared against the NEW (higher) minimum.
    let (env, client, _admin, market, _referral) = setup();
    let weakest = Address::generate(&env);
    client.add_pts(&market, &weakest, &100_u64, &true);
    for i in 1u64..50 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(100 + i * 10), &true);
    }
    assert_eq!(client.get_top_player_count(), 50);

    // Boost the weakest (100 -> 1000) so it is no longer the min.
    client.add_pts(&market, &weakest, &900_u64, &true);
    assert_eq!(client.get_points(&weakest), 1000);

    // The true new minimum is now 110 (second-lowest original). A newcomer with
    // 105 should be REJECTED (105 <= 110), proving the min recomputed correctly
    // rather than staying stale at 100.
    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &105_u64, &true);
    assert_eq!(client.get_rank(&newcomer), UNRANKED_RANK);
    assert_eq!(client.get_top_player_count(), 50);
}

// ── Issue #25: tie-aware min cache ────────────────────────────────────────────
// Equal-points players must never corrupt the min cache. Deterministic tie-break:
// FIFO — among equal-min players the OLDEST surviving tie is evicted next,
// tracked by a per-slot insertion sequence (not by slot index, since slots are
// reused after eviction).

#[test]
fn test_equal_min_newcomer_displaces_min_when_full() {
    // When the list is full, a newcomer whose points EQUAL the current min must
    // displace the incumbent min player (FIFO) instead of being rejected.
    let (env, client, _admin, market, _referral) = setup();
    let mut min_player: Option<Address> = None;
    for i in 0u64..50 {
        let user = Address::generate(&env);
        if i == 0 {
            min_player = Some(user.clone());
        }
        client.add_pts(&market, &user, &(100 + i), &true);
    }
    let min_player = min_player.unwrap();
    assert_eq!(client.get_player_count(), 50);
    assert_eq!(client.get_points(&min_player), 100);

    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &100_u64, &true);

    // Still capped at 50; the incumbent min (100) is evicted, the newcomer
    // enters, and the list now holds the newcomer instead of the old min.
    assert_eq!(client.get_player_count(), 50);
    assert_eq!(client.get_rank(&min_player), UNRANKED_RANK);
    assert_eq!(client.get_rank(&newcomer), 50);
}

#[test]
fn test_equal_min_fifo_evicts_oldest_tie() {
    // Several players tied at the min: the OLDEST tie (first inserted) is
    // displaced by a new equal-min player — deterministic FIFO.
    let (env, client, _admin, market, _referral) = setup();
    for i in 0u64..45 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(100 + i), &true);
    }
    let mut first_tie: Option<Address> = None;
    let mut last_tie: Option<Address> = None;
    for i in 0u64..5 {
        let user = Address::generate(&env);
        if i == 0 {
            first_tie = Some(user.clone());
        }
        if i == 4 {
            last_tie = Some(user.clone());
        }
        client.add_pts(&market, &user, &10_u64, &true);
    }
    let first_tie = first_tie.unwrap();
    let last_tie = last_tie.unwrap();
    assert_eq!(client.get_player_count(), 50);
    // 45 players scored higher (100..144) than every 10-point tie.
    assert_eq!(client.get_rank(&first_tie), 46);
    assert_eq!(client.get_rank(&last_tie), 46);

    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &10_u64, &true);

    assert_eq!(client.get_player_count(), 50);
    // FIFO: the oldest tied-at-min player is displaced; the newer tie stays.
    assert_eq!(client.get_rank(&first_tie), UNRANKED_RANK);
    assert_eq!(client.get_rank(&last_tie), 46);
    assert_eq!(client.get_rank(&newcomer), 46);
}

#[test]
fn test_fill_min_boost_keeps_cache_correct() {
    // Boosting the cached-min player while the list is still filling must
    // recompute the cache. Otherwise a later newcomer compares against a stale
    // (lower) minimum and wrongly displaces a stronger player.
    let (env, client, _admin, market, _referral) = setup();
    let weakest = Address::generate(&env);
    client.add_pts(&market, &weakest, &100_u64, &true);
    // Boost the (cached) min player before the list fills up.
    client.add_pts(&market, &weakest, &50_u64, &true);
    assert_eq!(client.get_points(&weakest), 150);

    for i in 0u64..48 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(200 + i), &true);
    }
    let last = Address::generate(&env);
    client.add_pts(&market, &last, &250_u64, &true);
    assert_eq!(client.get_player_count(), 50);

    // 120 is below the TRUE min (150) — must be rejected, and the boosted
    // player must remain in the list.
    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &120_u64, &true);
    assert_eq!(client.get_rank(&newcomer), UNRANKED_RANK);
    assert_eq!(client.get_rank(&weakest), 50);
}

#[test]
fn test_fifo_evicts_consecutive_oldest_ties_across_slot_reuse() {
    // Regression for PR #38 review: A and B tie at the min, with A in the lower
    // slot. C ties the min and evicts A. C now occupies A's reused lower slot,
    // yet B is the older SURVIVING tie — so the next tied newcomer D must evict
    // B, not C. Evicting the reused lowest slot again would be lowest-slot
    // eviction, not FIFO.
    let (env, client, _admin, market, _referral) = setup();

    // 48 strictly-higher scorers so the last two slots hold the tied minimum.
    for i in 0u64..48 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(1000 + i), &true);
    }
    // A is the older tied-at-min player (lower slot), B the newer one.
    let a = Address::generate(&env);
    let b = Address::generate(&env);
    client.add_pts(&market, &a, &100_u64, &true);
    client.add_pts(&market, &b, &100_u64, &true);
    assert_eq!(client.get_player_count(), 50);
    assert_eq!(client.get_rank(&a), 49);
    assert_eq!(client.get_rank(&b), 49);

    // C ties the min → evicts the oldest tie (A), even though A held the
    // lowest min slot.
    let c = Address::generate(&env);
    client.add_pts(&market, &c, &100_u64, &true);
    assert_eq!(client.get_rank(&a), UNRANKED_RANK);
    assert_eq!(client.get_rank(&b), 49);
    assert_eq!(client.get_rank(&c), 49);

    // D ties the min → B is now the oldest surviving tie (C reused A's slot).
    // D must evict B, NOT C. This is the FIFO-vs-lowest-slot discriminator.
    let d = Address::generate(&env);
    client.add_pts(&market, &d, &100_u64, &true);
    assert_eq!(client.get_rank(&a), UNRANKED_RANK);
    assert_eq!(client.get_rank(&b), UNRANKED_RANK);
    assert_eq!(client.get_rank(&c), 49);
    assert_eq!(client.get_rank(&d), 49);

    // E ties the min → C is now the oldest survivor (D reused B's slot).
    let e = Address::generate(&env);
    client.add_pts(&market, &e, &100_u64, &true);
    assert_eq!(client.get_rank(&c), UNRANKED_RANK);
    assert_eq!(client.get_rank(&d), 49);
    assert_eq!(client.get_rank(&e), 49);
}

// ── Lever G: reward() / add_bonus_pts() ───────────────────────────────────────

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
fn test_add_bonus_pts_rejects_non_referral_caller() {
    let (env, client, _admin, _market, _referral) = setup();
    let rando = Address::generate(&env);
    let user = Address::generate(&env);
    client.add_bonus_pts(&rando, &user, &5_u64);
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

// ── Issue #22: TopPlayerSlot ↔ TopPlayerAt integrity ─────────────────────────

#[test]
fn test_get_rank_cleans_stale_reverse_lookup() {
    // TTL expiry deletes TopPlayerAt with no hook to clear TopPlayerSlot.
    // get_rank must not trust the orphaned reverse key.
    let (env, client, _admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    client.add_pts(&market, &alice, &100_u64, &true);
    assert_eq!(client.get_rank(&alice), 1);

    env.as_contract(&client.address, || {
        env.storage().persistent().remove(&DataKey::TopPlayerAt(0));
    });

    assert_eq!(client.get_rank(&alice), UNRANKED_RANK);
    env.as_contract(&client.address, || {
        let slot: Option<u32> = env
            .storage()
            .persistent()
            .get(&DataKey::TopPlayerSlot(alice.clone()));
        assert!(slot.is_none());
    });
}

#[test]
fn test_reconcile_compacts_ttl_holes_and_restores_slots() {
    let (env, client, _admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let charlie = Address::generate(&env);
    client.add_pts(&market, &alice, &50_u64, &true);
    client.add_pts(&market, &bob, &100_u64, &true);
    client.add_pts(&market, &charlie, &75_u64, &true);
    assert_eq!(client.get_player_count(), 3);

    // Expire the middle forward entry (charlie at slot 1 after sort: bob, charlie, alice).
    env.as_contract(&client.address, || {
        env.storage().persistent().remove(&DataKey::TopPlayerAt(1));
    });

    client.reconcile_top_slots();

    assert_eq!(client.get_player_count(), 2);
    let top = client.get_top_players(&0_u32, &20_u32);
    assert_eq!(top.len(), 2);
    assert_eq!(top.get(0).unwrap().address, bob);
    assert_eq!(top.get(1).unwrap().address, alice);
    assert_eq!(client.get_rank(&bob), 1);
    assert_eq!(client.get_rank(&alice), 2);
    assert_eq!(client.get_rank(&charlie), UNRANKED_RANK);
}

#[test]
fn test_unranked_sentinel_for_user_not_in_list() {
    let (env, client, _admin, _market, _referral) = setup();
    let stranger = Address::generate(&env);
    assert_eq!(client.get_rank(&stranger), UNRANKED_RANK);
}

#[test]
fn test_unranked_rank_is_above_every_list_rank() {
    // The numeric rank invariant from issue #91: an unranked player must never
    // sort above (numerically lower than) a real position. The weakest player
    // in a full list holds rank MAX_TOP_PLAYERS, so the sentinel must be
    // strictly greater — never 0, which was less than every valid rank.
    let (env, client, _admin, market, _referral) = setup();
    for i in 0..MAX_TOP_PLAYERS {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(1000 + i as u64), &true);
    }
    assert_eq!(client.get_top_player_count(), MAX_TOP_PLAYERS);

    let weakest = client
        .get_top_players(&(MAX_TOP_PLAYERS - 1), &1)
        .get(0)
        .unwrap()
        .address
        .clone();
    assert_eq!(client.get_rank(&weakest), MAX_TOP_PLAYERS);

    let outside = Address::generate(&env);
    let outside_rank = client.get_rank(&outside);
    assert_eq!(outside_rank, UNRANKED_RANK);
    assert!(outside_rank > client.get_rank(&weakest));
}

#[test]
fn test_upsert_repairs_stale_slot_instead_of_panicking() {
    // In-place path used to unwrap TopPlayerAt; a TTL hole must re-insert.
    let (env, client, _admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    client.add_pts(&market, &alice, &100_u64, &true);

    env.as_contract(&client.address, || {
        env.storage().persistent().remove(&DataKey::TopPlayerAt(0));
    });

    client.add_pts(&market, &alice, &25_u64, &true);
    assert_eq!(client.get_points(&alice), 125);
    assert_eq!(client.get_rank(&alice), 1);
    assert_eq!(client.get_player_count(), 1);
}

#[test]
fn test_missing_reverse_lookup_does_not_duplicate_player() {
    // TopPlayerSlot TTL expiry must not append a second TopPlayerAt for the same user.
    let (env, client, _admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    client.add_pts(&market, &alice, &80_u64, &true);
    client.add_pts(&market, &bob, &40_u64, &true);

    env.as_contract(&client.address, || {
        env.storage()
            .persistent()
            .remove(&DataKey::TopPlayerSlot(alice.clone()));
    });

    client.add_pts(&market, &alice, &10_u64, &true);
    assert_eq!(client.get_player_count(), 2);
    assert_eq!(client.get_rank(&alice), 1);
    let top = client.get_top_players(&0_u32, &20_u32);
    assert_eq!(top.len(), 2);
    assert_eq!(top.get(0).unwrap().address, alice);
    assert_eq!(top.get(1).unwrap().address, bob);
}

#[test]
fn test_eviction_clears_reverse_lookup() {
    let (env, client, _admin, market, _referral) = setup();
    let lowest = Address::generate(&env);
    client.add_pts(&market, &lowest, &100_u64, &true);
    for i in 1u64..50 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(100 + i), &true);
    }
    assert_eq!(client.get_rank(&lowest), 50);

    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &500_u64, &true);

    assert_eq!(client.get_rank(&lowest), UNRANKED_RANK);
    assert_eq!(client.get_rank(&newcomer), 1);
    env.as_contract(&client.address, || {
        let slot: Option<u32> = env
            .storage()
            .persistent()
            .get(&DataKey::TopPlayerSlot(lowest.clone()));
        assert!(slot.is_none());
    });
}

// ── Issue #67: extra reverse-lookup cases kept from main ──────────────────────

#[test]
fn test_rank_unranked_for_user_not_in_list() {
    let (env, client, _admin, _market, _referral) = setup();
    let stranger = Address::generate(&env);
    assert_eq!(client.get_rank(&stranger), UNRANKED_RANK);
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
    assert_eq!(client.get_rank(&user), 1);

    env.as_contract(&client.address, || {
        env.storage().persistent().remove(&DataKey::TopPlayerAt(0));
    });

    client.add_pts(&market, &user, &50_u64, &true);
    assert_eq!(client.get_points(&user), 150);
    assert_eq!(client.get_rank(&user), 1); // re-entered the list

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
            weakest = Some(user.clone());
        }
        client.add_pts(&market, &user, &(100 + i), &true);
    }
    assert_eq!(client.get_player_count(), 50);
    let weakest = weakest.unwrap();

    env.as_contract(&client.address, || {
        env.storage().persistent().remove(&DataKey::TopPlayerAt(49));
    });

    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &500_u64, &true);

    assert_eq!(client.get_player_count(), 50);
    let top = client.get_top_players(&0_u32, &20_u32);
    assert_eq!(top.get(0).unwrap().address, newcomer);
    assert_eq!(top.get(0).unwrap().points, 500);
    assert_eq!(client.get_rank(&newcomer), 1);
    // The expired player (100) is gone; even though their orphaned
    // TopPlayerSlot survives, get_rank must not report a stale rank.
    assert_eq!(client.get_rank(&weakest), UNRANKED_RANK);
    // 101 is the new minimum — the repaired min cache agrees.
    assert_eq!(client.get_min_points(), 101);
}

#[test]
fn test_eviction_clears_reverse_mapping() {
    // Fill the board, then let a newcomer displace the weakest entry. The
    // evicted player's TopPlayerSlot must be removed so get_rank reads the
    // unranked sentinel.
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
    assert_eq!(client.get_rank(&weakest), 50);

    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &1000_u64, &true);
    assert_eq!(client.get_rank(&newcomer), 1);

    // Displaced player: unranked and no lingering reverse mapping.
    assert_eq!(client.get_rank(&weakest), UNRANKED_RANK);
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
    env.as_contract(&client.address, || {
        env.storage().persistent().remove(&DataKey::TopPlayerAt(49));
    });

    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &50_u64, &true);
    assert_eq!(client.get_rank(&newcomer), 50);
    assert_eq!(client.get_player_count(), 50);
    let last = client.get_top_players(&40_u32, &20_u32);
    assert_eq!(last.get(9).unwrap().points, 50);
}

#[test]
fn test_add_pts_emits_leaderboard_updated() {
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &100_u64, &true);
    // `env.events().all()` returns a `ContractEvents` in soroban-sdk 26, which
    // exposes its entries as an XDR slice rather than an indexable Vec of
    // (address, topics, data) tuples.
    let events = env.events().all();
    let emitted = events.events();
    assert!(!emitted.is_empty(), "add_pts emitted no event");
    let soroban_sdk::xdr::ContractEventBody::V0(body) = &emitted.last().unwrap().body;
    let topic0 = Val::try_from_val(&env, &body.topics[0]).unwrap();
    let name = Symbol::try_from_val(&env, &topic0).unwrap();
    assert_eq!(name, Symbol::new(&env, "leaderboard_updated"));
}

#[test]
fn test_add_pts_always_rejected() {
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    let rando = Address::generate(&env);
    let result = client.add_pts(&rando, &user, &10_u64, &true);
    assert!(result.is_err(), "add_pts should always return an error");
    match result {
        Err(LeaderboardError::UnauthorizedCaller) => {}
        other => panic!("add_pts returned unexpected error: {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Issue #79 — reward paths enforce PULSE supply cap
// ═══════════════════════════════════════════════════════════════════════════

use pulse_token::{PULSETokenContract, PULSETokenContractClient};
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};

/// Helper: deploy a fresh PULSE token, set supply cap, wire it into the
/// leaderboard, and return all necessary handles.
fn setup_with_token() -> (
    Env,
    LeaderboardContractClient<'static>,
    Address, // admin
    Address, // market
    Address, // referral
    PULSETokenContractClient<'static>,
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

    // Deploy PULSE token
    let token_id = env.register(PULSETokenContract, ());
    let token_client = PULSETokenContractClient::new(&env, &token_id);
    token_client.initialize(
        &admin,
        &soroban_sdk::String::from_str(&env, "PULSE"),
        &soroban_sdk::String::from_str(&env, "PLSE"),
        &7u32,
    );

    // Wire token into leaderboard
    client.set_token_contract(&admin, &token_id);

    (env, client, admin, market, referral, token_client)
}

#[test]
fn test_reward_mints_tokens() {
    let (env, client, _admin, market, _referral, token_client) = setup_with_token();
    let user = Address::generate(&env);

    // Verify token balance starts at 0
    assert_eq!(token_client.balance(&user), 0);

    // reward() with 10 PULSE — token contract must be invoked
    client.reward(&market, &user, &30_u64, &10_0000000_i128, &true);

    // Points updated AND token balance changed — proves mint was invoked
    assert_eq!(client.get_points(&user), 30);
    assert_eq!(token_client.balance(&user), 10_0000000_i128);
    assert_eq!(token_client.total_supply(), 10_0000000_i128);
}

#[test]
fn test_reward_updates_points_and_mints_tokens() {
    let (env, client, _admin, market, _referral, token_client) = setup_with_token();
    let user = Address::generate(&env);

    // Two reward calls: 30 pts + 10 PULSE, then 10 pts + 5 PULSE
    client.reward(&market, &user, &30_u64, &10_0000000_i128, &true);
    client.reward(&market, &user, &10_u64, &5_0000000_i128, &false);

    // Points accumulate, tokens accumulate
    assert_eq!(client.get_points(&user), 40);
    assert_eq!(token_client.balance(&user), 15_0000000_i128);
    assert_eq!(token_client.total_supply(), 15_0000000_i128);
}

#[test]
fn test_reward_enforces_supply_cap() {
    let (env, client, admin, market, _referral, token_client) = setup_with_token();
    let user = Address::generate(&env);

    // Set cap to 5 PULSE
    token_client.set_supply_cap(&admin, &5_0000000_i128);

    // Mint 5 PULSE — should succeed
    client.reward(&market, &user, &30_u64, &5_0000000_i128, &true);
    assert_eq!(token_client.balance(&user), 5_0000000_i128);
    assert_eq!(token_client.total_supply(), 5_0000000_i128);

    // Try to mint 1 more — should fail (cap exceeded)
    let result = client.try_reward(&market, &user, &10_u64, &1_0000000_i128, &false);
    assert!(result.is_err(), "reward should fail when supply cap exceeded");
    // Balance and supply unchanged — cap enforced, no state corruption
    assert_eq!(token_client.balance(&user), 5_0000000_i128);
    assert_eq!(token_client.total_supply(), 5_0000000_i128);
}

#[test]
fn test_reward_bonus_mints_tokens() {
    let (env, client, _admin, _market, referral, token_client) = setup_with_token();
    let user = Address::generate(&env);

    assert_eq!(token_client.balance(&user), 0);

    // reward_bonus() with 8 PULSE
    client.reward_bonus(&referral, &user, &5_u64, &8_0000000_i128);

    assert_eq!(client.get_points(&user), 5);
    assert_eq!(token_client.balance(&user), 8_0000000_i128);
    assert_eq!(token_client.total_supply(), 8_0000000_i128);
}

#[test]
fn test_reward_bonus_enforces_supply_cap() {
    let (env, client, admin, _market, referral, token_client) = setup_with_token();
    let user = Address::generate(&env);

    // Set cap to 3 PULSE
    token_client.set_supply_cap(&admin, &3_0000000_i128);

    // Mint 3 PULSE via reward_bonus — should succeed
    client.reward_bonus(&referral, &user, &5_u64, &3_0000000_i128);
    assert_eq!(token_client.balance(&user), 3_0000000_i128);

    // Try to mint 1 more — should fail
    let result = client.try_reward_bonus(&referral, &user, &5_u64, &1_0000000_i128);
    assert!(result.is_err(), "reward_bonus should fail when supply cap exceeded");
    assert_eq!(token_client.balance(&user), 3_0000000_i128);
}

#[test]
fn test_claim_pending_preserves_reward_on_cap_exceeded() {
    let (env, client, admin, market, _referral, token_client) = setup_with_token();
    let user = Address::generate(&env);

    // Set cap to 2 PULSE
    token_client.set_supply_cap(&admin, &2_0000000_i128);

    // Queue a reward for 3 PULSE — queue itself doesn't mint
    client.queue_reward(&market, &user, &30_u64, &3_0000000_i128, &true);
    assert_eq!(token_client.balance(&user), 0);

    // Claim — should fail because minting would exceed cap
    let result = client.try_claim_pending_rewards(&user);
    assert!(result.is_err(), "claim should fail when pending tokens exceed cap");

    // Critical: the pending reward must be PRESERVED (not lost) so the user
    // can retry once the cap is raised.
    let pending = client.get_pending_reward(&user).unwrap();
    assert_eq!(pending.tokens, 3_0000000_i128);
    assert_eq!(pending.points, 30);
    assert_eq!(token_client.balance(&user), 0);
    assert_eq!(token_client.total_supply(), 0);
}

#[test]
fn test_claim_pending_succeeds_after_cap_raised() {
    let (env, client, admin, market, _referral, token_client) = setup_with_token();
    let user = Address::generate(&env);

    // Set cap to 2 PULSE, queue 3 PULSE
    token_client.set_supply_cap(&admin, &2_0000000_i128);
    client.queue_reward(&market, &user, &30_u64, &3_0000000_i128, &true);

    // Claim fails — cap exceeded
    assert!(client.try_claim_pending_rewards(&user).is_err());
    assert_eq!(token_client.balance(&user), 0);
    assert!(client.get_pending_reward(&user).is_some());

    // Raise cap to 5 PULSE
    token_client.set_supply_cap(&admin, &5_0000000_i128);

    // Claim succeeds now
    client.claim_pending_rewards(&user);
    assert_eq!(token_client.balance(&user), 3_0000000_i128);
    assert_eq!(token_client.total_supply(), 3_0000000_i128);
    assert!(client.get_pending_reward(&user).is_none());
    assert_eq!(client.get_points(&user), 30);
}

#[test]
fn test_reward_zero_tokens_succeeds_without_token() {
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);

    // reward() with tokens=0 should work even without a token contract set
    client.reward(&market, &user, &30_u64, &0_i128, &true);
    assert_eq!(client.get_points(&user), 30);
}
