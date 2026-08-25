#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, vec, Address, BytesN,
    Env, Executable, IntoVal, String, Symbol, Val, Vec,
};

// ── Event schema (issue #52) ────────────────────────────────────────────────
// Topics: (event_name: Symbol, actor: Address [, market_id: u64])
// Data:   state deltas so an indexer can rebuild history without polling.
//
// market_created     (admin, id)              (category, end_time)
// bet_placed         (user, id)               (is_yes, amount, net)
// market_resolved    (caller, id)             (outcome, pool, fees)
// market_cancelled   (admin, id)              net_pool
// cancel_refund      (user, id)               gross
// claim_processed    (user, id)               (is_winner, payout)
// fees_withdrawn     (caller)                 (recipient, amount)
// withdraw_requested (caller)                 (recipient, amount)
// withdraw_cancelled (admin)                  caller
// config_changed     (admin)                  Config
// paused / unpaused  (admin)                  ()
// ────────────────────────────────────────────────────────────────────────────

// ── Constants ─────────────────────────────────────────────────────────────────

const MIN_BET: i128 = 10_000_000; // minimum net stake: 1 XLM in stroops

const MAX_BETS_PER_USER: u32 = 20;
const MAX_MARKETS_PER_HOUR: u32 = 10;
const MIN_MARKET_DURATION_SECS: u64 = 60; // issue #10: no instantly-expired markets
const MAX_BETTORS_PER_PAGE: u32 = 100;

// Fee constants — multiply before divide to avoid precision loss
const TOTAL_FEE_BPS: i128 = 200;
const PLATFORM_FEE_BPS: i128 = 150;
const BPS_DENOM: i128 = 10_000;
const NET_NUMERATOR: i128 = 9_800;

const WIN_POINTS: u64 = 30;
const LOSE_POINTS: u64 = 10;
const WIN_TOKENS: i128 = 10_0000000;
const LOSE_TOKENS: i128 = 2_0000000;

// Withdrawal safety (issue #12): a single payout is capped and the non-admin
// path is timelocked, so a compromised fee recipient cannot drain the whole
// accumulator to an arbitrary address in one call.
const WITHDRAW_DELAY_SECS: u64 = 86_400; // 24h timelock between request and payout
const MAX_WITHDRAWAL_BPS: i128 = 2_000; // per-request cap: 20% of accumulated fees
const CONFIG_DELAY_SECS: u64 = 86_400; // issue #51: dispute window before Config is live
const MAX_GOVERNORS: u32 = 10;
const LEGACY_MARKET_ID: u64 = 0; // unattributed pre-upgrade fee bucket
// Issue #3: challenge window after empty-side resolution before claims/fees unlock.
const DISPUTE_WINDOW_SECS: u64 = 604_800; // 7 days

// TTL: ~1yr threshold, ~2yr extend (mainnet: ~1 ledger/5s)
const TTL_BUMP: u32 = 3_153_600;
const TTL_HIGH: u32 = 6_307_200;
const MAX_TTL_REFRESH_PAGE: u32 = 20;

// Issue #84: bump whenever a function signature, argument order, or return
// type that a caller relies on changes.
pub const INTERFACE_VERSION: u32 = 1;

// The referral/leaderboard interface_version this contract was built
// against. A deployed dependency reporting a different version may have a
// changed credit/reward ABI — refuse the call rather than invoke blind
// (issue #84).
const EXPECTED_REFERRAL_INTERFACE_VERSION: u32 = 1;
const EXPECTED_LEADERBOARD_INTERFACE_VERSION: u32 = 1;

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum MarketError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotAdmin = 3,
    MarketNotFound = 4,
    MarketExpired = 5,
    MarketNotExpired = 6,
    MarketResolved = 7,
    MarketCancelled = 8,
    MarketNotResolved = 9,
    BetTooSmall = 10,
    OppositeSideBet = 11,
    AlreadyClaimed = 12,
    NoBetFound = 13,
    InvalidAmount = 14,
    NoFeesToWithdraw = 15,
    NotResolver = 16,
    TooManyBets = 17,
    NotAuthorized = 18,
    MarketNotCancelled = 19,
    RateLimitExceeded = 20,
    InvalidFeeRecipient = 21,
    WithdrawalTooLarge = 22,
    WithdrawalRequestExists = 23,
    NoWithdrawalRequest = 24,
    WithdrawalTooSoon = 25,
    ContractPaused = 26,
    InvalidDuration = 27, // issue #10: duration below the minimum
    InvalidDependency = 28, // issue #51: address is not the expected executable kind
    WasmHashMismatch = 29,  // issue #51: live WASM hash != pinned / pending hash
    ConfigChangeExists = 30,
    NoConfigChange = 31,
    ConfigChangeTooSoon = 32,
    InsufficientApprovals = 33,
    AlreadyApproved = 34,
    InvalidThreshold = 35,
    /// A dependency (referral_registry or leaderboard) reported an
    /// interface_version this contract wasn't built against (issue #84).
    /// Note: a matching version number alone does not prove the callee's
    /// actual function shape still matches, it only proves the callee's
    /// author intended it to. The guarantee only holds if every breaking
    /// ABI change (renamed function, changed argument order/count/type,
    /// changed return type) always increments INTERFACE_VERSION in the same
    /// commit. See EXPECTED_REFERRAL_INTERFACE_VERSION / EXPECTED_LEADERBOARD_INTERFACE_VERSION.
    IncompatibleInterface = 36,
    DisputePending = 37,
    NoZeroSideResolution = 38,
    DisputeWindowClosed = 39,
    /// Registered resolver attempted to bet, or tried to resolve a market they
    /// have a stake in (issue #3 collusion guard).
    ResolverConflict = 40,
}

// ── Storage Keys ──────────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    // Config addresses — all in instance storage (shared, cheap)
    Cfg, // single packed Config struct — 1 read instead of 5
    MarketCount,
    // Global settlement view (issue #4 / #57): cached Σ MarketFees + LegacyFees.
    // Never the source of truth for a cancel/withdraw — those use the ledger.
    AccumulatedFees,
    Market(u64),
    Bet(u64, Address), // net + gross + count packed; see BetEntry
    BettorCount(u64),
    BettorAt(u64, u32),
    Resolver(Address),
    FeeRecipient(Address),
    HasReferrer(Address),
    RateWindow, // packed u64: high32=window_start_hi, low32=count
    // ── Settlement-time payouts (issue #2) ───────────────────────────────
    Payout(u64, Address), // i128 — exact payout computed at resolve time
    // ── Timelocked withdrawal requests (issue #12) ───────────────────────
    PendingWithdrawal(Address), // caller -> WithdrawalRequest
    // ── Per-market fee ledger (issue #4 / #57) ───────────────────────────
    MarketFees(u64),   // i128 — genuine earned fees for this market
    LegacyFees,        // i128 — unattributed pre-migration balance
    FeeLedgerMigrated, // bool — one-shot migration of the old global scalar
    // ── Zero-side principal vault (issue #3) ─────────────────────────────
    // Holds empty-side pool metadata + locked platform fees. Never mixed into
    // the withdrawable AccumulatedFees sum until the dispute window ends.
    ForfeitedPool(u64),
    // ── Dependency governance (issue #51) ────────────────────────────────
    Governor(Address),
    GovernorCount,
    GovernorThreshold,
    PendingConfig,
    PinnedHashes,
    // ── Emergency circuit-breaker (issue #83) ─────────────────────────────
    Paused,
    // ── Reentrancy guard (issue #89) ─────────────────────────────────────
    BetLock(u64, Address), // market_id+user -> bool: prevents reentrant place_bet
}

// ── Config packed into one instance storage slot ───────────────────────────
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub token: Address,
    pub referral: Address,
    pub leaderboard: Address,
    pub xlm_sac: Address,
}

/// WASM hashes (or the SAC sentinel) pinned for each Config role.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinnedHashes {
    pub token: BytesN<32>,
    pub referral: BytesN<32>,
    pub leaderboard: BytesN<32>,
    pub xlm_sac: BytesN<32>,
}

/// Timelocked, multi-sig Config mutation. Inactive until execute_set_config.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingConfigChange {
    pub cfg: Config,
    pub hashes: PinnedHashes,
    pub requested_at: u64,
    pub approvers: Vec<Address>,
}

// ── BetEntry: Bet + Gross + BetCount in one slot ──────────────────────────
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BetEntry {
    pub net: i128,   // post-fee amount bet (used for payout)
    pub gross: i128, // pre-fee amount sent (used for cancel_refund)
    pub is_yes: bool,
    pub claimed: bool,
    pub count: u32, // how many times this user has bet on this market
}

