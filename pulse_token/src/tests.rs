use crate::{DataKey, PULSETokenContract, PULSETokenContractClient, TTL_BUMP};
use soroban_sdk::{
    testutils::{
        storage::{Instance as _, Persistent as _},
        Address as _, Events, Ledger as _, LedgerInfo,
    },
    Address, Env, String, Symbol, TryFromVal, Val,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Deploy a fresh PULSEToken contract and return its client.
fn setup(env: &Env) -> PULSETokenContractClient<'_> {
    let id = env.register(PULSETokenContract, ());
    PULSETokenContractClient::new(env, &id)
}

/// Initialize with standard PULSE metadata and return the admin address.
fn init(env: &Env, client: &PULSETokenContractClient<'_>) -> Address {
    let admin = Address::generate(env);
    client.initialize(
        &admin,
        &String::from_str(env, "PULSE"),
        &String::from_str(env, "PLSE"),
        &7,
    );
    admin
}

/// A ledger whose `max_entry_ttl` is large enough to hold TTL_HIGH, matching
/// the network parameters the contract is deployed against.
fn ttl_env() -> Env {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set(LedgerInfo {
        timestamp: 1_000_000,
        protocol_version: 26,
        sequence_number: 100,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 100,
        min_persistent_entry_ttl: 100,
        max_entry_ttl: 10_000_000,
    });
    env
}

fn advance_ledgers(env: &Env, n: u32) {
    let seq = env.ledger().sequence();
    env.ledger().set(LedgerInfo {
        timestamp: env.ledger().timestamp() + 1,
        protocol_version: 26,
        sequence_number: seq + n,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 100,
        min_persistent_entry_ttl: 100,
        max_entry_ttl: 10_000_000,
    });
}

/// The live TTL the ledger holds for `key`, read from inside the contract.
fn balance_ttl(env: &Env, contract: &Address, holder: &Address) -> u32 {
    let key = DataKey::Balance(holder.clone());
    env.as_contract(contract, || env.storage().persistent().get_ttl(&key))
}

fn instance_ttl(env: &Env, contract: &Address) -> u32 {
    env.as_contract(contract, || env.storage().instance().get_ttl())
}

/// Mint `amount` to `to` and return the authorized minter.
fn mint_to(client: &PULSETokenContractClient<'_>, env: &Env, to: &Address, amount: i128) -> Address {
    let minter = Address::generate(env);
    client.set_minter(&minter);
    client.mint(&minter, to, &amount);
    minter
}

// ═══════════════════════════════════════════════════════════════════════════════
//  1. Initialize with metadata
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_initialize_with_metadata() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    assert_eq!(client.name(), String::from_str(&env, "PULSE"));
    assert_eq!(client.symbol(), String::from_str(&env, "PLSE"));
    assert_eq!(client.decimals(), 7);
    assert_eq!(client.total_supply(), 0_i128);
}

// ═══════════════════════════════════════════════════════════════════════════════
//  2. Add multiple authorized minters via set_minter
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_add_multiple_minters() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let minter1 = Address::generate(&env); // e.g. PredictionMarket
    let minter2 = Address::generate(&env); // e.g. ReferralRegistry

    // Both succeed — no panic
    client.set_minter(&minter1);
    client.set_minter(&minter2);
}

// ═══════════════════════════════════════════════════════════════════════════════
//  3. Mint by first authorized minter (PredictionMarket)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_mint_by_first_minter() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);

    let recipient = Address::generate(&env);
    client.mint(&minter, &recipient, &10_0000000_i128);

    assert_eq!(client.balance(&recipient), 10_0000000_i128);
    assert_eq!(client.total_supply(), 10_0000000_i128);
}

// ═══════════════════════════════════════════════════════════════════════════════
//  4. Mint by second authorized minter (ReferralRegistry)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_mint_by_second_minter() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let minter1 = Address::generate(&env);
    let minter2 = Address::generate(&env);
    client.set_minter(&minter1);
    client.set_minter(&minter2);

    let recipient = Address::generate(&env);
    client.mint(&minter1, &recipient, &10_0000000_i128);
    client.mint(&minter2, &recipient, &1_0000000_i128);

    assert_eq!(client.balance(&recipient), 11_0000000_i128);
    assert_eq!(client.total_supply(), 11_0000000_i128);
}

// ═══════════════════════════════════════════════════════════════════════════════
//  5. Reject mint by non-minter
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_reject_mint_by_non_minter() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let not_a_minter = Address::generate(&env);
    let recipient = Address::generate(&env);

    client.mint(&not_a_minter, &recipient, &10_0000000_i128); // panics
}

