//! The in-memory double must pass the same contract as the durable store —
//! the orchestration tests trust it as their stand-in for SQLite.

use swarmbot_store::{MemoryStore, run_contract_suite};

#[test]
fn the_memory_store_satisfies_the_persistence_contract() {
    run_contract_suite(MemoryStore::new).expect("the memory store is contract-clean");
}

#[test]
fn a_fresh_memory_store_starts_empty() {
    use swarmbot_contracts::repo::{BotRepo, EventLog, SettingsStore};
    let store = MemoryStore::default();
    assert!(store.list_bots().unwrap().is_empty());
    assert_eq!(store.epoch().unwrap(), 1);
    assert_eq!(
        store.settings().unwrap(),
        swarmbot_contracts::Settings::default()
    );
}
