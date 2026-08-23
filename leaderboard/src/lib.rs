#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, vec, Address, Env, Vec,
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
    /// Admin attempted to remove/reset a player who has no Stats record and
    /// is not in the top list — i.e. they were never tracked.
    PlayerNotFound = 9,
    /// A banned player tried to earn points. Banned status is permanent until
    /// the admin explicitly lifts it (no lift function yet; add if needed).
    PlayerBanned = 10,
}

// OPT: was 4 separate keys per user (Points, TotalBets, WonBets, LostBets).
//      Now 1 key per user (Stats) — saves 3 storage reads + 3 writes on
//      every add_pts call and 3 reads on every get_stats call.
//      TopPlayerSlot retained as a reverse lookup for O(1) in-place update.
//      TopPlayerCount moves to instance storage (free to read with other keys).
//
// Invariant: for every live slot i < TopPlayerCount,
//   TopPlayerAt(i) = Some(entry)  <=>  TopPlayerSlot(entry.address) = Some(i)
// Both keys are written, TTL-bumped, and removed together via set_top_slot /
// clear_top_slot. TTL expiry has no contract hook, so reconcile_top_slots
// (and opportunistic repair on write) rebuilds the reverse index from the
// surviving forward entries.
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
    SeqCounter,          // u64 — monotonic counter feeding PlayerEntry::seq
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
    // Issue #20: permanently flagged addresses. Any write path checks this
    // key before crediting points and returns PlayerBanned if present.
    BannedPlayer(Address),
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

// External-facing stats struct (ABI stable).
// total_bets is derived at read time as won_bets + lost_bets + bonus_bets
// so that bonus-only activity is always reflected without polluting won/lost.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerStats {
    pub points: u64,
    // Total activity: settled wins + settled losses + bonus awards.
    // Derived at read time — not stored. See StoredStats.
    pub total_bets: u32,
    pub won_bets: u32,
    pub lost_bets: u32,
}

