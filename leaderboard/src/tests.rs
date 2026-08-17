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

    assert_eq!(client.get_rank(&bob), 1);
    assert_eq!(client.get_rank(&charlie), 2);
    assert_eq!(client.get_rank(&alice), 3);
    assert_eq!(client.get_rank(&dave), 0);
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

    // Weak user has stats/points recorded, but is NOT in the top list (rank 0).
    assert_eq!(client.get_points(&weak), 5);
    assert_eq!(client.get_rank(&weak), 0);
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
    assert_eq!(client.get_rank(&newcomer), 0);
    assert_eq!(client.get_player_count(), 50);
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
    assert_eq!(client.get_rank(&min_player), 0);
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
    assert_eq!(client.get_rank(&first_tie), 0);
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
    assert_eq!(client.get_rank(&newcomer), 0);
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
    assert_eq!(client.get_rank(&a), 0);
    assert_eq!(client.get_rank(&b), 49);
    assert_eq!(client.get_rank(&c), 49);

    // D ties the min → B is now the oldest surviving tie (C reused A's slot).
    // D must evict B, NOT C. This is the FIFO-vs-lowest-slot discriminator.
    let d = Address::generate(&env);
    client.add_pts(&market, &d, &100_u64, &true);
    assert_eq!(client.get_rank(&a), 0);
    assert_eq!(client.get_rank(&b), 0);
    assert_eq!(client.get_rank(&c), 49);
    assert_eq!(client.get_rank(&d), 49);

    // E ties the min → C is now the oldest survivor (D reused B's slot).
    let e = Address::generate(&env);
    client.add_pts(&market, &e, &100_u64, &true);
    assert_eq!(client.get_rank(&c), 0);
    assert_eq!(client.get_rank(&d), 49);
    assert_eq!(client.get_rank(&e), 49);
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
