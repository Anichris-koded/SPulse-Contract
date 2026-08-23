#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, vec, Address, Env, IntoVal, Symbol, Val,
    Vec,
};

pub const MAX_TOP_PLAYERS: u32 = 50;
/// Rank returned by `get_rank` for a player who is not in the top list.
///
/// It must be numerically greater than every valid in-list rank
/// (`1..=MAX_TOP_PLAYERS`) so that an unranked player never sorts above an
/// actual position. Historically this value was `0`, which was strictly less
/// than every valid rank and made "unranked" indistinguishable from "rank 0"
/// (issue #91). Callers should treat `rank > MAX_TOP_PLAYERS` as "not ranked".
pub const UNRANKED_RANK: u32 = MAX_TOP_PLAYERS + 1;
const MAX_PAGE_SIZE: u32 = 50;
const TTL_BUMP: u32 = 3_153_600;
const TTL_HIGH: u32 = 6_307_200;

// ── Point decay (issue #69) ──────────────────────────────────────────────────
//
// Points used to only ever increase, which made the board a cumulative
// history rather than a ranking: whoever accumulated first could never be
// overtaken except in absolute lifetime totals, no matter how inactive they
// became. Scores now lose value with time, so a rank reflects recent activity.
//
// Decay is quantised to whole periods and keyed off a *global* epoch derived
// from the ledger sequence, rather than a per-player "last touched" stamp.
// Two things follow from that, and both matter:
//
//   * A player cannot refresh their own clock by transacting. Writing every
//     six days does not dodge the weekly decay, because the epoch is not
//     theirs to reset. A per-player anchor would have made frequent tiny
//     writes a way to freeze a score forever.
//   * Every stored score is expressed in the same epoch, so they stay
//     directly comparable and the top list needs no re-sort — flooring
//     multiplication is monotone, so a descending list stays descending
//     after a uniform sweep.

/// Ledgers in one decay period — ~7 days at 5s/ledger.
const DECAY_PERIOD_LEDGERS: u32 = 120_960;
/// Each period a score keeps DECAY_RETAIN_NUM/DECAY_RETAIN_DEN of its value.
/// 9/10 is ~10% off per week; ~65% of a score survives a month of inactivity.
const DECAY_RETAIN_NUM: u64 = 9;
const DECAY_RETAIN_DEN: u64 = 10;
/// Past this many idle periods a score is treated as fully stale and floors
/// to zero. Derived from TTL_HIGH rather than picked: a score cannot outlive
/// the storage entry holding it, so there is no meaning in a residue that
/// survives longer than the entry would. It also bounds the decay loop,
/// keeping the cost of a sweep predictable. Works out to 52 periods (~1 year).
const DECAY_ZERO_AFTER_PERIODS: u32 = TTL_HIGH / DECAY_PERIOD_LEDGERS;

// Issue #84: bump whenever a function signature, argument order, or return
// type that a caller relies on changes. Callers pin the version they were
// built against and check it before invoking, so an incompatible upgrade
// fails with a clear error instead of a silently broken cross-contract call.
pub const INTERFACE_VERSION: u32 = 1;

