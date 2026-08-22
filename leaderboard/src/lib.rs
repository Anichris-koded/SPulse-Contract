#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, vec, Address, Env, IntoVal,
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
    // Write-time ordered index: slot 0 is highest points and the final slot
    // is the current minimum. Pagination reads these slots directly.
    TopPlayerAt(u32),
    TopPlayerCount,
    // Reverse lookup used to update an existing player without scanning.
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

    /// Compatibility alias used by the market and referral integration tests.
    pub fn set_token(env: Env, admin: Address, token: Address) -> Result<(), LeaderboardError> {
        Self::set_token_contract(env, admin, token)
    }

    /// Apply a settlement reward and optionally mint its token reward.
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
        Self::apply_reward(&env, &user, pts, tokens, is_won, false);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// Apply a referral or welcome bonus and optionally mint its token reward.
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
        Self::apply_reward(&env, &user, pts, tokens, false, true);
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

    /// Legacy no-op retained for callers that record activity separately from
    /// settlement. Activity is counted by add_pts/reward instead.
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

        // The index is bounded by MAX_PAGE_SIZE so each read has predictable
        // resource usage. Pagination reads only the requested slots; ordering
        // is maintained by update_top_players at write time.
        let page_size = page_size.min(MAX_PAGE_SIZE);
        let end = offset.saturating_add(page_size).min(count);
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

    /// Compatibility alias for clients using the original method name.
    pub fn get_player_count(env: Env) -> u32 {
        Self::get_top_player_count(env)
    }

    /// Return a 1-based rank for an entry in the persistent ordered index.
    /// Returns 0 when the user is not currently ranked.
    pub fn get_rank(env: Env, user: Address) -> u32 {
        let slot: u32 = match env
            .storage()
            .persistent()
            .get(&DataKey::TopPlayerSlot(user.clone()))
        {
            Some(slot) => slot,
            None => return 0,
        };

        match env.storage().persistent().get::<_, PlayerEntry>(&DataKey::TopPlayerAt(slot)) {
            Some(entry) if entry.address == user => slot + 1,
            _ => 0,
        }
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

    fn apply_reward(
        env: &Env,
        user: &Address,
        pts: u64,
        tokens: i128,
        is_won: bool,
        is_bonus: bool,
    ) {
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
        if !is_bonus {
            if is_won {
                stats.won_bets += 1;
            } else {
                stats.lost_bets += 1;
            }
        }

        let stats_key = DataKey::Stats(user.clone());
        env.storage().persistent().set(&stats_key, &stats);
        env.storage().persistent().extend_ttl(&stats_key, TTL_BUMP, TTL_HIGH);
        Self::update_top_players(env, user.clone(), stats.points);

        if tokens > 0 {
            if let Some(token) = env
                .storage()
                .instance()
                .get::<_, Address>(&DataKey::TokenContract)
            {
                let this = env.current_contract_address();
                let _: Val = env.invoke_contract(
                    &token,
                    &Symbol::new(env, "mint"),
                    vec![
                        env,
                        this.into_val(env),
                        user.clone().into_val(env),
                        tokens.into_val(env),
                    ],
                );
            }
        }
    }

    fn refresh_min(env: &Env, count: u32) {
        if count == 0 {
            return;
        }
        let min_slot = count - 1;
        if let Some(entry) = env
            .storage()
            .persistent()
            .get::<_, PlayerEntry>(&DataKey::TopPlayerAt(min_slot))
        {
            env.storage().instance().set(&DataKey::MinPoints, &entry.points);
            env.storage().instance().set(&DataKey::MinSlot, &min_slot);
        }
    }

    // Keep both halves of the ordered index alive together. A slot can move
    // during insertion or eviction, so refreshing only the value would allow
    // the reverse lookup to expire and create a duplicate entry later.
    fn set_top_entry(env: &Env, slot: u32, entry: &PlayerEntry) {
        let key = DataKey::TopPlayerAt(slot);
        env.storage().persistent().set(&key, entry);
        env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
    }

    fn set_top_slot(env: &Env, address: &Address, slot: u32) {
        let key = DataKey::TopPlayerSlot(address.clone());
        env.storage().persistent().set(&key, &slot);
        env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
    }

    fn update_top_players(env: &Env, user: Address, new_points: u64) {
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TopPlayerCount)
            .unwrap_or(0);

        // If user is already in the list, update their points and re-sort in place.
        if let Some(slot) = env.storage().persistent().get::<_, u32>(&DataKey::TopPlayerSlot(user.clone())) {
            let mut entry: PlayerEntry = env
                .storage()
                .persistent()
                .get(&DataKey::TopPlayerAt(slot))
                .unwrap();
            entry.points = new_points;
            Self::set_top_entry(env, slot, &entry);

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
                    Self::set_top_entry(env, current - 1, &entry);
                    Self::set_top_entry(env, current, &prev);
                    Self::set_top_slot(env, &entry.address, current - 1);
                    Self::set_top_slot(env, &prev.address, current);
                    current -= 1;
                } else {
                    break;
                }
            }

            Self::refresh_min(env, count);
            return;
        }

        // New user: insert if list not full or if they beat the current minimum.
        if count < MAX_TOP_PLAYERS {
            let slot = count;
            let entry = PlayerEntry {
                address: user.clone(),
                points: new_points,
            };
            Self::set_top_entry(env, slot, &entry);
            Self::set_top_slot(env, &user, slot);
            env.storage().instance().set(&DataKey::TopPlayerCount, &(count + 1));

            // Bubble up to maintain order.
            let mut current = slot;
            while current > 0 {
                let prev: PlayerEntry = env
                    .storage()
                    .persistent()
                    .get(&DataKey::TopPlayerAt(current - 1))
                    .unwrap();
                if prev.points < entry.points {
                    Self::set_top_entry(env, current - 1, &entry);
                    Self::set_top_entry(env, current, &prev);
                    Self::set_top_slot(env, &entry.address, current - 1);
                    Self::set_top_slot(env, &prev.address, current);
                    current -= 1;
                } else {
                    break;
                }
            }

            // The ordered index always keeps its minimum at the last slot.
            Self::refresh_min(env, count + 1);
        } else {
            // List full: replace the minimum if the new points beat it.
            let min_points: u64 = env.storage().instance().get(&DataKey::MinPoints).unwrap_or(0);
            if new_points > min_points {
                let min_slot: u32 = env.storage().instance().get(&DataKey::MinSlot).unwrap_or(0);
                let old_entry: PlayerEntry = env
                    .storage()
                    .persistent()
                    .get(&DataKey::TopPlayerAt(min_slot))
                    .unwrap();

                // Remove old slot mapping.
                env.storage().persistent().remove(&DataKey::TopPlayerSlot(old_entry.address.clone()));

                let new_entry = PlayerEntry {
                    address: user.clone(),
                    points: new_points,
                };
                Self::set_top_entry(env, min_slot, &new_entry);
                Self::set_top_slot(env, &user, min_slot);

                // Bubble up from min_slot.
                let mut current = min_slot;
                while current > 0 {
                    let prev: PlayerEntry = env
                        .storage()
                        .persistent()
                        .get(&DataKey::TopPlayerAt(current - 1))
                        .unwrap();
                    if prev.points < new_entry.points {
                        Self::set_top_entry(env, current - 1, &new_entry);
                        Self::set_top_entry(env, current, &prev);
                        Self::set_top_slot(env, &new_entry.address, current - 1);
                        Self::set_top_slot(env, &prev.address, current);
                        current -= 1;
                    } else {
                        break;
                    }
                }

                // The replacement was inserted into the sorted index, so the
                // minimum is again the final slot.
                Self::refresh_min(env, count);
            }
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod ttl_tests;
