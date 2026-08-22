#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, vec, Address, BytesN, Env, IntoVal,
    Symbol, Val, Vec,
};

pub const MAX_TOP_PLAYERS: u32 = 50;
const MAX_PAGE_SIZE: u32 = MAX_TOP_PLAYERS;
/// Bound on bubble swaps per write so a fill-to-capacity burst of ascending
/// inserts cannot blow the per-transaction write-footprint limit. Read paths
/// re-sort on decayed values, so ordering converges without full bubbles.
const MAX_BUBBLE_STEPS: u32 = 4;
/// Issue #69: one decay period is ~1 week of ledgers (at ~5s per ledger).
const DECAY_PERIOD_LEDGERS: u32 = 120_960;
/// Each period a score keeps DECAY_RETAIN_NUM/DECAY_RETAIN_DEN of its value.
const DECAY_RETAIN_NUM: u64 = 9;
const DECAY_RETAIN_DEN: u64 = 10;
/// A score fully stale (decayed to zero) after this many periods (~2 years):
/// derived from TTL_HIGH so a score cannot outlive its storage entry.
const DECAY_ZERO_AFTER_PERIODS: u32 = TTL_HIGH / DECAY_PERIOD_LEDGERS;
const TTL_BUMP: u32 = 3_153_600;
const TTL_HIGH: u32 = 6_307_200;

/// Rank returned by `get_rank` for a player who is not in the top list.
/// (`MAX_TOP_PLAYERS + 1`) so that an unranked player never sorts above a
/// real position and the "unranked" state is unambiguous (issue #91). Callers
/// should treat `rank > MAX_TOP_PLAYERS` as "not ranked".
pub const UNRANKED_RANK: u32 = MAX_TOP_PLAYERS + 1;

// Issue #84: bump whenever a function signature, argument order, or return
// type that a caller relies on changes. Callers pin the version they were
// built against and check it before invoking, so an incompatible upgrade
// fails loudly instead of misbehaving.
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
    /// against (issue #84).
    IncompatibleInterface = 7,
    /// reward()/reward_bonus() called with tokens > 0 but no TokenContract
    /// has been set via set_token_contract.
    TokenNotConfigured = 8,
}

// OPT: single key per user. `points` carries the decay epoch it was written
// in; won/lost/bonus counters are lifetime totals (issue #69/#64).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    MarketContract,
    ReferralContract,
    // Lever G: token address so reward() can mint PULSE internally — one
    // cross-call from the market instead of two (add_pts + mint).
    TokenContract,
    Stats(Address),
    PendingReward(Address), // issue #73: deferred reward queue
    TopPlayerAt(u32),
    TopPlayerCount,
    TopPlayerSlot(Address), // reverse lookup: address -> slot
    TopPlayerSeqAt(u32),    // u64 — FIFO insertion sequence for the player at a slot
    SeqCounter,             // u64 — monotonic counter feeding TopPlayerSeqAt
    MinPoints,              // u64 — decayed points of the weakest entry in the top list
    MinSlot,                // u32 — slot index of that weakest entry
    Paused,
}

// External-facing stats struct (ABI stable)
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct PlayerStats {
    pub points: u64,
    pub total_bets: u32,
    pub won_bets: u32,
    pub lost_bets: u32,
}

// OPT: PlayerEntry embeds points directly (avoids a Stats read during sort)
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerEntry {
    pub address: Address,
    pub points: u64,
    /// Issue #69: the decay epoch `points` is expressed in.
    ///
    /// Carrying it on the entry rather than in a side key is what keeps decay
    /// affordable: comparing two entries needs no extra ledger reads, so the
    /// eviction and ordering paths stay inside the 100-entry transaction
    /// footprint that a per-entry lookup would have blown.
    pub epoch: u32,
}

/// Internal per-user record. `points` decays; the counters are lifetime totals
/// derived into `PlayerStats::total_bets` at read time as
/// won_bets + lost_bets + bonus_bets so bonus-only users are never invisible
/// (issues #19/#64) and the three counters can never drift apart.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredStats {
    pub points: u64,
    pub epoch: u32, // the decay epoch `points` is expressed in
    pub won_bets: u32,
    pub lost_bets: u32,
    pub bonus_bets: u32,
}

