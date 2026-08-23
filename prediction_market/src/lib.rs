#![no_std]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, symbol_short, token, vec,
    Address, BytesN, Env, Executable, IntoVal, String, Symbol, TryIntoVal, Val, Vec,
};

// get_ttl() is a test-only SDK extension; gate it so non-test builds compile
// without the testutils feature.
#[cfg(any(test, feature = "testutils"))]
use soroban_sdk::testutils::storage::Persistent as _;

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
// issue #56: the creation rate limit is anchored to the ledger sequence —
// strictly monotonic on any Soroban network — instead of wall-clock
// timestamps, which can regress and previously underflowed here.
const RATE_WINDOW_LEDGERS: u32 = 720; // ≈1h of ledgers at ~5s per ledger
const MAX_MARKETS_PER_WINDOW: u32 = 10;
const MIN_MARKET_DURATION_SECS: u64 = 60; // issue #10: no instantly-expired markets
const MAX_BETTORS_PER_PAGE: u32 = 100;
/// Sentinel market id for the unattributed pre-upgrade fee balance (#57).
const LEGACY_MARKET_ID: u64 = 0;

// Fee adjustments: multiply before divide to avoid precision.
// net and total_fee are derived from ONE family so that
// `net + total_fee == amount` ALWAYS holds (no stroop leakage):
//   net       = floor(amount * 0.98)
//   total_fee = amount - net = ceil(amount * 0.02)
// (TOTAL fee rate is effectively 200 bps — split into 150 bps platform and
// the remainder referral once the platform share is resolved.)
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

// Issue #93: config changes are staged and only take effect after this delay.
// A compromised admin key can no longer redirect all fund flows instantly:
// off-chain monitors get a window to detect the change (via the emitted
// ConfigChangeStaged event) and the admin can cancel it before it lands.
const CONFIG_CHANGE_DELAY_SECS: u64 = 86_400; // 24h timelock
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
    // Retained for ABI stability — error codes must not shift on upgrade.
    // No longer produced: users may now hold positions on both sides.
    #[allow(dead_code)]
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
    // Issue #95: operation blocked because the contract is paused.
    Paused = 26,
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
    AccumulatedFees,
    Market(u64),
    // Per-market ledger of the fees this market actually contributed to
    // AccumulatedFees: platform fee for every bet, plus referral fee only
    // when it was NOT paid out to a referrer. cancel_market reclaims this
    // exact amount instead of reverse-engineering fees from the net pool
    // (issue #87).
    MarketAccumulatedFees(u64),
    Bet(u64, Address), // two-sided net_yes/net_no + gross + count packed; see BetEntry
    BettorCount(u64),
    BettorAt(u64, u32),
    Resolver(Address),
    FeeRecipient(Address),
    HasReferrer(Address),
    RateWindowSeq, // (u32 window_start_seq, u32 count) — ledger-sequence anchored (issue #56)
    // ── Settlement-time payouts (issue #2) ───────────────────────────────
    Payout(u64, Address), // i128 — exact payout computed at resolve time
    // ── Fee provenance (issue #4): per-market sub-ledger ──────────────────
    // ── Fee provenance (issue #57): per-market earned fees ───────────────────────
    MarketFees(u64),   // i128 — genuine earned fees for this market
    LegacyFees,        // i128 — unattributed pre-migration balance
    FeeLedgerMigrated, // bool — one-shot migration of the old global scalar
    // ── Timelocked withdrawal requests (issue #12) ───────────────────────
    PendingWithdrawal(Address), // caller -> WithdrawalRequest
    // ── Zero-side principal vault (issue #3) ─────────────────────────────
    // Holds empty-side pool metadata + locked platform fees. Never mixed into
    // the withdrawable AccumulatedFees sum until the dispute window ends.
    ForfeitedPool(u64),
    ZeroSideDisputeActive,
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

// Issue #93: emitted when set_config stages a change, so off-chain indexers
// can alert on suspicious address redirects before the timelock matures.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigChangeStaged {
    pub pending_at: u64,
    pub token: Address,
    pub referral: Address,
    pub leaderboard: Address,
    pub xlm_sac: Address,
}

// ── BetEntry: two-sided position + Gross + BetCount in one slot ────────────
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
    pub net_yes: i128, // post-fee net committed to YES (used for payout)
    pub net_no: i128,  // post-fee net committed to NO (used for payout)
    pub gross: i128,   // pre-fee total sent across both sides (used for cancel_refund)
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

