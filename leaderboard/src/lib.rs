#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, vec, Address, BytesN, Env, IntoVal,
    Symbol, Val, Vec,
};

const MAX_TOP_PLAYERS: u32 = 50;
const MAX_PAGE_SIZE: u32 = 20;
const TTL_BUMP: u32 = 3_153_600;
const TTL_HIGH: u32 = 6_307_200;

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum LeaderboardError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    UnauthorizedCaller = 3,
    InvalidPoints = 4,
    NotAdmin = 5,
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

    // ── Internal: maintain a persistent sorted top list ──────────────────────

    fn update_top_players(env: &Env, user: Address, new_points: u64) {
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0);

        if let Some(s) = env
            .storage()
            .persistent()
            .get::<_, u32>(&DataKey::TopPlayerSlot(user.clone()))
        {
            // Already in the list — in-place update, O(1). The FIFO age
            // (TopPlayerSeqAt) is unchanged; only the points are refreshed.
            let e = PlayerEntry {
                address: user.clone(),
                points: new_points,
            };
            env.storage().persistent().set(&DataKey::TopPlayerAt(s), &e);
            env.storage()
                .persistent()
                .extend_ttl(&DataKey::TopPlayerAt(s), TTL_BUMP, TTL_HIGH);
            let cached_min_pts: u64 = env
                .storage()
                .instance()
                .get(&DataKey::MinPoints)
                .unwrap_or(u64::MAX);
            let cached_min_slot: u32 = env.storage().instance().get(&DataKey::MinSlot).unwrap_or(0);
            // Recompute whenever the update can invalidate the min cache:
            // - the updated player IS the cached min (their change moves the min), or
            // - their new points are at/below the cached min (they now hold/tie it).
            if s == cached_min_slot || new_points <= cached_min_pts {
                Self::recompute_min(env, count);
            }
            return;
        }

        // New user: insert if list not full or if they beat the current minimum.
        if count < MAX_TOP_PLAYERS {
            let slot = count;
            let entry = PlayerEntry {
                address: user.clone(),
                points: new_points,
            };
            env.storage().persistent().set(&DataKey::TopPlayerAt(slot), &entry);
            env.storage()
                .persistent()
                .extend_ttl(&DataKey::TopPlayerAt(slot), TTL_BUMP, TTL_HIGH);
            let sk = DataKey::TopPlayerSlot(user.clone());
            env.storage().persistent().set(&sk, &slot);
            env.storage()
                .persistent()
                .extend_ttl(&sk, TTL_BUMP, TTL_HIGH);
            // FIFO: stamp the slot with a fresh, monotonically increasing sequence
            // so equal-min ties are broken by insertion order, not slot index.
            Self::stamp_seq(env, slot);
            let new_count = count + 1;
            env.storage()
                .instance()
                .set(&DataKey::TopPlayerCount, &new_count);
            // Lever E: maintain the cached min. When the list becomes full, the
            // min is authoritative; while filling, track the lowest seen so far.
            // Tie-break: strict `<` preserves the EARLIEST (oldest) tie, matching
            // recompute_min's FIFO rule; `<=` would point at an arbitrary later tie.
            let cur_min: u64 = env
                .storage()
                .instance()
                .get(&DataKey::MinPoints)
                .unwrap_or(u64::MAX);
            if new_count == 1 || new_points < cur_min {
                env.storage()
                    .instance()
                    .set(&DataKey::MinPoints, &new_points);
                env.storage().instance().set(&DataKey::MinSlot, &slot);
            }
            return;
        }

        let mut min_pts: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MinPoints)
            .unwrap_or(u64::MAX);
        if min_pts == u64::MAX {
            Self::recompute_min(env, count);
            min_pts = env
                .storage()
                .instance()
                .get(&DataKey::MinPoints)
                .unwrap_or(u64::MAX);
        }
        let min_slot: u32 = env.storage().instance().get(&DataKey::MinSlot).unwrap_or(0);
        // A newcomer tying the current min displaces the OLDEST tied-at-min
        // player (FIFO) instead of being rejected; only strictly weaker players
        // are turned away once the list is full.
        if new_points < min_pts {
            return;
        }

        if let Some(old) = env
            .storage()
            .persistent()
            .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(min_slot))
        {
            env.storage()
                .persistent()
                .remove(&DataKey::TopPlayerSlot(old.address));
        }
        let entry = PlayerEntry {
            address: user.clone(),
            points: new_points,
        };
        env.storage()
            .persistent()
            .set(&DataKey::TopPlayerAt(min_slot), &entry);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::TopPlayerAt(min_slot), TTL_BUMP, TTL_HIGH);
        let sk = DataKey::TopPlayerSlot(user.clone());
        env.storage().persistent().set(&sk, &min_slot);
        env.storage()
            .persistent()
            .extend_ttl(&sk, TTL_BUMP, TTL_HIGH);
        // FIFO: the newcomer is the youngest player, so stamp its reused slot
        // with a fresh (highest) sequence. This makes the NEXT tied newcomer
        // evict the older survivor instead of this freshly-reused slot.
        Self::stamp_seq(env, min_slot);
        // The slot we just overwrote held the oldest tied-at-min player; recompute.
        Self::recompute_min(env, count);
    }

    fn recompute_min(env: &Env, count: u32) {
        let mut min_pts = u64::MAX;
        let mut min_slot: u32 = 0;
        let mut min_seq = u64::MAX;
        for i in 0..count {
            if let Some(e) = env
                .storage()
                .persistent()
                .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(i))
            {
                // The FIFO sequence is read only for the running min / ties, so
                // the ledger footprint stays small when ties are rare.
                if e.points < min_pts {
                    min_pts = e.points;
                    min_slot = i;
                    min_seq = Self::seq_at(env, i);
                } else if e.points == min_pts {
                    let seq = Self::seq_at(env, i);
                    // FIFO: among equal-min players the oldest (lowest sequence)
                    // wins MinSlot, so eviction deterministically targets it.
                    if seq < min_seq {
                        min_slot = i;
                        min_seq = seq;
                    }
                }
            }
        }
        env.storage().instance().set(&DataKey::MinPoints, &min_pts);
        env.storage().instance().set(&DataKey::MinSlot, &min_slot);
    }

    fn seq_at(env: &Env, s: u32) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::TopPlayerSeqAt(s))
            .unwrap_or(0)
    }

    // FIFO age stamp: assign the next monotonically increasing sequence to a slot.
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
}

#[cfg(test)]
mod ttl_tests;