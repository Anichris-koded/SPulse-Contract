#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, vec, Address, BytesN, Env, IntoVal,
    String, Symbol, Val,
};

const WELCOME_BONUS_POINTS: u64 = 5;
const WELCOME_BONUS_TOKENS: i128 = 1_0000000;
const REFERRAL_BET_POINTS: u64 = 3;
const TTL_BUMP: u32 = 3_153_600;
const TTL_HIGH: u32 = 6_307_200;

// Issue #84: bump whenever a function signature, argument order, or return
// type that a caller relies on changes.
pub const INTERFACE_VERSION: u32 = 1;

// The leaderboard interface_version this contract was built against. If a
// deployed leaderboard reports a different version, its add_bonus_pts ABI
// may no longer match what we send — refuse the call instead of invoking
// blind and either panicking deep in argument decoding or silently
// misbehaving (issue #84).
const EXPECTED_LEADERBOARD_INTERFACE_VERSION: u32 = 1;

/// Maximum allowed depth of the referral chain. A new registration whose
/// referrer already sits at depth MAX_REFERRAL_DEPTH (i.e. has MAX_REFERRAL_DEPTH
/// ancestors) is rejected. This bounds both gas usage during the on-chain
/// traversal and sybil-amplification attack surface.
const MAX_REFERRAL_DEPTH: u32 = 5;

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ReferralError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    UnauthorizedCaller = 3,
    AlreadyRegistered = 4,
    SelfReferral = 5,
    NotAdmin = 6,
    // Issue #95: operation blocked by the contract being paused.
    Paused = 7,
    // Issue #99: the address given as `referrer` has never registered.
    ReferrerNotRegistered = 7,
    ContractPaused = 7,
    ReferrerNotRegistered = 8,
    /// leaderboard reported an interface_version this contract wasn't built
    /// against (issue #84). Note: a matching version number alone does not
    /// prove the callee's actual function shape still matches, it only
    /// proves the callee's author intended it to. The guarantee only holds
    /// if every breaking ABI change (renamed function, changed argument
    /// order/count/type, changed return type) always increments
    /// INTERFACE_VERSION in the same commit. See EXPECTED_LEADERBOARD_INTERFACE_VERSION.
    IncompatibleInterface = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    MarketContract,
    // ── Legacy per-user keys (pre-Lever-A) — still READ for users who
    //    registered before the upgrade. New registrations no longer write these.
    Referrer(Address),
    DisplayName(Address),
    Registered(Address),
    // ── Lever A: one packed entry per NEW registrant (display_name + referrer).
    //    Existence of this key implies "registered". Cuts a first-time
    //    registration from 3 new entries to 1.
    Profile(Address),
    // ReferralCount/Earnings are the REFERRER's counters (a different user),
    // updated in place — kept as separate keys (not part of the registrant pack).
    ReferralCount(Address),
    ReferralEarnings(Address),
    TokenContract,
    LeaderboardContract,
    XlmSacContract,
    Paused,
}

// Lever A: packed registrant profile — one storage slot instead of three.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserProfile {
    pub display_name: String,
    pub referrer: Option<Address>,
}

#[contract]
pub struct ReferralRegistryContract;

#[contractimpl]
impl ReferralRegistryContract {
    pub fn initialize(
        env: Env,
        admin: Address,
        market_contract: Address,
        token_contract: Address,
        leaderboard_contract: Address,
        xlm_sac: Address,
    ) -> Result<(), ReferralError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(ReferralError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::MarketContract, &market_contract);
        env.storage()
            .instance()
            .set(&DataKey::TokenContract, &token_contract);
        env.storage()
            .instance()
            .set(&DataKey::LeaderboardContract, &leaderboard_contract);
        env.storage()
            .instance()
            .set(&DataKey::XlmSacContract, &xlm_sac);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    /// The cross-contract ABI version this deployment implements (issue #84).
    pub fn interface_version(_env: Env) -> u32 {
        INTERFACE_VERSION
    }

    // ── Upgradeability & Config (admin only) ──────────────────────────────────

