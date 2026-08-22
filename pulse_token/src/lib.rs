#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, BytesN, Env, String, Symbol,
};

// Issue #84: bump whenever a function signature, argument order, or return
// type that a caller relies on changes, so a caller pinning this version can
// detect an incompatible upgrade before invoking.
pub const INTERFACE_VERSION: u32 = 1;

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TokenError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    UnauthorizedMinter = 3,
    InsufficientBalance = 4,
    InvalidAmount = 5,
    NotAdmin = 6,
    InsufficientAllowance = 7,
    InvalidExpirationLedger = 8,
    ContractPaused = 9,
    AlreadyMinter = 10,
    NotMinter = 11,
    MinterListFull = 12,
}

// TTL: ~1yr threshold, ~2yr extend
const TTL_BUMP: u32 = 3_153_600;
const TTL_HIGH: u32 = 6_307_200;
const MAX_MINTERS: u32 = 10;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    AuthorizedMinter(Address),
    Balance(Address),
    TotalSupply,
    Name,
    Symbol,
    Decimals,
    Allowance(Address, Address),
    Paused,
    MinterAt(u32),
    MinterCount,
    MinterIndex(Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllowanceValue {
    pub amount: i128,
    pub expiration_ledger: u32,
}

#[contract]
pub struct PULSETokenContract;

#[contractimpl]
impl PULSETokenContract {
    pub fn initialize(
        env: Env,
        admin: Address,
        name: String,
        symbol: String,
        decimals: u32,
    ) -> Result<(), TokenError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TokenError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Name, &name);
        env.storage().instance().set(&DataKey::Symbol, &symbol);
        env.storage().instance().set(&DataKey::Decimals, &decimals);
        env.storage().instance().set(&DataKey::TotalSupply, &0_i128);
        env.events().publish(
            (Symbol::new(&env, "initialized"), admin),
            (name, symbol, decimals),
        );
        Ok(())
    }

    /// Replace this contract's WASM in place. Admin only. Balances preserved.
    pub fn upgrade(env: Env, admin: Address, new_wasm_hash: BytesN<32>) -> Result<(), TokenError> {
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TokenError::NotInitialized)?;
        if admin != stored {
            return Err(TokenError::NotAdmin);
        }
        admin.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
        Ok(())
    }

    /// The cross-contract ABI version this deployment implements (issue #84).
    pub fn interface_version(_env: Env) -> u32 {
        INTERFACE_VERSION
    }

    /// Halt mint/transfer/burn in an emergency. Admin only. View functions
    /// (balance, total_supply, ...) keep working so integrators can still
    /// read state while the contract is paused.
    pub fn pause(env: Env, admin: Address) -> Result<(), TokenError> {
        let stored = Self::require_admin(&env)?;
        if admin != stored {
            return Err(TokenError::NotAdmin);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((Symbol::new(&env, "paused"), admin), true);
        Ok(())
    }

    /// Resume mint/transfer/burn. Admin only.
    pub fn unpause(env: Env, admin: Address) -> Result<(), TokenError> {
        let stored = Self::require_admin(&env)?;
        if admin != stored {
            return Err(TokenError::NotAdmin);
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

    pub fn set_minter(env: Env, minter: Address) -> Result<(), TokenError> {
        let admin: Address = Self::require_admin(&env)?;
        admin.require_auth();
        if env
            .storage()
            .persistent()
            .get::<_, bool>(&DataKey::AuthorizedMinter(minter.clone()))
            .unwrap_or(false)
        {
            return Err(TokenError::AlreadyMinter);
        }
        env.storage()
            .persistent()
            .set(&DataKey::AuthorizedMinter(minter.clone()), &true);
        // Track in the audit list
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MinterCount)
            .unwrap_or(0);
        if count >= MAX_MINTERS {
            return Err(TokenError::MinterListFull);
        }
        env.storage()
            .persistent()
            .set(&DataKey::MinterIndex(minter.clone()), &count);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::MinterIndex(minter.clone()), TTL_BUMP, TTL_HIGH);
        env.storage()
            .instance()
            .set(&DataKey::MinterAt(count), &minter);
        env.storage()
            .instance()
            .set(&DataKey::MinterCount, &(count + 1));
        env.events().publish((Symbol::new(&env, "minter_added"), minter), true);
        Ok(())
    }

    pub fn remove_minter(env: Env, minter: Address) -> Result<(), TokenError> {
        let admin: Address = Self::require_admin(&env)?;
        admin.require_auth();
        if !env
            .storage()
            .persistent()
            .get::<_, bool>(&DataKey::AuthorizedMinter(minter.clone()))
            .unwrap_or(false)
        {
            return Err(TokenError::NotMinter);
        }
        env.storage()
            .persistent()
            .remove(&DataKey::AuthorizedMinter(minter));
        Ok(())
    }

    pub fn get_authorized_minters(env: Env) -> soroban_sdk::Vec<Address> {
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MinterCount)
            .unwrap_or(0);
        let mut result: soroban_sdk::Vec<Address> = soroban_sdk::Vec::new(&env);
        for i in 0..count {
            if let Some(addr) = env
                .storage()
                .instance()
                .get::<DataKey, Address>(&DataKey::MinterAt(i))
            {
                if env
                    .storage()
                    .persistent()
                    .get::<_, bool>(&DataKey::AuthorizedMinter(addr.clone()))
                    .unwrap_or(false)
                {
                    result.push_back(addr);
                }
            }
        }
        result
    }

    pub fn mint(env: Env, minter: Address, to: Address, amount: i128) -> Result<(), TokenError> {
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(TokenError::InvalidAmount);
        }
        minter.require_auth();
        let is_minter: bool = env
            .storage()
            .persistent()
            .get(&DataKey::AuthorizedMinter(minter.clone()))
            .unwrap_or(false);
        if !is_minter {
            return Err(TokenError::UnauthorizedMinter);
        }
        let balance = Self::balance(env.clone(), to.clone());
        let to_key = DataKey::Balance(to.clone());
        env.storage()
            .persistent()
            .set(&to_key, &(balance + amount));
        env.storage()
            .persistent()
            .extend_ttl(&to_key, TTL_BUMP, TTL_HIGH);
        let supply: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &(supply + amount));
        env.events().publish(
            (Symbol::new(&env, "mint"), minter, to),
            amount,
        );
        Ok(())
    }

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) -> Result<(), TokenError> {
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(TokenError::InvalidAmount);
        }
        from.require_auth();
        let from_balance = Self::balance(env.clone(), from.clone());
        if from_balance < amount {
            return Err(TokenError::InsufficientBalance);
        }
        let from_key = DataKey::Balance(from.clone());
        env.storage()
            .persistent()
            .set(&from_key, &(from_balance - amount));
        env.storage()
            .persistent()
            .extend_ttl(&from_key, TTL_BUMP, TTL_HIGH);
        let to_balance = Self::balance(env.clone(), to.clone());
        let to_key = DataKey::Balance(to.clone());
        env.storage()
            .persistent()
            .set(&to_key, &(to_balance + amount));
        env.storage()
            .persistent()
            .extend_ttl(&to_key, TTL_BUMP, TTL_HIGH);
        env.events().publish(
            (Symbol::new(&env, "transfer"), from, to),
            amount,
        );
        Ok(())
    }

    /// Allow `spender` to transfer up to `amount` of `from`'s PULSE, until
    /// `expiration_ledger` (inclusive). Pass `amount == 0` to revoke.
    pub fn approve(
        env: Env,
        from: Address,
        spender: Address,
        amount: i128,
        expiration_ledger: u32,
    ) -> Result<(), TokenError> {
        if amount < 0 {
            return Err(TokenError::InvalidAmount);
        }
        from.require_auth();

        if amount > 0 && expiration_ledger < env.ledger().sequence() {
            return Err(TokenError::InvalidExpirationLedger);
        }

        let key = DataKey::Allowance(from, spender);
        if amount == 0 {
            env.storage().temporary().remove(&key);
            return Ok(());
        }

        let value = AllowanceValue {
            amount,
            expiration_ledger,
        };
        env.storage().temporary().set(&key, &value);
        let live_for = expiration_ledger
            .saturating_sub(env.ledger().sequence());
        env.storage()
            .temporary()
            .extend_ttl(&key, live_for, live_for);
        Ok(())
    }

    /// Amount `spender` is still allowed to transfer on `from`'s behalf.
    /// Returns 0 once the allowance has expired.
    pub fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        let key = DataKey::Allowance(from, spender);
        match env.storage().temporary().get::<DataKey, AllowanceValue>(&key) {
            Some(allowance) if allowance.expiration_ledger >= env.ledger().sequence() => {
                allowance.amount
            }
            _ => 0,
        }
    }

    /// Transfer `amount` from `from` to `to`, spending down the allowance
    /// previously granted to `spender` via `approve`.
    pub fn transfer_from(
        env: Env,
        spender: Address,
        from: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), TokenError> {
        if amount <= 0 {
            return Err(TokenError::InvalidAmount);
        }
        spender.require_auth();

        let current_allowance = Self::allowance(env.clone(), from.clone(), spender.clone());
        if current_allowance < amount {
            return Err(TokenError::InsufficientAllowance);
        }

        let from_balance = Self::balance(env.clone(), from.clone());
        if from_balance < amount {
            return Err(TokenError::InsufficientBalance);
        }

        let key = DataKey::Allowance(from.clone(), spender);
        let remaining = current_allowance - amount;
        if remaining == 0 {
            env.storage().temporary().remove(&key);
        } else {
            let expiration_ledger: u32 = env
                .storage()
                .temporary()
                .get::<DataKey, AllowanceValue>(&key)
                .map(|v| v.expiration_ledger)
                .unwrap_or(env.ledger().sequence());
            env.storage().temporary().set(
                &key,
                &AllowanceValue {
                    amount: remaining,
                    expiration_ledger,
                },
            );
        }

        env.storage()
            .persistent()
            .set(&DataKey::Balance(from.clone()), &(from_balance - amount));
        let to_balance = Self::balance(env.clone(), to.clone());
        let to_key = DataKey::Balance(to.clone());
        env.storage()
            .persistent()
            .set(&to_key, &(to_balance + amount));
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Balance(from.clone()), TTL_BUMP, TTL_HIGH);
        env.storage()
            .persistent()
            .extend_ttl(&to_key, TTL_BUMP, TTL_HIGH);
        env.events().publish(
            (Symbol::new(&env, "transfer"), from, to),
            amount,
        );
        Ok(())
    }

    pub fn burn(env: Env, from: Address, amount: i128) -> Result<(), TokenError> {
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(TokenError::InvalidAmount);
        }
        from.require_auth();
        let from_balance = Self::balance(env.clone(), from.clone());
        if from_balance < amount {
            return Err(TokenError::InsufficientBalance);
        }
        let from_key = DataKey::Balance(from.clone());
        env.storage()
            .persistent()
            .set(&from_key, &(from_balance - amount));
        env.storage()
            .persistent()
            .extend_ttl(&from_key, TTL_BUMP, TTL_HIGH);
        let supply: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &(supply - amount));
        env.events().publish((Symbol::new(&env, "burn"), from), amount);
        Ok(())
    }

    pub fn balance(env: Env, account: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Balance(account))
            .unwrap_or(0)
    }

    pub fn total_supply(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0)
    }

    pub fn name(env: Env) -> String {
        env.storage()
            .instance()
            .get(&DataKey::Name)
            .unwrap_or_else(|| String::from_str(&env, "PULSE"))
    }

    pub fn symbol(env: Env) -> String {
        env.storage()
            .instance()
            .get(&DataKey::Symbol)
            .unwrap_or_else(|| String::from_str(&env, "PLSE"))
    }

    pub fn decimals(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::Decimals)
            .unwrap_or(7)
    }

    fn require_admin(env: &Env) -> Result<Address, TokenError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TokenError::NotInitialized)
    }

    fn require_not_paused(env: &Env) -> Result<(), TokenError> {
        if Self::is_paused(env.clone()) {
            return Err(TokenError::ContractPaused);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
