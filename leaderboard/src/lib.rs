#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, vec, Address, Env, IntoVal, Symbol, Val,
    Vec,
};

pub const MAX_TOP_PLAYERS: u32 = 50;
/// Rank returned by `get_rank` for a player who is not in the top list.
/// Must be numerically greater than every valid in-list rank so an unranked
/// player never sorts above a real position (issue #91).
pub const UNRANKED_RANK: u32 = MAX_TOP_PLAYERS + 1;
const MAX_PAGE_SIZE: u32 = 20;
const TTL_BUMP: u32 = 3_153_600;
const TTL_HIGH: u32 = 6_307_200;

// ── Point decay (issue #69) ──────────────────────────────────────────────────
// Scores lose value with time, so a rank reflects recent activity. Decay is
// quantised to whole periods and keyed off a *global* epoch derived from the
// ledger sequence (never a per-player stamp a player could reset by
// transacting). Every stored score is expressed in the same epoch, so scores
// stay directly comparable and a descending list stays descending after a
// uniform sweep (flooring multiplication is monotone).

/// Ledgers in one decay period — ~7 days at 5s/ledger.
const DECAY_PERIOD_LEDGERS: u32 = 120_960;
/// Each period a score keeps DECAY_RETAIN_NUM/DECAY_RETAIN_DEN of its value.
const DECAY_RETAIN_NUM: u64 = 9;
const DECAY_RETAIN_DEN: u64 = 10;
/// Past this many idle periods a score floors to zero. Derived from TTL_HIGH:
/// a score cannot outlive the entry holding it, and this bounds the decay loop.
const DECAY_ZERO_AFTER_PERIODS: u32 = TTL_HIGH / DECAY_PERIOD_LEDGERS;

/// How many slots one call may bubble an entry through (a transaction may
/// write at most 50 ledger entries). An entry that cannot reach its place in
/// one call settles on subsequent writes; `get_top_players`/`get_rank` rank
/// on decayed values at read time regardless, so the reported order is exact.
const MAX_BUBBLE_STEPS: u32 = 8;

// Issue #84: bump whenever a function signature, argument order, or return
// type that a caller relies on changes.
pub const INTERFACE_VERSION: u32 = 1;
// pulse_token ABI version reward()/reward_bonus() were built against.
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
    IncompatibleInterface = 7,
    TokenNotConfigured = 8,
    /// Admin tried to remove/reset a player with no Stats record, no pending
    /// reward, and no top-list presence — i.e. never tracked.
    PlayerNotFound = 9,
    /// A banned player tried to earn points. Banned status is permanent until
    /// the admin explicitly lifts it (no lift function yet; add if needed).
    PlayerBanned = 10,
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
    SeqCounter, // u64 — monotonic counter feeding PlayerEntry::seq
    MinPoints,  // u64 — weakest live entry's (decayed) points
    MinSlot,    // u32 — slot index of that weakest entry
    Paused,
    StatsEpoch(Address),
    // Pull-based reward queue (issue #86).
    PendingReward(Address),
    // Issue #20: permanently flagged addresses. Every accrual path checks this
    // key and returns PlayerBanned if present.
    BannedPlayer(Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerEntry {
    pub address: Address,
    pub points: u64,
    /// Issue #69: the decay epoch `points` is expressed in. Carrying it on the
    /// entry keeps decay comparisons cheap (no extra ledger reads).
    pub epoch: u32,
    /// Monotonic insertion sequence (issue #70) — breaks ties at the minimum
    /// score FIFO-style: the oldest seq is evicted first.
    pub seq: u64,
}

// External-facing stats struct (ABI stable). total_bets is derived at read
// time as won_bets + lost_bets + bonus_bets.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerStats {
    pub points: u64,
    pub total_bets: u32,
    pub won_bets: u32,
    pub lost_bets: u32,
}

// Internal packed stats under DataKey::Stats. Issue #64: bonus_bets is tracked
// separately from won_bets/lost_bets so derived total_bets stays accurate.
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

// Pull-based pending reward. Fields accumulate so multiple rewards can be
// claimed together without losing win/loss accounting.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingReward {
    pub points: u64,
    pub tokens: i128,
    pub won_delta: u32,
    pub lost_delta: u32,
    pub bet_delta: u32,
}