// ═══════════════════════════════════════════════════════════════════════════════
//  6. Remove minter via remove_minter and reject subsequent mint
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_remove_minter_then_reject() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);

    let recipient = Address::generate(&env);

    // Should work before removal
    client.mint(&minter, &recipient, &5_0000000_i128);
    assert_eq!(client.balance(&recipient), 5_0000000_i128);

    // Remove authorization
    client.remove_minter(&minter);

    // Should fail after removal
    client.mint(&minter, &recipient, &5_0000000_i128); // panics
}

// ═══════════════════════════════════════════════════════════════════════════════
//  7. Balance check after mint
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_balance_check_after_mint() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);

    let user_a = Address::generate(&env);
    let user_b = Address::generate(&env);

    // Zero before minting
    assert_eq!(client.balance(&user_a), 0_i128);
    assert_eq!(client.balance(&user_b), 0_i128);

    client.mint(&minter, &user_a, &100_0000000_i128);
    client.mint(&minter, &user_b, &50_0000000_i128);

    assert_eq!(client.balance(&user_a), 100_0000000_i128);
    assert_eq!(client.balance(&user_b), 50_0000000_i128);
}

// ═══════════════════════════════════════════════════════════════════════════════
//  8. Transfer between accounts
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_transfer_between_accounts() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);

    client.mint(&minter, &alice, &100_0000000_i128);

    client.transfer(&alice, &bob, &30_0000000_i128);

    assert_eq!(client.balance(&alice), 70_0000000_i128);
    assert_eq!(client.balance(&bob), 30_0000000_i128);
    // Transfer does not change total supply
    assert_eq!(client.total_supply(), 100_0000000_i128);
}

// ═══════════════════════════════════════════════════════════════════════════════
//  9. Reject transfer with insufficient balance
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
#[should_panic(expected = "Error(Contract, #4)")]
fn test_reject_transfer_insufficient_balance() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);

    client.mint(&minter, &alice, &10_0000000_i128);

    // Attempt to transfer more than balance
    client.transfer(&alice, &bob, &20_0000000_i128); // panics
}

// ═══════════════════════════════════════════════════════════════════════════════
//  10. Burn tokens
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_burn_tokens() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);

    let user = Address::generate(&env);
    client.mint(&minter, &user, &50_0000000_i128);

    client.burn(&user, &20_0000000_i128);

    assert_eq!(client.balance(&user), 30_0000000_i128);
    assert_eq!(client.total_supply(), 30_0000000_i128);
}

// ═══════════════════════════════════════════════════════════════════════════════
//  11. Total supply tracking across mint, transfer, and burn
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_total_supply_tracking() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);

    // Mint 100 to Alice → supply = 100
    client.mint(&minter, &alice, &100_0000000_i128);
    assert_eq!(client.total_supply(), 100_0000000_i128);

    // Mint 50 to Bob → supply = 150
    client.mint(&minter, &bob, &50_0000000_i128);
    assert_eq!(client.total_supply(), 150_0000000_i128);

    // Transfer 20 from Alice to Bob → supply unchanged = 150
    client.transfer(&alice, &bob, &20_0000000_i128);
    assert_eq!(client.total_supply(), 150_0000000_i128);

    // Alice burns 30 → supply = 120
    client.burn(&alice, &30_0000000_i128);
    assert_eq!(client.total_supply(), 120_0000000_i128);

    // Bob burns 10 → supply = 110
    client.burn(&bob, &10_0000000_i128);
    assert_eq!(client.total_supply(), 110_0000000_i128);

    // Final: Alice = 100 - 20 - 30 = 50, Bob = 50 + 20 - 10 = 60
    assert_eq!(client.balance(&alice), 50_0000000_i128);
    assert_eq!(client.balance(&bob), 60_0000000_i128);
}

// ═══════════════════════════════════════════════════════════════════════════════
//  12. Cross-contract interface versioning (issue #84)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_interface_version_reported() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    assert_eq!(client.interface_version(), 1);
}

// ═══════════════════════════════════════════════════════════════════════════════
//  17. Emergency Pause (issue #83)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_pause_unpause_admin_only() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    assert!(!client.is_paused());
    client.pause(&_admin);
    assert!(client.is_paused());
    client.unpause(&_admin);
    assert!(!client.is_paused());
}

