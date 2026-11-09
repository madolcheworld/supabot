//! The durable double: the same contract suite, run against a private
//! in-memory SQLite database *and* against real files — plus the durability
//! promises that only a file store can make.

use std::sync::atomic::{AtomicU64, Ordering};

use swarmbot_contracts::repo::{BotRepo, EntryRepo, EventLog, SettingsStore};
use swarmbot_contracts::{Bot, ConversationId, Entry, LiveCursor, LiveEvent, Settings};
use swarmbot_store::{SqliteStore, run_contract_suite};

#[test]
fn the_sqlite_store_satisfies_the_contract_in_memory() {
    run_contract_suite(|| SqliteStore::open_in_memory().expect("open"))
        .expect("the sqlite store is contract-clean");
}

#[test]
fn the_sqlite_store_satisfies_the_contract_on_disk() {
    // The same suite against real files: every `make()` gets its own
    // database, so areas stay independent exactly as they are in memory.
    let dir = tempfile::tempdir().expect("temp dir");
    let counter = AtomicU64::new(0);
    run_contract_suite(move || {
        let index = counter.fetch_add(1, Ordering::Relaxed);
        SqliteStore::open(&dir.path().join(format!("store-{index}.sqlite"))).expect("open")
    })
    .expect("the file-backed sqlite store is contract-clean");
}

#[test]
fn a_file_store_survives_being_reopened() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("swarmbot.sqlite");
    let bot = Bot::new("lead", "Lead", "you lead", "fake", ConversationId::new());
    let entry = Entry::owner_message(bot.direct_conversation_id.clone(), 1, "remember me");
    let settings = Settings {
        max_hops: 4,
        ..Settings::default()
    };

    let cursor = {
        let store = SqliteStore::open(&path).expect("open");
        store.insert_bot(&bot).expect("insert bot");
        store.upsert_entry(&entry).expect("upsert entry");
        store.put_settings(&settings).expect("put settings");
        store
            .append_event(&LiveEvent::Synchronized {
                cursor: LiveCursor::default(),
            })
            .expect("append event")
    };

    // Reopening runs migrations a second time: that must be a no-op, not a
    // failure and not a duplicate — and the data must still be there.
    let reopened = SqliteStore::open(&path).expect("reopen");
    assert_eq!(reopened.get_bot(&bot.id).unwrap(), Some(bot));
    assert_eq!(
        reopened
            .entries_after(&entry.conversation_id, 0, 10)
            .unwrap(),
        vec![entry]
    );
    assert_eq!(reopened.settings().unwrap().max_hops, 4);
    assert_eq!(reopened.current_cursor().unwrap(), cursor);
    assert_eq!(reopened.epoch().unwrap(), cursor.epoch);
}

#[test]
fn opening_creates_missing_parent_directories() {
    let dir = tempfile::tempdir().expect("temp dir");
    // A first boot owns an empty (or missing) data dir; `open` must not ask
    // the caller to mkdir anything.
    let path = dir.path().join("a/b/c/swarmbot.sqlite");
    let store = SqliteStore::open(&path).expect("open creates parents");
    assert!(path.exists());
    assert_eq!(store.list_bots().unwrap(), vec![]);
}
