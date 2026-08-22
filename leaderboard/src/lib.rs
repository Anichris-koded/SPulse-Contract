#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, vec, Address, Env, IntoVal, Symbol, Val,
    Vec,
};

const MAX_TOP_PLAYERS: u32 = 50;
const MAX_PAGE_SIZE: u32 = 20;
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

/// How many slots one call may bubble an entry through.
///
/// Each swap writes four keys (two entries, two reverse lookups), and a
/// transaction may write at most 50 ledger entries. An unbounded bubble was
/// already able to exceed that — the pre-existing TTL tests insert in
/// descending order specifically to avoid it — and decay makes a newcomer
/// topping a decayed list the common case rather than a corner one, so the
/// walk is capped. An entry that cannot reach its place in one call settles
/// further on each subsequent write, and `get_top_players`/`get_rank` rank on
/// decayed values at read time regardless, so the reported order is exact
/// even while the stored index is still catching up.
const MAX_BUBBLE_STEPS: u32 = 8;

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

// OPT: was 4 separate keys per user (Points, TotalBets, WonBets, LostBets).
//      Now 1 key per user (Stats) — saves 3 storage reads + 3 writes on
//      every add_pts call and 3 reads on every get_stats call.
//      TopPlayerSlot retained as a reverse lookup for O(1) in-place update.
//      TopPlayerCount moves to instance storage (free to read with other keys).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    MarketContract,
    ReferralContract,
    // Lever G: token address so reward() can mint PULSE internally — one
    // cross-call from the market instead of two (add_pts + mint).
    TokenContract,
    Stats(Address), // was: Points + TotalBets + WonBets + LostBets (4 keys → 1)
    TopPlayerAt(u32),
    TopPlayerCount,
    TopPlayerSlot(Address),
    TopPlayerSeqAt(u32), // u64 — FIFO insertion sequence for the player at a slot
    SeqCounter,          // u64 — monotonic counter feeding TopPlayerSeqAt
    MinPoints, // u64 — points of the weakest entry currently in the top list
    MinSlot,   // u32 — slot index of that weakest entry
    Paused,
    // Issue #69: the epoch a player's stored points are expressed in. Kept
    // beside Stats rather than inside it so PlayerStats stays ABI-stable.
    // A player's TopPlayerAt entry is written at the same moment as their
    // Stats, so this one stamp dates both.
    StatsEpoch(Address),
    // Pull-based reward queue (issue #86). Expensive sorting and token minting
    // happen later in claim_pending_rewards, outside critical fund paths.
    PendingReward(Address),
}

// OPT: PlayerEntry now embeds points directly (avoids a Stats read during sort)
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
    ///
    /// It also makes the stored order durable. Two entries decay by the same
    /// factor per period, so the ratio between them is fixed from the moment
    /// both are written — a correctly sorted list stays correctly sorted, and
    /// the min cache keeps its meaning, however long the entries sit.
    ///
    /// On the way out of `get_top_players` this is normalised to the current
    /// epoch, so a reader always sees a score and the epoch it is current as of.
    pub epoch: u32,
}

