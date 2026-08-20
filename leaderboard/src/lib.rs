#![no_std]
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
    // Pull-based reward queue (issue #86): reward side-effects are written
    // here and claimed by the user in a separate transaction, decoupling
    // the fund-movement critical path from the optional reward side-effects.
    PendingReward(Address),
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

// Pull-based pending reward — queued by reward()/reward_bonus(), claimed
// later via claim_pending_rewards().  Separate fields allow safe accumulation
// when the user earns multiple rewards before claiming.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingReward {
    pub points: u64,
    pub tokens: i128,
    pub won_delta: u32,  // 0 or 1 — added to won_bets on claim
    pub lost_delta: u32, // 0 or 1 — added to lost_bets on claim
    pub bet_delta: u32,  // total activity counter increment
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

    /// Set or update the PULSE token contract address. Admin only.
    /// Required before reward() / reward_bonus() can mint tokens.
    /// Also accessible as set_token() for shorter call sites.
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

    /// Alias for set_token_contract — shorter name used in tests and frontend.
    pub fn set_token(env: Env, admin: Address, token: Address) -> Result<(), LeaderboardError> {
        Self::set_token_contract(env, admin, token)
    }

    // ── Lever G: consolidated reward entry points ─────────────────────────────
    //
    // reward() replaces the old market→leaderboard.add_pts + market→token.mint
    // two-hop pattern.  The market makes ONE cross-contract call; the
    // leaderboard handles the token mint internally.
    //
    // reward_bonus() does the same for the referral/welcome-bonus path.
    //
    // When tokens == 0 (e.g., no token contract set or test harness) the
    // minting step is skipped — the points update still happens.

    /// Called by the market contract at claim-time.
    /// Updates points + win/loss counters and optionally mints PULSE tokens.
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

    /// Called by the referral contract for welcome bonuses and per-bet bonuses.
    /// Updates points (no win/loss) and optionally mints PULSE tokens.
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

    // ── Pull-based reward queue (issue #86) ───────────────────────────────────
    //
    // queue_reward() / queue_bonus_reward() write a PendingReward entry
    // without performing any cross-contract calls.  The caller (market /
    // referral) stores the intent; the user later calls claim_pending_rewards()
    // in a separate transaction to actually apply points + mint tokens.
    //
    // This decouples the "critical" fund-movement from the "optional"
    // reward side-effects: place_bet / credit cannot be bricked by a gas
    // overrun in the leaderboard's update_top_players.

    /// Queue a settlement reward (win/loss) without executing it yet.
    /// Only the market contract may call this.
    pub fn queue_reward(
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

        Self::accumulate_pending(&env, &user, pts, tokens, is_won, false);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// Queue a bonus reward (referral/welcome) without executing it yet.
    /// Only the referral contract may call this.
    pub fn queue_bonus_reward(
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

        Self::accumulate_pending(&env, &user, pts, tokens, false, true);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// Claim all pending rewards for `user` in a separate transaction.
    /// Anyone can submit this on behalf of the user — the tokens always go
    /// to `user`, and only the stored pending entry is consumed.
    pub fn claim_pending_rewards(env: Env, user: Address) -> Result<(), LeaderboardError> {
        let key = DataKey::PendingReward(user.clone());
        let pending: PendingReward = match env.storage().persistent().get(&key) {
            Some(p) => p,
            None => return Ok(()), // nothing to claim — silent no-op
        };

        // Remove before applying to prevent double-claim.
        env.storage().persistent().remove(&key);

        // Apply stats + top-players update.
        let mut stats: PlayerStats = env
            .storage()
            .persistent()
            .get(&DataKey::Stats(user.clone()))
            .unwrap_or(PlayerStats { points: 0, total_bets: 0, won_bets: 0, lost_bets: 0 });

        stats.points += pending.points;
        stats.total_bets += pending.bet_delta;
        stats.won_bets += pending.won_delta;
        stats.lost_bets += pending.lost_delta;

        env.storage().persistent().set(&DataKey::Stats(user.clone()), &stats);
        env.storage().persistent().extend_ttl(&DataKey::Stats(user.clone()), TTL_BUMP, TTL_HIGH);
        Self::update_top_players(&env, user.clone(), stats.points);

        // Mint tokens if requested and token contract is configured.
        if pending.tokens > 0 {
            if let Some(token_addr) = env.storage().instance().get::<DataKey, Address>(&DataKey::TokenContract) {
                let this = env.current_contract_address();
                let _: Val = env.invoke_contract(
                    &token_addr,
                    &Symbol::new(&env, "mint"),
                    vec![
                        &env,
                        this.into_val(&env),
                        user.into_val(&env),
                        pending.tokens.into_val(&env),
                    ],
                );
            }
        }

        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// Return the pending (not-yet-claimed) reward entry for a user, or None.
    pub fn get_pending_reward(env: Env, user: Address) -> Option<PendingReward> {
        env.storage().persistent().get(&DataKey::PendingReward(user))
    }

    // ── Legacy entry points (preserved for ABI compatibility) ─────────────────

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

    /// No-op stub kept for ABI compatibility.
    /// total_bets is now derived from won_bets + lost_bets + bonus_bets,
    /// so explicit record_bet calls are no longer needed.
    pub fn record_bet(
        env: Env,
        _caller: Address,
        _user: Address,
    ) -> Result<(), LeaderboardError> {
        // Verify the contract is initialized before accepting any call.
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(LeaderboardError::NotInitialized);
        }
        // Intentional no-op — total_bets is maintained by add_pts / add_bonus_pts.
        Ok(())
    }

    // ── View Functions ────────────────────────────────────────────────────────

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

    /// Alias for get_top_player_count — shorter name used in tests and frontend.
    pub fn get_player_count(env: Env) -> u32 {
        Self::get_top_player_count(env)
    }

    /// Return the 1-based rank of `user` in the top-players list, or 0 if not ranked.
    /// Rank 1 = highest points. O(n) over the top list (at most MAX_TOP_PLAYERS = 50).
    pub fn get_rank(env: Env, user: Address) -> u32 {
        // Fast path: look up the user's slot directly.
        let slot: Option<u32> = env
            .storage()
            .persistent()
            .get(&DataKey::TopPlayerSlot(user.clone()));
        match slot {
            None => 0, // not in the top list
            Some(s) => {
                // The list is maintained in descending order (slot 0 = highest).
                // rank = slot + 1 (1-based).
                s + 1
            }
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

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Apply a reward immediately: update stats, update top-players, optionally mint.
    fn apply_reward(env: &Env, user: &Address, pts: u64, tokens: i128, is_won: bool, is_bonus: bool) {
        let mut stats: PlayerStats = env
            .storage()
            .persistent()
            .get(&DataKey::Stats(user.clone()))
            .unwrap_or(PlayerStats { points: 0, total_bets: 0, won_bets: 0, lost_bets: 0 });

        stats.points += pts;
        stats.total_bets += 1;
        if is_bonus {
            // bonus awards don't affect won/lost
        } else if is_won {
            stats.won_bets += 1;
        } else {
            stats.lost_bets += 1;
        }

        env.storage().persistent().set(&DataKey::Stats(user.clone()), &stats);
        env.storage().persistent().extend_ttl(&DataKey::Stats(user.clone()), TTL_BUMP, TTL_HIGH);
        Self::update_top_players(env, user.clone(), stats.points);

        // Mint tokens inline if a token contract is configured.
        if tokens > 0 {
            if let Some(token_addr) = env.storage().instance().get::<DataKey, Address>(&DataKey::TokenContract) {
                let this = env.current_contract_address();
                let _: Val = env.invoke_contract(
                    &token_addr,
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

    /// Accumulate a pending reward without applying it yet.
    fn accumulate_pending(
        env: &Env,
        user: &Address,
        pts: u64,
        tokens: i128,
        is_won: bool,
        is_bonus: bool,
    ) {
        let key = DataKey::PendingReward(user.clone());
        let mut pending: PendingReward = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(PendingReward { points: 0, tokens: 0, won_delta: 0, lost_delta: 0, bet_delta: 0 });

        pending.points += pts;
        pending.tokens += tokens;
        pending.bet_delta += 1;
        if is_bonus {
            // no won/lost delta
        } else if is_won {
            pending.won_delta += 1;
        } else {
            pending.lost_delta += 1;
        }

        env.storage().persistent().set(&key, &pending);
        env.storage().persistent().extend_ttl(&key, TTL_BUMP, TTL_HIGH);
    }

    // ── Internal: maintain a persistent sorted top list ──────────────────────

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
            env.storage().persistent().set(&DataKey::TopPlayerAt(slot), &entry);
            env.storage().persistent().extend_ttl(&DataKey::TopPlayerAt(slot), TTL_BUMP, TTL_HIGH);

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
                    env.storage().persistent().set(&DataKey::TopPlayerAt(current - 1), &entry);
                    env.storage().persistent().set(&DataKey::TopPlayerAt(current), &prev);
                    env.storage().persistent().set(&DataKey::TopPlayerSlot(entry.address.clone()), &(current - 1));
                    env.storage().persistent().set(&DataKey::TopPlayerSlot(prev.address.clone()), &current);
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
                env.storage().instance().set(&DataKey::MinPoints, &min_entry.points);
                env.storage().instance().set(&DataKey::MinSlot, &min_slot);
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
            env.storage().persistent().set(&DataKey::TopPlayerSlot(user.clone()), &slot);
            env.storage().persistent().extend_ttl(&DataKey::TopPlayerAt(slot), TTL_BUMP, TTL_HIGH);
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
                    env.storage().persistent().set(&DataKey::TopPlayerAt(current - 1), &entry);
                    env.storage().persistent().set(&DataKey::TopPlayerAt(current), &prev);
                    env.storage().persistent().set(&DataKey::TopPlayerSlot(entry.address.clone()), &(current - 1));
                    env.storage().persistent().set(&DataKey::TopPlayerSlot(prev.address.clone()), &current);
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
                env.storage().instance().set(&DataKey::MinPoints, &min_entry.points);
                env.storage().instance().set(&DataKey::MinSlot, &min_slot);
            }
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
                env.storage().persistent().set(&DataKey::TopPlayerAt(min_slot), &new_entry);
                env.storage().persistent().set(&DataKey::TopPlayerSlot(user.clone()), &min_slot);
                env.storage().persistent().extend_ttl(&DataKey::TopPlayerAt(min_slot), TTL_BUMP, TTL_HIGH);

                // Bubble up from min_slot.
                let mut current = min_slot;
                while current > 0 {
                    let prev: PlayerEntry = env
                        .storage()
                        .persistent()
                        .get(&DataKey::TopPlayerAt(current - 1))
                        .unwrap();
                    if prev.points < new_entry.points {
                        env.storage().persistent().set(&DataKey::TopPlayerAt(current - 1), &new_entry);
                        env.storage().persistent().set(&DataKey::TopPlayerAt(current), &prev);
                        env.storage().persistent().set(&DataKey::TopPlayerSlot(new_entry.address.clone()), &(current - 1));
                        env.storage().persistent().set(&DataKey::TopPlayerSlot(prev.address.clone()), &current);
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
                env.storage().instance().set(&DataKey::MinPoints, &new_min_entry.points);
                env.storage().instance().set(&DataKey::MinSlot, &new_min_slot);
            }
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod ttl_tests;