#[test]
#[should_panic(expected = "Error(Contract, #6)")]
fn test_pause_rejects_non_admin() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let not_admin = Address::generate(&env);
    client.pause(&not_admin);
}

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn test_paused_rejects_mint() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);

    client.pause(&admin);

    let recipient = Address::generate(&env);
    client.mint(&minter, &recipient, &10_0000000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn test_paused_rejects_transfer() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);
    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    client.mint(&minter, &alice, &50_0000000_i128);

    client.pause(&admin);
    client.transfer(&alice, &bob, &10_0000000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn test_paused_rejects_burn() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);
    let user = Address::generate(&env);
    client.mint(&minter, &user, &50_0000000_i128);

    client.pause(&admin);
    client.burn(&user, &10_0000000_i128);
}

#[test]
fn test_view_functions_work_while_paused() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);
    let user = Address::generate(&env);
    client.mint(&minter, &user, &50_0000000_i128);

    client.pause(&admin);

    assert_eq!(client.balance(&user), 50_0000000_i128);
    assert_eq!(client.total_supply(), 50_0000000_i128);
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Issue #80: minter audit list
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn test_set_minter_idempotent() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);
    // Second call with same address must fail with AlreadyMinter (#10)
    client.set_minter(&minter);
}

#[test]
#[should_panic(expected = "Error(Contract, #11)")]
fn test_remove_minter_not_minter() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let not_minter = Address::generate(&env);
    // Must fail with NotMinter (#11)
    client.remove_minter(&not_minter);
}

#[test]
fn test_get_authorized_minters() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let minter1 = Address::generate(&env);
    let minter2 = Address::generate(&env);
    let minter3 = Address::generate(&env);

    client.set_minter(&minter1);
    client.set_minter(&minter2);
    client.set_minter(&minter3);

    let minters = client.get_authorized_minters();
    assert_eq!(minters.len(), 3);
    assert!(minters.contains(&minter1));
    assert!(minters.contains(&minter2));
    assert!(minters.contains(&minter3));

    // Remove one and verify the list shrinks
    client.remove_minter(&minter2);
    let minters = client.get_authorized_minters();
    assert_eq!(minters.len(), 2);
    assert!(minters.contains(&minter1));
    assert!(!minters.contains(&minter2));
    assert!(minters.contains(&minter3));
}

#[test]
fn test_mint_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let minter = Address::generate(&env);
    client.set_minter(&minter);
    let user = Address::generate(&env);
    client.mint(&minter, &user, &10_0000000_i128);

    // `env.events().all()` returns a `ContractEvents` in soroban-sdk 26, which
    // exposes its entries as an XDR slice rather than the older indexable Vec
    // of (address, topics, data) tuples.
    let events = env.events().all();
    let emitted = events.events();
    assert!(!emitted.is_empty(), "mint emitted no event");
    let soroban_sdk::xdr::ContractEventBody::V0(body) = &emitted.last().unwrap().body;
    let topic0 = Val::try_from_val(&env, &body.topics[0]).unwrap();
    let name = Symbol::try_from_val(&env, &topic0).unwrap();
    assert_eq!(name, Symbol::new(&env, "mint"));
}

// ══════════════════════════════════════════════════════════════════════════════
//  Issue #95 — pause / circuit breaker
// ══════════════════════════════════════════════════════════════════════════════

#[test]
#[should_panic(expected = "Error(Contract, #6)")]
fn test_pause_requires_admin() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    init(&env, &client);
    let rando = Address::generate(&env);
    client.set_paused(&rando, &true);
}

// ══════════════════════════════════════════════════════════════════════════════
//  Issue #97 — extend TTL on storage keys
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_transfer_extends_ttl_on_both_balance_keys() {
    let env = ttl_env();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    mint_to(&client, &env, &alice, 10_000_0000000_i128);
    // Give bob an entry too, so we are measuring an extension and not a
    // first-time write.
    mint_to(&client, &env, &bob, 1_0000000_i128);

    // Both holders go quiet for long enough that their entries are deep into
    // the TTL window (but not yet expired).
    advance_ledgers(&env, 4_000_000);

    let alice_before = balance_ttl(&env, &client.address, &alice);
    let bob_before = balance_ttl(&env, &client.address, &bob);
    assert!(
        alice_before < TTL_BUMP,
        "setup invariant broken: alice's TTL ({alice_before}) should have decayed below TTL_BUMP"
    );

    client.transfer(&alice, &bob, &500_0000000_i128);

    let alice_after = balance_ttl(&env, &client.address, &alice);
    let bob_after = balance_ttl(&env, &client.address, &bob);
    assert!(
        alice_after > alice_before && alice_after >= TTL_BUMP,
        "transfer did not extend the sender's balance TTL ({alice_before} -> {alice_after})"
    );
    assert!(
        bob_after > bob_before && bob_after >= TTL_BUMP,
        "transfer did not extend the recipient's balance TTL ({bob_before} -> {bob_after})"
    );
}

