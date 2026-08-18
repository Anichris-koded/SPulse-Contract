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
        env.storage().instance().set(&DataKey::TokenContract, &token);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    // ── Bet-settlement path ───────────────────────────────────────────────────

    /// Called by the market contract after a bet is settled.
    /// Updates points + won/lost counts and optionally mints PULSE tokens.
    /// Replaces the old two-call pattern (add_pts + separate mint).
    pub fn reward(
        env: Env,
        caller: Address,
        user: Address,
        pts: u64,
        tokens: i128,
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
            .instance()
            .get(&DataKey::MarketContract)
            .ok_or(LeaderboardError::NotInitialized)?;
        if caller != market {
            return Err(LeaderboardError::UnauthorizedCaller);
        }
        caller.require_auth();
        Ok(())
    }

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
    }

    // ── Internal: maintain a persistent sorted top list ──────────────────────

    fn update_top_players(env: &Env, user: Address, new_points: u64) {
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
            return;
        }

        // New user: insert if list not full or if they beat the current minimum.
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

                let new_entry = PlayerEntry {
                    address: user.clone(),
                    points: new_points,
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
            }
        }
    }
}

#[cfg(test)]
mod ttl_tests;