// ── Contract ──────────────────────────────────────────────────────────────────

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
        env.storage().instance().set(&DataKey::TopPlayerCount, &0_u32);
        env.storage().instance().set(&DataKey::MinPoints, &0_u64);
        env.storage().instance().set(&DataKey::MinSlot, &0_u32);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// Set the PULSE token contract used by reward()/reward_bonus() for
    /// internal minting. Admin only. `set_token` is the pre-#23 alias.
    pub fn set_token(
        env: Env,
        admin: Address,
        token: Address,
    ) -> Result<(), LeaderboardError> {
        Self::write_token_contract(&env, &admin, &token)
    }

    pub fn set_token_contract(
        env: Env,
        admin: Address,
        token: Address,
    ) -> Result<(), LeaderboardError> {
        Self::write_token_contract(&env, &admin, &token)
    }

    // ── Bet-settlement path ───────────────────────────────────────────────────

    /// Called by the market contract after a bet is settled.
    /// The cross-contract ABI version this deployment implements (issue #84).
    pub fn interface_version(_env: Env) -> u32 {
        INTERFACE_VERSION
    }

    /// Halt point/reward accrual in an emergency. Admin only. Views keep working.
    pub fn pause(env: Env, admin: Address) -> Result<(), LeaderboardError> {
        Self::require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((Symbol::new(&env, "paused"), admin), true);
        Ok(())
    }

    /// Resume point/reward accrual. Admin only.
    pub fn unpause(env: Env, admin: Address) -> Result<(), LeaderboardError> {
        Self::require_admin(&env, &admin)?;
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

    /// Original ABI name — kept for callers that deploy against the pre-#23
    /// interface (prediction_market and referral_registry tests use it).
    pub fn set_token(
    // ── Bet-settlement path ───────────────────────────────────────────────────

    /// Called by the market contract after a bet is settled. Credits points,
    /// updates win/loss counts, and optionally mints PULSE tokens. Replaces
    /// the old two-call pattern (add_pts + separate mint).
    ///
    /// A banned player is rejected with `PlayerBanned` (#10) before any state
    /// is touched.
    pub fn reward(
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
        if points == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }
        Self::require_not_banned(&env, &user)?;
        Self::credit_points(&env, &user, points, Some(is_winner));
        if tokens > 0 {
            Self::mint_reward(&env, &user, tokens)?;
        }
        Ok(())
    }

    /// Legacy market-contract entrypoint; like reward() but without internal
    /// token minting. A banned player is rejected with `PlayerBanned` (#10).
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
        // add_pts historically accepts 0 (a recorded loss with no points), so
        // unlike reward() it does not reject 0.
        Self::require_not_banned(&env, &user)?;
        Self::credit_points(&env, &user, pts, Some(is_won));
        Ok(())
    }

    // ── Pull-based reward flow (issue #86) ───────────────────────────────────

    /// Queue a settled-bet reward for later claim. A banned player is rejected
    /// with `PlayerBanned` (#10) so they cannot accrue pending points.
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
        if points == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }
        Self::require_not_banned(&env, &user)?;
        Self::accumulate_pending(&env, &user, points, tokens, is_winner, false);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// Queue a referral/welcome bonus reward for later claim. A banned player
    /// is rejected with `PlayerBanned` (#10).
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
        if points == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }
        Self::require_not_banned(&env, &user)?;
        Self::accumulate_pending(&env, &user, points, tokens, false, true);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// Apply all pending points and mint tokens in a separate transaction.
    /// Anyone may submit this; the stored rewards always belong to `user`.
    /// A banned player is rejected with `PlayerBanned` (#10); their pending
    /// reward is left untouched.
    pub fn claim_pending_rewards(env: Env, user: Address) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        Self::require_not_banned(&env, &user)?;
        let key = DataKey::PendingReward(user.clone());
        let pending: PendingReward = match env.storage().persistent().get(&key) {
            Some(p) => p,
            None => return Ok(()),
        };
        env.storage().persistent().remove(&key);

        let mut s = Self::stats_for_update(&env, &user);
        s.points += pending.points;
        s.won_bets += pending.won_delta;
        s.lost_bets += pending.lost_delta;
        // Bonus-only queues increment only bet_delta; route the excess to
        // bonus_bets so derived total_bets stays accurate.
        s.bonus_bets += pending
            .bet_delta
            .saturating_sub(pending.won_delta + pending.lost_delta);
        Self::commit_stats(&env, &user, &s);
        Self::update_top_players(&env, user.clone(), s.points);

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
        tokens: i128,
        is_won: bool,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        let market: Address = env
            .storage()
            .instance()
            .get(&DataKey::MarketContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if caller != market {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        caller.require_auth();
        Self::credit_points(&env, &user, pts, Some(is_won));
        Ok(())
    }
    // ── reward_bonus / add_bonus_pts (referral path) ─────────────────────────

    /// Called by the referral contract for welcome / per-bet referral bonuses.
    /// Increments bonus_bets (not won/lost) so derived total_bets stays
    /// accurate. Optionally mints PULSE. A banned player is rejected with
    /// `PlayerBanned` (#10).
    pub fn reward_bonus(
        env: Env,
        caller: Address,
        user: Address,
        pts: u64,
        tokens: i128,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        Self::require_referral_contract(&env, &caller)?;
        caller.require_auth();
        if pts == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }
        Self::require_not_banned(&env, &user)?;
        Self::credit_bonus(&env, &user, pts);
        if tokens > 0 {
            Self::mint_reward(&env, &user, tokens)?;
        }
        Ok(())
    }

    /// Legacy referral entrypoint; like reward_bonus() but without internal
    /// token minting. A banned player is rejected with `PlayerBanned` (#10).
    pub fn add_bonus_pts(
        env: Env,
        caller: Address,
        user: Address,
        pts: u64,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        Self::require_referral_contract(&env, &caller)?;
        caller.require_auth();
        if pts == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }
        Self::require_not_banned(&env, &user)?;
        Self::credit_bonus(&env, &user, pts);
        Ok(())
    }

    /// No-op stub retained for ABI compatibility. total_bets is derived at read
    /// time, so a standalone "bet recorded" call does nothing.
    pub fn record_bet(env: Env, caller: Address, _user: Address) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        Self::require_market_contract(&env, &caller)?;

        let mut stats = Self::stats_for_update(&env, &user);

    // ── Legacy write functions (kept for backward-compat) ─────────────────────

    /// Deprecated: use `reward()` instead. This function always returns
    /// `UnauthorizedCaller` and will be removed in a future version.
    pub fn add_pts(
        _env: Env,
        _caller: Address,
        _user: Address,
        _pts: u64,
        _is_won: bool,
    ) -> Result<(), LeaderboardError> {
        Err(LeaderboardError::UnauthorizedCaller)
    }

    /// Legacy: called by the referral contract to award bonus points.
    /// Prefer reward_bonus() for new integrations (adds token minting).
    pub fn add_bonus_pts(
        env: Env,
        caller: Address,
        user: Address,
        pts: u64,
    ) -> Result<(), LeaderboardError> {
        let referral: Address = env
            .storage()
            .instance()
            .get(&DataKey::ReferralContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if caller != referral {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        caller.require_auth();
        Ok(())
    }

    // ── View functions ────────────────────────────────────────────────────────

    /// Points as of *now*, with decay applied (issue #69). A read — it never
    /// writes the decayed value back; the next accrual does that.
    pub fn get_points(env: Env, user: Address) -> u64 {
        Self::decayed_stats(&env, &user).points
    }

    /// Stats as of *now*. `points` carries decay; activity counters are
    /// lifetime totals and are deliberately left alone.
    pub fn get_stats(env: Env, user: Address) -> PlayerStats {
        Self::decayed_stats(&env, &user)
    }

    /// 1-based rank inside the top list, computed on decayed values. Players
    /// outside the list get `UNRANKED_RANK` (MAX_TOP_PLAYERS + 1), never 0.
    pub fn get_rank(env: Env, user: Address) -> u32 {
        let Some((slot, entry)) = Self::top_slot_entry(&env, &user) else {
            return UNRANKED_RANK;
        };
        let mine = Self::entry_points_now(&env, &entry);
        let count = Self::top_count(&env);
        let mut rank: u32 = 1;
        for i in 0..count {
            if i == slot {
                continue;
            }
            if let Some(e) = Self::forward_entry(&env, i) {
                if Self::entry_points_now(&env, &e) > mine {
                    rank += 1;
                }
            }
        }
        rank
    }

    /// Number of players currently in the top list (≤ MAX_TOP_PLAYERS).
    pub fn get_top_player_count(env: Env) -> u32 {
        Self::top_count(&env)
    }

    pub fn get_player_count(env: Env) -> u32 {
        Self::top_count(&env)
    }

    /// Page of the top list, ranked on decayed values at read time.
    pub fn get_top_players(env: Env, offset: u32, page_size: u32) -> Vec<PlayerEntry> {
        let count = Self::top_count(&env);
        if offset >= count || page_size == 0 {
            return vec![&env];
        }
        let page_size = page_size.min(MAX_PAGE_SIZE);
        let now = Self::current_epoch(&env);

        let mut ranked: Vec<PlayerEntry> = Vec::new(&env);
        for i in 0..count {
            if let Some(mut entry) = Self::forward_entry(&env, i) {
                entry.points = Self::entry_points_now(&env, &entry);
                entry.epoch = now;
                ranked.push_back(entry);
            }
        }

        // Selection sort, descending — bounded by MAX_TOP_PLAYERS.
        let n = ranked.len() as u32;
        for i in 0..n {
            let mut max_idx = i;
            for j in (i + 1)..n {
                if ranked.get(j).unwrap().points > ranked.get(max_idx).unwrap().points {
                    max_idx = j;
                }
            }
            if max_idx != i {
                let a = ranked.get(i).unwrap();
                let b = ranked.get(max_idx).unwrap();
                ranked.set(i, b);
                ranked.set(max_idx, a);
            }
        }

        let end = (offset + page_size).min(n);
        let mut result = Vec::new(&env);
        for i in offset..end {
            result.push_back(ranked.get(i).unwrap());
        }
        result
    }

    /// Points of the weakest entry currently in the top list, decayed to now.
    pub fn get_min_points(env: Env) -> u64 {
        let slot: u32 = env.storage().instance().get(&DataKey::MinSlot).unwrap_or(0);
        match Self::forward_entry(&env, slot) {
            Some(entry) => Self::entry_points_now(&env, &entry),
            None => env
                .storage()
                .instance()
                .get(&DataKey::MinPoints)
                .unwrap_or(0),
        }
    }

    pub fn get_min_slot(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MinSlot)
            .unwrap_or(0)
    }

    /// Rebuild `TopPlayerSlot` from live `TopPlayerAt` entries, compact holes
    /// left by TTL expiry or admin removal, and refresh the min cache. Anyone
    /// may call this (keeper/repair); it only writes keys that restore the
    /// index invariant.
    pub fn reconcile_top_slots(env: Env) {
        Self::repair_top_index(&env);
    }

    /// Permissionless keeper: extend a player's Stats + top-list mapping and
    /// the instance cache so idle entries cannot vanish (issue #21 / #54).
    pub fn refresh_player_ttl(env: Env, user: Address) {
        let stats_key = DataKey::Stats(user.clone());
        if env.storage().persistent().has(&stats_key) {
            env.storage()
                .persistent()
                .extend_ttl(&stats_key, TTL_BUMP, TTL_HIGH);
        }
        if let Some((slot, _)) = Self::top_slot_entry(&env, &user) {
            env.storage()
                .persistent()
                .extend_ttl(&DataKey::TopPlayerAt(slot), TTL_BUMP, TTL_HIGH);
            env.storage()
                .persistent()
                .extend_ttl(&DataKey::TopPlayerSlot(user), TTL_BUMP, TTL_HIGH);
        }
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
    }
    // ── Issue #20: Admin governance — remove / ban / reset ───────────────────

    /// Remove a player from the top-50 list and erase all their stored state
    /// (Stats, StatsEpoch, PendingReward). Admin only.
    ///
    /// After removal the player is treated as if they never played: they can
    /// re-earn points and re-enter the top list from scratch. To prevent
    /// re-entry use `ban_player` instead (or call both).
    ///
    /// Returns `PlayerNotFound` when the address has no Stats record, no
    /// pending reward, and is not in the top list.
    pub fn remove_player(
        env: Env,
        admin: Address,
        user: Address,
    ) -> Result<(), LeaderboardError> {
        Self::require_admin(&env, &admin)?;

        let stats_key = DataKey::Stats(user.clone());
        let has_stats = env.storage().persistent().has(&stats_key);
        let has_pending = env
            .storage()
            .persistent()
            .has(&DataKey::PendingReward(user.clone()));
        let count = Self::top_count(&env);
        let slot_opt = Self::resolved_slot(&env, &user, count);

        if !has_stats && !has_pending && slot_opt.is_none() {
            return Err(LeaderboardError::PlayerNotFound);
        }

        // 1. Erase Stats, StatsEpoch, PendingReward.
        env.storage().persistent().remove(&stats_key);
        env.storage()
            .persistent()
            .remove(&DataKey::StatsEpoch(user.clone()));
        env.storage()
            .persistent()
            .remove(&DataKey::PendingReward(user.clone()));

        // 2. Erase top-list presence (both forward and reverse keys), then
        //    compact the gap so update_top_players never sees a hole.
        if let Some(slot) = slot_opt {
            Self::clear_top_slot(&env, slot);
            Self::repair_top_index(&env);
        }

        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        env.events().publish(
            (Symbol::new(&env, "player_removed"), admin),
            user,
        );
        Ok(())
    }

    /// Permanently ban a player. Banning:
    ///   * Removes them from the top list and erases their stats (same as
    ///     `remove_player`).
    ///   * Writes a `BannedPlayer` flag that causes every future accrual path
    ///     (`add_pts`, `reward`, `reward_bonus`, `add_bonus_pts`,
    ///     `queue_reward`, `queue_bonus_reward`, `claim_pending_rewards`) to
    ///     return `PlayerBanned` immediately.
    ///
    /// Calling `ban_player` on an already-banned address is idempotent: it
    /// re-confirms the flag, re-removes any residual stats, and returns Ok.
    /// Banning an unknown address simply records the ban (idempotent — no
    /// error).
    pub fn ban_player(
        env: Env,
        admin: Address,
        user: Address,
    ) -> Result<(), LeaderboardError> {
        Self::require_admin(&env, &admin)?;

        // Erase any residual state (stats, pending rewards, top-list slot).
        env.storage()
            .persistent()
            .remove(&DataKey::Stats(user.clone()));
        env.storage()
            .persistent()
            .remove(&DataKey::StatsEpoch(user.clone()));
        env.storage()
            .persistent()
            .remove(&DataKey::PendingReward(user.clone()));

        let count = Self::top_count(&env);
        if let Some(slot) = Self::resolved_slot(&env, &user, count) {
            Self::clear_top_slot(&env, slot);
            Self::repair_top_index(&env);
        }

        // Set the persistent ban flag.
        let ban_key = DataKey::BannedPlayer(user.clone());
        env.storage().persistent().set(&ban_key, &true);
        env.storage()
            .persistent()
            .extend_ttl(&ban_key, TTL_BUMP, TTL_HIGH);

        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        env.events().publish(
            (Symbol::new(&env, "player_banned"), admin),
            user,
        );
        Ok(())
    }

    /// Reset a player's points to zero while keeping their win/loss history.
    /// The player is removed from the top list; their stats record is zeroed
    /// rather than deleted so their bet history is preserved. Any pending
    /// reward is erased so the reset cannot be undone by a later claim.
    ///
    /// Works for an unranked player who has a Stats record (e.g. a low scorer
    /// kept out of a full list). Returns `PlayerNotFound` when the address has
    /// never accrued points and is not in the top list.
    pub fn reset_player(
        env: Env,
        admin: Address,
        user: Address,
    ) -> Result<(), LeaderboardError> {
        Self::require_admin(&env, &admin)?;

        let stats_key = DataKey::Stats(user.clone());
        let stored_opt: Option<StoredStats> =
            env.storage().persistent().get(&stats_key);

        let count = Self::top_count(&env);
        let slot_opt = Self::resolved_slot(&env, &user, count);

        if stored_opt.is_none() && slot_opt.is_none() {
            return Err(LeaderboardError::PlayerNotFound);
        }

        // Zero out the points; preserve won/lost/bonus bet counters.
        let mut stored = stored_opt.unwrap_or_else(StoredStats::zero);
        stored.points = 0;
        env.storage().persistent().set(&stats_key, &stored);
        env.storage().persistent().extend_ttl(&stats_key, TTL_BUMP, TTL_HIGH);

        // Update the epoch stamp so the zeroed score isn't accidentally decayed
        // further from a stale baseline.
        let epoch_key = DataKey::StatsEpoch(user.clone());
        env.storage()
            .persistent()
            .set(&epoch_key, &Self::current_epoch(&env));
        env.storage()
            .persistent()
            .extend_ttl(&epoch_key, TTL_BUMP, TTL_HIGH);

        // Unclaimed points earned before the reset must not be reclaimable.
        env.storage()
            .persistent()
            .remove(&DataKey::PendingReward(user.clone()));

        // Remove from the top list.
        if let Some(slot) = slot_opt {
            Self::clear_top_slot(&env, slot);
            Self::repair_top_index(&env);
        }

        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        env.events().publish(
            (Symbol::new(&env, "player_reset"), admin),
            user,
        );
        Ok(())
    }

    /// Returns true if the player is banned, false otherwise.
    pub fn is_banned(env: Env, user: Address) -> bool {
        env.storage()
            .persistent()
            .get::<_, bool>(&DataKey::BannedPlayer(user))
            .unwrap_or(false)
    }

    // ── Internal: shared auth guards ──────────────────────────────────────────

    fn require_admin(env: &Env, admin: &Address) -> Result<(), LeaderboardError> {
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(LeaderboardError::NotInitialized)?;
        if *admin != stored {
            return Err(LeaderboardError::NotAdmin);
        }
        admin.require_auth();
        Ok(())
    }

    fn require_not_banned(env: &Env, user: &Address) -> Result<(), LeaderboardError> {
        if env
            .storage()
            .persistent()
            .get::<_, bool>(&DataKey::BannedPlayer(user.clone()))
            .unwrap_or(false)
        {
            return Err(LeaderboardError::PlayerBanned);
        }
        Ok(())
    }

    #[inline]
    fn require_not_paused(env: &Env) -> Result<(), LeaderboardError> {
        if env.storage().instance().get(&DataKey::Paused).unwrap_or(false) {
            return Err(LeaderboardError::ContractPaused);
        }
        Ok(())
    }

    #[inline]
    fn require_market_contract(env: &Env, caller: &Address) -> Result<(), LeaderboardError> {
        let mkt: Address = env
            .storage()
            .instance()
            .get(&DataKey::MarketContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if *caller != mkt {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        Ok(())
    }

    #[inline]
    fn require_referral_contract(env: &Env, caller: &Address) -> Result<(), LeaderboardError> {
        let ref_: Address = env
            .storage()
            .instance()
            .get(&DataKey::ReferralContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if *caller != ref_ {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        Ok(())
    }

    fn write_token_contract(
        env: &Env,
        admin: &Address,
        token: &Address,
    ) -> Result<(), LeaderboardError> {
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(LeaderboardError::NotInitialized)?;
        if *admin != stored {
            return Err(LeaderboardError::NotAdmin);
        }
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::TokenContract, token);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    // Issue #84: check pulse_token's reported ABI version before invoking mint.
    fn require_compatible_token(env: &Env, token: &Address) -> Result<(), LeaderboardError> {
        let version: u32 =
            env.invoke_contract(token, &Symbol::new(env, "interface_version"), vec![&env]);
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
                &env,
                this.into_val(env),
                user.into_val(env),
                tokens.into_val(env),
            ],
        );
        Ok(())
    }

    // ── Internal: stats (decay-aware) ─────────────────────────────────────────

    fn load_stored(env: &Env, user: &Address) -> StoredStats {
        env.storage()
            .persistent()
            .get(&DataKey::Stats(user.clone()))
            .unwrap_or_else(StoredStats::zero)
    }

    fn save_stored(env: &Env, user: &Address, s: &StoredStats) {
        let key = DataKey::Stats(user.clone());
        env.storage().persistent().set(&key, s);
        env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
    }

    /// A player's stats brought forward to the current epoch for a write. The
    /// value written back is stamped as current by `commit_stats`.
    fn stats_for_update(env: &Env, user: &Address) -> StoredStats {
        let mut s = Self::load_stored(env, user);
        if s.points != 0 {
            let now = Self::current_epoch(env);
            let written_at: u32 = env
                .storage()
                .persistent()
                .get(&DataKey::StatsEpoch(user.clone()))
                .unwrap_or(now);
            s.points = Self::decay(s.points, now.saturating_sub(written_at));
        }
        s
    }

    /// Persist stats, stamping the epoch they are expressed in.
    fn commit_stats(env: &Env, user: &Address, s: &StoredStats) {
        Self::save_stored(env, user, s);
        let epoch_key = DataKey::StatsEpoch(user.clone());
        env.storage()
            .persistent()
            .set(&epoch_key, &Self::current_epoch(env));
        env.storage()
            .persistent()
            .extend_ttl(&epoch_key, TTL_BUMP, TTL_HIGH);
    }

    /// A player's stats brought forward to the current epoch. Read-only.
    fn decayed_stats(env: &Env, user: &Address) -> PlayerStats {
        let s = Self::load_stored(env, user);
        if s.points == 0 {
            return s.to_player_stats();
        }
        let now = Self::current_epoch(env);
        let written_at: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::StatsEpoch(user.clone()))
            .unwrap_or(now);
        let mut out = s.to_player_stats();
        out.points = Self::decay(s.points, now.saturating_sub(written_at));
        out
    }

    fn credit_points(env: &Env, user: &Address, pts: u64, is_won: Option<bool>) {
        let mut s = Self::stats_for_update(env, user);
        s.points += pts;
        match is_won {
            Some(true) => s.won_bets += 1,
            Some(false) => s.lost_bets += 1,
            None => {}
        }
        Self::commit_stats(env, user, &s);
        Self::update_top_players(env, user.clone(), s.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        env.events().publish(
            (Symbol::new(&env, "leaderboard_updated"), user.clone()),
            (s.points, s.won_bets, s.lost_bets),
        );
    }

    fn credit_bonus(env: &Env, user: &Address, pts: u64) {
        let mut s = Self::stats_for_update(env, user);
        s.points += pts;
        s.bonus_bets += 1; // Issue #64: count bonus award without touching won/lost
        Self::commit_stats(env, user, &s);
        Self::update_top_players(env, user.clone(), s.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        env.events().publish(
            (Symbol::new(&env, "leaderboard_updated"), user.clone()),
            (s.points, s.bonus_bets),
        );
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
        let mut pending: PendingReward = env.storage().persistent().get(&key).unwrap_or(
            PendingReward {
                points: 0,
                tokens: 0,
                won_delta: 0,
                lost_delta: 0,
                bet_delta: 0,
            },
        );
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

    // ── Internal: decay ───────────────────────────────────────────────────────

    /// Which decay period the ledger is currently in.
    fn current_epoch(env: &Env) -> u32 {
        env.ledger().sequence() / DECAY_PERIOD_LEDGERS
    }

    /// Apply `periods` worth of decay to a score. Iterated rather than
    /// closed-form because the contract has no float and integer flooring must
    /// happen at each step for the result to be self-consistent.
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

    /// A top-list entry's score as of now. Pure arithmetic — the epoch rides
    /// on the entry, so this costs no ledger read.
    fn entry_points_now(env: &Env, entry: &PlayerEntry) -> u64 {
        let now = Self::current_epoch(env);
        Self::decay(entry.points, now.saturating_sub(entry.epoch))
    }

    // ── Internal: maintain a persistent sorted top list ──────────────────────

    fn top_count(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0)
    }

    fn forward_entry(env: &Env, slot: u32) -> Option<PlayerEntry> {
        env.storage().persistent().get(&DataKey::TopPlayerAt(slot))
    }

    /// Write `TopPlayerAt(slot)` and `TopPlayerSlot(address)` together, and
    /// bump both TTLs. This is the only way the two keys are created/updated.
    fn set_top_slot(env: &Env, slot: u32, entry: &PlayerEntry) {
        let at_key = DataKey::TopPlayerAt(slot);
        env.storage().persistent().set(&at_key, entry);
        env.storage()
            .persistent()
            .extend_ttl(&at_key, TTL_BUMP, TTL_HIGH);
        let slot_key = DataKey::TopPlayerSlot(entry.address.clone());
        env.storage().persistent().set(&slot_key, &slot);
        env.storage()
            .persistent()
            .extend_ttl(&slot_key, TTL_BUMP, TTL_HIGH);
    }

    /// Remove both sides of the mapping for `slot`. No-op if the forward entry
    /// is already gone (TTL); still drops a leftover reverse key.
    fn clear_top_slot(env: &Env, slot: u32) {
        if let Some(old) = Self::forward_entry(env, slot) {
            env.storage()
                .persistent()
                .remove(&DataKey::TopPlayerSlot(old.address));
        }
        env.storage()
            .persistent()
            .remove(&DataKey::TopPlayerAt(slot));
    }

    /// Resolve a user's slot only if the reverse lookup is consistent with the
    /// forward index. Stale reverse keys are deleted. If the reverse key is
    /// missing, scan the forward index to recover from `TopPlayerSlot` TTL
    /// expiry (avoids inserting a duplicate).
    fn resolved_slot(env: &Env, user: &Address, count: u32) -> Option<u32> {
        if let Some(slot) = env
            .storage()
            .persistent()
            .get::<_, u32>(&DataKey::TopPlayerSlot(user.clone()))
        {
            match Self::forward_entry(env, slot) {
                Some(entry) if entry.address == *user => return Some(slot),
                _ => {
                    env.storage()
                        .persistent()
                        .remove(&DataKey::TopPlayerSlot(user.clone()));
                }
            }
        }
        for i in 0..count {
            if let Some(entry) = Self::forward_entry(env, i) {
                if entry.address == *user {
                    Self::set_top_slot(env, i, &entry);
                    return Some(i);
                }
            }
        }
        None
    }

    /// Like `resolved_slot`, but returns the full entry as well.
    fn top_slot_entry(env: &Env, user: &Address) -> Option<(u32, PlayerEntry)> {
        if let Some(slot) = env
            .storage()
            .persistent()
            .get::<_, u32>(&DataKey::TopPlayerSlot(user.clone()))
        {
            match Self::forward_entry(env, slot) {
                Some(entry) if entry.address == *user => return Some((slot, entry)),
                _ => {
                    env.storage()
                        .persistent()
                        .remove(&DataKey::TopPlayerSlot(user.clone()));
                }
            }
        }
        let count = Self::top_count(env);
        for i in 0..count {
            if let Some(entry) = Self::forward_entry(env, i) {
                if entry.address == *user {
                    Self::set_top_slot(env, i, &entry);
                    return Some((i, entry));
                }
            }
        }
        None
    }

    /// Compact holes and rewrite every reverse lookup from surviving forward
    /// entries. Returns the new live count.
    ///
    /// Robust across sequential removals: each call is self-contained — it
    /// reads the whole index and rebuilds a dense, consistent index from
    /// whatever survives, so removing players one after another cannot leave a
    /// stale `TopPlayerCount`, dangling `TopPlayerSlot`, or a hole that a
    /// later `update_top_players` would trip over.
    fn repair_top_index(env: &Env) -> u32 {
        let count = Self::top_count(env);
        let mut write: u32 = 0;
        for read in 0..count {
            if let Some(entry) = Self::forward_entry(env, read) {
                Self::set_top_slot(env, write, &entry);
                if write != read {
                    env.storage()
                        .persistent()
                        .remove(&DataKey::TopPlayerAt(read));
                }
                write += 1;
            } else {
                env.storage()
                    .persistent()
                    .remove(&DataKey::TopPlayerAt(read));
            }
        }
        env.storage()
            .instance()
            .set(&DataKey::TopPlayerCount, &write);
        Self::recompute_min(env);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        write
    }

    /// If any live slot in `0..count` is missing, compact the index and return
    /// the corrected count.
    fn ensure_consistent(env: &Env, count: u32) -> u32 {
        for i in 0..count {
            if Self::forward_entry(env, i).is_none() {
                return Self::repair_top_index(env);
            }
        }
        count
    }

    /// Monotonic FIFO sequence counter — fed into `PlayerEntry::seq` so that,
    /// when several players share the minimum score, the *oldest* (smallest
    /// seq) is evicted first instead of whichever sits at the lowest slot.
    fn next_seq(env: &Env) -> u64 {
        let s: u64 = env
            .storage()
            .instance()
            .get(&DataKey::SeqCounter)
            .unwrap_or(0);
        env.storage().instance().set(&DataKey::SeqCounter, &(s + 1));
        s
    }

    /// Recompute the weakest live entry (decayed points, oldest seq on ties)
    /// and cache it in MinPoints/MinSlot.
    fn recompute_min(env: &Env) {
        let count = Self::top_count(env);
        if count == 0 {
            env.storage().instance().set(&DataKey::MinPoints, &0_u64);
            env.storage().instance().set(&DataKey::MinSlot, &0_u32);
            return;
        }
        let mut min_slot: u32 = 0;
        let mut min_points: u64 = u64::MAX;
        let mut min_seq: u64 = u64::MAX;
        let mut found = false;
        for slot in 0..count {
            if let Some(e) = Self::forward_entry(env, slot) {
                let pts = Self::entry_points_now(env, &e);
                if !found || pts < min_points || (pts == min_points && e.seq < min_seq) {
                    min_points = pts;
                    min_slot = slot;
                    min_seq = e.seq;
                    found = true;
                }
            }
        }
        if found {
            env.storage()
                .instance()
                .set(&DataKey::MinPoints, &min_points);
            env.storage().instance().set(&DataKey::MinSlot, &min_slot);
        }
    }

    /// Bubbles a (possibly new) entry up from `slot` until the list is
    /// descending again, comparing decayed values (issue #69). Forward and
    /// reverse indexes are written together so the pair cannot drift apart;
    /// TTLs are not bumped per swap to keep the write footprint bounded.
    fn bubble_up(env: &Env, entry: &PlayerEntry, mut slot: u32) {
        let mut steps = 0;
        while slot > 0 && steps < MAX_BUBBLE_STEPS {
            steps += 1;
            let prev: Option<PlayerEntry> =
                env.storage().persistent().get(&DataKey::TopPlayerAt(slot - 1));
            match prev {
                Some(prev)
                    if Self::entry_points_now(env, &prev) < Self::entry_points_now(env, entry) => {
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerAt(slot - 1), entry);
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerAt(slot), &prev);
                    env.storage().persistent().set(
                        &DataKey::TopPlayerSlot(entry.address.clone()),
                        &(slot - 1),
                    );
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerSlot(prev.address.clone()), &slot);
                    slot -= 1;
                }
                _ => break,
            }
        }
    }

    /// Insert or update a player's place in the top list after a point change.
    /// - Already listed: update points/epoch in place, bubble up, refresh min.
    /// - Not listed, room left: append and bubble.
    /// - Not listed, list full: evict the weakest live entry (decayed, oldest
    ///   seq on ties) when the newcomer is at least as strong.
    fn update_top_players(env: &Env, user: Address, new_points: u64) {
        let count = Self::ensure_consistent(env, Self::top_count(env));

        if let Some((slot, mut entry)) = Self::top_slot_entry(env, &user) {
            entry.points = new_points;
            entry.epoch = Self::current_epoch(env);
            Self::set_top_slot(env, slot, &entry);
            Self::bubble_up(env, &entry, slot);
            Self::recompute_min(env);
            return;
        }

        if count < MAX_TOP_PLAYERS {
            let slot = count;
            let entry = PlayerEntry {
                address: user.clone(),
                points: new_points,
                epoch: Self::current_epoch(env),
                seq: Self::next_seq(env),
            };
            Self::set_top_slot(env, slot, &entry);
            env.storage()
                .instance()
                .set(&DataKey::TopPlayerCount, &(slot + 1));
            Self::bubble_up(env, &entry, slot);
            if slot + 1 == MAX_TOP_PLAYERS {
                Self::recompute_min(env);
            }
            return;
        }

        // Full list: evict the weakest live entry when the newcomer is at
        // least as strong (equal scores displace — FIFO among ties).
        Self::recompute_min(env);
        let min_slot: u32 = env.storage().instance().get(&DataKey::MinSlot).unwrap_or(0);
        let Some(min_entry) = Self::forward_entry(env, min_slot) else {
            Self::repair_top_index(env);
            Self::update_top_players(env, user, new_points);
            return;
        };
        if new_points < Self::entry_points_now(env, &min_entry) {
            return;
        }
        env.storage()
            .persistent()
            .remove(&DataKey::TopPlayerSlot(min_entry.address.clone()));
        let new_entry = PlayerEntry {
            address: user.clone(),
            points: new_points,
            epoch: Self::current_epoch(env),
            seq: Self::next_seq(env),
        };
        Self::set_top_slot(env, min_slot, &new_entry);
        Self::bubble_up(env, &new_entry, min_slot);
        Self::recompute_min(env);
    }
}

#[cfg(test)]
mod decay_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod ttl_tests;
#[cfg(test)]
mod admin_tests;