#[test]
fn test_burn_extends_ttl_on_remaining_balance() {
    let env = ttl_env();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let alice = Address::generate(&env);
    mint_to(&client, &env, &alice, 10_000_0000000_i128);
    advance_ledgers(&env, 4_000_000);

    let before = balance_ttl(&env, &client.address, &alice);
    assert!(before < TTL_BUMP, "setup invariant broken: TTL ({before}) should be below TTL_BUMP");

    // A partial burn leaves a live balance behind — that remainder needs its
    // TTL refreshed just as much as a transfer's does.
    client.burn(&alice, &1_000_0000000_i128);

    let after = balance_ttl(&env, &client.address, &alice);
    assert!(
        after > before && after >= TTL_BUMP,
        "burn did not extend the remaining balance's TTL ({before} -> {after})"
    );
    assert_eq!(client.balance(&alice), 9_000_0000000_i128);
}

#[test]
fn test_transfer_from_extends_ttl_on_both_balance_keys() {
    let env = ttl_env();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let spender = Address::generate(&env);
    mint_to(&client, &env, &alice, 10_000_0000000_i128);
    mint_to(&client, &env, &bob, 1_0000000_i128);

    advance_ledgers(&env, 4_000_000);
    // Approve *after* the fast-forward so the allowance itself is still live.
    client.approve(&alice, &spender, &1_000_0000000_i128, &(env.ledger().sequence() + 1_000));

    let alice_before = balance_ttl(&env, &client.address, &alice);
    let bob_before = balance_ttl(&env, &client.address, &bob);

    client.transfer_from(&spender, &alice, &bob, &500_0000000_i128);

    assert!(
        balance_ttl(&env, &client.address, &alice) > alice_before,
        "transfer_from did not extend the sender's balance TTL"
    );
    assert!(
        balance_ttl(&env, &client.address, &bob) > bob_before,
        "transfer_from did not extend the recipient's balance TTL"
    );
}

#[test]
fn test_mint_extends_balance_and_instance_ttl() {
    let env = ttl_env();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let alice = Address::generate(&env);
    let minter = mint_to(&client, &env, &alice, 1_0000000_i128);
    advance_ledgers(&env, 4_000_000);

    let balance_before = balance_ttl(&env, &client.address, &alice);
    let instance_before = instance_ttl(&env, &client.address);

    client.mint(&minter, &alice, &1_0000000_i128);

    assert!(
        balance_ttl(&env, &client.address, &alice) > balance_before,
        "mint did not extend the recipient's balance TTL"
    );
    // TotalSupply lives in instance storage — the other half of the
    // total_supply == sum(balances) invariant must stay alive too.
    assert!(
        instance_ttl(&env, &client.address) > instance_before,
        "mint did not extend the instance TTL that carries TotalSupply"
    );
}

// ── The headline: an active holder's tokens survive past the original TTL ────

#[test]
fn test_balance_survives_beyond_original_ttl_and_supply_stays_consistent() {
    // The adversarial scenario from the issue: Alice holds PULSE and the
    // ledger runs far past the TTL her entry was created with. Before the fix
    // her entry is evicted, her balance reads 0, and total_supply keeps
    // counting the tokens she can no longer touch. After the fix, every
    // operation that touches her balance renews it, so the invariant
    // total_supply == sum(balances) holds the whole way through.
    let env = ttl_env();
    let client = setup(&env);
    let _admin = init(&env, &client);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    mint_to(&client, &env, &alice, 10_000_0000000_i128);
    mint_to(&client, &env, &bob, 10_000_0000000_i128);

    let original_ttl = balance_ttl(&env, &client.address, &alice);

    // Run the ledger well past the entry's original lifetime, transacting
    // periodically the way a live holder would.
    let mut elapsed: u32 = 0;
    while elapsed < original_ttl * 2 {
        advance_ledgers(&env, 4_000_000);
        elapsed += 4_000_000;
        client.transfer(&alice, &bob, &1_0000000_i128);
        client.transfer(&bob, &alice, &1_0000000_i128);
    }

    // Balances intact...
    assert_eq!(client.balance(&alice), 10_000_0000000_i128);
    assert_eq!(client.balance(&bob), 10_000_0000000_i128);
    // ...and the token's core invariant still holds.
    assert_eq!(
        client.total_supply(),
        client.balance(&alice) + client.balance(&bob),
        "total_supply diverged from the sum of balances"
    );
    // Every entry still has a healthy lifetime ahead of it.
    assert!(balance_ttl(&env, &client.address, &alice) >= TTL_BUMP);
    assert!(balance_ttl(&env, &client.address, &bob) >= TTL_BUMP);
    assert!(instance_ttl(&env, &client.address) >= TTL_BUMP);
}