impl StoredStats {
    fn zero() -> Self {
        StoredStats {
            points: 0,
            epoch: 0,
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

/// A deferred reward (issue #73): queued by the market at claim time and
/// materialised when the user calls claim_pending_rewards.
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
        env.storage().instance().set(&DataKey::TopPlayerCount, &0_u32);
        env.storage().instance().set(&DataKey::MinPoints, &0_u64);
        env.storage().instance().set(&DataKey::MinSlot, &0_u32);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// The cross-contract ABI version this deployment implements (issue #84).
    pub fn interface_version(_env: Env) -> u32 {
        INTERFACE_VERSION
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
        Self::write_token_contract(&env, token);
        Ok(())
    }

    /// Halt point/reward accrual in an emergency. Admin only. View functions
    /// keep working so the frontend can still read state.
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
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// Resume point/reward accrual. Admin only.
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
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get::<_, bool>(&DataKey::Paused)
            .unwrap_or(false)
    }

    // ── Immediate accrual paths ──────────────────────────────────────────────

    /// Called by the market contract when a bet is placed: lifetime win/loss
    /// accounting + points, no token minting.
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
        Self::credit_points(&env, &user, pts, Some(is_won));
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// One-shot immediate reward: points + win/loss accounting + PULSE mint
    /// in a single cross-contract hop (Lever G).
    pub fn reward(
        env: Env,
        caller: Address,
        user: Address,
        points: u64,
        tokens: i128,
        is_won: bool,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        Self::require_market_contract(&env, &caller)?;
        caller.require_auth();
        if points == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }
        Self::credit_points(&env, &user, points, Some(is_won));
        Self::mint_tokens(&env, &user, tokens)?;
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// Called by the referral contract for welcome / per-bet referral bonuses.
    /// Increments bonus_bets (not won/lost) so total_bets stays accurate and
    /// won_bets/lost_bets are never polluted with non-bet activity.
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
        Self::credit_bonus(&env, &user, pts);
        Self::mint_tokens(&env, &user, tokens)?;
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// Legacy: called by the referral contract to award bonus points.
    /// Prefer reward_bonus() for new integrations (adds token minting).
    pub fn add_bonus_pts(
        env: Env,
        caller: Address,
        user: Address,
        pts: u64,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        Self::require_referral_contract(&env, &caller)?;
        caller.require_auth();
        Self::credit_bonus(&env, &user, pts);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// Notification hook from the market that a bet was recorded. Only auth
    /// and caller identity are verified — no accrual happens here.
    pub fn record_bet(
        env: Env,
        caller: Address,
        _user: Address,
    ) -> Result<(), LeaderboardError> {
        Self::require_market_contract(&env, &caller)?;
        caller.require_auth();
        Ok(())
    }

    // ── Deferred rewards (issue #73) ─────────────────────────────────────────

    /// Queue a reward instead of applying it immediately. A missing, paused,
    /// incompatible, or out-of-budget leaderboard must not be able to roll
    /// back the caller's primary operation, so the queue is written by the
    /// market inside its own failure-tolerant path.
    pub fn queue_reward(
        env: Env,
        caller: Address,
        user: Address,
        points: u64,
        tokens: i128,
        is_won: bool,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        Self::require_market_contract(&env, &caller)?;
        caller.require_auth();
        Self::accumulate_pending(&env, &user, points, tokens, is_won, false);
        Ok(())
    }

    /// Materialise everything queued for `user`: points and counters are
    /// applied (on top of the decayed balance) and queued PULSE is minted.
    /// Permissionless — a user always has standing to collect their own
    /// pending rewards.
    pub fn claim_pending_rewards(env: Env, user: Address) -> Result<(), LeaderboardError> {
        let key = DataKey::PendingReward(user.clone());
        let pending: PendingReward = match env.storage().persistent().get(&key) {
            Some(p) => p,
            None => return Ok(()),
        };
        env.storage().persistent().remove(&key);

        if pending.points > 0 || pending.bet_delta > 0 {
            let mut s = Self::load_stored(&env, &user);
            s.points = Self::decay(s.points, Self::current_epoch(&env).saturating_sub(s.epoch));
            s.epoch = Self::current_epoch(&env);
            s.points += pending.points;
            s.won_bets += pending.won_delta;
            s.lost_bets += pending.lost_delta;
            Self::save_stored(&env, &user, &s);
            Self::update_top_players(&env, user.clone(), s.points);
        }

        if pending.tokens > 0 {
            Self::mint_tokens(&env, &user, pending.tokens)?;
        }
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn get_pending_reward(env: Env, user: Address) -> Option<PendingReward> {
        env.storage().persistent().get(&DataKey::PendingReward(user))
    }

    // ── Views ────────────────────────────────────────────────────────────────

    /// Points as of *now*, with decay applied (issue #69). This is a read —
    /// it never writes the decayed value back; the next accrual does that.
    pub fn get_points(env: Env, user: Address) -> u64 {
        Self::decayed_stats(&env, &user).points
    }

    /// Stats as of *now*. `points` carries decay; the activity counters are
    /// lifetime totals and are deliberately left alone (issue #69 is about
    /// ranking freshness, not rewriting a player's history).
    pub fn get_stats(env: Env, user: Address) -> PlayerStats {
        Self::decayed_stats(&env, &user)
    }

    /// Returns the number of players currently in the top list (≤ MAX_TOP_PLAYERS).
    pub fn get_player_count(env: Env) -> u32 {
        Self::top_count(&env)
    }

    pub fn get_top_player_count(env: Env) -> u32 {
        Self::top_count(&env)
    }

    /// Page through the top list ranked on *current* (decayed) scores.
    pub fn get_top_players(env: Env, offset: u32, page_size: u32) -> Vec<PlayerEntry> {
        let count = Self::top_count(&env);

        if offset >= count || page_size == 0 {
            return Vec::new(&env);
        }

        let page_size = page_size.min(MAX_PAGE_SIZE);

        // Issue #69: the reported order is computed here, on decayed values,
        // rather than trusted from storage order. Each entry carries its own
        // epoch, so this costs no reads beyond the slots themselves — and it
        // means a bounded bubble on the write path never shows up as a wrong
        // ranking to a reader. Points are normalised to the current epoch, so
        // a caller sees a score together with the epoch it is current as of.
        let now = Self::current_epoch(&env);
        let mut ranked: Vec<PlayerEntry> = Vec::new(&env);
        for i in 0..count {
            if let Some(mut entry) = env
                .storage()
                .persistent()
                .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(i))
            {
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
                let a = ranked.get(i).unwrap().clone();
                let b = ranked.get(max_idx).unwrap().clone();
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

    /// Rank is the 1-based position among *current* (decayed) scores:
    /// 1 + number of players strictly ahead. Players outside the top list get
    /// UNRANKED_RANK, never 0, so an unranked player can never sort above
    /// (numerically lower than) a real position (issue #91). The reverse
    /// lookup is validated against the forward entry first so an orphaned or
    /// stale TopPlayerSlot can never produce a fake rank.
    pub fn get_rank(env: Env, user: Address) -> u32 {
        let count = Self::top_count(&env);
        let slot = match Self::resolved_slot(&env, &user, count) {
            Some(s) => s,
            None => return UNRANKED_RANK,
        };
        let mine = Self::forward_entry(&env, slot)
            .map(|e| Self::entry_points_now(&env, &e))
            .unwrap_or(0);

        let now = Self::current_epoch(&env);
        let mut rank: u32 = 1;
        for i in 0..count {
            if i == slot {
                continue;
            }
            if let Some(e) = env
                .storage()
                .persistent()
                .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(i))
            {
                if Self::decay(e.points, now.saturating_sub(e.epoch)) > mine {
                    rank += 1;
                }
            }
        }
        rank
    }

    /// Points of the weakest entry currently in the top list, decayed to now.
    pub fn get_min_points(env: Env) -> u64 {
        let slot: u32 = env.storage().instance().get(&DataKey::MinSlot).unwrap_or(0);
        match env
            .storage()
            .persistent()
            .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(slot))
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
        env.storage().instance().get(&DataKey::MinSlot).unwrap_or(0)
    }

    /// Rebuild `TopPlayerSlot` from live `TopPlayerAt` entries, compact holes
    /// left by TTL expiry, and refresh the min cache. Anyone may call this
    /// (keeper/repair); it only writes keys that restore the index invariant.
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
        let count = Self::top_count(&env);
        if let Some(slot) = env
            .storage()
            .persistent()
            .get::<_, u32>(&DataKey::TopPlayerSlot(user.clone()))
        {
            if Self::forward_entry(&env, slot).is_some() && slot < count {
                env.storage()
                    .persistent()
                    .extend_ttl(&DataKey::TopPlayerAt(slot), TTL_BUMP, TTL_HIGH);
                env.storage().persistent().extend_ttl(
                    &DataKey::TopPlayerSlot(user),
                    TTL_BUMP,
                    TTL_HIGH,
                );
            }
        }
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
    }

    // ── Internal: accrual ────────────────────────────────────────────────────

    /// Add settled-bet points. The stored balance is brought forward to the
    /// current epoch first, so accrual always builds on what the score has
    /// *become*, not on what it once was (issue #69).
    fn credit_points(env: &Env, user: &Address, pts: u64, is_won: Option<bool>) {
        let mut s = Self::load_stored(env, user);
        s.points = Self::decay(s.points, Self::current_epoch(env).saturating_sub(s.epoch));
        s.epoch = Self::current_epoch(env);
        s.points += pts;
        match is_won {
            Some(true) => s.won_bets += 1,
            Some(false) => s.lost_bets += 1,
            None => {}
        }
        Self::save_stored(env, user, &s);
        Self::update_top_players(env, user.clone(), s.points);
        env.events().publish(
            (Symbol::new(env, "leaderboard_updated"), user.clone()),
            (s.points, s.won_bets, s.lost_bets),
        );
    }

    /// Add referral/welcome bonus points. Counts as activity (bonus_bets) but
    /// never as a won or lost bet (issue #64).
    fn credit_bonus(env: &Env, user: &Address, pts: u64) {
        let mut s = Self::load_stored(env, user);
        s.points = Self::decay(s.points, Self::current_epoch(env).saturating_sub(s.epoch));
        s.epoch = Self::current_epoch(env);
        s.points += pts;
        s.bonus_bets += 1;
        Self::save_stored(env, user, &s);
        Self::update_top_players(env, user.clone(), s.points);
        env.events().publish(
            (Symbol::new(env, "leaderboard_updated"), user.clone()),
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

    // ── Internal: maintain a persistent sorted top list ──────────────────────

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

    fn load_stored(env: &Env, user: &Address) -> StoredStats {
        env.storage()
            .persistent()
            .get(&DataKey::Stats(user.clone()))
            .unwrap_or_else(|| {
                StoredStats {
                    epoch: Self::current_epoch(env),
                    ..StoredStats::zero()
                }
            })
    }

    fn save_stored(env: &Env, user: &Address, s: &StoredStats) {
        env.storage()
            .persistent()
            .set(&DataKey::Stats(user.clone()), s);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Stats(user.clone()), TTL_BUMP, TTL_HIGH);
    }

    fn top_count(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0)
    }

    fn forward_entry(env: &Env, slot: u32) -> Option<PlayerEntry> {
        env.storage().persistent().get(&DataKey::TopPlayerAt(slot))
    }

    /// Remove both sides of the mapping for `slot`. No-op if the forward
    /// entry is already gone (TTL); still drops a leftover reverse key when
    /// the forward entry is present.
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

    /// Resolve a user's slot only if the reverse lookup is consistent with
    /// the forward index. Stale reverse keys are deleted. If the reverse key
    /// is missing, scan the forward index to recover from `TopPlayerSlot` TTL
    /// (avoids inserting a duplicate).
    fn resolved_slot(env: &Env, user: &Address, count: u32) -> Option<u32> {
        if let Some(slot) = env
            .storage()
            .persistent()
            .get::<_, u32>(&DataKey::TopPlayerSlot(user.clone()))
        {
            match Self::forward_entry(env, slot) {
                Some(entry) if entry.address == *user && slot < count => return Some(slot),
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

    /// Bubbles a (possibly new) entry up from `slot` until the list is
    /// descending again, comparing decayed scores. Forward and reverse
    /// indexes are always written together; FIFO sequences travel with the
    /// player (not the slot) and the cached minimum is re-pointed at the same
    /// player when it moves.
    fn bubble_up(env: &Env, entry: &PlayerEntry, mut slot: u32) {
        // Bounded: a fill-to-capacity burst of ascending inserts would
        // otherwise rewrite O(n) slots per call and blow the transaction
        // write-footprint limit. Read paths re-sort on decayed values anyway.
        let mut steps = 0_u32;
        while slot > 0 && steps < MAX_BUBBLE_STEPS {
            steps += 1;
            let prev: Option<PlayerEntry> =
                env.storage().persistent().get(&DataKey::TopPlayerAt(slot - 1));
            match prev {
                Some(prev)
                    if Self::entry_points_now(env, &prev)
                        < Self::entry_points_now(env, entry) =>
                {
                    let key_hi = DataKey::TopPlayerAt(slot - 1);
                    let key_lo = DataKey::TopPlayerAt(slot);
                    env.storage().persistent().set(&key_hi, entry);
                    env.storage().persistent().set(&key_lo, &prev);
                    env.storage().persistent().set(
                        &DataKey::TopPlayerSlot(entry.address.clone()),
                        &(slot - 1),
                    );
                    env.storage().persistent().set(
                        &DataKey::TopPlayerSlot(prev.address.clone()),
                        &slot,
                    );
                    // FIFO sequences travel with the player, not the slot.
                    let seq_hi = Self::seq_at(env, slot - 1);
                    let seq_lo = Self::seq_at(env, slot);
                    let sk_hi = DataKey::TopPlayerSeqAt(slot - 1);
                    let sk_lo = DataKey::TopPlayerSeqAt(slot);
                    env.storage().persistent().set(&sk_hi, &seq_lo);
                    env.storage().persistent().set(&sk_lo, &seq_hi);
                    // Keep the cached minimum pointing at the same PLAYER.
                    let cached_min_slot: u32 =
                        env.storage().instance().get(&DataKey::MinSlot).unwrap_or(slot);
                    if slot == cached_min_slot {
                        env.storage().instance().set(&DataKey::MinSlot, &(slot - 1));
                    } else if slot - 1 == cached_min_slot {
                        env.storage().instance().set(&DataKey::MinSlot, &slot);
                    }
                    slot -= 1;
                }
                _ => break,
            }
        }
    }

    fn seq_at(env: &Env, s: u32) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::TopPlayerSeqAt(s))
            .unwrap_or(0)
    }

    /// FIFO age stamp: assign the next monotonically increasing sequence to a
    /// slot so equal-score ties are evicted oldest-first (issue #25 / #70).
    fn stamp_seq(env: &Env, s: u32) {
        let counter: u64 = env
            .storage()
            .instance()
            .get(&DataKey::SeqCounter)
            .unwrap_or(0);
        let seq = counter + 1;
        env.storage().instance().set(&DataKey::SeqCounter, &seq);
        let key = DataKey::TopPlayerSeqAt(s);
        env.storage().persistent().set(&key, &seq);
        env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
    }

    /// Refresh the cached minimum over the bounded list, comparing decayed
    /// scores. Among equal minima the OLDEST insertion (lowest sequence)
    /// wins MinSlot, so tie eviction is deterministic FIFO (issue #25/#70).
    /// Sequences are read lazily — only when a tie is actually met — keeping
    /// the per-call ledger-read footprint low.
    fn refresh_min(env: &Env, count: u32) {
        if count == 0 {
            env.storage().instance().set(&DataKey::MinPoints, &0_u64);
            env.storage().instance().set(&DataKey::MinSlot, &0_u32);
            return;
        }
        let mut min_pts = u64::MAX;
        let mut min_slot: u32 = 0;
        let mut min_seq: Option<u64> = None;
        for i in 0..count {
            if let Some(e) = Self::forward_entry(env, i) {
                let pts = Self::entry_points_now(env, &e);
                if pts < min_pts {
                    min_pts = pts;
                    min_slot = i;
                    min_seq = None;
                } else if pts == min_pts {
                    let cur = *min_seq.get_or_insert_with(|| Self::seq_at(env, min_slot));
                    let seq = Self::seq_at(env, i);
                    if seq < cur {
                        min_slot = i;
                        min_seq = Some(seq);
                    }
                }
            }
        }
        env.storage().instance().set(&DataKey::MinPoints, &min_pts);
        env.storage().instance().set(&DataKey::MinSlot, &min_slot);
    }

    /// Appends a brand-new entry at `slot`, bumps the count, bubbles it into
    /// place and stamps its FIFO insertion sequence.
    fn insert_new(env: &Env, user: &Address, points: u64, slot: u32) {
        let entry = PlayerEntry {
            address: user.clone(),
            points,
            epoch: Self::current_epoch(env),
        };
        Self::set_top_slot(env, slot, &entry);
        Self::stamp_seq(env, slot);
        env.storage().instance().set(&DataKey::TopPlayerCount, &(slot + 1));

        Self::bubble_up(env, &entry, slot);

        // A full list makes the cached minimum authoritative for eviction.
        if slot + 1 == MAX_TOP_PLAYERS {
            Self::refresh_min(env, Self::top_count(env));
        }
    }

    /// Compact holes and rewrite every reverse lookup from surviving forward
    /// entries. Returns the new live count.
    fn repair_top_index(env: &Env) -> u32 {
        let count = Self::top_count(env);
        let mut write: u32 = 0;
        for read in 0..count {
            if let Some(entry) = Self::forward_entry(env, read) {
                if write != read {
                    Self::set_top_slot(env, write, &entry);
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
        Self::refresh_min(env, write);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        write
    }

    /// Insert or update a player's top-list entry, maintaining descending
    /// order and the cached minimum. Equal-score newcomers displace the
    /// OLDEST tied-at-minimum entry (deterministic FIFO via sequences).
    fn update_top_players(env: &Env, user: Address, new_points: u64) {
        let count = Self::top_count(env);

        // A stale (dead-forward) reverse key proves a hole exists in the list
        // — compact it so `count` stays authoritative before appending.
        let had_reverse = env
            .storage()
            .persistent()
            .has(&DataKey::TopPlayerSlot(user.clone()));

        // Fast path: already ranked — rewrite in place and re-bubble.
        if let Some(slot) = Self::resolved_slot(env, &user, count) {
            let entry = PlayerEntry {
                address: user,
                points: new_points,
                epoch: Self::current_epoch(env),
            };
            Self::set_top_slot(env, slot, &entry);
            Self::bubble_up(env, &entry, slot);

            // Only a change at/into the cached minimum can invalidate it.
            let cached_min_slot: u32 =
                env.storage().instance().get(&DataKey::MinSlot).unwrap_or(0);
            if slot == cached_min_slot || slot + 1 == count {
                Self::refresh_min(env, count);
            }
            return;
        }

        // The user has no live entry. If their reverse key was present but
        // stale, a TTL hole exists — compact the list so the count (and
        // therefore the append slot / eviction target) stays authoritative.
        let count = if had_reverse {
            let c = Self::repair_top_index(env);
            if c < MAX_TOP_PLAYERS {
                Self::insert_new(env, &user, new_points, c);
                return;
            }
            c
        } else {
            count
        };

        if count < MAX_TOP_PLAYERS {
            Self::insert_new(env, &user, new_points, count);
            return;
        }

        // List full: the cached-minimum entry competes with the newcomer. An
        // equal-score incumbent is displaced (FIFO among ties); only strictly
        // weaker newcomers are turned away.
        let min_slot: u32 = env.storage().instance().get(&DataKey::MinSlot).unwrap_or(0);
        let Some(min_entry) = Self::forward_entry(env, min_slot) else {
            // The cached minimum points at an expired hole — compact once and
            // retry with a freshly rebuilt index.
            Self::repair_top_index(env);
            Self::update_top_players(env, user, new_points);
            return;
        };

        if new_points < Self::entry_points_now(env, &min_entry) {
            return;
        }

        Self::clear_top_slot(env, min_slot);

        let new_entry = PlayerEntry {
            address: user,
            points: new_points,
            epoch: Self::current_epoch(env),
        };
        Self::set_top_slot(env, min_slot, &new_entry);
        Self::stamp_seq(env, min_slot);
        Self::bubble_up(env, &new_entry, min_slot);
        Self::refresh_min(env, Self::top_count(env));
    }

    // ── Point decay (issue #69) ───────────────────────────────────────────
    //
    // Scores are time-weighted. Nothing is recomputed on a timer: each
    // stored score carries the epoch it was written in, and the value for a
    // later epoch is derived from it. Writes materialise that; reads apply it
    // on the fly.

    /// Which decay period the ledger is currently in.
    fn current_epoch(env: &Env) -> u32 {
        env.ledger().sequence() / DECAY_PERIOD_LEDGERS
    }

    /// Apply `periods` worth of decay to a score.
    ///
    /// Iterated rather than closed-form because the contract has no float and
    /// integer flooring must happen at each step for the result to be
    /// self-consistent: decaying by `a` then by `b` has to equal decaying by
    /// `a + b`, or a player's stats and their top-list entry — which are
    /// swept on different schedules — would drift apart. The loop is bounded
    /// by DECAY_ZERO_AFTER_PERIODS.
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
    /// on the entry, so this costs no ledger read and is safe to call inside
    /// comparison loops on the write path.
    fn entry_points_now(env: &Env, entry: &PlayerEntry) -> u64 {
        let now = Self::current_epoch(env);
        Self::decay(entry.points, now.saturating_sub(entry.epoch))
    }

    /// A player's stats brought forward to the current epoch. Read-only.
    fn decayed_stats(env: &Env, user: &Address) -> PlayerStats {
        let stored: StoredStats = env
            .storage()
            .persistent()
            .get(&DataKey::Stats(user.clone()))
            .unwrap_or_else(|| StoredStats {
                epoch: Self::current_epoch(env),
                ..StoredStats::zero()
            });
        let points = Self::decay(stored.points, Self::current_epoch(env).saturating_sub(stored.epoch));
        PlayerStats {
            points,
            total_bets: stored.won_bets + stored.lost_bets + stored.bonus_bets,
            won_bets: stored.won_bets,
            lost_bets: stored.lost_bets,
        }
    }

    // ── Internal: auth guards & minting ──────────────────────────────────────

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

    fn require_not_paused(env: &Env) -> Result<(), LeaderboardError> {
        if Self::is_paused(env.clone()) {
            return Err(LeaderboardError::ContractPaused);
        }
        Ok(())
    }

    // Issue #84: check pulse_token's reported ABI version before invoking mint.
    fn require_compatible_token(env: &Env, token: &Address) -> Result<(), LeaderboardError> {
        let version: u32 =
            env.invoke_contract(token, &Symbol::new(env, "interface_version"), vec![env]);
        if version != EXPECTED_TOKEN_INTERFACE_VERSION {
            return Err(LeaderboardError::IncompatibleInterface);
        }
        Ok(())
    }

    fn write_token_contract(env: &Env, token: Address) {
        env.storage().instance().set(&DataKey::TokenContract, &token);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
    }

    fn mint_tokens(env: &Env, user: &Address, tokens: i128) -> Result<(), LeaderboardError> {
        if tokens <= 0 {
            return Ok(());
        }
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
            vec![env, this.into_val(env), user.into_val(env), tokens.into_val(env)],
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