// Kept for ABI compatibility — frontend reads Bet fields.
// For a two-sided position, amount/is_yes report the dominant side.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bet {
    pub amount: i128,
    pub is_yes: bool,
    pub claimed: bool,
}

// Full two-sided position view — exposes both sides of a user's bet.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Position {
    pub net_yes: i128,
    pub net_no: i128,
    pub gross: i128,
    pub claimed: bool,
    pub count: u32,
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
        env.storage().instance().set(&DataKey::FeeLedgerMigrated, &true);
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

    /// Stage a config change (token / referral / leaderboard / xlm_sac). Admin
    /// only. The change does NOT take effect immediately: it must mature past
    /// CONFIG_CHANGE_DELAY_SECS via execute_set_config, giving off-chain
    /// monitors time to detect it (via the ConfigChangeStaged event) and the
    /// admin time to cancel it with cancel_set_config (issue #93).
    /// Propose a Config change. Does **not** take effect immediately.
    ///
    /// Live WASM hashes are read on-chain (not caller-supplied), the
    /// proposal is emitted for monitors, and it only becomes active after
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
        let staged = ConfigChangeStaged {
            pending_at: pending.requested_at,
            token: pending.cfg.token.clone(),
            referral: pending.cfg.referral.clone(),
            leaderboard: pending.cfg.leaderboard.clone(),
            xlm_sac: pending.cfg.xlm_sac.clone(),
        };
        env.storage()
            .instance()
            .set(&DataKey::PendingConfig, &pending);
        staged.publish(&env);
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

        // Re-read live WASM hashes so a dependency cannot swap bytecode
        // during the delay window.
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

    /// Read the currently staged (not yet effective) config change, if any.
    pub fn get_pending_config(env: Env) -> Option<PendingConfigChange> {
        env.storage().instance().get(&DataKey::PendingConfig)
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

    // ── Emergency circuit breaker (issue #95) ───────────────────────────────

    /// Halt (or resume) all risk-creating, settlement and withdrawal
    /// operations: place_bet, create_market, resolve_market, cancel_market,
    /// withdraw_fees, request_withdraw_fees and execute_withdraw_fees are
    /// blocked while paused. User recovery paths — claim() and
    /// cancel_refund() — stay available on purpose, so an emergency pause
    /// never locks user funds in the contract. Admin only; idempotent.
    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), MarketError> {
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

    fn require_not_paused(env: &Env) -> Result<(), MarketError> {
        if env
            .storage()
            .instance()
            .get::<_, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(MarketError::Paused);
        }
        Ok(())
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
    }

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

        // Spam guard from single read (both sides share the bet counter)
        if let Some(ref e) = existing {
            if e.count >= MAX_BETS_PER_USER {
                env.storage().persistent().remove(&lock_key);
                return Err(MarketError::TooManyBets);
            }
        }

        let is_increase = existing.is_some();

        // ── Exact fee decomposition (net + platform + referral == amount) ──
        let net = amount * NET_NUMERATOR / BPS_DENOM;
        let total_fee = amount - net;
        let platform_fee = amount * PLATFORM_FEE_BPS / BPS_DENOM;
        let referral_fee = total_fee - platform_fee;

        // OPT: one Config read instead of 4 separate instance reads
        let cfg: Config = env.storage().instance().get(&DataKey::Cfg).unwrap();

        // ── Issue 89: Write ALL state BEFORE external calls (check-effects-interaction) ──

        // Credit the platform fee to this market's provenance ledger. The
        // referral fee's disposition is decided by credit() below: paid to a
        // registered referrer -> gone; unregistered/no referrer -> the fee is
        // returned and retained as earned fees for this market (#78/#87/#57).
        Self::credit_market_fees(&env, market_id, platform_fee);

        // ── Write BetEntry (net + gross + count in one write) ─────────────
        let new_entry = match existing {
            Some(mut e) => {
                if is_yes {
                    e.net_yes += net;
                } else {
                    e.net_no += net;
                }
                e.gross += amount;
                e.count += 1;
                e
            }
            None => BetEntry {
                net_yes: if is_yes { net } else { 0 },
                net_no: if is_yes { 0 } else { net },
                gross: amount,
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

        // ── External calls (issue 89: after ALL state writes) ─────────────

        // ── XLM transfer user → this contract ────────────────────────────
        let xlm = token::Client::new(&env, &cfg.xlm_sac);
        let this = env.current_contract_address();
        xlm.transfer(&user, &this, &amount);

        // ── Referral (live lookup — no stale cache) ───────────────────────
        Self::require_compatible_referral(&env, &cfg.referral)?;
        xlm.transfer(&this, &cfg.referral, &referral_fee);
        let paid_referrer: bool = env.invoke_contract(
            &cfg.referral,
            &Symbol::new(&env, "credit"),
            vec![
                &env,
                this.clone().into_val(&env),
                user.clone().into_val(&env),
                referral_fee.into_val(&env),
            ],
        );
        if !paid_referrer {
            // credit() returned the fee: the market keeps it, so it counts as
            // genuine earned fees for this market.
            Self::credit_market_fees(&env, market_id, referral_fee);
        }

        // ── Release reentrancy lock ──────────────────────────────────────
        env.storage().persistent().remove(&lock_key);
        env.events().publish(
            (Symbol::new(&env, "bet_placed"), user, market_id),
            (is_yes, amount, net),
        );
        Ok(())
    }

    // ── Position management (issue #98) ────────────────────────────────────

    // Users may REDUCE — or fully CLOSE — an existing same-side position while
    // the market is live, which is the accounting-consistent way to manage
    // exposure: the payout model is one-entry-per-user (resolve_market computes
    // per-winner payouts from the per-user single entry), so opening a hedge on
    // the opposite side would break pool math by letting one user count toward
    // both sides. Reduction keeps every invariant intact:
    //   - market totals shrink by exactly the net portion being released;
    //   - fees are released back only if they are still held by the contract
    //     (platform always; referral only when it was never paid to a referrer);
    //   - claim()/resolve payouts stay exact (Σ payouts + dust == pool).
    // Comparable to cancel_refund, but scoped to a live market and a portion.
    pub fn reduce_position(
        env: Env,
        user: Address,
        market_id: u64,
        amount: i128,
    ) -> Result<i128, MarketError> {
        user.require_auth();

        if amount <= 0 {
            return Err(MarketError::InvalidAmount);
        }

        let mut market = Self::load_market(&env, market_id)?;
        if market.cancelled {
            return Err(MarketError::MarketCancelled);
        }
        if market.resolved {
            return Err(MarketError::MarketResolved);
        }
        if env.ledger().timestamp() >= market.end_time {
            return Err(MarketError::MarketExpired);
        }

        let cfg: Config = env.storage().instance().get(&DataKey::Cfg).unwrap();

        // Live referral lookup: only users whose referral fee was actually
        // retained (no registered referrer) get that share back on reduction.
        let has_referrer: bool = env.invoke_contract(
            &cfg.referral,
            &Symbol::new(&env, "has_referrer"),
            vec![&env, user.clone().into_val(&env)],
        );

        let bet_key = DataKey::Bet(market_id, user.clone());
        let mut entry: BetEntry = env
            .storage()
            .persistent()
            .get(&bet_key)
            .ok_or(MarketError::NoBetFound)?;
        if entry.gross < amount {
            return Err(MarketError::InvalidAmount);
        }

        // Decompose exactly like place_bet so partial reductions stay integral.
        let net_part = amount * NET_NUMERATOR / BPS_DENOM;
        let plat_part = amount * PLATFORM_FEE_BPS / BPS_DENOM;
        let ref_part = amount - net_part - plat_part;

        let refund = net_part + plat_part + if has_referrer { 0 } else { ref_part };

        // Determine which side to reduce from.
        let is_yes = entry.net_yes >= entry.net_no;
        let dominated_net = if is_yes {
            &mut entry.net_yes
        } else {
            &mut entry.net_no
        };

        // ── State FIRST, external call last ───────────────────────────────
        *dominated_net -= net_part;
        entry.gross -= amount;

        // The released fee share (platform + retained referral) leaves this
        // market's provenance ledger and the global accumulator (#4/#57).
        let released = plat_part + if has_referrer { 0 } else { ref_part };
        Self::debit_market_fees(&env, market_id, released);

        if is_yes {
            market.total_yes -= net_part;
        } else {
            market.total_no -= net_part;
        }

        let fully_closed = entry.gross == 0;
        if fully_closed {
            // A fully-reduced position is removed entirely: no payout entry is
            // created for it at resolution, and claim() reports NoBetFound
            // (no free PULSE/points for an empty position).
            env.storage().persistent().remove(&bet_key);
        } else {
            env.storage().persistent().set(&bet_key, &entry);
            env.storage()
                .persistent()
                .extend_ttl(&bet_key, TTL_BUMP, TTL_HIGH);
        }
        let mkt_key = DataKey::Market(market_id);
        env.storage().persistent().set(&mkt_key, &market);
        env.storage()
            .persistent()
            .extend_ttl(&mkt_key, TTL_BUMP, TTL_HIGH);

        let cfg: Config = env.storage().instance().get(&DataKey::Cfg).unwrap();
        token::Client::new(&env, &cfg.xlm_sac).transfer(
            &env.current_contract_address(),
            &user,
            &refund,
        );
        Ok(refund)
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

        let mut acc_fees: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0);

        let mut dust: i128 = 0;
        if winning_side == 0 {
            // Issues #3/#57: never sweep user principal into the fee pot.
            // Credit each bettor their net via the payout ledger so claim()
            // can return it; lock this market's platform fees into
            // ForfeitedPool for the dispute window.
            let bettors_count: u32 = env
                .storage()
                .persistent()
                .get(&DataKey::BettorCount(market_id))
                .unwrap_or(0);
            let mut principal: i128 = 0;
            for i in 0..bettors_count {
                let slot_key = DataKey::BettorAt(market_id, i);
                let bettor: Address =
                    if let Some(a) = env.storage().persistent().get(&slot_key) {
                        a
                    } else {
                        continue;
                    };
                let bet_key = DataKey::Bet(market_id, bettor.clone());
                if let Some(entry) = env.storage().persistent().get::<DataKey, BetEntry>(&bet_key) {
                    let entry_net = entry.net_yes + entry.net_no;
                    if entry_net > 0 {
                        principal += entry_net;
                        let payout_key = DataKey::Payout(market_id, bettor.clone());
                        env.storage().persistent().set(&payout_key, &entry_net);
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
            env.storage()
                .persistent()
                .set(&DataKey::ZeroSideDisputeActive, &true);
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
                let bet_key = DataKey::Bet(market_id, bettor.clone());                    if let Some(entry) = env.storage().persistent().get::<DataKey, BetEntry>(&bet_key) {
                    let entry_net = if outcome { entry.net_yes } else { entry.net_no };
                    if entry_net > 0 {
                        let payout = (entry_net * total_pool) / winning_side;
                        let payout_key = DataKey::Payout(market_id, bettor.clone());
                        env.storage().persistent().set(&payout_key, &payout);
                        env.storage()
                            .persistent()
                            .extend_ttl(&payout_key, TTL_BUMP, TTL_HIGH);
                        payout_sum += payout;
                    }
                }
            }

            dust = total_pool - payout_sum;
            debug_assert!(dust >= 0, "payouts must never exceed the pool");
        }

        // Re-read: the zero-side branch may have debited locked fees out of
        // the accumulator (ForfeitedPool), so never clobber with a stale local.
        acc_fees = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0);
        acc_fees += dust;
        env.storage()
            .instance()
            .set(&DataKey::AccumulatedFees, &acc_fees);

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

        // Reclaim this market's remaining provenance-ledger balance (the
        // genuine earned-but-unsettled fees it retained) so a cancelled
        // market cannot leave withdrawable phantom fees behind (#57).
        let reclaim = Self::market_fee_balance(&env, market_id);
        Self::debit_market_fees(&env, market_id, reclaim);

        // The market is cancelled and refunded in full — its fee ledger is
        // dropped above.

        let net_pool = market.total_yes + market.total_no;
        env.events().publish(
            (Symbol::new(&env, "market_cancelled"), admin, market_id),
            net_pool,
        );
        Ok(())
    }

    pub fn cancel_refund(env: Env, user: Address, market_id: u64) -> Result<i128, MarketError> {
        user.require_auth();

        let mut market = Self::load_market(&env, market_id)?;
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
        let net_yes = entry.net_yes;
        let net_no = entry.net_no;
        // Issue #58: zero both gross (idempotency guard) and nets so that
        // get_bet no longer reports a staked amount after the refund.
        entry.gross = 0;
        entry.net_yes = 0;
        entry.net_no = 0;
        env.storage().persistent().set(&bet_key, &entry);

        // Issue #58: decrement market totals so total_yes/total_no reflect
        // that this bet has been refunded.
        let mkt_key = DataKey::Market(market_id);
        market.total_yes = market.total_yes.saturating_sub(net_yes);
        market.total_no = market.total_no.saturating_sub(net_no);
        env.storage().persistent().set(&mkt_key, &market);

        // Read-time TTL refresh (issue #9): a refund must not be able to observe
        // an expired bet/market record — keep both alive so a user who returns
        // late to a cancelled market can still pull their refund.
        env.storage()
            .persistent()
            .extend_ttl(&bet_key, TTL_BUMP, TTL_HIGH);
        env.storage()
            .persistent()
            .extend_ttl(&mkt_key, TTL_BUMP, TTL_HIGH);

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

        // Winning payout is driven by the net committed to the winning side
        // only; the losing side's net stays in the pool for all winners.
        let winning_net = if market.outcome {
            entry.net_yes
        } else {
            entry.net_no
        };
        let is_winner = winning_net > 0;
        let total_pool = market.total_yes + market.total_no;
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

        // XLM payout straight from the settlement-time payout ledger.
        // Winners are exactly the bettors who own a Payout entry; empty-side
        // bettors (#3) get their net principal back through the same ledger.
        let payout: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Payout(market_id, user.clone()))
            .unwrap_or(0);
        // Gate on the dispute window (#3): a freshly resolved empty-side
        // market keeps its vault frozen until DISPUTE_WINDOW_SECS passes.
        let zero_side_frozen = env
            .storage()
            .persistent()
            .get::<DataKey, ForfeitedPool>(&DataKey::ForfeitedPool(market_id))
            .map(|fp| fp.frozen || env.ledger().timestamp() < fp.resolved_at + DISPUTE_WINDOW_SECS)
            .unwrap_or(false);
        if !zero_side_frozen && payout > 0 {
            token::Client::new(&env, &cfg.xlm_sac).transfer(&this, &user, &payout);
        }

        let real_win = is_winner && winning_side > 0;
        let (points, tokens): (u64, i128) = if real_win {
            (WIN_POINTS, WIN_TOKENS)
        } else {
            (LOSE_POINTS, LOSE_TOKENS)
        };

        // Immediate reward settlement (issue #139): add_pts is deprecated in
        // favor of reward(), which credits points/win-loss and mints the
        // PULSE reward in one cross-contract hop.
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
            (is_winner, payout, real_win),
        );
        Ok(())
    }

    // ── Withdraw Fees ─────────────────────────────────────────────────────
    // Issue #12: the unbounded, instant, arbitrary-recipient withdrawal is
    // gone. The immediate path is admin-only and its recipient must be the
    // caller, the admin, or a registered fee recipient. Fee recipients must
    // use the timelocked request_withdraw_fees -> execute_withdraw_fees flow,
    // which is also capped so the accumulator can never be drained at once.

    pub fn withdraw_fees(
        env: Env,
        caller: Address,
        recipient: Address,
    ) -> Result<i128, MarketError> {
        Self::require_not_paused(&env)?;
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        Self::require_valid_fee_recipient(&env, &caller, &recipient)?;

        // Issue #3: while any empty-side dispute window is open (or the market
        // is frozen), nothing is withdrawable.
        if env.storage().persistent().has(&DataKey::ZeroSideDisputeActive) {
            return Err(MarketError::NoFeesToWithdraw);
        }

        // Only fees that are EARNED (market settled / swept) may be withdrawn.
        // Fees of open markets are reserved to back a possible cancellation
        // refund, so they are excluded from what is withdrawable.
        let fees: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0);
        if fees <= 0 {
            return Err(MarketError::NoFeesToWithdraw);
        }
        // Issue #57: the admin's instant withdrawal is capped per call so a
        // compromised fee recipient cannot drain the pot at once; provenance
        // is debited pro-rata (legacy first, then newest markets).
        let mut amount = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
        if amount == 0 {
            // Dust-safety: never trap the tail of the pot behind the cap.
            amount = fees;
        }
        Self::debit_proven_fees(&env, amount)?;

        let cfg: Config = env.storage().instance().get(&DataKey::Cfg).unwrap();
        token::Client::new(&env, &cfg.xlm_sac).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        env.events().publish(
            (Symbol::new(&env, "fees_withdrawn"), caller, recipient.clone()),
            amount,
        );
        Ok(amount)
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

        let fees: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0);
        // Only earned fees may be scheduled (#57): the requested amount is
        // debited immediately from provenance (legacy first, then newest
        // markets), so pending requests can never double-spend.
        if amount > fees {
            return Err(MarketError::WithdrawalTooLarge);
        }
        Self::debit_proven_fees(&env, amount)?;

        // Cap: a single request may take at most MAX_WITHDRAWAL_BPS of the
        // earned accumulator, so even a compromised recipient cannot drain it fully.
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

        // Re-validate at payout time: the caller's fee-recipient role and the
        // destination can both be revoked/changed during the timelock, so a
        // compromised recipient removed mid-window must not be able to collect.
        Self::require_admin_or_fee_recipient(&env, &caller)?;
        Self::require_valid_fee_recipient(&env, &caller, &req.recipient)?;

        let now = env.ledger().timestamp();
        if now < req.requested_at || now - req.requested_at < WITHDRAW_DELAY_SECS {
            return Err(MarketError::WithdrawalTooSoon);
        }

        // The accumulator was already debited at request time (issue #57
        // provenance), so executing only releases the pending record and pays.
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
        let req: WithdrawalRequest = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(MarketError::NoWithdrawalRequest)?;
        env.storage().persistent().remove(&key);

        // Refund the recorded amount back to the accumulator: the request no
        // longer exists, so those fees are withdrawable again.
        let acc: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::AccumulatedFees, &(acc + req.amount));
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
            amount: e.net_yes.max(e.net_no),
            is_yes: e.net_yes >= e.net_no,
            claimed: e.claimed,
        })
    }

    // Full two-sided position view (0 on an untouched side)
    pub fn get_position(env: Env, market_id: u64, user: Address) -> Result<Position, MarketError> {
        let e: BetEntry = env
            .storage()
            .persistent()
            .get(&DataKey::Bet(market_id, user))
            .ok_or(MarketError::NoBetFound)?;
        Ok(Position {
            net_yes: e.net_yes,
            net_no: e.net_no,
            gross: e.gross,
            claimed: e.claimed,
            count: e.count,
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
        env.storage()
            .instance()
            .get(&DataKey::AccumulatedFees)
            .unwrap_or(0)
    }

    /// Remaining TTL (ledgers) of the Market key. 0 means missing/expired —
    /// integrators can warn before funds become unrecoverable (issue #54).
    ///
    /// Test-only: reading a live TTL requires the SDK's testutils extension,
    /// so this view function is compiled out of non-test WASM builds.
    #[cfg(any(test, feature = "testutils"))]
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

    // Fee provenance views (issue #57)
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
        !entry.claimed && (entry.net_yes > 0 || entry.net_no > 0 || entry.gross > 0)
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
        env.storage()
            .persistent()
            .remove(&DataKey::ZeroSideDisputeActive);
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

    // ── Fee provenance internals (issue #57) ──────────────────────────────

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

    /// Debit `amount` of earned fees from provenance: the unattributed legacy
    /// balance first, then per-market ledgers newest-first. Reverts with
    /// WithdrawalTooLarge if provenance cannot fully cover the amount.
    fn debit_proven_fees(env: &Env, amount: i128) -> Result<(), MarketError> {
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
    // issue #56: the window is anchored to the ledger sequence rather than
    // wall-clock time. Ledger sequences are strictly monotonic on any Soroban
    // network, so timestamp regressions can neither underflow the elapsed
    // computation nor reset an active rate-limit window.
    fn check_rate(env: &Env) -> Result<(), MarketError> {
        let seq = env.ledger().sequence();
        // (window_start_seq, count) — 1 read, cheap tuple serialization
        let (ws, cnt): (u32, u32) = env
            .storage()
            .instance()
            .get(&DataKey::RateWindowSeq)
            .unwrap_or((seq, 0));

        // Defensive fail-closed: if a hostile/incompatible host ever reported
        // an out-of-order sequence, treat it as zero elapsed and keep the
        // current window instead of resetting the rate limit.
        let elapsed = seq.saturating_sub(ws);
        let (new_ws, new_cnt) = if elapsed < RATE_WINDOW_LEDGERS {
            if cnt >= MAX_MARKETS_PER_WINDOW {
                return Err(MarketError::RateLimitExceeded);
            }
            (ws, cnt + 1)
        } else {
            (seq, 1)
        };
        env.storage()
            .instance()
            .set(&DataKey::RateWindowSeq, &(new_ws, new_cnt));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
