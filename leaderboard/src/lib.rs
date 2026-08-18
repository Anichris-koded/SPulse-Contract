#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, vec, Address, Env, IntoVal, Symbol, Val,
    Vec,
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
    ContractPaused = 6,
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

    pub fn reward(
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
        Self::credit_points(&env, &user, pts, Some(is_won));
        Ok(())
    }

    pub fn reward_bonus(
        env: Env,
        caller: Address,
        user: Address,
        points: u64,
        tokens: i128,
    ) -> Result<(), LeaderboardError> {
        caller.require_auth();
        Self::require_referral_contract(&env, &caller)?;
        if points == 0 {
            return Err(LeaderboardError::InvalidPoints);
        }
        Self::credit_points(&env, &user, points, None);
        Self::mint_tokens(&env, &user, tokens)
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

    fn require_not_paused(env: &Env) -> Result<(), LeaderboardError> {
        if Self::is_paused(env.clone()) {
            return Err(LeaderboardError::ContractPaused);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod ttl_tests;