// Internal packed stats stored under DataKey::Stats.
// Issue #64: bonus_bets tracks bonus awards (referral/welcome) separately from
// won_bets/lost_bets so that total_bets = won + lost + bonus is always correct.
// total_bets is NOT stored — it is derived in get_stats to stay consistent.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredStats {
    pub points: u64,
    pub won_bets: u32,
    pub lost_bets: u32,
    pub bonus_bets: u32, // Issue #64: counts bonus-path awards (reward_bonus / add_bonus_pts)
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

    /// Derive the external PlayerStats, computing total_bets at read time.
    fn to_player_stats(&self) -> PlayerStats {
        PlayerStats {
            points: self.points,
            total_bets: self.won_bets + self.lost_bets + self.bonus_bets,
            won_bets: self.won_bets,
            lost_bets: self.lost_bets,
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

    // ── Bet-settlement path ───────────────────────────────────────────────────

    /// Called by the market contract after a bet is settled.
    /// Updates points + won/lost counts and optionally mints PULSE tokens.
    /// Replaces the old two-call pattern (add_pts + separate mint).
    pub fn reward(
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

    pub fn reward(
    /// Original ABI name — kept for callers that deploy against the pre-#23
    /// interface (prediction_market and referral_registry tests use it).
    pub fn set_token(
        env: Env,
        caller: Address,
        user: Address,
        points: u64,
        tokens: i128,
        is_winner: bool,
    ) -> Result<(), LeaderboardError> {
        caller.require_auth();
        Self::require_market_contract(&env, &caller)?;
        if points == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }
        Self::credit_points(&env, &user, points, Some(is_winner));
        Self::mint_tokens(&env, &user, tokens)
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

    pub fn reward_bonus(

        let mut s = Self::load_stored(&env, &user);
        s.points += pts;
        let mut stats = Self::stats_for_update(&env, &user);

        stats.points += pts;
        stats.total_bets += 1;
        if is_won {
            s.won_bets += 1;
        } else {
            s.lost_bets += 1;
        }
        Self::save_stored(&env, &user, &s);
        Self::update_top_players(&env, user.clone(), s.points);

        // Mint PULSE tokens if wired and amount > 0.
        if tokens > 0 {
            if let Some(token) = env
                .storage()
                .instance()
                .get::<DataKey, Address>(&DataKey::TokenContract)
            {
                let this = env.current_contract_address();
                let _: Val = env.invoke_contract(
                    &token,
                    &Symbol::new(&env, "mint"),
                    vec![
                        &env,
                        this.into_val(&env),
                        user.into_val(&env),
                        tokens.into_val(&env),
                    ],
                );
            }
        }

        Self::commit_stats(&env, &user, &stats);

        Self::update_top_players(&env, user.clone(), stats.points);
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

    /// Called by the referral contract for welcome / per-bet referral bonuses.
    /// Increments bonus_bets (not won/lost) so total_bets stays accurate and
    /// won_bets/lost_bets are never polluted with non-bet activity.
    pub fn reward_bonus(
        env: Env,
        caller: Address,
        user: Address,
        pts: u64,
        tokens: i128,
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
    ) -> Result<(), LeaderboardError> {
        Self::require_not_paused(&env)?;
        caller.require_auth();
        Self::require_referral_contract(&env, &caller)?;
        if points == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }
        Self::credit_points(&env, &user, points, None);
        Self::mint_tokens(&env, &user, tokens)

        let mut stats = Self::stats_for_update(&env, &user);
        stats.points += points;
        stats.total_bets += 1;
        if is_winner {
            stats.won_bets += 1;
        } else {
            stats.lost_bets += 1;
        }
        Self::commit_stats(&env, &user, &stats);

        Self::update_top_players(&env, user.clone(), stats.points);
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

    pub fn add_bonus_pts(
        env: Env,
        caller: Address,
        user: Address,
        pts: u64,
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

        let mut s = Self::load_stored(&env, &user);
        s.points += pts;
        s.bonus_bets += 1; // Issue #64: count bonus award without touching won/lost
        Self::save_stored(&env, &user, &s);
        Self::update_top_players(&env, user.clone(), s.points);

        // Mint PULSE tokens if wired and amount > 0.
        if tokens > 0 {
            if let Some(token) = env
                .storage()
                .instance()
                .get::<DataKey, Address>(&DataKey::TokenContract)
            {
                let this = env.current_contract_address();
                let _: Val = env.invoke_contract(
                    &token,
                    &Symbol::new(&env, "mint"),
                    vec![
                        &env,
                        this.into_val(&env),
                        user.into_val(&env),
                        tokens.into_val(&env),
                    ],
                );
            }
        }

        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// No-op stub retained for ABI compatibility.
    /// total_bets is now derived as won_bets + lost_bets + bonus_bets at read
    /// time, so a separate record_bet increment is no longer needed.
    pub fn record_bet(
        env: Env,
        caller: Address,
        _user: Address,
    ) -> Result<(), LeaderboardError> {
        let market: Address = env
            .storage()
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

        Self::update_top_players(&env, user.clone(), stats.points);
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
        Self::credit_points(&env, &user, pts, None);
        Ok(())
    }

    pub fn record_bet(
        env: Env,
        caller: Address,
        _user: Address,
    ) -> Result<(), LeaderboardError> {
        caller.require_auth();
        Self::require_market_contract(&env, &caller)?;

        let mut stats = Self::stats_for_update(&env, &user);

    // ── Legacy write functions (kept for backward-compat) ─────────────────────

    /// Legacy: called by the market contract to add points and update win/loss.
    /// Prefer reward() for new integrations (adds token minting in one call).
    pub fn add_pts(
        env: Env,
        caller: Address,
        user: Address,
        pts: u64,
        is_won: bool,
    ) -> Result<(), LeaderboardError> {
        let market: Address = env
            .storage()
            .instance()
            .get(&DataKey::MarketContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if caller != market {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        caller.require_auth();

        let mut s = Self::load_stored(&env, &user);
        s.points += pts;
        if is_won {
            s.won_bets += 1;
        } else {
            s.lost_bets += 1;
        }
        Self::save_stored(&env, &user, &s);
        Self::update_top_players(&env, user, s.points);
        Self::commit_stats(&env, &user, &stats);

        Self::update_top_players(&env, user.clone(), stats.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        env.events().publish(
            (Symbol::new(&env, "leaderboard_updated"), user),
            stats.points,
        );
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
        let referral: Address = env
            .storage()
            .instance()
            .get(&DataKey::ReferralContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if caller != referral {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        caller.require_auth();

        let mut s = Self::load_stored(&env, &user);
        s.points += pts;
        s.bonus_bets += 1; // Issue #64: bonus activity counted without polluting won/lost
        Self::save_stored(&env, &user, &s);
        Self::update_top_players(&env, user, s.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    // ── View functions ────────────────────────────────────────────────────────

    pub fn get_points(env: Env, user: Address) -> u64 {
        env.storage()
            .persistent()
            .get::<_, StoredStats>(&DataKey::Stats(user))
            .map(|s| s.points)
            .unwrap_or(0)
    }

    /// Returns PlayerStats with total_bets derived as won_bets + lost_bets +
    /// bonus_bets, so bonus-only users always show a non-zero total_bets.
    pub fn get_stats(env: Env, user: Address) -> PlayerStats {
        env.storage()
            .persistent()
            .get::<_, StoredStats>(&DataKey::Stats(user))
            .map(|s| s.to_player_stats())
            .unwrap_or(PlayerStats {
                points: 0,
                total_bets: 0,
                won_bets: 0,
                lost_bets: 0,
            })
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

    /// Returns the 1-based rank of the user in the top-players list.
    /// Returns 0 if the user is not in the list.
    pub fn get_rank(env: Env, user: Address) -> u32 {
        // The top list is kept in descending-points order; the slot index is
        // 0-based, so rank = slot + 1.
        env.storage()
            .persistent()
            .get::<_, u32>(&DataKey::TopPlayerSlot(user))
            .map(|slot| slot + 1)
            .unwrap_or(0)
    }

    /// Returns the number of players currently in the top list (≤ MAX_TOP_PLAYERS).
    pub fn get_player_count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0)
    }

    pub fn get_top_players(env: Env, offset: u32, page_size: u32) -> Vec<PlayerEntry> {
        let count: u32 = Self::top_count(&env);

        if offset >= count || page_size == 0 {
            return vec![&env];
        }

        let page_size = page_size.min(MAX_PAGE_SIZE);
        let end = (offset + page_size).min(count);
        let mut result = Vec::new(&env);
        for i in offset..end {
            if let Some(entry) = Self::forward_entry(&env, i) {
                result.push_back(entry);
            }
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

    pub fn get_top_player_count(env: Env) -> u32 {
        Self::top_count(&env)
    }

    pub fn get_player_count(env: Env) -> u32 {
        Self::top_count(&env)
    }

    pub fn get_min_points(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::MinPoints)
            .unwrap_or(0)
    }

    pub fn get_min_slot(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MinSlot)
            .unwrap_or(0)
    }

    // ── Rank (issues #67, #91) ─────────────────────────────────────────────
    // A 1-based rank inside the top list. Players outside the list get
    // UNRANKED_RANK (MAX_TOP_PLAYERS + 1), never 0, so an unranked player can
    // never sort above (numerically lower than) a real position and the
    // "unranked" state is unambiguous. The reverse lookup is validated
    // against the forward entry first so an orphaned/stale TopPlayerSlot can
    // never produce a fake rank.

    pub fn get_rank(env: Env, user: Address) -> u32 {
        let Some((slot, entry)) = Self::top_slot_entry(&env, &user) else {
            return UNRANKED_RANK;
    /// Rank is 1-based position in the sorted top list, or 0 if the user is
    /// not currently in it. A reverse lookup is only trusted when the forward
    /// entry still exists and points back at `user`; otherwise the stale
    /// `TopPlayerSlot` is deleted on the spot.
    pub fn get_rank(env: Env, user: Address) -> u32 {
        let Some(slot) = env
            .storage()
            .persistent()
            .get::<_, u32>(&DataKey::TopPlayerSlot(user.clone()))
        else {
            return 0;
        };
        match Self::forward_entry(&env, slot) {
            Some(entry) if entry.address == user => slot + 1,
            _ => {
                env.storage()
                    .persistent()
                    .remove(&DataKey::TopPlayerSlot(user));
                0
            }
        }
    }

    /// Rebuild `TopPlayerSlot` from live `TopPlayerAt` entries, compact holes
    /// left by TTL expiry, and refresh the min cache. Anyone may call this
    /// (keeper/repair); it only writes keys that restore the index invariant.
    pub fn reconcile_top_slots(env: Env) {
        Self::repair_top_index(&env);
    }

    // ── Internal: atomic forward/reverse index ───────────────────────────────

    fn top_count(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0)
    // ── Rank (issue #67) ───────────────────────────────────────────────────
    // A 1-based rank inside the tracked top list. None explicitly means the
    // player is outside the top list; this contract does not maintain a global
    // player index and therefore cannot report a whole-population rank. The
    // reverse lookup is validated first so an orphaned/stale TopPlayerSlot can
    // never produce a fake rank.
    pub fn get_rank(env: Env, user: Address) -> Option<u32> {
        let Some((slot, entry)) = Self::top_slot_entry(&env, &user) else {
            return None;
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
        Some(rank)
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

    fn forward_entry(env: &Env, slot: u32) -> Option<PlayerEntry> {
        env.storage()
            .persistent()
            .get(&DataKey::TopPlayerAt(slot))
    }

    /// Write `TopPlayerAt(slot)` and `TopPlayerSlot(address)` together, and
    /// bump both TTLs. This is the only way the two keys are created/updated.
    fn set_top_slot(env: &Env, slot: u32, entry: &PlayerEntry) {
        let at_key = DataKey::TopPlayerAt(slot);
        env.storage().persistent().set(&at_key, entry);
        env.storage()
            .persistent()
            .extend_ttl(&at_key, TTL_BUMP, TTL_HIGH);
    // ── Internal helpers ──────────────────────────────────────────────────────

    fn load_stored(env: &Env, user: &Address) -> StoredStats {
        env.storage()
            .persistent()
            .get(&DataKey::Stats(user.clone()))
            .unwrap_or_else(StoredStats::zero)
    }

    fn save_stored(env: &Env, user: &Address, s: &StoredStats) {
        env.storage()
            .persistent()
            .set(&DataKey::Stats(user.clone()), s);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Stats(user.clone()), TTL_BUMP, TTL_HIGH);
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

        let slot_key = DataKey::TopPlayerSlot(entry.address.clone());
        env.storage().persistent().set(&slot_key, &slot);
        env.storage()
            .persistent()
            .extend_ttl(&slot_key, TTL_BUMP, TTL_HIGH);
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
    /// Bubbles a (possibly new) entry up from `slot` until the list is
    /// descending again.
    ///
    /// Issue #69: comparisons are on decayed values, so an entry that is only
    /// ahead because it is old gets passed. Because both sides carry their own
    /// epoch, that costs no extra ledger reads. Forward and reverse indexes are always written
    /// together so the pair cannot drift apart; TTL freshness is refreshed
    /// at the owner-touch points (insert / update / eviction) instead of
    /// per swap, to keep the write footprint bounded.
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
                    slot -= 1;
                }
            }
        }
        None
    }

    fn refresh_min(env: &Env, count: u32) {
        if count == 0 {
            env.storage().instance().set(&DataKey::MinPoints, &0_u64);
            env.storage().instance().set(&DataKey::MinSlot, &0_u32);
            return;
        }
        let min_slot = count - 1;
        if let Some(min_entry) = Self::forward_entry(env, min_slot) {
            env.storage()
                .instance()
                .set(&DataKey::MinPoints, &min_entry.points);
            env.storage().instance().set(&DataKey::MinSlot, &min_slot);
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
        env.storage().persistent().set(&key, &entry);
        env.storage().persistent().set(&DataKey::TopPlayerSlot(user.clone()), &slot);
        env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::TopPlayerSlot(user.clone()), TTL_BUMP, TTL_HIGH);
        env.storage().instance().set(&DataKey::TopPlayerCount, &(slot + 1));

        Self::bubble_up(env, &entry, slot);

        // Keep the min cache consistent after a (possibly first) insertion.
        Self::recompute_min(env);
    }

    /// Compact holes and rewrite every reverse lookup from surviving forward
    /// entries. Returns the new live count.
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
        Self::refresh_min(env, write);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        write
    }

    fn ensure_consistent(env: &Env, count: u32) -> u32 {
        for i in 0..count {
            if Self::forward_entry(env, i).is_none() {
                return Self::repair_top_index(env);
            }
        }
        count
    }

    fn bubble_up(env: &Env, mut current: u32, entry: &PlayerEntry) {
        let mut repaired = false;
        while current > 0 {
            match Self::forward_entry(env, current - 1) {
                Some(prev) if prev.points < entry.points => {
                    Self::set_top_slot(env, current - 1, entry);
                    Self::set_top_slot(env, current, &prev);
                    current -= 1;
                }
                Some(_) => break,
                None => {
                    if repaired {
                        break;
                    }
                    repaired = true;
                    let count = Self::repair_top_index(env);
                    current = Self::resolved_slot(env, &entry.address, count).unwrap_or(0);
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
        }
    }

    fn credit_points(env: &Env, user: &Address, pts: u64, is_won: Option<bool>) {
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

        stats.points += pts;
        stats.total_bets += 1;
        match is_won {
            Some(true) => stats.won_bets += 1,
            Some(false) => stats.lost_bets += 1,
            None => {}
        // 3. Write the dense list back with fresh TTLs and correct reverse
        //    lookups, then drop whatever is left in the old tail slots.
        for slot in 0..n {
            let mut entry = entries.get(slot).unwrap();
            entry.seq = Self::next_seq(env);
            let key = DataKey::TopPlayerAt(slot);
            env.storage().persistent().set(&key, &entry);
            env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
            env.storage().persistent().set(&DataKey::TopPlayerSlot(entry.address.clone()), &slot);
            env.storage().persistent().extend_ttl(
                &DataKey::TopPlayerSlot(entry.address.clone()),
                TTL_BUMP,
                TTL_HIGH,
            );
        }

        env.storage()
            .persistent()
            .set(&DataKey::Stats(user.clone()), &stats);
        env.storage().persistent().extend_ttl(
            &DataKey::Stats(user.clone()),
            TTL_BUMP,
            TTL_HIGH,
        );

        Self::update_top_players(env, user.clone(), stats.points);
        // Instance storage (TopPlayerCount, MinPoints, MinSlot, Admin, etc.)
        // has its own TTL that is never bumped by persistent-key writes above —
        // refresh it on every write so the leaderboard's cached min survives.
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
            .ok_or(LeaderboardError::NotInitialized)?;
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

    fn require_market_contract(env: &Env, caller: &Address) -> Result<(), LeaderboardError> {
        let mkt: Address = env
            .storage()
            .instance()
            .get(&DataKey::MarketContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if *caller != mkt {
            return Err(LeaderboardError::UnauthorizedCaller);
    /// Monotonic FIFO sequence counter — fed into `PlayerEntry::seq` so that,
    /// when several players share the minimum score, the *oldest* (smallest
    /// seq) is evicted first instead of whichever sits at the lowest slot.
    fn next_seq(env: &Env) -> u64 {
        let s: u64 = env.storage().instance().get(&DataKey::SeqCounter).unwrap_or(0);
        env.storage().instance().set(&DataKey::SeqCounter, &(s + 1));
        s
    }

    fn recompute_min(env: &Env) {
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0);

        // If user is already in the list, update their points and re-sort in place.
        if let Some(slot) =
            env.storage()
                .persistent()
                .get::<_, u32>(&DataKey::TopPlayerSlot(user.clone()))
        {
            let mut entry: PlayerEntry = env
                .storage()
                .persistent()
                .get(&DataKey::TopPlayerAt(slot))
                .unwrap();
            entry.points = new_points;
            env.storage()
                .persistent()
                .set(&DataKey::TopPlayerAt(slot), &entry);
            env.storage()
                .persistent()
                .extend_ttl(&DataKey::TopPlayerAt(slot), TTL_BUMP, TTL_HIGH);

            // Bubble the updated entry up to maintain descending order.
            let mut current = slot;
            while current > 0 {
                let prev: PlayerEntry = env
                    .storage()
                    .persistent()
                    .get(&DataKey::TopPlayerAt(current - 1))
                    .unwrap();
                if prev.points < entry.points {
                    // Swap
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerAt(current - 1), &entry);
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerAt(current), &prev);
                    env.storage().persistent().set(
                        &DataKey::TopPlayerSlot(entry.address.clone()),
                        &(current - 1),
                    );
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerSlot(prev.address.clone()), &current);
                    current -= 1;
                } else {
                    break;
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
            if let Some(e) = env
                .storage()
                .persistent()
                .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(slot))
            {
                // Among equal-min entries keep the *oldest* (smallest seq) so
                // tie eviction is FIFO, not lowest-slot (issue #70).
                if !found || e.points < min_points || (e.points == min_points && e.seq < min_seq) {
                    min_points = e.points;
                    min_slot = slot;
                    min_seq = e.seq;
                    found = true;
                }
            }
        }
        if found {
            env.storage().instance().set(&DataKey::MinPoints, &min_points);
            env.storage().instance().set(&DataKey::MinSlot, &min_slot);
        }
    }

            // Update min points/slot if this was the last slot.
            if count > 0 {
                let min_slot = count - 1;
                let min_entry: PlayerEntry = env
                    .storage()
                    .persistent()
                    .get(&DataKey::TopPlayerAt(min_slot))
                    .unwrap();
                env.storage()
                    .instance()
                    .set(&DataKey::MinPoints, &min_entry.points);
                env.storage()
                    .instance()
                    .set(&DataKey::MinSlot, &min_slot);
            }
    fn update_top_players(env: &Env, user: Address, new_points: u64) {
        // Fast path: the user is already in the list — in-place update backed
        // by a validated reverse lookup (issue #67).
        if let Some((slot, mut entry)) = Self::top_slot_entry(env, &user) {
            entry.points = new_points;
            entry.epoch = Self::current_epoch(env);
            let key = DataKey::TopPlayerAt(slot);
            env.storage().persistent().set(&key, &entry);
            env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);

            Self::bubble_up(env, &entry, slot);

            // Keep the min cache consistent after an in-place update.
            Self::recompute_min(env);
            return;
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

    fn update_top_players(env: &Env, user: Address, new_points: u64) {
        let mut count = Self::ensure_consistent(env, Self::top_count(env));

        if let Some(slot) = Self::resolved_slot(env, &user, count) {
            let entry = PlayerEntry {
                address: user,
                points: new_points,
            };
            Self::set_top_slot(env, slot, &entry);
            Self::bubble_up(env, slot, &entry);
            count = Self::top_count(env);
            Self::refresh_min(env, count);
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0);
        if count < MAX_TOP_PLAYERS {
            let slot = count;
            let entry = PlayerEntry {
                address: user.clone(),
                points: new_points,
            };
            env.storage()
                .persistent()
                .set(&DataKey::TopPlayerAt(slot), &entry);
            env.storage()
                .persistent()
                .set(&DataKey::TopPlayerSlot(user.clone()), &slot);
            env.storage()
                .persistent()
                .extend_ttl(&DataKey::TopPlayerAt(slot), TTL_BUMP, TTL_HIGH);
            env.storage()
                .instance()
                .set(&DataKey::TopPlayerCount, &(count + 1));

            // Bubble up to maintain order.
            let mut current = slot;
            while current > 0 {
                let prev: PlayerEntry = env
                    .storage()
                    .persistent()
                    .get(&DataKey::TopPlayerAt(current - 1))
                    .unwrap();
                if prev.points < entry.points {
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerAt(current - 1), &entry);
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerAt(current), &prev);
                    env.storage().persistent().set(
                        &DataKey::TopPlayerSlot(entry.address.clone()),
                        &(current - 1),
                    );
                    env.storage()
                        .persistent()
                        .set(&DataKey::TopPlayerSlot(prev.address.clone()), &current);
                    current -= 1;
                } else {
                    break;
                }
            }

            // Update min if we just filled the last slot.
            if count + 1 == MAX_TOP_PLAYERS {
                let min_slot = MAX_TOP_PLAYERS - 1;
                let min_entry: PlayerEntry = env
                    .storage()
                    .persistent()
                    .get(&DataKey::TopPlayerAt(min_slot))
                    .unwrap();
                env.storage()
                    .instance()
                    .set(&DataKey::MinPoints, &min_entry.points);
                env.storage()
                    .instance()
                    .set(&DataKey::MinSlot, &min_slot);
            }
        } else {
            // List full: replace the minimum if the new points beat it.
            let min_points: u64 =
                env.storage().instance().get(&DataKey::MinPoints).unwrap_or(0);
            if new_points > min_points {
                let min_slot: u32 =
                    env.storage().instance().get(&DataKey::MinSlot).unwrap_or(0);
                let old_entry: PlayerEntry = env
                    .storage()
                    .persistent()
                    .get(&DataKey::TopPlayerAt(min_slot))
                    .unwrap();

                // Remove old slot mapping.
                env.storage()
                    .persistent()
                    .remove(&DataKey::TopPlayerSlot(old_entry.address.clone()));
            Self::insert_new(env, &user, new_points, count);
            return;
        }

        if count < MAX_TOP_PLAYERS {
            let slot = count;
            let entry = PlayerEntry {
                address: user,
                points: new_points,
            };
            Self::set_top_slot(env, slot, &entry);
            let new_count = count + 1;
            env.storage()
                .instance()
                .set(&DataKey::TopPlayerCount, &new_count);
            Self::bubble_up(env, slot, &entry);
            count = Self::top_count(env);
            if count == MAX_TOP_PLAYERS {
                Self::refresh_min(env, count);
            }
            return;
        }

        // Sorted list: the weakest live entry is always the last slot. Never
        // evict from the cached MinSlot — that cache going stale is what
        // let a low-points player overwrite a high-points one (issue #1/#22).
        let min_slot = count - 1;
        let Some(min_entry) = Self::forward_entry(env, min_slot) else {
            Self::repair_top_index(env);
            Self::update_top_players(env, user, new_points);
            return;
        };
        if new_points <= min_entry.points {
            return;
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
                env.storage()
                    .persistent()
                    .set(&DataKey::TopPlayerAt(min_slot), &new_entry);
                env.storage()
                    .persistent()
                    .set(&DataKey::TopPlayerSlot(user.clone()), &min_slot);
                env.storage()
                    .persistent()
                    .extend_ttl(&DataKey::TopPlayerAt(min_slot), TTL_BUMP, TTL_HIGH);

                // Bubble up from min_slot.
                let mut current = min_slot;
                while current > 0 {
                    let prev: PlayerEntry = env
                        .storage()
                        .persistent()
                        .get(&DataKey::TopPlayerAt(current - 1))
                        .unwrap();
                    if prev.points < new_entry.points {
                        env.storage()
                            .persistent()
                            .set(&DataKey::TopPlayerAt(current - 1), &new_entry);
                        env.storage()
                            .persistent()
                            .set(&DataKey::TopPlayerAt(current), &prev);
                        env.storage().persistent().set(
                            &DataKey::TopPlayerSlot(new_entry.address.clone()),
                            &(current - 1),
                        );
                        env.storage().persistent().set(
                            &DataKey::TopPlayerSlot(prev.address.clone()),
                            &current,
                        );
                        current -= 1;
                    } else {
                        break;
                    }
                }

                // Recompute min (now at the last slot after bubbling).
                let new_min_slot = MAX_TOP_PLAYERS - 1;
                let new_min_entry: PlayerEntry = env
                    .storage()
                    .persistent()
                    .get(&DataKey::TopPlayerAt(new_min_slot))
                    .unwrap();
                env.storage()
                    .instance()
                    .set(&DataKey::MinPoints, &new_min_entry.points);
                env.storage()
                    .instance()
                    .set(&DataKey::MinSlot, &new_min_slot);
                let key = DataKey::TopPlayerAt(min_slot);
                env.storage().persistent().set(&key, &new_entry);
                env.storage().persistent().set(&DataKey::TopPlayerSlot(user.clone()), &min_slot);
                env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
                env.storage().persistent().extend_ttl(
                    &DataKey::TopPlayerSlot(user.clone()),
                    TTL_BUMP,
                    TTL_HIGH,
                );

                Self::bubble_up(env, &new_entry, min_slot);

                // Recompute the min over the bounded list (handles ties —
                // the weakest entry may now sit at any slot).
                Self::recompute_min(env);
            }
            _ => {}
        }

        Self::clear_top_slot(env, min_slot);

        let new_entry = PlayerEntry {
            address: user,
            points: new_points,
        };
        Self::set_top_slot(env, min_slot, &new_entry);
        Self::bubble_up(env, min_slot, &new_entry);
        Self::refresh_min(env, Self::top_count(env));
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

    // ── Issue #20: Admin governance — remove / ban / reset ───────────────────

    /// Remove a player from the top-50 list and erase all their stored
    /// state (Stats, StatsEpoch, PendingReward). Admin only.
    ///
    /// After removal the player is treated as if they never played: they can
    /// re-earn points and re-enter the top list from scratch. If you want to
    /// prevent re-entry, use `ban_player` instead (or call both).
    ///
    /// Returns `PlayerNotFound` when the address has no Stats record **and**
    /// is not in the top list, so the admin gets a clear signal when they
    /// supply a wrong address.
    pub fn remove_player(
        env: Env,
        admin: Address,
        user: Address,
    ) -> Result<(), LeaderboardError> {
        Self::require_admin(&env, &admin)?;

        let stats_key = DataKey::Stats(user.clone());
        let has_stats = env.storage().persistent().has(&stats_key);
        let count = Self::top_count(&env);
        let slot_opt = Self::resolved_slot(&env, &user, count);

        if !has_stats && slot_opt.is_none() {
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

        // 2. Erase top-list presence (both forward and reverse keys).
        if let Some(slot) = slot_opt {
            Self::clear_top_slot(&env, slot);
            // Compact the gap so update_top_players never sees a hole.
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
    ///     `claim_pending_rewards`) to return `PlayerBanned` immediately.
    ///
    /// Calling `ban_player` on an already-banned address is idempotent: it
    /// re-confirms the ban flag, re-removes any residual stats, and returns Ok.
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
    /// rather than deleted so their bet history is preserved.
    ///
    /// Returns `PlayerNotFound` when the address has never accrued points.
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

    // ── Internal: shared admin auth ───────────────────────────────────────────

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
#[cfg(test)]
mod admin_tests;