// ── WithdrawalRequest: capped, recipient-validated, timelocked (issue #12) ──
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithdrawalRequest {
    pub recipient: Address,
    pub amount: i128,
    pub requested_at: u64,
}

/// Per-market vault for empty-side principal + locked fees (issue #3).
/// Principal is paid via Payout; locked_fees rejoin MarketFees only after the
/// dispute window (or stay out forever if the market is frozen/cancelled).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForfeitedPool {
    pub amount: i128,
    pub locked_fees: i128,
    pub resolved_at: u64,
    pub frozen: bool,
}

// ── Domain Structs ────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Category {
    Crypto,
    Sports,
    Politics,
    Entertainment,
    Science,
    Other,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Market {
    pub id: u64,
    pub question: String,
    pub image_url: String,
    pub category: Category,
    pub end_time: u64,
    pub total_yes: i128,
    pub total_no: i128,
    pub resolved: bool,
    pub outcome: bool,
    pub cancelled: bool,
    pub creator: Address,
    pub bet_count: u32,
}

// Kept for ABI compatibility — frontend reads Bet fields
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bet {
    pub amount: i128,
    pub is_yes: bool,
    pub claimed: bool,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct PredictionMarketContract;

#[contractimpl]
impl PredictionMarketContract {
    pub fn initialize(
        env: Env,
        admin: Address,
        token_contract: Address,
        referral_contract: Address,
        leaderboard_contract: Address,
        xlm_sac: Address,
    ) -> Result<(), MarketError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(MarketError::AlreadyInitialized);
        }
        admin.require_auth();

        env.storage().instance().set(&DataKey::Admin, &admin);
        // OPT: pack all 4 contract addresses into one slot
        env.storage().instance().set(
            &DataKey::Cfg,
            &Config {
                token: token_contract.clone(),
                referral: referral_contract.clone(),
                leaderboard: leaderboard_contract.clone(),
                xlm_sac: xlm_sac.clone(),
            },
        );
        env.storage().instance().set(&DataKey::MarketCount, &0_u64);
        env.storage()
            .instance()
            .set(&DataKey::AccumulatedFees, &0_i128);
        env.storage().instance().set(&DataKey::LegacyFees, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::FeeLedgerMigrated, &true);
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);

        // Bootstrap governance: the initializer is the first governor with
        // a 1-of-1 threshold. Production deploys should add more governors
        // and raise the threshold before relying on set_config.
        env.storage()
            .persistent()
            .set(&DataKey::Governor(admin.clone()), &true);
        env.storage().instance().set(&DataKey::GovernorCount, &1_u32);
        env.storage()
            .instance()
            .set(&DataKey::GovernorThreshold, &1_u32);
        if let Ok(hashes) = Self::fingerprint_config(
            &env,
            &token_contract,
            &referral_contract,
            &leaderboard_contract,
            &xlm_sac,
        ) {
            env.storage()
                .instance()
                .set(&DataKey::PinnedHashes, &hashes);
        }
        env.events().publish(
            (Symbol::new(&env, "initialized"), admin),
            (token_contract, referral_contract, leaderboard_contract, xlm_sac),
        );
        Ok(())
    }

    // ── Upgradeability & Config (admin only) ──────────────────────────────────
    // Allows fixing a bad config (e.g. wrong XLM SAC) or shipping a bug fix
    // without redeploying and losing all markets/bets/contract address.

    /// Replace this contract's WASM bytecode in place. Admin only.
    /// Storage is preserved — only the executable changes.
    pub fn upgrade(env: Env, admin: Address, new_wasm_hash: BytesN<32>) -> Result<(), MarketError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
        Ok(())
    }

    /// Propose a Config change. Does **not** take effect immediately.
    ///
    /// Issue #51: live WASM hashes are read on-chain (not caller-supplied),
    /// the proposal is emitted for monitors, and it only becomes active after
    /// `CONFIG_DELAY_SECS` **and** `GovernorThreshold` approvals via
    /// `execute_set_config`. Any governor can `cancel_set_config` in between.
    pub fn set_config(
        env: Env,
        caller: Address,
        token_contract: Address,
        referral_contract: Address,
        leaderboard_contract: Address,
        xlm_sac: Address,
    ) -> Result<(), MarketError> {
        caller.require_auth();
        Self::require_governor(&env, &caller)?;
        if env.storage().instance().has(&DataKey::PendingConfig) {
            return Err(MarketError::ConfigChangeExists);
        }

        let hashes = Self::fingerprint_config(
            &env,
            &token_contract,
            &referral_contract,
            &leaderboard_contract,
            &xlm_sac,
        )?;
        let mut approvers: Vec<Address> = Vec::new(&env);
        approvers.push_back(caller.clone());
        let pending = PendingConfigChange {
            cfg: Config {
                token: token_contract,
                referral: referral_contract,
                leaderboard: leaderboard_contract,
                xlm_sac,
            },
            hashes,
            requested_at: env.ledger().timestamp(),
            approvers,
        };
        env.storage()
            .instance()
            .set(&DataKey::PendingConfig, &pending);
        env.events().publish(
            (Symbol::new(&env, "cfg_req"), caller),
            pending,
        );
        Ok(())
    }

    /// A governor attests a pending Config change during the dispute window.
    pub fn approve_set_config(env: Env, caller: Address) -> Result<u32, MarketError> {
        caller.require_auth();
        Self::require_governor(&env, &caller)?;
        let mut pending: PendingConfigChange = env
            .storage()
            .instance()
            .get(&DataKey::PendingConfig)
            .ok_or(MarketError::NoConfigChange)?;
        if Self::approver_index(&pending.approvers, &caller).is_some() {
            return Err(MarketError::AlreadyApproved);
        }
        pending.approvers.push_back(caller.clone());
        let count = pending.approvers.len();
        env.storage()
            .instance()
            .set(&DataKey::PendingConfig, &pending);
        env.events().publish(
            (Symbol::new(&env, "cfg_ok"), caller),
            count,
        );
        Ok(count)
    }

    /// Activate a matured, sufficiently-approved Config change. Re-reads live
    /// executables so a dependency cannot swap WASM during the delay.
    pub fn execute_set_config(env: Env, caller: Address) -> Result<(), MarketError> {
        caller.require_auth();
        Self::require_governor(&env, &caller)?;
        let pending: PendingConfigChange = env
            .storage()
            .instance()
            .get(&DataKey::PendingConfig)
            .ok_or(MarketError::NoConfigChange)?;

        let now = env.ledger().timestamp();
        if now < pending.requested_at || now - pending.requested_at < CONFIG_DELAY_SECS {
            return Err(MarketError::ConfigChangeTooSoon);
        }
        let threshold: u32 = env
            .storage()
            .instance()
            .get(&DataKey::GovernorThreshold)
            .unwrap_or(1);
        if pending.approvers.len() < threshold {
            return Err(MarketError::InsufficientApprovals);
        }

        let live = Self::fingerprint_config(
            &env,
            &pending.cfg.token,
            &pending.cfg.referral,
            &pending.cfg.leaderboard,
            &pending.cfg.xlm_sac,
        )?;
        if live != pending.hashes {
            return Err(MarketError::WasmHashMismatch);
        }

        env.storage().instance().set(&DataKey::Cfg, &pending.cfg);
        env.storage()
            .instance()
            .set(&DataKey::PinnedHashes, &pending.hashes);
        env.storage().instance().remove(&DataKey::PendingConfig);
        env.events().publish(
            (Symbol::new(&env, "cfg_act"), caller),
            pending.cfg,
        );
        env.events().publish(
            (Symbol::new(&env, "config_changed"), admin),
            (token_contract, referral_contract, leaderboard_contract, xlm_sac),
        );
        Ok(())
    }

    /// Cancel a pending Config change during the dispute window.
    pub fn cancel_set_config(env: Env, caller: Address) -> Result<(), MarketError> {
        caller.require_auth();
        Self::require_governor(&env, &caller)?;
        if !env.storage().instance().has(&DataKey::PendingConfig) {
            return Err(MarketError::NoConfigChange);
        }
        env.storage().instance().remove(&DataKey::PendingConfig);
        env.events().publish(
            (Symbol::new(&env, "cfg_can"), caller),
            1_u32,
        );
        Ok(())
    }

    pub fn add_governor(env: Env, admin: Address, governor: Address) -> Result<(), MarketError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        let key = DataKey::Governor(governor.clone());
        if env.storage().persistent().get(&key).unwrap_or(false) {
            return Ok(());
        }
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::GovernorCount)
            .unwrap_or(0);
        if count >= MAX_GOVERNORS {
            return Err(MarketError::RateLimitExceeded);
        }
        env.storage().persistent().set(&key, &true);
        env.storage()
            .instance()
            .set(&DataKey::GovernorCount, &(count + 1));
        Ok(())
    }

    pub fn remove_governor(env: Env, admin: Address, governor: Address) -> Result<(), MarketError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::GovernorCount)
            .unwrap_or(0);
        let threshold: u32 = env
            .storage()
            .instance()
            .get(&DataKey::GovernorThreshold)
            .unwrap_or(1);
        if count <= threshold {
            return Err(MarketError::InvalidThreshold);
        }
        let key = DataKey::Governor(governor);
        if !env.storage().persistent().get(&key).unwrap_or(false) {
            return Err(MarketError::NotAuthorized);
        }
        env.storage().persistent().remove(&key);
        env.storage()
            .instance()
            .set(&DataKey::GovernorCount, &(count - 1));
        Ok(())
    }

    pub fn set_governor_threshold(
        env: Env,
        admin: Address,
        threshold: u32,
    ) -> Result<(), MarketError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::GovernorCount)
            .unwrap_or(0);
        if threshold == 0 || threshold > count {
            return Err(MarketError::InvalidThreshold);
        }
        env.storage()
            .instance()
            .set(&DataKey::GovernorThreshold, &threshold);
        Ok(())
    }

    /// Read the current Config (for verification/admin tooling).
    pub fn get_config(env: Env) -> Config {
        env.storage().instance().get(&DataKey::Cfg).unwrap()
    }

    pub fn get_pending_config(env: Env) -> Option<PendingConfigChange> {
        env.storage().instance().get(&DataKey::PendingConfig)
    }

    pub fn get_pinned_hashes(env: Env) -> Option<PinnedHashes> {
        env.storage().instance().get(&DataKey::PinnedHashes)
    }

    pub fn is_governor(env: Env, account: Address) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Governor(account))
            .unwrap_or(false)
    }

    pub fn get_governor_threshold(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::GovernorThreshold)
            .unwrap_or(1)
    }

    pub fn get_governor_count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::GovernorCount)
            .unwrap_or(0)
    /// The cross-contract ABI version this deployment implements (issue #84).
    pub fn interface_version(_env: Env) -> u32 {
        INTERFACE_VERSION
    }

    // ── Emergency Pause (issue #83) ─────────────────────────────────────────
    // Halts market creation, betting, resolution, claims, and fee withdrawals
    // so an in-progress exploit (e.g. a malicious resolver or a reentrancy
    // attempt) can be contained while a fix is prepared. cancel_refund and
    // cancel_withdrawal_request stay open even while paused: refunds are the
    // users' emergency exit from a cancelled market, and cancelling a pending
    // withdrawal request is itself a safety action the admin needs.

    pub fn pause(env: Env, admin: Address) -> Result<(), MarketError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((Symbol::new(&env, "paused"), admin), true);
        Ok(())
    }

    pub fn unpause(env: Env, admin: Address) -> Result<(), MarketError> {
        Self::require_admin(&env, &admin)?;
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

    // ── Resolver Management ───────────────────────────────────────────────

    pub fn add_resolver(env: Env, admin: Address, resolver: Address) -> Result<(), MarketError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        // Issue #3: resolvers must not hold open stakes on any unsettled market.
        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MarketCount)
            .unwrap_or(0);
        for id in 1..=count {
            if let Ok(m) = Self::load_market(&env, id) {
                if !m.resolved && !m.cancelled && Self::has_bet_on_market(&env, id, &resolver) {
                    return Err(MarketError::ResolverConflict);
                }
            }
        }
        let key = DataKey::Resolver(resolver);
        env.storage().persistent().set(&key, &true);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn remove_resolver(env: Env, admin: Address, resolver: Address) -> Result<(), MarketError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        env.storage()
            .persistent()
            .remove(&DataKey::Resolver(resolver));
        Ok(())
    }

    pub fn is_resolver(env: Env, resolver: Address) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Resolver(resolver))
            .unwrap_or(false)
    }

    // ── Fee Recipient Management ──────────────────────────────────────────

    pub fn add_fee_recipient(
        env: Env,
        admin: Address,
        recipient: Address,
    ) -> Result<(), MarketError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        let key = DataKey::FeeRecipient(recipient);
        env.storage().persistent().set(&key, &true);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn remove_fee_recipient(
        env: Env,
        admin: Address,
        recipient: Address,
    ) -> Result<(), MarketError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        env.storage()
            .persistent()
            .remove(&DataKey::FeeRecipient(recipient));
        Ok(())
    }

    // ── Market Management ─────────────────────────────────────────────────

    pub fn create_market(
        env: Env,
        admin: Address,
        question: String,
        image_url: String,
        category: Category,
        duration_secs: u64,
    ) -> Result<u64, MarketError> {
        Self::require_not_paused(&env)?;
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        if duration_secs < MIN_MARKET_DURATION_SECS {
            return Err(MarketError::InvalidDuration);
        }
        Self::check_rate(&env)?;

        // OPT: single instance read for count (was already one read)
        let market_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MarketCount)
            .unwrap_or(0)
            + 1;
        let end_time = env.ledger().timestamp() + duration_secs;

        let market = Market {
            id: market_id,
            question,
            image_url,
            category,
            end_time,
            total_yes: 0,
            total_no: 0,
            resolved: false,
            outcome: false,
            cancelled: false,
            creator: admin,
            bet_count: 0,
        };

        let mkt_key = DataKey::Market(market_id);
        env.storage().persistent().set(&mkt_key, &market);
        env.storage()
            .persistent()
            .extend_ttl(&mkt_key, TTL_BUMP, TTL_HIGH);
        // OPT: removed BettorCount write here — now written lazily on first bet
        env.storage()
            .instance()
            .set(&DataKey::MarketCount, &market_id);

        env.events().publish(
            (Symbol::new(&env, "market_created"), market.creator.clone(), market_id),
            (market.category.clone(), market.end_time),
        );
        Ok(market_id)
    }

    // ── Betting ───────────────────────────────────────────────────────────
    pub fn place_bet(
        env: Env,
        user: Address,
        market_id: u64,
        is_yes: bool,
        amount: i128,
    ) -> Result<(), MarketError> {
        Self::require_not_paused(&env)?;
        user.require_auth();

        // Issue #3: resolvers must stay neutral — no betting on markets they
        // may later resolve (admin is exempt; they are protocol-trusted).
        if Self::is_registered_resolver(&env, &user) {
            return Err(MarketError::ResolverConflict);
        }

        // Issue 89: reentrancy guard — set lock before any external call
        let lock_key = DataKey::BetLock(market_id, user.clone());
        if env.storage().persistent().get(&lock_key).unwrap_or(false) {
            return Err(MarketError::NotAuthorized); // reentrancy attempt
        }
        env.storage().persistent().set(&lock_key, &true);

        let net = amount * NET_NUMERATOR / BPS_DENOM;
        if net < MIN_BET {
            env.storage().persistent().remove(&lock_key);
            return Err(MarketError::BetTooSmall);
        }

        // OPT: load market first — cheapest early-exit if not found
        let mut market = Self::load_market(&env, market_id)?;
        if market.cancelled {
            env.storage().persistent().remove(&lock_key);
            return Err(MarketError::MarketCancelled);
        }
        if market.resolved {
            env.storage().persistent().remove(&lock_key);
            return Err(MarketError::MarketResolved);
        }
        if env.ledger().timestamp() >= market.end_time {
            env.storage().persistent().remove(&lock_key);
            return Err(MarketError::MarketExpired);
        }

        // OPT: single read for BetEntry (was 3 separate reads: Bet + BetGross + UserBetCount)
        let bet_key = DataKey::Bet(market_id, user.clone());
        let existing: Option<BetEntry> = env.storage().persistent().get(&bet_key);

        // Spam guard + side check combined from single read
        if let Some(ref e) = existing {
            if e.count >= MAX_BETS_PER_USER {
                env.storage().persistent().remove(&lock_key);
                return Err(MarketError::TooManyBets);
            }
            if e.is_yes != is_yes {
                env.storage().persistent().remove(&lock_key);
                return Err(MarketError::OppositeSideBet);
            }
        }

        let is_increase = existing.is_some();

        // ── Fee calculation — use precomputed multipliers ─────────────────
        let total_fee = amount * TOTAL_FEE_BPS / BPS_DENOM;
        let platform_fee = amount * PLATFORM_FEE_BPS / BPS_DENOM;
        let referral_fee = total_fee - platform_fee;

        // OPT: one Config read instead of 4 separate instance reads
        let cfg: Config = env.storage().instance().get(&DataKey::Cfg).unwrap();

        // ── Issue 89: Write ALL state BEFORE external calls (check-effects-interaction) ──

        // Credit only the platform fee to this market's ledger. The referral
        // fee is either sent to the referrer or held by the referral contract
        // as surplus (issue #78), so the market never holds it for withdrawal.
        Self::credit_market_fees(&env, market_id, platform_fee);

        // ── Write BetEntry (net + gross + count in one write) ─────────────
        let new_entry = match existing {
            Some(mut e) => {
                e.net += net;
                e.gross += amount;
                e.count += 1;
                e
            }
            None => BetEntry {
                net,
                gross: amount,
                is_yes,
                claimed: false,
                count: 1,
            },
        };
        env.storage().persistent().set(&bet_key, &new_entry);
        env.storage()
            .persistent()
            .extend_ttl(&bet_key, TTL_BUMP, TTL_HIGH);

        // ── Bettor index (first bet only) ─────────────────────────────────
        if !is_increase {
            let cnt_key = DataKey::BettorCount(market_id);
            let count: u32 = env.storage().persistent().get(&cnt_key).unwrap_or(0);
            let slot_key = DataKey::BettorAt(market_id, count);
            env.storage().persistent().set(&slot_key, &user.clone());
            env.storage()
                .persistent()
                .extend_ttl(&slot_key, TTL_BUMP, TTL_HIGH);
            let new_count = count + 1;
            env.storage().persistent().set(&cnt_key, &new_count);
            env.storage()
                .persistent()
                .extend_ttl(&cnt_key, TTL_BUMP, TTL_HIGH);
            market.bet_count += 1;
        }

        // ── Market totals ─────────────────────────────────────────────────
        if is_yes {
            market.total_yes += net;
        } else {
            market.total_no += net;
        }
        let mkt_key = DataKey::Market(market_id);
        env.storage().persistent().set(&mkt_key, &market);
        env.storage()
            .persistent()
            .extend_ttl(&mkt_key, TTL_BUMP, TTL_HIGH);

        // ── HasReferrer cache write ───────────────────────────────────────
        let hr_key = DataKey::HasReferrer(user.clone());
        let cached: Option<bool> = env.storage().persistent().get(&hr_key);

        // ── External calls (issue 89: after ALL state writes) ─────────────

        // ── XLM transfer user → this contract ────────────────────────────
        let xlm = token::Client::new(&env, &cfg.xlm_sac);
        let this = env.current_contract_address();
        xlm.transfer(&user, &this, &amount);

        // ── Referral (skip if cached no-referrer) ─────────────────────────
        let _paid_referrer = if cached == Some(false) {
            false
        } else {
            Self::require_compatible_referral(&env, &cfg.referral)?;
            xlm.transfer(&this, &cfg.referral, &referral_fee);
            let result: bool = env.invoke_contract(
                &cfg.referral,
                &Symbol::new(&env, "credit"),
                vec![
                    &env,
                    this.clone().into_val(&env),
                    user.clone().into_val(&env),
                    referral_fee.into_val(&env),
                ],
            );
            if cached.is_none() {
                env.storage().persistent().set(&hr_key, &result);
                env.storage()
                    .persistent()
                    .extend_ttl(&hr_key, TTL_BUMP, TTL_HIGH);
            }
            result
        };

        // ── Release reentrancy lock ──────────────────────────────────────
        env.storage().persistent().remove(&lock_key);
        env.events().publish(
            (Symbol::new(&env, "bet_placed"), user, market_id),
            (is_yes, amount, net),
        );
        Ok(())
    }

    // ── Resolution ────────────────────────────────────────────────────────

    pub fn resolve_market(
        env: Env,
        caller: Address,
        market_id: u64,
        outcome: bool,
    ) -> Result<(), MarketError> {
        Self::require_not_paused(&env)?;
        caller.require_auth();
        Self::require_admin_or_resolver(&env, &caller)?;

        // Issue #3: anyone resolving must not hold a stake — blocks resolver
        // collusion where they grief to the empty side then claim principal.
        if Self::has_bet_on_market(&env, market_id, &caller) {
            return Err(MarketError::ResolverConflict);
        }

        let mut market = Self::load_market(&env, market_id)?;
        if market.resolved {
            return Err(MarketError::MarketResolved);
        }
        if market.cancelled {
            return Err(MarketError::MarketCancelled);
        }
        if env.ledger().timestamp() < market.end_time {
            return Err(MarketError::MarketNotExpired);
        }

        let total_pool: i128 = market.total_yes + market.total_no;
        let winning_side: i128 = if outcome {
            market.total_yes
        } else {
            market.total_no
        };

        if winning_side == 0 {
            // Issue #3: NEVER sweep principal into fees. Credit each bettor
            // their net via Payout, lock this market's platform fees into
            // ForfeitedPool (out of withdraw_fees), and open a dispute window.
            let bettors: u32 = env
                .storage()
                .persistent()
                .get(&DataKey::BettorCount(market_id))
                .unwrap_or(0);
            let mut principal: i128 = 0;
            for i in 0..bettors {
                let slot_key = DataKey::BettorAt(market_id, i);
                let bettor: Address =
                    if let Some(a) = env.storage().persistent().get(&slot_key) {
                        a
                    } else {
                        continue;
                    };
                let bet_key = DataKey::Bet(market_id, bettor.clone());
                if let Some(entry) =
                    env.storage().persistent().get::<DataKey, BetEntry>(&bet_key)
                {
                    if entry.net > 0 {
                        principal += entry.net;
                        let payout_key = DataKey::Payout(market_id, bettor.clone());
                        env.storage().persistent().set(&payout_key, &entry.net);
                        env.storage()
                            .persistent()
                            .extend_ttl(&payout_key, TTL_BUMP, TTL_HIGH);
                    }
                }
            }

            // Pull this market's fees out of the withdrawable pot for the
            // dispute window — withdraw_fees cannot touch them.
            let locked = Self::market_fee_balance(&env, market_id);
            if locked > 0 {
                Self::debit_market_fees(&env, market_id, locked);
            }

            let fp_key = DataKey::ForfeitedPool(market_id);
            env.storage().persistent().set(
                &fp_key,
                &ForfeitedPool {
                    amount: if principal > 0 { principal } else { total_pool },
                    locked_fees: locked,
                    resolved_at: env.ledger().timestamp(),
                    frozen: false,
                },
            );
            env.storage()
                .persistent()
                .extend_ttl(&fp_key, TTL_BUMP, TTL_HIGH);
            env.events()
                .publish((symbol_short!("zero_side"), market_id), total_pool);
        } else {
            // Settlement-time payouts (issue #2): compute EXACT per-winner
            // payouts and the deterministic remainder (dust) once, here, so:
            //   Σ payouts + dust == total_pool   (no money can get trapped)
            //   payouts never exceed the pool (floor per user)
            //   claim() performs no division and cannot double-pay.
            let mut payout_sum: i128 = 0;
            let bettors: u32 = env
                .storage()
                .persistent()
                .get(&DataKey::BettorCount(market_id))
                .unwrap_or(0);

            for i in 0..bettors {
                let slot_key = DataKey::BettorAt(market_id, i);
                let bettor: Address =
                    if let Some(a) = env.storage().persistent().get(&slot_key) {
                        a
                    } else {
                        continue;
                    };
                let bet_key = DataKey::Bet(market_id, bettor.clone());
                if let Some(entry) = env.storage().persistent().get::<DataKey, BetEntry>(&bet_key) {
                    if entry.is_yes == outcome {
                        let payout = (entry.net * total_pool) / winning_side;
                        let payout_key = DataKey::Payout(market_id, bettor.clone());
                        env.storage().persistent().set(&payout_key, &payout);
                        env.storage()
                            .persistent()
                            .extend_ttl(&payout_key, TTL_BUMP, TTL_HIGH);
                        payout_sum += payout;
                    }
                }
            }

            let dust: i128 = total_pool - payout_sum;
            debug_assert!(dust >= 0, "payouts must never exceed the pool");
            if dust > 0 {
                Self::credit_market_fees(&env, market_id, dust);
            }
        }

        market.resolved = true;
        market.outcome = outcome;
        let mkt_key = DataKey::Market(market_id);
        env.storage().persistent().set(&mkt_key, &market);
        // Issue #54: keep every fund-bearing key for this market alive at
        // settlement so later claims cannot observe an expired Bet/Payout.
        let _ = Self::refresh_market_keys(&env, market_id);
        env.storage()
            .persistent()
            .extend_ttl(&mkt_key, TTL_BUMP, TTL_HIGH);
        let acc_fees: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0);
        env.events().publish(
            (Symbol::new(&env, "market_resolved"), caller, market_id),
            (outcome, total_pool, acc_fees),
        );
        Ok(())
    }

    // ── Zero-side dispute (issue #3) ──────────────────────────────────────
    // Empty-side resolution opens a challenge window. Admin may freeze into
    // cancel_refund (gross) during that window. After the window, anyone may
    // finalize so locked platform fees rejoin the fee ledger.

    pub fn freeze_market(env: Env, admin: Address, market_id: u64) -> Result<(), MarketError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();

        let mut market = Self::load_market(&env, market_id)?;
        if market.cancelled {
            return Err(MarketError::MarketCancelled);
        }
        if !market.resolved {
            return Err(MarketError::MarketNotResolved);
        }

        let fp_key = DataKey::ForfeitedPool(market_id);
        let mut pool: ForfeitedPool = env
            .storage()
            .persistent()
            .get(&fp_key)
            .ok_or(MarketError::NoZeroSideResolution)?;
        if pool.frozen {
            return Err(MarketError::MarketCancelled);
        }
        if Self::dispute_window_closed(&env, pool.resolved_at) {
            return Err(MarketError::DisputeWindowClosed);
        }

        pool.frozen = true;
        env.storage().persistent().set(&fp_key, &pool);
        env.storage()
            .persistent()
            .extend_ttl(&fp_key, TTL_BUMP, TTL_HIGH);

        // Clear settlement payouts — users must use cancel_refund for GROSS.
        let bettors: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::BettorCount(market_id))
            .unwrap_or(0);
        for i in 0..bettors {
            if let Some(bettor) = env
                .storage()
                .persistent()
                .get::<DataKey, Address>(&DataKey::BettorAt(market_id, i))
            {
                env.storage()
                    .persistent()
                    .remove(&DataKey::Payout(market_id, bettor));
            }
        }

        market.cancelled = true;
        let mkt_key = DataKey::Market(market_id);
        env.storage().persistent().set(&mkt_key, &market);
        env.storage()
            .persistent()
            .extend_ttl(&mkt_key, TTL_BUMP, TTL_HIGH);
        Ok(())
    }

    pub fn finalize_zero_side(env: Env, market_id: u64) -> Result<(), MarketError> {
        Self::load_market(&env, market_id)?;
        let fp_key = DataKey::ForfeitedPool(market_id);
        let mut pool: ForfeitedPool = env
            .storage()
            .persistent()
            .get(&fp_key)
            .ok_or(MarketError::NoZeroSideResolution)?;
        if pool.frozen {
            return Err(MarketError::MarketCancelled);
        }
        if !Self::dispute_window_closed(&env, pool.resolved_at) {
            return Err(MarketError::DisputePending);
        }
        Self::release_locked_fees(&env, market_id, &mut pool);
        Ok(())
    }

    pub fn get_forfeited_pool(env: Env, market_id: u64) -> Option<ForfeitedPool> {
        env.storage()
            .persistent()
            .get(&DataKey::ForfeitedPool(market_id))
    }

    // ── Cancellation ──────────────────────────────────────────────────────

    pub fn cancel_market(env: Env, admin: Address, market_id: u64) -> Result<(), MarketError> {
        Self::require_not_paused(&env)?;
        Self::require_admin(&env, &admin)?;
        admin.require_auth();

        let mut market = Self::load_market(&env, market_id)?;
        if market.resolved {
            return Err(MarketError::MarketResolved);
        }
        if market.cancelled {
            return Err(MarketError::MarketCancelled);
        }

        market.cancelled = true;
        let mkt_key = DataKey::Market(market_id);
        env.storage().persistent().set(&mkt_key, &market);
        let _ = Self::refresh_market_keys(&env, market_id);

        // Reclaim only the platform fees attributable to this market's pool.
        // Never debit the full ledger blindly — cap at pool-derived fees so a
        // stale/inflated per-market balance cannot eat unrelated markets' fees.
        let net_pool = market.total_yes + market.total_no;
        let pool_fees = net_pool * PLATFORM_FEE_BPS / NET_NUMERATOR;
        let ledger = Self::market_fee_balance(&env, market_id);
        let reclaim = if pool_fees < ledger { pool_fees } else { ledger };
        if reclaim > 0 {
            Self::debit_market_fees(&env, market_id, reclaim);
        }

        env.events().publish(
            (Symbol::new(&env, "market_cancelled"), admin, market_id),
            net_pool,
        );
        Ok(())
    }

    pub fn cancel_refund(env: Env, user: Address, market_id: u64) -> Result<i128, MarketError> {
        user.require_auth();

        let market = Self::load_market(&env, market_id)?;
        if !market.cancelled {
            return Err(MarketError::MarketNotCancelled);
        }

        // OPT: read BetEntry (which now contains gross) — was a separate BetGross key
        let bet_key = DataKey::Bet(market_id, user.clone());
        let mut entry: BetEntry = env
            .storage()
            .persistent()
            .get(&bet_key)
            .ok_or(MarketError::NoBetFound)?;

        if entry.gross == 0 {
            return Err(MarketError::NoBetFound);
        }

        let gross = entry.gross;
        entry.gross = 0;
        entry.net = 0;
        env.storage().persistent().set(&bet_key, &entry);
        // Read-time TTL refresh (issue #9): a refund must not be able to observe
        // an expired bet/market record — keep both alive so a user who returns
        // late to a cancelled market can still pull their refund.
        env.storage()
            .persistent()
            .extend_ttl(&bet_key, TTL_BUMP, TTL_HIGH);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Market(market_id), TTL_BUMP, TTL_HIGH);

        let cfg: Config = env.storage().instance().get(&DataKey::Cfg).unwrap();
        token::Client::new(&env, &cfg.xlm_sac).transfer(
            &env.current_contract_address(),
            &user,
            &gross,
        );

        env.events().publish(
            (Symbol::new(&env, "cancel_refund"), user, market_id),
            gross,
        );
        Ok(gross)
    }

    // ── Claim ─────────────────────────────────────────────────────────────
    // OPT: one Config read replaces 3 separate reads (xlm_sac, leaderboard, token)

    pub fn claim(env: Env, user: Address, market_id: u64) -> Result<(), MarketError> {
        Self::require_not_paused(&env)?;
        user.require_auth();

        let market = Self::load_market(&env, market_id)?;
        if market.cancelled {
            return Err(MarketError::MarketCancelled);
        }
        if !market.resolved {
            return Err(MarketError::MarketNotResolved);
        }
        // Issue #3: zero-side principal cannot move until the dispute window
        // closes (or the market is frozen into cancel_refund).
        Self::enforce_zero_side_claim_window(&env, market_id)?;

        let bet_key = DataKey::Bet(market_id, user.clone());
        let mut entry: BetEntry = env
            .storage()
            .persistent()
            .get(&bet_key)
            .ok_or(MarketError::NoBetFound)?;

        if entry.claimed {
            return Err(MarketError::AlreadyClaimed);
        }

        let is_winner = entry.is_yes == market.outcome;
        let winning_side = if market.outcome {
            market.total_yes
        } else {
            market.total_no
        };

        // SECURITY: mark claimed BEFORE any external calls.
        entry.claimed = true;
        env.storage().persistent().set(&bet_key, &entry);
        env.storage()
            .persistent()
            .extend_ttl(&bet_key, TTL_BUMP, TTL_HIGH);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Market(market_id), TTL_BUMP, TTL_HIGH);
        Self::bump_if_present(&env, &DataKey::Payout(market_id, user.clone()));

        let cfg: Config = env.storage().instance().get(&DataKey::Cfg).unwrap();
        let this = env.current_contract_address();

        // Two-sided: only winners have a Payout and receive XLM.
        // Zero-side: populated-side bettors receive net principal refund.
        let zero_side = env
            .storage()
            .persistent()
            .has(&DataKey::ForfeitedPool(market_id));
        let ledger_payout: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Payout(market_id, user.clone()))
            .unwrap_or(0);
        let xlm_payout = if ledger_payout > 0 && (zero_side || is_winner) {
            ledger_payout
        } else {
            0
        };
        if xlm_payout > 0 {
            token::Client::new(&env, &cfg.xlm_sac).transfer(&this, &user, &xlm_payout);
        }

        let real_win = is_winner && winning_side > 0;
        let (points, tokens): (u64, i128) = if real_win {
            (WIN_POINTS, WIN_TOKENS)
        } else {
            (LOSE_POINTS, LOSE_TOKENS)
        };

        Self::require_compatible_leaderboard(&env, &cfg.leaderboard)?;
        let _: Val = env.invoke_contract(
            &cfg.leaderboard,
            &Symbol::new(&env, "reward"),
            vec![
                &env,
                this.clone().into_val(&env),
                user.clone().into_val(&env),
                points.into_val(&env),
                tokens.into_val(&env),
                real_win.into_val(&env),
            ],
        );

        env.events().publish(
            (Symbol::new(&env, "claim_processed"), user, market_id),
            (is_winner, xlm_payout, real_win),
        );
        Ok(())
    }

    // ── Withdraw Fees ─────────────────────────────────────────────────────
    // Issue #12: the unbounded, instant, arbitrary-recipient withdrawal is
    // gone. The immediate path is admin-only and its recipient must be the
    // caller, the admin, or a registered fee recipient. Fee recipients must
    // use the timelocked request_withdraw_fees -> execute_withdraw_fees flow,
    // which is also capped so the accumulator can never be drained at once.
    //
    // Issue #57: AccumulatedFees is a cached sum of proven platform fees
    // (per-market ledger + pre-upgrade LegacyFees). Empty-side principal
    // never enters this pot. Admin instant withdraw is capped per call
    // (MAX_WITHDRAWAL_BPS) like the timelocked recipient path.

    pub fn withdraw_fees(
        env: Env,
        caller: Address,
        recipient: Address,
    ) -> Result<i128, MarketError> {
        Self::require_not_paused(&env)?;
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        Self::require_valid_fee_recipient(&env, &caller, &recipient)?;

        Self::ensure_fee_ledger_migrated(&env);
        let fees: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0);
        if fees <= 0 {
            return Err(MarketError::NoFeesToWithdraw);
        }
        let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
        Self::debit_proven_fees(&env, cap)?;

        let cfg: Config = env.storage().instance().get(&DataKey::Cfg).unwrap();
        token::Client::new(&env, &cfg.xlm_sac).transfer(
            &env.current_contract_address(),
            &recipient,
            &cap,
        );

        env.events().publish(
            (Symbol::new(&env, "fees_withdrawn"), caller, recipient.clone()),
            cap,
        );
        Ok(cap)
    }

    /// Issue #12: request a capped, timelocked withdrawal. The payout lands
    /// only after WITHDRAW_DELAY_SECS via execute_withdraw_fees, and the admin
    /// can cancel the request before then (see cancel_withdrawal_request).
    pub fn request_withdraw_fees(
        env: Env,
        caller: Address,
        recipient: Address,
        amount: i128,
    ) -> Result<(), MarketError> {
        Self::require_not_paused(&env)?;
        caller.require_auth();
        Self::require_admin_or_fee_recipient(&env, &caller)?;
        Self::require_valid_fee_recipient(&env, &caller, &recipient)?;

        if amount <= 0 {
            return Err(MarketError::InvalidAmount);
        }
        let key = DataKey::PendingWithdrawal(caller.clone());
        if env.storage().persistent().has(&key) {
            return Err(MarketError::WithdrawalRequestExists);
        }

        Self::ensure_fee_ledger_migrated(&env);
        let fees: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0);
        if amount > fees {
            return Err(MarketError::WithdrawalTooLarge);
        }
        // Cap: a single request may take at most MAX_WITHDRAWAL_BPS of the
        // accumulator, so even a compromised recipient cannot drain it fully.
        let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
        if amount > cap {
            return Err(MarketError::WithdrawalTooLarge);
        }

        env.storage().persistent().set(
            &key,
            &WithdrawalRequest {
                recipient: recipient.clone(),
                amount,
                requested_at: env.ledger().timestamp(),
            },
        );
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_BUMP, TTL_HIGH);
        env.events().publish(
            (Symbol::new(&env, "withdraw_requested"), caller, recipient),
            amount,
        );
        Ok(())
    }

    /// Issue #12: pay out a matured withdrawal request. Reverts while the
    /// WITHDRAW_DELAY_SECS timelock is still running.
    pub fn execute_withdraw_fees(env: Env, caller: Address) -> Result<i128, MarketError> {
        Self::require_not_paused(&env)?;
        caller.require_auth();
        let key = DataKey::PendingWithdrawal(caller.clone());
        let req: WithdrawalRequest = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(MarketError::NoWithdrawalRequest)?;

        let now = env.ledger().timestamp();
        if now < req.requested_at || now - req.requested_at < WITHDRAW_DELAY_SECS {
            return Err(MarketError::WithdrawalTooSoon);
        }

        // Debit the per-market / legacy ledger so the cached sum stays in
        // lockstep. Effects before interaction so a reentrant recipient
        // cannot re-read stale accumulator state.
        Self::debit_proven_fees(&env, req.amount)?;
        env.storage().persistent().remove(&key);

        let cfg: Config = env.storage().instance().get(&DataKey::Cfg).unwrap();
        token::Client::new(&env, &cfg.xlm_sac).transfer(
            &env.current_contract_address(),
            &req.recipient,
            &req.amount,
        );

        env.events().publish(
            (Symbol::new(&env, "fees_withdrawn"), caller, req.recipient.clone()),
            req.amount,
        );
        Ok(req.amount)
    }

    /// Issue #12: the admin can cancel a pending (not yet executed) withdrawal
    /// request, stopping a compromised fee recipient mid-timelock.
    pub fn cancel_withdrawal_request(
        env: Env,
        admin: Address,
        caller: Address,
    ) -> Result<(), MarketError> {
        Self::require_admin(&env, &admin)?;
        admin.require_auth();
        let key = DataKey::PendingWithdrawal(caller.clone());
        if !env.storage().persistent().has(&key) {
            return Err(MarketError::NoWithdrawalRequest);
        }
        env.storage().persistent().remove(&key);
        env.events().publish(
            (Symbol::new(&env, "withdraw_cancelled"), admin),
            caller,
        );
        Ok(())
    }

    // ── View Functions ────────────────────────────────────────────────────

    pub fn get_market(env: Env, market_id: u64) -> Result<Market, MarketError> {
        Self::load_market(&env, market_id)
    }

    // OPT: returns Bet (ABI-compatible) derived from BetEntry
    pub fn get_bet(env: Env, market_id: u64, user: Address) -> Result<Bet, MarketError> {
        let e: BetEntry = env
            .storage()
            .persistent()
            .get(&DataKey::Bet(market_id, user))
            .ok_or(MarketError::NoBetFound)?;
        Ok(Bet {
            amount: e.net,
            is_yes: e.is_yes,
            claimed: e.claimed,
        })
    }

    pub fn get_market_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::MarketCount)
            .unwrap_or(0)
    }

    /// Return the first bounded page of bettors for compatibility with the
    /// original ABI. Call `get_market_bettors_page` for later pages.
    pub fn get_market_bettors(env: Env, market_id: u64) -> Result<Vec<Address>, MarketError> {
        Self::get_market_bettors_page(env, market_id, 0, MAX_BETTORS_PER_PAGE)
    }

    /// Return at most `limit` bettors starting at the given index.
    ///
    /// The upper bound keeps each request's storage work predictable. The
    /// `start` index maps directly to the append-only bettor index, so paging
    /// does not scan or deserialize earlier entries.
    pub fn get_market_bettors_page(
        env: Env,
        market_id: u64,
        start: u32,
        limit: u32,
    ) -> Result<Vec<Address>, MarketError> {
        Self::load_market(&env, market_id)?;
        let count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::BettorCount(market_id))
            .unwrap_or(0);
        let page_limit = limit.min(MAX_BETTORS_PER_PAGE);
        let end = start.saturating_add(page_limit).min(count);
        let mut result: Vec<Address> = Vec::new(&env);
        for i in start..end {
            if let Some(addr) = env
                .storage()
                .persistent()
                .get::<DataKey, Address>(&DataKey::BettorAt(market_id, i))
            {
                result.push_back(addr);
            }
        }
        Ok(result)
    }

    pub fn get_accumulated_fees(env: Env) -> i128 {
        Self::ensure_fee_ledger_migrated(&env);
        env.storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0)
    }

    /// Genuine platform fees attributed to `market_id`. Independent of the
    /// cached global sum, so a cancel or withdraw on another market cannot
    /// change this value.
    pub fn get_market_fees(env: Env, market_id: u64) -> i128 {
        Self::market_fee_balance(&env, market_id)
    }

    /// Unattributed pre-upgrade balance. `market_id == 0` in the ledger.
    pub fn get_legacy_fees(env: Env) -> i128 {
        Self::ensure_fee_ledger_migrated(&env);
        env.storage()
            .instance()
            .get(&DataKey::LegacyFees)
            .unwrap_or(0)
    }

    /// Permissionless one-shot: snapshot the pre-upgrade global scalar into
    /// LegacyFees. Fresh deploys already set FeeLedgerMigrated at initialize.
    pub fn migrate_fee_ledger(env: Env) {
        Self::ensure_fee_ledger_migrated(&env);
    }

    /// Remaining TTL (ledgers) of the Market key. 0 means missing/expired —
    /// integrators can warn before funds become unrecoverable (issue #54).
    pub fn get_market_ttl(env: Env, market_id: u64) -> u32 {
        let key = DataKey::Market(market_id);
        if !env.storage().persistent().has(&key) {
            return 0;
        }
        env.storage().persistent().get_ttl(&key)
    }

    /// Permissionless keeper: anyone may pay to extend this market's
    /// Market/Bet/Payout/bettor-index keys. Does not resurrect expired entries.
    pub fn refresh_market_ttl(env: Env, market_id: u64) -> Result<u32, MarketError> {
        Self::refresh_market_keys(&env, market_id)
    }

    /// Permissionless migration: bump existing markets in
    /// `[start_id, start_id + limit)`. After a WASM upgrade this is how
    /// pre-existing entries get a fresh TTL without waiting for a user claim.
    pub fn refresh_markets(
        env: Env,
        start_id: u64,
        limit: u32,
    ) -> Result<u32, MarketError> {
        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MarketCount)
            .unwrap_or(0);
        let limit = limit.min(MAX_TTL_REFRESH_PAGE).max(1);
        let mut bumped: u32 = 0;
        let mut id = if start_id == 0 { 1 } else { start_id };
        let end = id.saturating_add(limit as u64);
        while id < end && id <= count {
            if Self::refresh_market_keys(&env, id).is_ok() {
                bumped += 1;
            }
            id += 1;
        }
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(bumped)
    }

    pub fn is_fee_recipient(env: Env, recipient: Address) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::FeeRecipient(recipient))
            .unwrap_or(false)
    }

    pub fn get_pending_withdrawal(env: Env, caller: Address) -> Option<WithdrawalRequest> {
        env.storage()
            .persistent()
            .get(&DataKey::PendingWithdrawal(caller))
    }

    pub fn get_payout(env: Env, market_id: u64, user: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Payout(market_id, user))
            .unwrap_or(0)
    }

    pub fn get_user_bet_count(env: Env, market_id: u64, user: Address) -> u32 {
        env.storage()
            .persistent()
            .get::<DataKey, BetEntry>(&DataKey::Bet(market_id, user))
            .map(|e| e.count)
            .unwrap_or(0)
    }

    pub fn get_bet_gross(env: Env, market_id: u64, user: Address) -> i128 {
        env.storage()
            .persistent()
            .get::<DataKey, BetEntry>(&DataKey::Bet(market_id, user))
            .map(|e| e.gross)
            .unwrap_or(0)
    }

    // ── Internal Helpers ──────────────────────────────────────────────────

    /// Snapshot the pre-upgrade AccumulatedFees scalar into LegacyFees.
    /// After this, AccumulatedFees is only a cached sum of the ledger.
    fn ensure_fee_ledger_migrated(env: &Env) {
        if env.storage().instance().has(&DataKey::FeeLedgerMigrated) {
            return;
        }
        let acc: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0);
        env.storage().instance().set(&DataKey::LegacyFees, &acc);
        env.storage()
            .instance()
            .set(&DataKey::FeeLedgerMigrated, &true);
    }

    fn market_fee_balance(env: &Env, market_id: u64) -> i128 {
        Self::ensure_fee_ledger_migrated(env);
        if market_id == LEGACY_MARKET_ID {
            env.storage()
                .instance()
                .get(&DataKey::LegacyFees)
                .unwrap_or(0)
        } else {
            env.storage()
                .persistent()
                .get(&DataKey::MarketFees(market_id))
                .unwrap_or(0)
        }
    }

    fn set_market_fee_balance(env: &Env, market_id: u64, amount: i128) {
        if market_id == LEGACY_MARKET_ID {
            env.storage().instance().set(&DataKey::LegacyFees, &amount);
        } else {
            let key = DataKey::MarketFees(market_id);
            env.storage().persistent().set(&key, &amount);
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_BUMP, TTL_HIGH);
        }
    }

    fn credit_market_fees(env: &Env, market_id: u64, amount: i128) {
        if amount <= 0 {
            return;
        }
        Self::ensure_fee_ledger_migrated(env);
        let next = Self::market_fee_balance(env, market_id) + amount;
        Self::set_market_fee_balance(env, market_id, next);
        let acc: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::AccumulatedFees, &(acc + amount));
    }

    fn debit_market_fees(env: &Env, market_id: u64, amount: i128) {
        if amount <= 0 {
            return;
        }
        let bal = Self::market_fee_balance(env, market_id);
        let take = if amount < bal { amount } else { bal };
        Self::set_market_fee_balance(env, market_id, bal - take);
        let acc: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0);
        let next_acc = if take < acc { acc - take } else { 0 };
        env.storage()
            .instance()
            .set(&DataKey::AccumulatedFees, &next_acc);
    }

    /// Drain LegacyFees first, then per-market balances from newest to oldest,
    /// keeping AccumulatedFees in lockstep. Used by withdraw paths.
    fn debit_proven_fees(env: &Env, amount: i128) -> Result<(), MarketError> {
        Self::ensure_fee_ledger_migrated(env);
        if amount <= 0 {
            return Err(MarketError::InvalidAmount);
        }
        let acc: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0);
        if amount > acc {
            return Err(MarketError::WithdrawalTooLarge);
        }

        let mut remaining = amount;
        let legacy = Self::market_fee_balance(env, LEGACY_MARKET_ID);
        let take_legacy = if remaining < legacy { remaining } else { legacy };
        if take_legacy > 0 {
            Self::debit_market_fees(env, LEGACY_MARKET_ID, take_legacy);
            remaining -= take_legacy;
        }
        if remaining > 0 {
            let count: u64 = env
                .storage()
                .instance()
                .get(&DataKey::MarketCount)
                .unwrap_or(0);
            let mut id = count;
            while remaining > 0 && id > 0 {
                let mf = Self::market_fee_balance(env, id);
                if mf > 0 {
                    let take = if remaining < mf { remaining } else { mf };
                    Self::debit_market_fees(env, id, take);
                    remaining -= take;
                }
                id -= 1;
            }
        }
        if remaining > 0 {
            return Err(MarketError::WithdrawalTooLarge);
        }
        Ok(())
    }

    fn sac_sentinel(env: &Env) -> BytesN<32> {
        BytesN::from_array(env, &[0u8; 32])
    }

    /// Live executable fingerprint for a dependency.
    /// token / referral / leaderboard must be WASM; xlm_sac must be the SAC.
    fn fingerprint(env: &Env, addr: &Address, expect_sac: bool) -> Result<BytesN<32>, MarketError> {
        match addr.executable() {
            Some(Executable::Wasm(hash)) => {
                if expect_sac {
                    return Err(MarketError::InvalidDependency);
                }
                Ok(hash)
            }
            Some(Executable::StellarAsset) => {
                if !expect_sac {
                    return Err(MarketError::InvalidDependency);
                }
                Ok(Self::sac_sentinel(env))
            }
            Some(Executable::Account) | None => Err(MarketError::InvalidDependency),
        }
    }

    fn fingerprint_config(
        env: &Env,
        token: &Address,
        referral: &Address,
        leaderboard: &Address,
        xlm_sac: &Address,
    ) -> Result<PinnedHashes, MarketError> {
        Ok(PinnedHashes {
            token: Self::fingerprint(env, token, false)?,
            referral: Self::fingerprint(env, referral, false)?,
            leaderboard: Self::fingerprint(env, leaderboard, false)?,
            xlm_sac: Self::fingerprint(env, xlm_sac, true)?,
        })
    }

    fn approver_index(approvers: &Vec<Address>, who: &Address) -> Option<u32> {
        let n = approvers.len();
        for i in 0..n {
            if approvers.get(i).unwrap() == *who {
                return Some(i);
            }
        }
        None
    }

    fn require_governor(env: &Env, caller: &Address) -> Result<(), MarketError> {
        if env
            .storage()
            .persistent()
            .get(&DataKey::Governor(caller.clone()))
            .unwrap_or(false)
        {
            return Ok(());
        }
        Err(MarketError::NotAuthorized)
    }

    #[inline]
    fn load_market(env: &Env, market_id: u64) -> Result<Market, MarketError> {
        env.storage()
            .persistent()
            .get(&DataKey::Market(market_id))
            .ok_or(MarketError::MarketNotFound)
    }

    fn bump_if_present(env: &Env, key: &DataKey) {
        if env.storage().persistent().has(key) {
            env.storage()
                .persistent()
                .extend_ttl(key, TTL_BUMP, TTL_HIGH);
        }
    }

    /// Extend every live fund-bearing key for `market_id`. Used by resolve
    /// (read-bump), the permissionless keeper, and the upgrade migration.
    fn refresh_market_keys(env: &Env, market_id: u64) -> Result<u32, MarketError> {
        let mkt_key = DataKey::Market(market_id);
        if !env.storage().persistent().has(&mkt_key) {
            return Err(MarketError::MarketNotFound);
        }
        Self::bump_if_present(env, &mkt_key);

        let bettors: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::BettorCount(market_id))
            .unwrap_or(0);
        Self::bump_if_present(env, &DataKey::BettorCount(market_id));
        for i in 0..bettors {
            let slot_key = DataKey::BettorAt(market_id, i);
            if let Some(addr) = env
                .storage()
                .persistent()
                .get::<DataKey, Address>(&slot_key)
            {
                Self::bump_if_present(env, &slot_key);
                Self::bump_if_present(env, &DataKey::Bet(market_id, addr.clone()));
                Self::bump_if_present(env, &DataKey::Payout(market_id, addr));
            }
        }
        Self::bump_if_present(env, &DataKey::MarketFees(market_id));
        Self::bump_if_present(env, &DataKey::ForfeitedPool(market_id));
        env.storage().instance().extend_ttl(TTL_BUMP, TTL_HIGH);
        Ok(bettors)
    }

    /// True if `user` still holds an unclaimed / unrefunded stake on `market_id`.
    fn has_bet_on_market(env: &Env, market_id: u64, user: &Address) -> bool {
        let Some(entry) = env
            .storage()
            .persistent()
            .get::<DataKey, BetEntry>(&DataKey::Bet(market_id, user.clone()))
        else {
            return false;
        };
        !entry.claimed && (entry.net > 0 || entry.gross > 0)
    }

    fn is_registered_resolver(env: &Env, user: &Address) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Resolver(user.clone()))
            .unwrap_or(false)
    }

    fn dispute_window_closed(env: &Env, resolved_at: u64) -> bool {
        let now = env.ledger().timestamp();
        now >= resolved_at && now - resolved_at >= DISPUTE_WINDOW_SECS
    }

    fn release_locked_fees(env: &Env, market_id: u64, pool: &mut ForfeitedPool) {
        if pool.frozen || pool.locked_fees <= 0 {
            return;
        }
        let amount = pool.locked_fees;
        pool.locked_fees = 0;
        Self::credit_market_fees(env, market_id, amount);
        let fp_key = DataKey::ForfeitedPool(market_id);
        env.storage().persistent().set(&fp_key, pool);
        env.storage()
            .persistent()
            .extend_ttl(&fp_key, TTL_BUMP, TTL_HIGH);
    }

    fn enforce_zero_side_claim_window(env: &Env, market_id: u64) -> Result<(), MarketError> {
        let fp_key = DataKey::ForfeitedPool(market_id);
        let mut pool: ForfeitedPool = match env.storage().persistent().get(&fp_key) {
            Some(p) => p,
            None => return Ok(()),
        };
        if pool.frozen {
            return Err(MarketError::MarketCancelled);
        }
        if !Self::dispute_window_closed(env, pool.resolved_at) {
            return Err(MarketError::DisputePending);
        }
        Self::release_locked_fees(env, market_id, &mut pool);
        Ok(())
    }

    // Issue #84: check a dependency's reported ABI version before invoking
    // it, so a unilateral upgrade with an incompatible credit/reward
    // signature fails with a clear error instead of an opaque
    // invoke_contract failure or silent misbehavior.
    fn require_compatible_referral(env: &Env, referral: &Address) -> Result<(), MarketError> {
        let version: u32 =
            env.invoke_contract(referral, &Symbol::new(env, "interface_version"), vec![env]);
        if version != EXPECTED_REFERRAL_INTERFACE_VERSION {
            return Err(MarketError::IncompatibleInterface);
        }
        Ok(())
    }

    fn require_compatible_leaderboard(env: &Env, leaderboard: &Address) -> Result<(), MarketError> {
        let version: u32 = env.invoke_contract(
            leaderboard,
            &Symbol::new(env, "interface_version"),
            vec![env],
        );
        if version != EXPECTED_LEADERBOARD_INTERFACE_VERSION {
            return Err(MarketError::IncompatibleInterface);
        }
        Ok(())
    }

    #[inline]
    fn require_not_paused(env: &Env) -> Result<(), MarketError> {
        if Self::is_paused(env.clone()) {
            return Err(MarketError::ContractPaused);
        }
        Ok(())
    }

    #[inline]
    fn require_admin(env: &Env, caller: &Address) -> Result<(), MarketError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(MarketError::NotInitialized)?;
        if *caller != admin {
            return Err(MarketError::NotAdmin);
        }
        Ok(())
    }

    fn require_admin_or_resolver(env: &Env, caller: &Address) -> Result<(), MarketError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(MarketError::NotInitialized)?;
        if *caller == admin {
            return Ok(());
        }
        if env
            .storage()
            .persistent()
            .get(&DataKey::Resolver(caller.clone()))
            .unwrap_or(false)
        {
            return Ok(());
        }
        Err(MarketError::NotResolver)
    }

    fn require_admin_or_fee_recipient(env: &Env, caller: &Address) -> Result<(), MarketError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(MarketError::NotInitialized)?;
        if *caller == admin {
            return Ok(());
        }
        if env
            .storage()
            .persistent()
            .get(&DataKey::FeeRecipient(caller.clone()))
            .unwrap_or(false)
        {
            return Ok(());
        }
        Err(MarketError::NotAuthorized)
    }

    // Issue #12: fees may only be paid to the caller, the admin, or a
    // registered fee recipient — never to an arbitrary address.
    fn require_valid_fee_recipient(
        env: &Env,
        caller: &Address,
        recipient: &Address,
    ) -> Result<(), MarketError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(MarketError::NotInitialized)?;
        if *recipient == *caller
            || *recipient == admin
            || env
                .storage()
                .persistent()
                .get(&DataKey::FeeRecipient(recipient.clone()))
                .unwrap_or(false)
        {
            return Ok(());
        }
        Err(MarketError::InvalidFeeRecipient)
    }

    // OPT: CreationWindow packed into two u32s stored as separate u32 keys
    // to avoid struct serialization. Actually simpler: store as (u64, u32) tuple
    // via a single key — Soroban serializes tuples efficiently.
    fn check_rate(env: &Env) -> Result<(), MarketError> {
        let now = env.ledger().timestamp();
        // (window_start, count) packed — 1 read instead of 1 struct deserialize
        let (ws, cnt): (u64, u32) = env
            .storage()
            .instance()
            .get(&DataKey::RateWindow)
            .unwrap_or((now, 0));

        // A timestamp regression must remain in the existing window. Using
        // checked subtraction prevents underflow from resetting the limit and
        // allowing an extra burst of market creations.
        let elapsed = now.checked_sub(ws).unwrap_or(0);
        let (new_ws, new_cnt) = if elapsed < 3600 {
            if cnt >= MAX_MARKETS_PER_HOUR {
                return Err(MarketError::RateLimitExceeded);
            }
            (ws, cnt + 1)
        } else {
            (now, 1)
        };
        env.storage()
            .instance()
            .set(&DataKey::RateWindow, &(new_ws, new_cnt));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
