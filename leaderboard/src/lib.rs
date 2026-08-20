#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, vec, Address, Env, IntoVal, Symbol, Val,
    Vec,
};

const MAX_TOP_PLAYERS: u32 = 50;
const TTL_BUMP: u32 = 3_153_600;
const TTL_HIGH: u32 = 6_307_200;

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
}

// OPT: PlayerEntry now embeds points directly (avoids a Stats read during sort)
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerEntry {
    pub address: Address,
    pub points: u64,
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

// Internal packed stats —

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
        if is_won {
            stats.won_bets += 1;
        } else {
            stats.lost_bets += 1;
        }

        env.storage().persistent().set(&DataKey::Stats(user.clone()), &stats);
        env.storage().persistent().extend_ttl(&DataKey::Stats(user.clone()), TTL_BUMP, TTL_HIGH);

        Self::update_top_players(&env, user, stats.points);
        // Instance storage (TopPlayerCount, MinPoints, MinSlot, Admin, etc.)
        // has its own TTL that is never bumped by persistent-key writes above —
        // refresh it on every write so the leaderboard's cached min survives.
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
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
        stats.total_bets += 1;
        if is_winner {
            stats.won_bets += 1;
        } else {
            stats.lost_bets += 1;
        }
        env.storage().persistent().set(&DataKey::Stats(user.clone()), &stats);
        env.storage().persistent().extend_ttl(&DataKey::Stats(user.clone()), TTL_BUMP, TTL_HIGH);

        Self::update_top_players(&env, user.clone(), stats.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);

        if tokens > 0 {
            Self::mint_reward(&env, &user, tokens)?;
        }
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
        env.storage().persistent().set(&DataKey::Stats(user.clone()), &stats);
        env.storage().persistent().extend_ttl(&DataKey::Stats(user.clone()), TTL_BUMP, TTL_HIGH);

        Self::update_top_players(&env, user.clone(), stats.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);

        if tokens > 0 {
            Self::mint_reward(&env, &user, tokens)?;
        }
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
        stats.total_bets += 1; // bonus counts as activity

        env.storage().persistent().set(&DataKey::Stats(user.clone()), &stats);
        env.storage().persistent().extend_ttl(&DataKey::Stats(user.clone()), TTL_BUMP, TTL_HIGH);

        Self::update_top_players(&env, user, stats.points);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn get_points(env: Env, user: Address) -> u64 {
        env.storage()
            .persistent()
            .get::<_, PlayerStats>(&DataKey::Stats(user))
            .map(|s| s.points)
            .unwrap_or(0)
    }

    pub fn get_stats(env: Env, user: Address) -> PlayerStats {
        env.storage()
            .persistent()
            .get(&DataKey::Stats(user))
            .unwrap_or(PlayerStats {
                points: 0,
                total_bets: 0,
                won_bets: 0,
                lost_bets: 0,
            })
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

        let end = (offset + page_size).min(count);
        let mut result = Vec::new(&env);
        for i in offset..end {
            if let Some(entry) = env.storage().persistent().get(&DataKey::TopPlayerAt(i)) {
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
                if e.points > entry.points {
                    rank += 1;
                }
            }
        }
        rank
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

    /// Bubbles a (possibly new) entry up from `slot` until the list is
    /// descending again. Forward and reverse indexes are always written
    /// together so the pair cannot drift apart; TTL freshness is refreshed
    /// at the owner-touch points (insert / update / eviction) instead of
    /// per swap, to keep the write footprint bounded.
    fn bubble_up(env: &Env, entry: &PlayerEntry, mut slot: u32) {
        while slot > 0 {
            let prev: Option<PlayerEntry> =
                env.storage().persistent().get(&DataKey::TopPlayerAt(slot - 1));
            match prev {
                Some(prev) if prev.points < entry.points => {
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
                if entries.get(j).unwrap().points > entries.get(max_idx).unwrap().points {
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
            env.storage().persistent().set(&key, &entry);
            env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
            env.storage().persistent().set(&DataKey::TopPlayerSlot(entry.address.clone()), &slot);
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

    fn update_top_players(env: &Env, user: Address, new_points: u64) {
        // Fast path: the user is already in the list — in-place update backed
        // by a validated reverse lookup (issue #67).
        if let Some((slot, mut entry)) = Self::top_slot_entry(env, &user) {
            entry.points = new_points;
            let key = DataKey::TopPlayerAt(slot);
            env.storage().persistent().set(&key, &entry);
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
            Some(old) if new_points > old.points => {
                // The newcomer displaces the weakest — clear the evicted
                // player's reverse mapping so they cannot read a stale rank.
                env.storage()
                    .persistent()
                    .remove(&DataKey::TopPlayerSlot(old.address.clone()));

                let new_entry = PlayerEntry {
                    address: user.clone(),
                    points: new_points,
                };
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

    fn mint_pulse(env: &Env, user: Address, amount: i128) {
    // Issue #84: check pulse_token's reported ABI version before invoking
    // mint(), so an incompatible token upgrade fails with a clear error
    // instead of an opaque invoke_contract failure or, worse, a call that
    // still type-checks against a changed signature and silently misbehaves.
    // A matching version number alone does not prove the callee's shape is
    // still compatible; see IncompatibleInterface's doc comment.
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
mod tests;
#[cfg(test)]
mod ttl_tests;