// Issue #84: the version of pulse_token's ABI that reward()/reward_bonus()
// were built against. Bump this whenever a breaking change is made to the
// mint() signature/argument order/return type that this contract relies on.
const EXPECTED_TOKEN_INTERFACE_VERSION: u32 = 1;

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum LeaderboardError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    UnauthorizedCaller = 3,
    InvalidPoints = 4,
    NotAdmin = 5,
    ContractPaused = 6,
    /// pulse_token reported an interface_version this contract wasn't built
    /// against (issue #84). Note: a matching version number alone does not
    /// prove the callee's actual function shape still matches; it only
    /// proves the callee's author intended it to. The guarantee only holds
    /// if every breaking ABI change (renamed function, changed argument
    /// order/count/type, changed return type) always increments
    /// INTERFACE_VERSION in the same commit. See EXPECTED_TOKEN_INTERFACE_VERSION.
    IncompatibleInterface = 7,
    /// reward()/reward_bonus() called with tokens > 0 but no TokenContract
    /// has been set via set_token_contract.
    TokenNotConfigured = 8,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    MarketContract,
    ReferralContract,
    TokenContract,
    Stats(Address),
    TopPlayerAt(u32),
    TopPlayerCount,
    TopPlayerSlot(Address),
    TopPlayerSeqAt(u32),
    SeqCounter,
    MinPoints,
    MinSlot,
    Paused,
    StatsEpoch(Address),
    PendingReward(Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerEntry {
    pub address: Address,
    pub points: u64,
    pub epoch: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerStats {
    pub points: u64,
    pub total_bets: u32,
    pub won_bets: u32,
    pub lost_bets: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredStats {
    pub points: u64,
    pub won_bets: u32,
    pub lost_bets: u32,
    pub bonus_bets: u32,
}

impl StoredStats {
    fn zero() -> Self {
        StoredStats {
            points: 0,
            won_bets: 0,
            lost_bets: 0,
            bonus_bets: 0,
        }
    }

    fn to_player_stats(&self) -> PlayerStats {
        PlayerStats {
            points: self.points,
            total_bets: self.won_bets + self.lost_bets + self.bonus_bets,
            won_bets: self.won_bets,
            lost_bets: self.lost_bets,
        }
    }
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingReward {
    pub points: u64,
    pub tokens: i128,
    pub won_delta: u32,
    pub lost_delta: u32,
    pub bet_delta: u32,
}

#[contract]
pub struct LeaderboardContract;

#[contractimpl]
impl LeaderboardContract {
    pub fn initialize(
        env: Env,
        admin: Address,
        market_contract: Address,
        referral_contract: Address,
    ) -> Result<(), LeaderboardError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(LeaderboardError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::MarketContract, &market_contract);
        env.storage()
            .instance()
            .set(&DataKey::ReferralContract, &referral_contract);
        env.storage()
            .instance()
            .set(&DataKey::TopPlayerCount, &0_u32);
        env.storage().instance().set(&DataKey::MinPoints, &0_u64);
        env.storage().instance().set(&DataKey::MinSlot, &0_u32);
        env.storage().instance().set(&DataKey::SeqCounter, &0_u64);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn set_token(
        env: Env,
        admin: Address,
        token: Address,
    ) -> Result<(), LeaderboardError> {
        Self::set_token_contract(env, admin, token)
    }

    pub fn set_token_contract(
        env: Env,
        admin: Address,
        token: Address,
    ) -> Result<(), LeaderboardError> {
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(LeaderboardError::NotInitialized)?;
        if admin != stored {
            return Err(LeaderboardError::NotAdmin);
        }
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::TokenContract, &token);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn interface_version(_env: Env) -> u32 {
        INTERFACE_VERSION
    }

    pub fn pause(env: Env, admin: Address) -> Result<(), LeaderboardError> {
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(LeaderboardError::NotInitialized)?;
        if admin != stored {
            return Err(LeaderboardError::NotAdmin);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((Symbol::new(&env, "paused"), admin), true);
        Ok(())
    }

    pub fn unpause(env: Env, admin: Address) -> Result<(), LeaderboardError> {
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(LeaderboardError::NotInitialized)?;
        if admin != stored {
            return Err(LeaderboardError::NotAdmin);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events().publish((Symbol::new(&env, "unpaused"), admin), true);
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    pub fn queue_reward(
        env: Env,
        caller: Address,
        user: Address,
        points: u64,
        tokens: i128,
        is_winner: bool,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        Self::require_market_contract(&env, &caller)?;
        caller.require_auth();
        Self::accumulate_pending(&env, &user, points, tokens, is_winner, false);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn queue_bonus_reward(
        env: Env,
        caller: Address,
        user: Address,
        points: u64,
        tokens: i128,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        Self::require_referral_contract(&env, &caller)?;
        caller.require_auth();
        Self::accumulate_pending(&env, &user, points, tokens, false, true);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn claim_pending_rewards(env: Env, user: Address) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        let key = DataKey::PendingReward(user.clone());
        let pending: PendingReward = match env.storage().persistent().get(&key) {
            Some(p) => p,
            None => return Ok(()),
        };
        env.storage().persistent().remove(&key);

        let mut stored = Self::stats_for_update(&env, &user);
        stored.points += pending.points;
        stored.won_bets += pending.won_delta;
        stored.lost_bets += pending.lost_delta;
        stored.bonus_bets += pending.bet_delta.saturating_sub(pending.won_delta + pending.lost_delta);
        Self::commit_stats(&env, &user, &stored);

        Self::update_top_players(&env, user.clone(), stored.points);

        if pending.tokens > 0 {
            Self::mint_reward(&env, &user, pending.tokens)?;
        }
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn get_pending_reward(env: Env, user: Address) -> Option<PendingReward> {
        env.storage().persistent().get(&DataKey::PendingReward(user))
    }

    pub fn add_pts(
        env: Env,
        caller: Address,
        user: Address,
        pts: u64,
        is_won: bool,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        Self::require_market_contract(&env, &caller)?;
        caller.require_auth();

        let mut stored = Self::stats_for_update(&env, &user);
        stored.points += pts;
        if is_won {
            stored.won_bets += 1;
        } else {
            stored.lost_bets += 1;
        }
        Self::commit_stats(&env, &user, &stored);

        Self::update_top_players(&env, user.clone(), stored.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        env.events().publish(
            (Symbol::new(&env, "leaderboard_updated"), user),
            (stored.points, stored.won_bets, stored.lost_bets),
        );
        Ok(())
    }

    pub fn reward(
        env: Env,
        caller: Address,
        user: Address,
        points: u64,
        tokens: i128,
        is_winner: bool,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        caller.require_auth();
        Self::require_market_contract(&env, &caller)?;
        if points == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }

        let mut stored = Self::stats_for_update(&env, &user);
        stored.points += points;
        if is_winner {
            stored.won_bets += 1;
        } else {
            stored.lost_bets += 1;
        }
        Self::commit_stats(&env, &user, &stored);

        Self::update_top_players(&env, user.clone(), stored.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);

        if tokens > 0 {
            Self::mint_reward(&env, &user, tokens)?;
        }
        env.events().publish(
            (Symbol::new(&env, "leaderboard_updated"), user),
            (stored.points, is_winner, tokens),
        );
        Ok(())
    }

    pub fn reward_bonus(
        env: Env,
        caller: Address,
        user: Address,
        pts: u64,
        tokens: i128,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        caller.require_auth();
        Self::require_referral_contract(&env, &caller)?;
        if pts == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }

        let mut stored = Self::stats_for_update(&env, &user);
        stored.points += pts;
        stored.bonus_bets += 1;
        Self::commit_stats(&env, &user, &stored);

        Self::update_top_players(&env, user.clone(), stored.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);

        if tokens > 0 {
            Self::mint_reward(&env, &user, tokens)?;
        }
        env.events().publish(
            (Symbol::new(&env, "leaderboard_updated"), user),
            (stored.points, tokens),
        );
        Ok(())
    }

    pub fn add_bonus_pts(
        env: Env,
        caller: Address,
        user: Address,
        pts: u64,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        caller.require_auth();
        Self::require_referral_contract(&env, &caller)?;
        if pts == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }

        let mut stored = Self::stats_for_update(&env, &user);
        stored.points += pts;
        stored.bonus_bets += 1;
        Self::commit_stats(&env, &user, &stored);

        Self::update_top_players(&env, user.clone(), stored.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn record_bet(
        env: Env,
        caller: Address,
        _user: Address,
    ) -> Result<(), LeaderboardError> {
        Self::require_market_contract(&env, &caller)?;
        caller.require_auth();
        Ok(())
    }

    pub fn get_points(env: Env, user: Address) -> u64 {
        Self::decayed_stats(&env, &user).points
    }

    pub fn get_stats(env: Env, user: Address) -> PlayerStats {
        Self::decayed_stats(&env, &user)
    }

    pub fn get_rank(env: Env, user: Address) -> u32 {
        if let Some((slot, entry)) = Self::top_slot_entry(&env, &user) {
            let my_pts = Self::entry_points_now(&env, &entry);
            for s in 0..=slot {
                if let Some(e) = env
                    .storage()
                    .persistent()
                    .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(s))
                {
                    if Self::entry_points_now(&env, &e) == my_pts {
                        return s + 1;
                    }
                }
            }
            slot + 1
        } else {
            UNRANKED_RANK
        }
    }

    pub fn get_player_count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0)
    }

    pub fn get_top_players(env: Env, offset: u32, page_size: u32) -> Vec<PlayerEntry> {
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0);

        if offset >= count || page_size == 0 {
            return vec![&env];
        }

        let page_size = page_size.min(MAX_PAGE_SIZE);
        let end = (offset + page_size).min(count);
        let now = Self::current_epoch(&env);
        let mut result = Vec::new(&env);
        for i in offset..end {
            if let Some(mut entry) = env
                .storage()
                .persistent()
                .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(i))
            {
                entry.points = Self::entry_points_now(&env, &entry);
                entry.epoch = now;
                result.push_back(entry);
            }
        }
        result
    }

    pub fn get_top_player_count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0)
    }

    pub fn get_min_points(env: Env) -> u64 {
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0);
        if count == 0 {
            return 0;
        }
        match env
            .storage()
            .persistent()
            .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(count - 1))
        {
            Some(entry) => Self::entry_points_now(&env, &entry),
            None => env
                .storage()
                .instance()
                .get(&DataKey::MinPoints)
                .unwrap_or(0),
        }
    }

    pub fn get_min_slot(env: Env) -> u32 {
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0);
        if count == 0 {
            0
        } else {
            count - 1
        }
    }

    pub fn reconcile_top_slots(env: Env) {
        Self::repair_top_list(&env);
    }

    pub fn refresh_player_ttl(env: Env, user: Address) {
        let stats_key = DataKey::Stats(user.clone());
        if env.storage().persistent().has(&stats_key) {
            env.storage()
                .persistent()
                .extend_ttl(&stats_key, TTL_BUMP, TTL_HIGH);
        }
        if let Some((slot, _)) = Self::top_slot_entry(&env, &user) {
            env.storage().persistent().extend_ttl(
                &DataKey::TopPlayerAt(slot),
                TTL_BUMP,
                TTL_HIGH,
            );
            env.storage().persistent().extend_ttl(
                &DataKey::TopPlayerSlot(user),
                TTL_BUMP,
                TTL_HIGH,
            );
        }
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
    }

    fn top_slot_entry(env: &Env, user: &Address) -> Option<(u32, PlayerEntry)> {
        let slot: Option<u32> = env
            .storage()
            .persistent()
            .get(&DataKey::TopPlayerSlot(user.clone()));
        if let Some(s) = slot {
            if let Some(entry) = env
                .storage()
                .persistent()
                .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(s))
            {
                if entry.address == *user {
                    return Some((s, entry));
                }
            }
            env.storage()
                .persistent()
                .remove(&DataKey::TopPlayerSlot(user.clone()));
        }
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0);
        for s in 0..count {
            if let Some(entry) = env
                .storage()
                .persistent()
                .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(s))
            {
                if entry.address == *user {
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerSlot(user.clone()), &s);
                    return Some((s, entry));
                }
            }
        }
        None
    }

    fn seq_at(env: &Env, s: u32) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::TopPlayerSeqAt(s))
            .unwrap_or(0)
    }

    fn stamp_seq(env: &Env, s: u32) {
        let counter: u64 = env
            .storage()
            .instance()
            .get(&DataKey::SeqCounter)
            .unwrap_or(0)
            + 1;
        env.storage().instance().set(&DataKey::SeqCounter, &counter);
        let seq_key = DataKey::TopPlayerSeqAt(s);
        env.storage().persistent().set(&seq_key, &counter);
        env.storage()
            .persistent()
            .extend_ttl(&seq_key, TTL_BUMP, TTL_HIGH);
    }

    fn recompute_min(env: &Env, count: u32) {
        if count == 0 {
            env.storage().instance().set(&DataKey::MinPoints, &0_u64);
            env.storage().instance().set(&DataKey::MinSlot, &0_u32);
            return;
        }
        let min_slot = count - 1;
        let min_pts = match env
            .storage()
            .persistent()
            .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(min_slot))
        {
            Some(entry) => Self::entry_points_now(env, &entry),
            None => 0,
        };
        env.storage().instance().set(&DataKey::MinPoints, &min_pts);
        env.storage().instance().set(&DataKey::MinSlot, &min_slot);
    }

    fn bubble_up(env: &Env, entry: &PlayerEntry, mut slot: u32) {
        while slot > 0 {
            match env
                .storage()
                .persistent()
                .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(slot - 1))
            {
                Some(prev) => {
                    let prev_pts = Self::entry_points_now(env, &prev);
                    let entry_pts = Self::entry_points_now(env, entry);
                    let should_swap = if entry_pts > prev_pts {
                        true
                    } else if entry_pts == prev_pts {
                        let entry_seq = Self::seq_at(env, slot);
                        let prev_seq = Self::seq_at(env, slot - 1);
                        entry_seq > prev_seq
                    } else {
                        false
                    };
                    if !should_swap {
                        break;
                    }
                    let key_hi = DataKey::TopPlayerAt(slot - 1);
                    let key_lo = DataKey::TopPlayerAt(slot);
                    env.storage().persistent().set(&key_hi, entry);
                    env.storage().persistent().set(&key_lo, &prev);
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerSlot(entry.address.clone()), &(slot - 1));
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerSlot(prev.address.clone()), &slot);
                    let seq_hi = Self::seq_at(env, slot - 1);
                    let seq_lo = Self::seq_at(env, slot);
                    let seq_key_hi = DataKey::TopPlayerSeqAt(slot - 1);
                    let seq_key_lo = DataKey::TopPlayerSeqAt(slot);
                    env.storage().persistent().set(&seq_key_hi, &seq_lo);
                    env.storage().persistent().set(&seq_key_lo, &seq_hi);
                    slot -= 1;
                }
                None => {
                    let key_hi = DataKey::TopPlayerAt(slot - 1);
                    let key_lo = DataKey::TopPlayerAt(slot);
                    env.storage().persistent().set(&key_hi, entry);
                    env.storage().persistent().remove(&key_lo);
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerSlot(entry.address.clone()), &(slot - 1));
                    let seq_lo = Self::seq_at(env, slot);
                    let seq_key_hi = DataKey::TopPlayerSeqAt(slot - 1);
                    let seq_key_lo = DataKey::TopPlayerSeqAt(slot);
                    env.storage().persistent().set(&seq_key_hi, &seq_lo);
                    env.storage().persistent().remove(&seq_key_lo);
                    let count: u32 = env
                        .storage()
                        .instance()
                        .get(&DataKey::TopPlayerCount)
                        .unwrap_or(0);
                    if count > 1 {
                        env.storage()
                            .instance()
                            .set(&DataKey::TopPlayerCount, &(count - 1));
                    }
                    slot -= 1;
                }
            }
        }
    }

    fn insert_new(env: &Env, user: &Address, points: u64, slot: u32) {
        let entry = PlayerEntry {
            address: user.clone(),
            points,
            epoch: Self::current_epoch(env),
        };
        let key = DataKey::TopPlayerAt(slot);
        env.storage().persistent().set(&key, &entry);
        env.storage()
            .persistent()
            .set(&DataKey::TopPlayerSlot(user.clone()), &slot);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_BUMP, TTL_HIGH);
        env.storage().persistent().extend_ttl(
            &DataKey::TopPlayerSlot(user.clone()),
            TTL_BUMP,
            TTL_HIGH,
        );
        Self::stamp_seq(env, slot);
        let new_count = slot + 1;
        env.storage()
            .instance()
            .set(&DataKey::TopPlayerCount, &new_count);

        Self::bubble_up(env, &entry, slot);

        Self::recompute_min(env, new_count);
    }

    fn repair_top_list(env: &Env) -> u32 {
        let mut entries: Vec<PlayerEntry> = Vec::new(env);
        let mut seqs: Vec<u64> = Vec::new(env);
        for i in 0..MAX_TOP_PLAYERS {
            if let Some(e) = env
                .storage()
                .persistent()
                .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(i))
            {
                let s = Self::seq_at(env, i);
                entries.push_back(e);
                seqs.push_back(s);
            }
        }

        let n = entries.len();
        for i in 0..n {
            let mut max_idx = i;
            for j in (i + 1)..n {
                let a = Self::entry_points_now(env, &entries.get(j).unwrap());
                let b = Self::entry_points_now(env, &entries.get(max_idx).unwrap());
                if a > b {
                    max_idx = j;
                }
            }
            if max_idx != i {
                let a = entries.get(i).unwrap().clone();
                let b = entries.get(max_idx).unwrap().clone();
                entries.set(i, b);
                entries.set(max_idx, a);
                let sa = seqs.get(i).unwrap();
                let sb = seqs.get(max_idx).unwrap();
                seqs.set(i, sb);
                seqs.set(max_idx, sa);
            }
        }

        for slot in 0..n {
            let entry = entries.get(slot).unwrap();
            let seq = seqs.get(slot).unwrap();
            let key = DataKey::TopPlayerAt(slot);
            env.storage().persistent().set(&key, &entry);
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_BUMP, TTL_HIGH);
            env.storage()
                .persistent()
                .set(&DataKey::TopPlayerSlot(entry.address.clone()), &slot);
            env.storage().persistent().extend_ttl(
                &DataKey::TopPlayerSlot(entry.address.clone()),
                TTL_BUMP,
                TTL_HIGH,
            );
            let seq_key = DataKey::TopPlayerSeqAt(slot);
            env.storage().persistent().set(&seq_key, &seq);
            env.storage()
                .persistent()
                .extend_ttl(&seq_key, TTL_BUMP, TTL_HIGH);
        }
        for slot in n..MAX_TOP_PLAYERS {
            env.storage()
                .persistent()
                .remove(&DataKey::TopPlayerAt(slot));
            env.storage()
                .persistent()
                .remove(&DataKey::TopPlayerSeqAt(slot));
        }

        env.storage().instance().set(&DataKey::TopPlayerCount, &n);
        if n > 0 {
            Self::recompute_min(env, n);
        } else {
            env.storage().instance().set(&DataKey::MinPoints, &0_u64);
            env.storage().instance().set(&DataKey::MinSlot, &0_u32);
        }
        n
    }

    fn update_top_players(env: &Env, user: Address, new_points: u64) {
        if let Some((slot, mut entry)) = Self::top_slot_entry(env, &user) {
            entry.points = new_points;
            entry.epoch = Self::current_epoch(env);
            let key = DataKey::TopPlayerAt(slot);
            env.storage().persistent().set(&key, &entry);
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_BUMP, TTL_HIGH);

            Self::bubble_up(env, &entry, slot);

            let count: u32 = env
                .storage()
                .instance()
                .get(&DataKey::TopPlayerCount)
                .unwrap_or(0);
            Self::recompute_min(env, count);
            return;
        }

        let mut count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0);

        if count < MAX_TOP_PLAYERS {
            Self::insert_new(env, &user, new_points, count);
            return;
        }

        let min_slot = count - 1;
        let mut old_entry: Option<PlayerEntry> = env
            .storage()
            .persistent()
            .get(&DataKey::TopPlayerAt(min_slot));

        if old_entry.is_none() {
            count = Self::repair_top_list(env);
            if count < MAX_TOP_PLAYERS {
                Self::insert_new(env, &user, new_points, count);
                return;
            }
            old_entry = env
                .storage()
                .persistent()
                .get(&DataKey::TopPlayerAt(min_slot));
        }

        if let Some(old) = old_entry {
            let old_pts = Self::entry_points_now(env, &old);
            if new_points < old_pts {
                return;
            }
            env.storage()
                .persistent()
                .remove(&DataKey::TopPlayerSlot(old.address.clone()));

            let new_entry = PlayerEntry {
                address: user.clone(),
                points: new_points,
                epoch: Self::current_epoch(env),
            };
            let key = DataKey::TopPlayerAt(min_slot);
            env.storage().persistent().set(&key, &new_entry);
            env.storage()
                .persistent()
                .set(&DataKey::TopPlayerSlot(user.clone()), &min_slot);
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_BUMP, TTL_HIGH);
            env.storage().persistent().extend_ttl(
                &DataKey::TopPlayerSlot(user.clone()),
                TTL_BUMP,
                TTL_HIGH,
            );
            Self::stamp_seq(env, min_slot);

            Self::bubble_up(env, &new_entry, min_slot);

            Self::recompute_min(env, MAX_TOP_PLAYERS);
        }
    }

    fn current_epoch(env: &Env) -> u32 {
        env.ledger().sequence() / DECAY_PERIOD_LEDGERS
    }

    fn decay(points: u64, periods: u32) -> u64 {
        if points == 0 || periods == 0 {
            return points;
        }
        if periods >= DECAY_ZERO_AFTER_PERIODS {
            return 0;
        }
        let mut value = points as u128;
        for _ in 0..periods {
            value = value * DECAY_RETAIN_NUM as u128 / DECAY_RETAIN_DEN as u128;
            if value == 0 {
                return 0;
            }
        }
        value as u64
    }

    fn entry_points_now(env: &Env, entry: &PlayerEntry) -> u64 {
        let now = Self::current_epoch(env);
        Self::decay(entry.points, now.saturating_sub(entry.epoch))
    }

    fn decayed_stats(env: &Env, user: &Address) -> PlayerStats {
        let stored: StoredStats = env
            .storage()
            .persistent()
            .get(&DataKey::Stats(user.clone()))
            .unwrap_or_else(StoredStats::zero);
        if stored.points == 0 {
            return stored.to_player_stats();
        }
        let now = Self::current_epoch(env);
        let written_at: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::StatsEpoch(user.clone()))
            .unwrap_or(now);
        let decayed_pts = Self::decay(stored.points, now.saturating_sub(written_at));
        let mut stats = stored.to_player_stats();
        stats.points = decayed_pts;
        stats
    }

    fn stats_for_update(env: &Env, user: &Address) -> StoredStats {
        let mut stored: StoredStats = env
            .storage()
            .persistent()
            .get(&DataKey::Stats(user.clone()))
            .unwrap_or_else(StoredStats::zero);
        if stored.points > 0 {
            let now = Self::current_epoch(env);
            let written_at: u32 = env
                .storage()
                .persistent()
                .get(&DataKey::StatsEpoch(user.clone()))
                .unwrap_or(now);
            stored.points = Self::decay(stored.points, now.saturating_sub(written_at));
        }
        stored
    }

    fn commit_stats(env: &Env, user: &Address, stats: &StoredStats) {
        let key = DataKey::Stats(user.clone());
        env.storage().persistent().set(&key, stats);
        env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);

        let epoch_key = DataKey::StatsEpoch(user.clone());
        env.storage().persistent().set(&epoch_key, &Self::current_epoch(env));
        env.storage().persistent().extend_ttl(&epoch_key, TTL_BUMP, TTL_HIGH);
    }

    fn accumulate_pending(
        env: &Env,
        user: &Address,
        points: u64,
        tokens: i128,
        is_winner: bool,
        is_bonus: bool,
    ) {
        let key = DataKey::PendingReward(user.clone());
        let mut pending: PendingReward = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(PendingReward {
                points: 0,
                tokens: 0,
                won_delta: 0,
                lost_delta: 0,
                bet_delta: 0,
            });
        pending.points += points;
        pending.tokens += tokens;
        pending.bet_delta += 1;
        if !is_bonus {
            if is_winner {
                pending.won_delta += 1;
            } else {
                pending.lost_delta += 1;
            }
        }
        env.storage().persistent().set(&key, &pending);
        env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
    }

    fn require_market_contract(env: &Env, caller: &Address) -> Result<(), LeaderboardError> {
        let market: Address = env
            .storage()
            .instance()
            .get(&DataKey::MarketContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if *caller != market {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        Ok(())
    }

    fn require_referral_contract(env: &Env, caller: &Address) -> Result<(), LeaderboardError> {
        let referral: Address = env
            .storage()
            .instance()
            .get(&DataKey::ReferralContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if *caller != referral {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        Ok(())
    }

    fn require_not_paused(env: &Env) -> Result<(), LeaderboardError> {
        if env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(LeaderboardError::ContractPaused);
        }
        Ok(())
    }

    fn require_compatible_token(env: &Env, token: &Address) -> Result<(), LeaderboardError> {
        let version: u32 =
            env.invoke_contract(token, &Symbol::new(env, "interface_version"), vec![env]);
        if version != EXPECTED_TOKEN_INTERFACE_VERSION {
            return Err(LeaderboardError::IncompatibleInterface);
        }
        Ok(())
    }

    fn mint_reward(env: &Env, user: &Address, tokens: i128) -> Result<(), LeaderboardError> {
        let token: Address = env
            .storage()
            .instance()
            .get(&DataKey::TokenContract)
            .ok_or(LeaderboardError::TokenNotConfigured)?;
        Self::require_compatible_token(env, &token)?;
        let this = env.current_contract_address();
        let _: Val = env.invoke_contract(
            &token,
            &Symbol::new(env, "mint"),
            vec![
                env,
                this.into_val(env),
                user.into_val(env),
                tokens.into_val(env),
            ],
        );
        Ok(())
    }
}

#[cfg(test)]
mod decay_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod ttl_tests;