// External-facing stats struct (ABI stable)
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerStats {
    pub points: u64,
    // Total activity: settled wins + settled losses + bonus awards.
    pub total_bets: u32,
    pub won_bets: u32,
    pub lost_bets: u32,
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
        env.storage().instance().set(&DataKey::MarketContract, &market_contract);
        env.storage().instance().set(&DataKey::ReferralContract, &referral_contract);
        env.storage().instance().set(&DataKey::TopPlayerCount, &0_u32);
        env.storage().instance().set(&DataKey::MinPoints, &0_u64);
        env.storage().instance().set(&DataKey::MinSlot, &0_u32);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn set_token_contract(env: Env, admin: Address, token: Address) -> Result<(), LeaderboardError> {
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(LeaderboardError::NotInitialized)?;
        if admin != stored {
            return Err(LeaderboardError::NotAdmin);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::TokenContract, &token);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// The cross-contract ABI version this deployment implements (issue #84).
    /// Callers that invoke add_pts/add_bonus_pts/reward/reward_bonus should
    /// check this before calling so an upgrade with a breaking signature
    /// change fails loudly instead of misbehaving.
    pub fn interface_version(_env: Env) -> u32 {
        INTERFACE_VERSION
    }

    /// Halt point/reward accrual in an emergency. Admin only. View functions
    /// (get_points, get_top_players, ...) keep working.
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
        env: Env,
        admin: Address,
        token: Address,
    ) -> Result<(), LeaderboardError> {
        Self::set_token_contract(env, admin, token)
    }

    // ── Pull-based reward flow (issue #86) ───────────────────────────────────

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

    /// Apply all pending points and mint tokens in a separate transaction.
    /// Anyone may submit this; the stored rewards always belong to `user`.
    pub fn claim_pending_rewards(env: Env, user: Address) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        let key = DataKey::PendingReward(user.clone());
        let pending: PendingReward = match env.storage().persistent().get(&key) {
            Some(p) => p,
            None => return Ok(()),
        };
        env.storage().persistent().remove(&key);

        let mut stats: PlayerStats = env
            .storage()
            .persistent()
            .get(&DataKey::Stats(user.clone()))
            .unwrap_or(PlayerStats {
                points: 0,
                total_bets: 0,
                won_bets: 0,
                lost_bets: 0,
            });
        stats.points += pending.points;
        stats.total_bets += pending.bet_delta;
        stats.won_bets += pending.won_delta;
        stats.lost_bets += pending.lost_delta;
        env.storage().persistent().set(&DataKey::Stats(user.clone()), &stats);
        env.storage().persistent().extend_ttl(&DataKey::Stats(user.clone()), TTL_BUMP, TTL_HIGH);
        Self::update_top_players(&env, user.clone(), stats.points);

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
        let market: Address = env
            .storage()
            .instance()
            .get(&DataKey::MarketContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if caller != market {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        caller.require_auth();

        let mut stats = Self::stats_for_update(&env, &user);

        stats.points += pts;
        stats.total_bets += 1;
        if is_won {
            stats.won_bets += 1;
        } else {
            stats.lost_bets += 1;
        }

        Self::commit_stats(&env, &user, &stats);

        Self::maintain_ordered_top_index(&env, user.clone(), stats.points);
        // Instance storage (TopPlayerCount, MinPoints, MinSlot, Admin, etc.)
        // has its own TTL that is never bumped by persistent-key writes above —
        // refresh it on every write so the leaderboard's cached min survives.
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        env.events().publish(
            (Symbol::new(&env, "leaderboard_updated"), user),
            (stats.points, stats.won_bets, stats.lost_bets),
        );
        Ok(())
    }

    // ── reward() / reward_bonus() ──────────────────────────────────────────
    // Restored ABI: prediction_market.claim() and referral_registry.
    // register_referral() still invoke these entries, which the issue #23
    // rewrite dropped. Points/win-loss accounting matches add_pts/
    // add_bonus_pts; the PULSE mint happens here so the callers only pay one
    // cross-contract hop (Lever G).

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
        let market: Address = env
            .storage()
            .instance()
            .get(&DataKey::MarketContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if caller != market {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        if points == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }

        let mut stats = Self::stats_for_update(&env, &user);
        stats.points += points;
        stats.total_bets += 1;
        if is_winner {
            stats.won_bets += 1;
        } else {
            stats.lost_bets += 1;
        }
        Self::commit_stats(&env, &user, &stats);

        Self::maintain_ordered_top_index(&env, user.clone(), stats.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);

        if tokens > 0 {
            Self::mint_reward(&env, &user, tokens)?;
        }
        env.events().publish(
            (Symbol::new(&env, "leaderboard_updated"), user),
            (stats.points, is_winner, tokens),
        );
        Ok(())
    }

    pub fn reward_bonus(
        env: Env,
        caller: Address,
        user: Address,
        points: u64,
        tokens: i128,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        caller.require_auth();
        let referral: Address = env
            .storage()
            .instance()
            .get(&DataKey::ReferralContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if caller != referral {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        if points == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }

        let mut stats = Self::stats_for_update(&env, &user);
        let mut stats: PlayerStats = env
            .storage()
            .persistent()
            .get(&DataKey::Stats(user.clone()))
            .unwrap_or(PlayerStats {
                points: 0,
                total_bets: 0,
                won_bets: 0,
                lost_bets: 0,
            });
        stats.points += points;
        stats.total_bets += 1; // bonus awards count as activity
        Self::commit_stats(&env, &user, &stats);

        Self::maintain_ordered_top_index(&env, user.clone(), stats.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);

        if tokens > 0 {
            Self::mint_reward(&env, &user, tokens)?;
        }
        env.events().publish(
            (Symbol::new(&env, "leaderboard_updated"), user),
            (stats.points, tokens),
        );
        Ok(())
    }

    // Kept for ABI compatibility — total_bets is derived from won + lost +
    // bonus at read time, so a standalone "bet recorded" call is a no-op.
    pub fn record_bet(env: Env, caller: Address, _user: Address) -> Result<(), LeaderboardError> {
        let market: Address = env
            .storage()
            .instance()
            .get(&DataKey::MarketContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if caller != market {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        caller.require_auth();
        Ok(())
    }

    pub fn add_bonus_pts(
        env: Env,
        caller: Address,
        user: Address,
        pts: u64,
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        let referral: Address = env
            .storage()
            .instance()
            .get(&DataKey::ReferralContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if caller != referral {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        caller.require_auth();

        let mut stats = Self::stats_for_update(&env, &user);

        stats.points += pts;
        stats.total_bets += 1; // bonus counts as activity

        Self::commit_stats(&env, &user, &stats);

        Self::maintain_ordered_top_index(&env, user.clone(), stats.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        env.events().publish(
            (Symbol::new(&env, "leaderboard_updated"), user),
            stats.points,
        );
        Ok(())
    }

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

    pub fn get_top_players(env: Env, offset: u32, page_size: u32) -> Vec<PlayerEntry> {
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0);

        if offset >= count || page_size == 0 {
            return vec![&env];
        }

        // Read only a bounded range from the write-time ordered index. The
        // saturating addition also keeps an untrusted offset from overflowing
        // before it is clamped to the current player count.
        let page_size = page_size.min(MAX_PAGE_SIZE);
        let end = offset.saturating_add(page_size).min(count);
        let mut result = Vec::new(&env);
        for i in offset..end {
            result.push_back(ranked.get(i).unwrap());
        }
        result
    }

    pub fn get_top_player_count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0)
    }

    /// Pre-#23 ABI name — same value as get_top_player_count().
    pub fn get_player_count(env: Env) -> u32 {
        Self::get_top_player_count(env)
    }

    // ── Rank (issue #67) ───────────────────────────────────────────────────
    // A 1-based rank inside the top list; 0 means "not in the list". The
    // reverse lookup is validated against the forward entry first so an
    // orphaned/stale TopPlayerSlot can never produce a fake rank.

    pub fn get_rank(env: Env, user: Address) -> u32 {
        let Some((slot, entry)) = Self::top_slot_entry(&env, &user) else {
            return 0;
        };
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0);
        // Compare decayed values: an entry that has not been touched in a
        // long time must not outrank a fresher one on a stale stored score
        // (issue #69).
        let mine = Self::entry_points_now(&env, &entry);
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
                if Self::entry_points_now(&env, &e) > mine {
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
        env.storage()
            .instance()
            .get(&DataKey::MinSlot)
            .unwrap_or(0)
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

    /// Validated reverse lookup. Returns the user's slot together with the
    /// entry stored there, but only when the forward and reverse indexes
    /// agree. Any orphaned TopPlayerSlot (entry expired via TTL, overwritten
    /// by an eviction, or otherwise inconsistent) is removed so the caller
    /// can never:
    ///   • panic on a .unwrap() of a missing entry, or
    ///   • update the wrong player's entry through a bogus mapping.
    fn top_slot_entry(env: &Env, user: &Address) -> Option<(u32, PlayerEntry)> {
        let slot: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::TopPlayerSlot(user.clone()))?;
        match env
            .storage()
            .persistent()
            .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(slot))
        {
            Some(entry) if entry.address == *user => Some((slot, entry)),
            _ => {
                // Orphaned or stale reverse mapping.
                env.storage().persistent().remove(&DataKey::TopPlayerSlot(user.clone()));
                None
            }
        }
    }

    /// Write one entry into the forward and reverse indexes as one logical
    /// operation. Every write path goes through this helper, so the slots
    /// consumed by get_top_players are always kept in sync with lookups.
    fn write_ordered_entry(env: &Env, slot: u32, entry: &PlayerEntry) {
        let key = DataKey::TopPlayerAt(slot);
        env.storage().persistent().set(&key, entry);
        env.storage()
            .persistent()
            .set(&DataKey::TopPlayerSlot(entry.address.clone()), &slot);
    }

    /// Bubbles a (possibly new) entry up from `slot` until the list is
    /// descending again. Forward and reverse indexes are always written
    /// together; TTL freshness is refreshed at the owner-touch points
    /// (insert / update / eviction) instead of per swap.
    fn bubble_up(env: &Env, entry: &PlayerEntry, mut slot: u32) {
        let mut steps = 0;
        while slot > 0 && steps < MAX_BUBBLE_STEPS {
            steps += 1;
            let prev: Option<PlayerEntry> =
                env.storage().persistent().get(&DataKey::TopPlayerAt(slot - 1));
            match prev {
                Some(prev)
                    if Self::entry_points_now(env, &prev)
                        < Self::entry_points_now(env, &entry) => {
                    // Write both indexes together. TTLs are NOT bumped per swap:
                    // each extend_ttl counts against the ledger write footprint,
                    // and a bubble can rewrite dozens of slots in one call.
                    // TTL freshness is maintained at the owner-touch points
                    // (insert / in-place update / eviction) instead.
                    Self::write_ordered_entry(env, slot - 1, entry);
                    Self::write_ordered_entry(env, slot, &prev);
                    slot -= 1;
                }
                // A missing entry above means the list has a TTL-expired hole;
                // stop here — reconciliation handles compaction at the next
                // full-list eviction.
                _ => break,
            }
        }
    }

    /// Appends a brand-new entry at `slot`, bumping the count, bubbling it
    /// into place and refreshing the min cache when the list becomes full.
    fn insert_new(env: &Env, user: &Address, points: u64, slot: u32) {
        let entry = PlayerEntry {
            address: user.clone(),
            points,
            epoch: Self::current_epoch(env),
        };
        let key = DataKey::TopPlayerAt(slot);
        Self::write_ordered_entry(env, slot, &entry);
        env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::TopPlayerSlot(user.clone()), TTL_BUMP, TTL_HIGH);
        env.storage().instance().set(&DataKey::TopPlayerCount, &(slot + 1));

        Self::bubble_up(env, &entry, slot);

        // The last slot now holds the weakest entry — cache it for the
        // full-list eviction path.
        if slot + 1 == MAX_TOP_PLAYERS {
            let min_slot = MAX_TOP_PLAYERS - 1;
            let min_entry: PlayerEntry = env
                .storage()
                .persistent()
                .get(&DataKey::TopPlayerAt(min_slot))
                .unwrap();
            env.storage().instance().set(&DataKey::MinPoints, &min_entry.points);
            env.storage().instance().set(&DataKey::MinSlot, &min_slot);
        }
    }

    /// Reconciliation pass — runs only when corruption is detected (a cached
    /// minimum whose slot is empty, e.g. after a TTL expiry). Rebuilds the
    /// list densely from the entries that actually survive: sorted, with a
    /// corrected count and refreshed reverse mappings. Bounded by
    /// MAX_TOP_PLAYERS, so the hot path keeps its O(1) cost.
    fn repair_top_list(env: &Env) -> u32 {
        // 1. Collect every surviving entry.
        let mut entries: Vec<PlayerEntry> = Vec::new(env);
        for i in 0..MAX_TOP_PLAYERS {
            if let Some(e) = env
                .storage()
                .persistent()
                .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(i))
            {
                entries.push_back(e);
            }
        }

        // 2. Sort descending (stable) — bounded (≤ MAX_TOP_PLAYERS) swaps.
        let n = entries.len() as u32;
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
            }
        }

        // 3. Write the dense list back with fresh TTLs and correct reverse
        //    lookups, then drop whatever is left in the old tail slots.
        for slot in 0..n {
            let entry = entries.get(slot).unwrap();
            let key = DataKey::TopPlayerAt(slot);
            Self::write_ordered_entry(env, slot, &entry);
            env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
            env.storage().persistent().extend_ttl(
                &DataKey::TopPlayerSlot(entry.address.clone()),
                TTL_BUMP,
                TTL_HIGH,
            );
        }
        for slot in n..MAX_TOP_PLAYERS {
            env.storage().persistent().remove(&DataKey::TopPlayerAt(slot));
        }

        // 4. Fix the count + min caches.
        env.storage().instance().set(&DataKey::TopPlayerCount, &n);
        if n > 0 {
            let min_entry = entries.get(n - 1).unwrap();
            env.storage().instance().set(&DataKey::MinPoints, &min_entry.points);
            env.storage().instance().set(&DataKey::MinSlot, &(n - 1));
        }
        n
    }

    fn maintain_ordered_top_index(env: &Env, user: Address, new_points: u64) {
        // Fast path: the user is already in the list — in-place update backed
        // by a validated reverse lookup (issue #67).
        if let Some((slot, mut entry)) = Self::top_slot_entry(env, &user) {
            entry.points = new_points;
            entry.epoch = Self::current_epoch(env);
            let key = DataKey::TopPlayerAt(slot);
            Self::write_ordered_entry(env, slot, &entry);
            env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);

            Self::bubble_up(env, &entry, slot);

            // The weakest entry sits at the last slot — keep the min cache fresh.
            let count: u32 = env
                .storage()
                .instance()
                .get(&DataKey::TopPlayerCount)
                .unwrap_or(0);
            if count > 0 {
                let min_slot = count - 1;
                let min_entry: PlayerEntry = env
                    .storage()
                    .persistent()
                    .get(&DataKey::TopPlayerAt(min_slot))
                    .unwrap();
                env.storage().instance().set(&DataKey::MinPoints, &min_entry.points);
                env.storage().instance().set(&DataKey::MinSlot, &min_slot);
            }
            return;
        }

        // New user: append while there is room.
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0);
        if count < MAX_TOP_PLAYERS {
            Self::insert_new(env, &user, new_points, count);
            return;
        }

        // List full: evict the weakest entry if the newcomer beats it.
        let mut min_slot: u32 = env.storage().instance().get(&DataKey::MinSlot).unwrap_or(0);
        let mut old_entry: Option<PlayerEntry> =
            env.storage().persistent().get(&DataKey::TopPlayerAt(min_slot));

        // If the cached minimum points at a missing entry (TTL expiry or any
        // earlier corruption), reconcile the whole list before deciding.
        if old_entry.is_none() {
            let n = Self::repair_top_list(env);
            if n < MAX_TOP_PLAYERS {
                Self::insert_new(env, &user, new_points, n);
                return;
            }
            min_slot = env.storage().instance().get(&DataKey::MinSlot).unwrap_or(0);
            old_entry = env.storage().persistent().get(&DataKey::TopPlayerAt(min_slot));
        }

        match old_entry {
            // Decay the incumbent before comparing, so an entry that is only
            // ahead because it is old can be displaced (issue #69).
            Some(old) if new_points > Self::entry_points_now(env, &old) => {
                // The newcomer displaces the weakest — clear the evicted
                // player's reverse mapping so they cannot read a stale rank.
                env.storage()
                    .persistent()
                    .remove(&DataKey::TopPlayerSlot(old.address.clone()));

                let new_entry = PlayerEntry {
                    address: user.clone(),
                    points: new_points,
                    epoch: Self::current_epoch(env),
                };
                let key = DataKey::TopPlayerAt(min_slot);
                Self::write_ordered_entry(env, min_slot, &new_entry);
                env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
                env.storage().persistent().extend_ttl(
                    &DataKey::TopPlayerSlot(user.clone()),
                    TTL_BUMP,
                    TTL_HIGH,
                );

                Self::bubble_up(env, &new_entry, min_slot);

                // Recompute the min (weakest now sits at the last slot).
                let new_min_slot = MAX_TOP_PLAYERS - 1;
                let new_min_entry: PlayerEntry = env
                    .storage()
                    .persistent()
                    .get(&DataKey::TopPlayerAt(new_min_slot))
                    .unwrap();
                env.storage().instance().set(&DataKey::MinPoints, &new_min_entry.points);
                env.storage().instance().set(&DataKey::MinSlot, &new_min_slot);
            }
            _ => {}
        }
    }

    // ── Point decay (issue #69) ───────────────────────────────────────────
    //
    // The board used to be a cumulative counter: `points += n`, never down.
    // An early adopter who stopped playing kept their rank forever, because
    // a newcomer had to out-earn their entire lifetime total to pass them.
    //
    // Scores are now time-weighted. Nothing is recomputed on a timer: each
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
        let mut stats: PlayerStats = env
            .storage()
            .persistent()
            .get(&DataKey::Stats(user.clone()))
            .unwrap_or(PlayerStats {
                points: 0,
                total_bets: 0,
                won_bets: 0,
                lost_bets: 0,
            });
        if stats.points == 0 {
            return stats;
        }
        let now = Self::current_epoch(env);
        let written_at: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::StatsEpoch(user.clone()))
            .unwrap_or(now);
        stats.points = Self::decay(stats.points, now.saturating_sub(written_at));
        stats
    }

    /// Read a player's stats for an accrual. The value written back is
    /// expressed in the current epoch, and stamped as such by `commit_stats`.
    fn stats_for_update(env: &Env, user: &Address) -> PlayerStats {
        Self::decayed_stats(env, user)
    }

    /// Persist stats, stamping the epoch they are expressed in.
    fn commit_stats(env: &Env, user: &Address, stats: &PlayerStats) {
        let key = DataKey::Stats(user.clone());
        env.storage().persistent().set(&key, stats);
        env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);

        let epoch_key = DataKey::StatsEpoch(user.clone());
        env.storage().persistent().set(&epoch_key, &Self::current_epoch(env));
        env.storage().persistent().extend_ttl(&epoch_key, TTL_BUMP, TTL_HIGH);
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

    #[inline]
    fn require_not_paused(env: &Env) -> Result<(), LeaderboardError> {
        if env.storage().instance().get(&DataKey::Paused).unwrap_or(false) {
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