    /// Replace this contract's WASM bytecode in place. Admin only.
    pub fn upgrade(
        env: Env,
        admin: Address,
        new_wasm_hash: BytesN<32>,
    ) -> Result<(), ReferralError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
        Ok(())
    }

    /// Correct the native XLM SAC address set at initialize time. Admin only.
    pub fn set_xlm_sac(env: Env, admin: Address, xlm_sac: Address) -> Result<(), ReferralError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::XlmSacContract, &xlm_sac);
        env.events().publish((Symbol::new(&env, "xlm_sac_set"), admin), xlm_sac);
        Ok(())
    }

    /// Halt registration and crediting in an emergency. Admin only. View
    /// functions keep working so the frontend can still read state.
    pub fn pause(env: Env, admin: Address) -> Result<(), ReferralError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((Symbol::new(&env, "paused"), admin), true);
        Ok(())
    }

    /// Resume registration and crediting. Admin only.
    pub fn unpause(env: Env, admin: Address) -> Result<(), ReferralError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events().publish((Symbol::new(&env, "unpaused"), admin), true);
        Ok(())
    }

    /// Issue #95 circuit breaker: halt registration and credit disbursements
    /// during an emergency. Admin only; idempotent. Read/view paths stay open.
    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), ReferralError> {
        Self::require_admin(&env, &caller)?;
        caller.require_auth();
        env.storage().instance().set(&DataKey::Paused, &paused);
        Ok(())
    }

    pub fn paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get::<_, bool>(&DataKey::Paused)
            .unwrap_or(false)
    }

    fn require_not_paused(env: &Env) -> Result<(), ReferralError> {
        if env
            .storage()
            .instance()
            .get::<_, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(ReferralError::Paused);
        }
        Ok(())
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    pub fn register_referral(
        env: Env,
        user: Address,
        display_name: String,
        referrer: Option<Address>,
    ) -> Result<(), ReferralError> {
        Self::require_not_paused(&env)?;
        user.require_auth();
        if Self::is_registered(env.clone(), user.clone()) {
            return Err(ReferralError::AlreadyRegistered);
        }
        if let Some(ref ref_addr) = referrer {
            if *ref_addr == user {
                return Err(ReferralError::SelfReferral);
            }
            // Issue #99: a referral relationship may only point at a registered
            // participant. Without this, anyone could name an arbitrary
            // (attacker-controlled) address as their referrer and have it
            // receive referral fees and accrue ReferralCount/ReferralEarnings
            // without ever registering.
            if !Self::is_registered(env.clone(), ref_addr.clone()) {
                return Err(ReferralError::ReferrerNotRegistered);
            }
        }

        // Lever A: write ONE packed Profile entry (display_name + referrer)
        // instead of the three legacy keys (Registered + DisplayName + Referrer).
        // Existence of Profile(user) is what is_registered() now checks.
        env.storage().persistent().set(
            &DataKey::Profile(user.clone()),
            &UserProfile {
                display_name,
                referrer: referrer.clone(),
            },
        );
        env.storage().persistent().extend_ttl(
            &DataKey::Profile(user.clone()),
            TTL_BUMP,
            TTL_HIGH,
        );
        // The referrer's counter is a DIFFERENT user's entry — update in place.
        if let Some(ref ref_addr) = referrer {
            let count: u32 = env
                .storage()
                .persistent()
                .get(&DataKey::ReferralCount(ref_addr.clone()))
                .unwrap_or(0);
            let count_key = DataKey::ReferralCount(ref_addr.clone());
            env.storage()
                .persistent()
                .set(&count_key, &(count + 1));
            env.storage()
                .persistent()
                .extend_ttl(&count_key, TTL_BUMP, TTL_HIGH);
        }

        let this = env.current_contract_address();
        let leaderboard: Address = env
            .storage()
            .instance()
            .get(&DataKey::LeaderboardContract)
            .unwrap();
        // The leaderboard API renamed reward_bonus -> add_bonus_pts (which
        // enforces caller == ReferralContract and no longer takes a token
        // amount); keep the welcome-bonus points call in sync. The 1 PULSE
        // welcome bonus is minted directly here — the registry is the
        // authorized minter for this wiring.
        let _: Val = env.invoke_contract(
            &leaderboard,
            &Symbol::new(&env, "add_bonus_pts"),
            vec![
                &env,
                this.clone().into_val(&env),
            vec![
                &env,
                this.clone().into_val(&env),
        // Queue the welcome reward as an optional side effect. Registration
        // remains successful if the leaderboard is paused, unavailable, or
        // exceeds the remaining invocation budget.
        let _ = env.try_invoke_contract::<Val, soroban_sdk::Error>(
            &leaderboard,
            &Symbol::new(&env, "queue_bonus_reward"),
            vec![
                &env,
                this.into_val(&env),
                user.clone().into_val(&env),
                WELCOME_BONUS_POINTS.into_val(&env),
            ],
        );
        let token_contract: Address = env
            .storage()
            .instance()
            .get(&DataKey::TokenContract)
            .unwrap();
        // PULSE is a custom token contract; mint via its exported `mint` ABI.
        let _: Val = env.invoke_contract(
            &token_contract,
            &Symbol::new(&env, "mint"),
            vec![
                &env,
                this.clone().into_val(&env),
                user.clone().into_val(&env),
                WELCOME_BONUS_TOKENS.into_val(&env),
            ],
        );
        env.events().publish(
            (Symbol::new(&env, "referral_registered"), user),
            referrer,
        );
        Ok(())
    }

    pub fn credit(
        env: Env,
        caller: Address,
        user: Address,
        referral_fee: i128,
    ) -> Result<bool, ReferralError> {
        Self::require_not_paused(&env)?;
        caller.require_auth();
        Self::require_market_contract(&env, &caller)?;
        // Lever A: resolve referrer via packed Profile (new) or legacy key (old).
        let referrer: Option<Address> = Self::load_profile(&env, &user).and_then(|p| p.referrer);
        match referrer {
            Some(ref_addr) => {
                // Issue #99 defense-in-depth: even if a referral relationship
                // exists in storage (e.g. written by pre-validation legacy code
                // or malformed state), never pay out to — or accrue counters
                // for — an address that is not a registered participant.
                // The fee is refunded to the caller and we report "no referral",
                // so the market keeps it in AccumulatedFees.
                if !Self::is_registered(env.clone(), ref_addr.clone()) {
                    if referral_fee > 0 {
                        let xlm_sac: Address = env
                            .storage()
                            .instance()
                            .get(&DataKey::XlmSacContract)
                            .unwrap();
                        token::Client::new(&env, &xlm_sac).transfer(
                            &env.current_contract_address(),
                            &caller,
                            &referral_fee,
                        );
                    }
                    return Ok(false);
                }

                let xlm_sac: Address = env
                    .storage()
                    .instance()
                    .get(&DataKey::XlmSacContract)
                    .unwrap();
                token::Client::new(&env, &xlm_sac).transfer(
                    &env.current_contract_address(),
                    &ref_addr,
                    &referral_fee,
                );
                let leaderboard: Address = env
                    .storage()
                    .instance()
                    .get(&DataKey::LeaderboardContract)
                    .unwrap();
                // Queue points as an optional side effect. The XLM transfer
                // and earnings update remain critical referral accounting.
                let _ = env.try_invoke_contract::<Val, soroban_sdk::Error>(
                    &leaderboard,
                    &Symbol::new(&env, "queue_bonus_reward"),
                    vec![
                        &env,
                        env.current_contract_address().into_val(&env),
                        ref_addr.clone().into_val(&env),
                        REFERRAL_BET_POINTS.into_val(&env),
                        0_i128.into_val(&env),
                    ],
                );
                let earnings: i128 = env
                    .storage()
                    .persistent()
                    .get(&DataKey::ReferralEarnings(ref_addr.clone()))
                    .unwrap_or(0);
                let earn_key = DataKey::ReferralEarnings(ref_addr.clone());
                env.storage()
                    .persistent()
                    .set(&earn_key, &(earnings + referral_fee));
                env.storage()
                    .persistent()
                    .extend_ttl(&earn_key, TTL_BUMP, TTL_HIGH);
                env.events().publish(
                    (Symbol::new(&env, "referral_credited"), user, ref_addr),
                    referral_fee,
                );
                Ok(true)
            }
            None => {
                if referral_fee > 0 {
                    let xlm_sac: Address = env
                        .storage()
                        .instance()
                        .get(&DataKey::XlmSacContract)
                        .unwrap();
                    token::Client::new(&env, &xlm_sac).transfer(
                        &env.current_contract_address(),
                        &caller,
                        &referral_fee,
                    );
                }
                env.events().publish(
                    (Symbol::new(&env, "referral_missed"), user),
                    referral_fee,
                );
                Ok(false)
            }
        }
    }

    /// Issue #78: Admin-callable legacy migration. Reads old Registered/DisplayName/Referrer keys,
    /// writes the packed Profile entry, and removes the legacy keys. Idempotent — no-op if
    /// Profile already exists.
    pub fn migrate_user(env: Env, admin: Address, user: Address) -> Result<(), ReferralError> {
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap();
        if admin != stored_admin {
            return Err(ReferralError::NotAdmin);
        }
        admin.require_auth();
        // Already migrated — Profile exists, nothing to do
        if env
            .storage()
            .persistent()
            .has(&DataKey::Profile(user.clone()))
        {
            return Ok(());
        }
        // No legacy keys — nothing to migrate
        if !env
            .storage()
            .persistent()
            .get::<_, bool>(&DataKey::Registered(user.clone()))
            .unwrap_or(false)
        {
            return Ok(());
        }
        let display_name: String = env
            .storage()
            .persistent()
            .get(&DataKey::DisplayName(user.clone()))
            .unwrap_or_else(|| String::from_str(&env, "user"));
        let referrer: Option<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Referrer(user.clone()));
        // Write the packed profile
        env.storage().persistent().set(
            &DataKey::Profile(user.clone()),
            &UserProfile {
                display_name,
                referrer: referrer.clone(),
            },
        );
        // If this user has a referrer, increment that referrer's count
        if let Some(ref ref_addr) = referrer {
            let count: u32 = env
                .storage()
                .persistent()
                .get(&DataKey::ReferralCount(ref_addr.clone()))
                .unwrap_or(0);
            env.storage()
                .persistent()
                .set(&DataKey::ReferralCount(ref_addr.clone()), &(count + 1));
        }
        // Remove legacy keys
        env.storage().persistent().remove(&DataKey::Registered(user.clone()));
        env.storage().persistent().remove(&DataKey::DisplayName(user.clone()));
        env.storage().persistent().remove(&DataKey::Referrer(user));
        Ok(())
    }

    fn load_profile(env: &Env, user: &Address) -> Option<UserProfile> {
        if let Some(p) = env
            .storage()
            .persistent()
            .get::<DataKey, UserProfile>(&DataKey::Profile(user.clone()))
        {
            return Some(p);
        }
        // Legacy fallback: reconstruct a profile from the old keys.
        if env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::Registered(user.clone()))
            .unwrap_or(false)
        {
            let display_name = env
                .storage()
                .persistent()
                .get(&DataKey::DisplayName(user.clone()))
                .unwrap_or_else(|| String::from_str(env, ""));
            let referrer = env
                .storage()
                .persistent()
                .get(&DataKey::Referrer(user.clone()));
            return Some(UserProfile {
                display_name,
                referrer,
            });
        }
        None
    }

    pub fn get_referrer(env: Env, user: Address) -> Option<Address> {
        Self::load_profile(&env, &user).and_then(|p| p.referrer)
    }

    pub fn get_display_name(env: Env, user: Address) -> String {
        Self::load_profile(&env, &user)
            .map(|p| p.display_name)
            .unwrap_or_else(|| String::from_str(&env, ""))
    }

    pub fn get_referral_count(env: Env, user: Address) -> u32 {
        let key = DataKey::ReferralCount(user);
        let count = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(0);
        if env.storage().persistent().has(&key) {
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_BUMP, TTL_HIGH);
        }
        count
    }

    pub fn get_earnings(env: Env, user: Address) -> i128 {
        let key = DataKey::ReferralEarnings(user);
        let earnings = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(0);
        if env.storage().persistent().has(&key) {
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_BUMP, TTL_HIGH);
        }
        earnings
    }

    /// Permissionless keeper: extend a referrer's count/earnings + profile
    /// so inactive referrers do not lose history (issue #28 / #54).
    pub fn refresh_referrer_ttl(env: Env, user: Address) {
        let keys = [
            DataKey::Profile(user.clone()),
            DataKey::ReferralCount(user.clone()),
            DataKey::ReferralEarnings(user.clone()),
            DataKey::Registered(user.clone()),
            DataKey::Referrer(user.clone()),
            DataKey::DisplayName(user),
        ];
        for key in keys {
            if env.storage().persistent().has(&key) {
                env.storage()
                    .persistent()
                    .extend_ttl(&key, TTL_BUMP, TTL_HIGH);
            }
        }
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
    }

    pub fn has_referrer(env: Env, user: Address) -> bool {
        Self::get_referrer(env, user).is_some()
    }

    pub fn is_registered(env: Env, user: Address) -> bool {
        Self::load_profile(&env, &user).is_some()
    }

    fn require_market_contract(env: &Env, caller: &Address) -> Result<(), ReferralError> {
        let market: Address = env
            .storage()
            .instance()
            .get(&DataKey::MarketContract)
            .ok_or(ReferralError::NotInitialized)?;
        if *caller != market {
            return Err(ReferralError::UnauthorizedCaller);
        }
        Ok(())
    }

    fn require_admin(env: &Env, caller: &Address) -> Result<(), ReferralError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(ReferralError::NotInitialized)?;
        if *caller != admin {
            return Err(ReferralError::NotAdmin);
        }
        Ok(())
    }

    // Issue #84: verify the configured leaderboard contract reports the ABI
    // version we were built against before invoking it. Catches a unilateral
    // leaderboard upgrade that changed add_pts/add_bonus_pts's signature and
    // turns what would otherwise be an opaque invoke_contract failure (or,
    // worse, a type-compatible-but-semantically-different call) into a clear
    // IncompatibleInterface error.
    fn require_compatible_leaderboard(env: &Env, leaderboard: &Address) -> Result<(), ReferralError> {
        let version: u32 = env.invoke_contract(
            leaderboard,
            &Symbol::new(env, "interface_version"),
            vec![env],
        );
        if version != EXPECTED_LEADERBOARD_INTERFACE_VERSION {
            return Err(ReferralError::IncompatibleInterface);
        }
        Ok(())
    }

    fn require_not_paused(env: &Env) -> Result<(), ReferralError> {
        if Self::is_paused(env.clone()) {
            return Err(ReferralError::ContractPaused);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
