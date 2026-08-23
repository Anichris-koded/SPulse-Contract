// ── Issue #69: points decay, so a ranking reflects activity not history ──────
//
// The board used to be a cumulative counter — `points += n`, never down. An
// early adopter who stopped playing held their rank forever, because a
// newcomer had to out-earn their entire lifetime total to pass them.
//
// The test that matters here is not "points still go up". That passes against
// the old monotonic model too, so it proves nothing. The criterion is whether
// **the ranking can change without the newcomer out-earning the leader** — so
// these tests assert relative rank over simulated time, and the headline one
// is written so that the newcomer earns strictly less in absolute terms.

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
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

/// Move the ledger forward by whole decay periods.
fn advance_periods(env: &Env, periods: u32) {
    let seq = env.ledger().sequence();
    env.ledger()
        .set_sequence_number(seq + periods * DECAY_PERIOD_LEDGERS);
}

// ── The headline ────────────────────────────────────────────────────────────

#[test]
fn test_idle_leader_is_overtaken_by_a_newer_player_who_earned_far_less() {
    // Alice banks 10,000 points and stops playing. Forty weeks later Bob
    // shows up and earns 500 — one twentieth of Alice's total, and he never
    // comes close to out-earning her in absolute terms.
    //
    // Under the old model this was unwinnable for Bob by construction. Now
    // Alice's score has decayed past his, and the ranking flips.
    let (env, client, _admin, market, _referral) = setup();
    let alice = Address::generate(&env);
    let bob = Address::generate(&env);

    client.add_pts(&market, &alice, &10_000_u64, &true);
    assert_eq!(client.get_rank(&alice), 1);

    advance_periods(&env, 40);

    client.add_pts(&market, &bob, &500_u64, &true);

    let alice_now = client.get_points(&alice);
    let bob_now = client.get_points(&bob);
    assert!(
        alice_now < bob_now,
        "an idle 10,000-point leader ({alice_now}) must fall behind an active \
         500-point newcomer ({bob_now}) after 40 weeks"
    );
    assert_eq!(client.get_rank(&bob), 1, "the newcomer should now lead");
    assert_eq!(client.get_rank(&alice), 2);

    // And Bob genuinely never out-earned her: 500 lifetime versus 10,000.
    assert!(bob_now < 10_000);
}

// ── The property that makes the decay clock un-gameable ─────────────────────

#[test]
fn test_frequent_small_writes_do_not_reset_the_decay_clock() {
    // The obvious way to break a decay model is a per-player "last touched"
    // stamp: transact just often enough and your score never ages. The epoch
    // here is global and not the player's to reset, so a player who writes
    // inside every period decays exactly as much as one who never writes.
    let (env, client, _admin, market, _referral) = setup();
    let grinder = Address::generate(&env);
    let idler = Address::generate(&env);

    client.add_pts(&market, &grinder, &10_000_u64, &true);
    client.add_pts(&market, &idler, &10_000_u64, &true);

    // The grinder touches their entry 20 times over 20 periods, adding a
    // token amount each time; the idler does nothing.
    let mut grinder_added: u64 = 0;
    for _ in 0..20 {
        advance_periods(&env, 1);
        client.add_pts(&market, &grinder, &1_u64, &true);
        grinder_added += 1;
    }

    let grinder_now = client.get_points(&grinder);
    let idler_now = client.get_points(&idler);

    // Both started at 10,000 and both aged 20 periods. The grinder is ahead
    // only by roughly what he actually contributed — not by a frozen score.
    assert!(
        grinder_now < 10_000,
        "writing every period must not freeze a score ({grinder_now})"
    );
    assert!(
        grinder_now - idler_now <= grinder_added,
        "the grinder's advantage ({} over {idler_now}) must not exceed what \
         he actually earned ({grinder_added})",
        grinder_now
    );
}

// ── Decay mechanics ─────────────────────────────────────────────────────────

#[test]
fn test_points_do_not_decay_within_a_single_period() {
    // Decay is quantised: nothing moves until a period boundary is crossed,
    // so a score is stable across a normal play session.
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &1_000_u64, &true);

    let seq = env.ledger().sequence();
    env.ledger()
        .set_sequence_number(seq + DECAY_PERIOD_LEDGERS - 1);

    assert_eq!(client.get_points(&user), 1_000);
}

#[test]
fn test_one_period_decays_by_the_documented_rate() {
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &1_000_u64, &true);

    advance_periods(&env, 1);

    // 9/10 of 1000.
    assert_eq!(client.get_points(&user), 900);
}

#[test]
fn test_decay_accumulates_across_periods() {
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &1_000_u64, &true);

    advance_periods(&env, 3);

    // 1000 -> 900 -> 810 -> 729, flooring at each step.
    assert_eq!(client.get_points(&user), 729);
}

#[test]
fn test_a_small_score_decays_away_entirely() {
    // Integer flooring means low scores reach zero quickly rather than
    // lingering as a fractional residue forever.
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &5_u64, &true);

    advance_periods(&env, 12);

    assert_eq!(client.get_points(&user), 0);
}