// ═══════════════════════════════════════════════════════════════════════════
// Issue #79 — supply cap prevents unbounded PULSE inflation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_supply_cap_default_unlimited() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);
    let minter = Address::generate(&env);
    let alice = Address::generate(&env);
    client.set_minter(&minter);

    // Default cap is 0 (unlimited) — minting should work.
    assert_eq!(client.get_supply_cap(), 0);
    client.mint(&minter, &alice, &1_000_0000000_i128);
    assert_eq!(client.total_supply(), 1_000_0000000_i128);
}

#[test]
fn test_set_supply_cap() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let admin = init(&env, &client);
    let minter = Address::generate(&env);
    let alice = Address::generate(&env);
    client.set_minter(&minter);

    // Set cap to 100 PULSE.
    client.set_supply_cap(&admin, &100_0000000_i128);
    assert_eq!(client.get_supply_cap(), 100_0000000_i128);

    // Mint 50 PULSE — should succeed.
    client.mint(&minter, &alice, &50_0000000_i128);
    assert_eq!(client.total_supply(), 50_0000000_i128);

    // Mint 60 more PULSE — total would be 110, exceeding cap.
    assert!(client.try_mint(&minter, &alice, &60_0000000_i128).is_err());
    assert_eq!(client.total_supply(), 50_0000000_i128); // unchanged
}

#[test]
fn test_supply_cap_exact_boundary() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let admin = init(&env, &client);
    let minter = Address::generate(&env);
    let alice = Address::generate(&env);
    client.set_minter(&minter);

    // Set cap to 100 PULSE.
    client.set_supply_cap(&admin, &100_0000000_i128);

    // Mint exactly 100 PULSE — should succeed.
    client.mint(&minter, &alice, &100_0000000_i128);
    assert_eq!(client.total_supply(), 100_0000000_i128);

    // Mint 1 stroop more — should fail.
    assert!(client.try_mint(&minter, &alice, &1).is_err());
    assert_eq!(client.total_supply(), 100_0000000_i128); // unchanged
}

#[test]
fn test_supply_cap_clear() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let admin = init(&env, &client);
    let minter = Address::generate(&env);
    let alice = Address::generate(&env);
    client.set_minter(&minter);

    // Set cap, mint up to it.
    client.set_supply_cap(&admin, &10_0000000_i128);
    client.mint(&minter, &alice, &10_0000000_i128);
    assert!(client.try_mint(&minter, &alice, &1).is_err());

    // Clear cap (set to 0) — minting resumes.
    client.set_supply_cap(&admin, &0);
    assert_eq!(client.get_supply_cap(), 0);
    client.mint(&minter, &alice, &1_0000000_i128);
    assert_eq!(client.total_supply(), 11_0000000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #6)")]
fn test_set_supply_cap_requires_admin() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let _admin = init(&env, &client);
    let rando = Address::generate(&env);

    // Non-admin must fail with NotAdmin.
    client.set_supply_cap(&rando, &1_000_0000000_i128);
}

#[test]
fn test_supply_cap_balance_unchanged_on_reject() {
    let env = Env::default();
    env.mock_all_auths();
    let client = setup(&env);
    let admin = init(&env, &client);
    let minter = Address::generate(&env);
    let alice = Address::generate(&env);
    client.set_minter(&minter);

    // Set cap and mint up to it.
    client.set_supply_cap(&admin, &10_0000000_i128);
    client.mint(&minter, &alice, &10_0000000_i128);

    // Attempt to exceed cap — balance must remain unchanged.
    let balance_before = client.balance(&alice);
    assert!(client.try_mint(&minter, &alice, &1_0000000_i128).is_err());
    assert_eq!(client.balance(&alice), balance_before); // no state corruption
}
