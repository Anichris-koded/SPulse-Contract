#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, vec, Address, Env,
    IntoVal, String, Symbol, Val, Vec,
};

pub const INTERFACE_VERSION: u32 = 1;

const MAX_REFERRAL_DEPTH: u32 = 5;
const WELCOME_BONUS_PTS: u64 = 5;
const WELCOME_BONUS_TOKENS: i128 = 1_0000000;
const REFERRAL_BONUS_PTS: u64 = 3;
const TTL_BUMP: u32 = 3_153_600;
const TTL_HIGH: u32 = 6_307_200;

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ReferralError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotAuthorized = 3,
    AlreadyRegistered = 4,
    InvalidReferrer = 5,
    DepthLimitExceeded = 6,
    SelfReferral = 7,
    ContractPaused = 8,
    IncompatibleInterface = 9,
    TokenNotConfigured = 10,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    MarketContract,
    TokenContract,
    LeaderboardContract,
    XlmSac,
    Referrer(Address),
    DisplayName(Address),
    ReferrerCount(Address),
    Earnings(Address),
    Paused,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferrerInfo {
    pub address: Address,
    pub display_name: String,
    pub referrer_count: u32,
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
        env.storage().instance().set(&DataKey::XlmSac, &xlm_sac);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn interface_version(_env: Env) -> u32 {
        INTERFACE_VERSION
    }

    pub fn pause(env: Env, admin: Address) -> Result<(), ReferralError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events()
            .publish((Symbol::new(&env, "paused"), admin), true);
        Ok(())
    }

    pub fn unpause(env: Env, admin: Address) -> Result<(), ReferralError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events()
            .publish((Symbol::new(&env, "unpaused"), admin), true);
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    pub fn set_token_contract(
        env: Env,
        admin: Address,
        token: Address,
    ) -> Result<(), ReferralError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::TokenContract, &token);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn register_referral(
        env: Env,
        user: Address,
        display_name: String,
        referrer: Option<Address>,
    ) -> Result<(), ReferralError> {
        Self::require_not_paused(&env)?;
        user.require_auth();

        let key = DataKey::Referrer(user.clone());
        if env.storage().persistent().has(&key) {
            return Err(ReferralError::AlreadyRegistered);
        }

        if let Some(ref ref_addr) = referrer {
            if ref_addr == &user {
        
                return Err(ReferralError::SelfReferral);
            }
            if !env.storage().persistent().has(&DataKey::Referrer(ref_addr.clone())) {
                return Err(ReferralError::InvalidReferrer);
            }
            let depth = Self::referral_depth(&env, &ref_addr);
            if depth >= MAX_REFERRAL_DEPTH {
                return Err(ReferralError::DepthLimitExceeded);
            }
            let ref_key = DataKey::ReferrerCount(ref_addr.clone());
            let count: u32 = env.storage().persistent().get(&ref_key).unwrap_or(0);
            env.storage().persistent().set(&ref_key, &(count + 1));
            env.storage()
                .persistent()
                .extend_ttl(&ref_key, TTL_BUMP, TTL_HIGH);
        }

        env.storage().persistent().set(&key, &referrer);
        env.storage()
            .persistent()
            .set(&DataKey::DisplayName(user.clone()), &display_name);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_BUMP, TTL_HIGH);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::DisplayName(user.clone()), TTL_BUMP, TTL_HIGH);

        let lb = Self::leaderboard_contract(&env)?;
        let this = env.current_contract_address();
        let _: Val = env.invoke_contract(
            &lb,
            &Symbol::new(&env, "reward_bonus"),
            vec![
                &env,
                this.into_val(&env),
                user.into_val(&env),
                WELCOME_BONUS_PTS.into_val(&env),
                0_i128.into_val(&env),
            ],
        );

        env.events().publish(
            (Symbol::new(&env, "referral_registered"), user),
            referrer.is_some(),
        );
        Ok(())
    }

    pub fn credit(
        env: Env,
        caller: Address,
        user: Address,
        amount: i128,
    ) -> Result<bool, ReferralError> {
        Self::require_not_paused(&env)?;
        Self::require_market_contract(&env, &caller)?;
        caller.require_auth();

        let referrer: Option<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Referrer(user.clone()));

        match referrer {
            None => {
                let this = env.current_contract_address();
                let xlm_sac: Address = env.storage().instance().get(&DataKey::XlmSac).unwrap();
                let xlm = token::Client::new(&env, &xlm_sac);
                xlm.transfer(&this, &user, &amount);
                Ok(false)
            }
            Some(ref_addr) => {
                let xlm_sac: Address = env.storage().instance().get(&DataKey::XlmSac).unwrap();
                let xlm = token::Client::new(&env, &xlm_sac);
                let this = env.current_contract_address();
                xlm.transfer(&this, &ref_addr, &amount);

                let lb = Self::leaderboard_contract(&env)?;
                let _: Val = env.invoke_contract(
                    &lb,
                    &Symbol::new(&env, "reward_bonus"),
                    vec![
                        &env,
                        this.into_val(&env),
                        ref_addr.into_val(&env),
                        REFERRAL_BONUS_PTS.into_val(&env),
                        0_i128.into_val(&env),
                    ],
                );

                let earnings_key = DataKey::Earnings(ref_addr.clone());
                let earnings: i128 = env
                    .storage()
                    .persistent()
                    .get(&earnings_key)
                    .unwrap_or(0);
                env.storage()
                    .persistent()
                    .set(&earnings_key, &(earnings + amount));
                env.storage()
                    .persistent()
                    .extend_ttl(&earnings_key, TTL_BUMP, TTL_HIGH);

                Ok(true)
            }
        }
    }

    pub fn get_referrer(env: Env, user: Address) -> Option<Address> {
        env.storage()
            .persistent()
            .get(&DataKey::Referrer(user))
    }

    pub fn get_display_name(env: Env, user: Address) -> Option<String> {
        env.storage()
            .persistent()
            .get(&DataKey::DisplayName(user))
    }

    pub fn get_referrer_count(env: Env, referrer: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::ReferrerCount(referrer))
            .unwrap_or(0)
    }

    pub fn get_earnings(env: Env, referrer: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Earnings(referrer))
            .unwrap_or(0)
    }

    fn require_admin(env: &Env, admin: &Address) -> Result<(), ReferralError> {
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(ReferralError::NotInitialized)?;
        if *admin != stored {
            return Err(ReferralError::NotAuthorized);
        }
        Ok(())
    }

    fn require_not_paused(env: &Env) -> Result<(), ReferralError> {
        if env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(ReferralError::ContractPaused);
        }
        Ok(())
    }

    fn require_market_contract(env: &Env, caller: &Address) -> Result<(), ReferralError> {
        let market: Address = env
            .storage()
            .instance()
            .get(&DataKey::MarketContract)
            .ok_or(ReferralError::NotInitialized)?;
        if *caller != market {
            return Err(ReferralError::NotAuthorized);
        }
        Ok(())
    }

    fn leaderboard_contract(env: &Env) -> Result<Address, ReferralError> {
        env.storage()
            .instance()
            .get(&DataKey::LeaderboardContract)
            .ok_or(ReferralError::NotInitialized)
    }

    fn referral_depth(env: &Env, user: &Address) -> u32 {
        let mut depth = 0;
        let mut current = user.clone();
        loop {
            match env
                .storage()
                .persistent()
                .get::<DataKey, Option<Address>>(&DataKey::Referrer(current.clone()))
            {
                None => break,
                Some(None) => break,
                Some(Some(ref_addr)) => {
                    depth += 1;
                    current = ref_addr;
                }
            }
        }
        depth
    }
}

#[cfg(test)]
mod tests;