#[test]
fn test_score_floors_to_zero_once_fully_stale() {
    // At DECAY_ZERO_AFTER_PERIODS a score is written off rather than left as
    // a rounding artefact.
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &1_000_000_u64, &true);

    advance_periods(&env, DECAY_ZERO_AFTER_PERIODS);

    assert_eq!(client.get_points(&user), 0);
}

#[test]
fn test_accrual_builds_on_the_decayed_value_not_the_original() {
    // A returning player resumes from what their score has become, not from
    // where they left it.
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &1_000_u64, &true);

    advance_periods(&env, 1); // 1000 -> 900
    client.add_pts(&market, &user, &100_u64, &true);

    assert_eq!(client.get_points(&user), 1_000);
}

#[test]
fn test_activity_counters_are_lifetime_totals_and_never_decay() {
    // Decay is about ranking freshness, not rewriting a player's record.
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &500_u64, &true);
    client.add_pts(&market, &user, &500_u64, &false);

    advance_periods(&env, 30);

    let stats = client.get_stats(&user);
    assert_eq!(stats.won_bets, 1);
    assert_eq!(stats.lost_bets, 1);
    assert_eq!(stats.total_bets, 2);
    assert!(stats.points < 1_000, "points should have decayed");
}

// ── The top list stays coherent under decay ─────────────────────────────────

#[test]
fn test_top_list_reports_a_descending_order_after_decay() {
    let (env, client, _admin, market, _referral) = setup();
    for i in 0..10u64 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(1_000 + i * 100), &true);
    }

    advance_periods(&env, 5);
    let top = client.get_top_players(&0_u32, &20_u32);
    let mut previous = u64::MAX;
    for entry in top.iter() {
        assert!(
            entry.points <= previous,
            "top list lost its descending order once decay was applied"
        );
        previous = entry.points;
    }
}

#[test]
fn test_views_reflect_decay_with_no_intervening_write() {
    // Decay is never written into storage by a background pass, so between a
    // player's last write and now, storage still holds their pre-decay score.
    // A view must not report that stale value.
    let (env, client, _admin, market, _referral) = setup();
    let user = Address::generate(&env);
    client.add_pts(&market, &user, &1_000_u64, &true);

    advance_periods(&env, 2); // nothing writes in between

    assert_eq!(client.get_points(&user), 810);
    let top = client.get_top_players(&0_u32, &10_u32);
    assert_eq!(top.get(0).unwrap().points, 810);
}

#[test]
fn test_a_newcomer_enters_on_a_score_the_old_model_would_have_rejected() {
    // The crux of the eviction path. The list is full of scores 100..=149.
    // After five periods those are worth 59..=88, but *storage* still says
    // 100..=149 — decay is applied when they are compared, not by rewriting
    // every slot.
    //
    // The newcomer scores 70: more than the weakest entry is now worth (59),
    // less than what that entry has stored (100). Under the old monotonic
    // model the comparison was against the stored 100 and the newcomer was
    // turned away. Now the incumbent is decayed first, and the newcomer gets
    // in on merit.
    let (env, client, _admin, market, _referral) = setup();

    let weakest = Address::generate(&env);
    client.add_pts(&market, &weakest, &100_u64, &true);
    for i in 1..MAX_TOP_PLAYERS as u64 {
        let user = Address::generate(&env);
        client.add_pts(&market, &user, &(100 + i), &true);
    }
    assert_eq!(client.get_top_player_count(), MAX_TOP_PLAYERS);
    assert_eq!(client.get_min_points(), 100);

    advance_periods(&env, 5);

    // 100 -> 90 -> 81 -> 72 -> 64 -> 57, flooring at each step.
    assert_eq!(client.get_min_points(), 57);

    let newcomer = Address::generate(&env);
    client.add_pts(&market, &newcomer, &70_u64, &true);

    assert_ne!(
        client.get_rank(&newcomer),
        UNRANKED_RANK,
        "a score of 70 must displace an incumbent now worth 59, even though \
         that incumbent still has 100 in storage"
    );
    assert_eq!(
        client.get_rank(&weakest),
        UNRANKED_RANK,
        "the decayed weakest entry should have been evicted"
    );
    assert_eq!(client.get_top_player_count(), MAX_TOP_PLAYERS);
}

#[test]
fn test_a_player_who_keeps_playing_holds_their_rank() {
    // The flip side of the headline: decay must not punish activity. A player
    // who keeps earning stays ahead of one who stopped.
    let (env, client, _admin, market, _referral) = setup();
    let active = Address::generate(&env);
    let quitter = Address::generate(&env);

    client.add_pts(&market, &active, &1_000_u64, &true);
    client.add_pts(&market, &quitter, &1_000_u64, &true);

    for _ in 0..10 {
        advance_periods(&env, 1);
        client.add_pts(&market, &active, &200_u64, &true);
    }

    assert!(client.get_points(&active) > client.get_points(&quitter));
    assert_eq!(client.get_rank(&active), 1);
}
