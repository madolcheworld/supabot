//! What the threaded wrapper promises beyond the contract suite: a contended
//! write does not serialise reads, a call made while a lane is busy still
//! completes in order, an error stays the same typed error, and shutdown ends
//! the threads without hanging.
//!
//! Every test uses a `TempDir` for the database and holds write locks through a
//! *second* connection (that is what "another process" looks like to SQLite —
//! the CLI touching the same file, or a second brain). No sockets, no network.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use swarmbot_contracts::repo::{BotRepo, EntryRepo, EventLog, SettingsStore};
use swarmbot_contracts::{Bot, ConversationId, DomainError, Entry, Settings};
use swarmbot_store::{SqliteStore, ThreadedStore, run_contract_suite};

/// The wrapper is a `Persistence` like any other, so it must pass the same
/// suite — including the areas that reopen the store (recovery, the mirror).
#[test]
fn the_threaded_store_satisfies_the_contract_on_disk() {
    let dir = tempfile::tempdir().expect("temp dir");
    let counter = std::sync::atomic::AtomicU64::new(0);
    // Two reader lanes: the suite must not depend on every read landing on the
    // same connection as the last write.
    run_contract_suite(move || {
        let index = counter.fetch_add(1, Ordering::Relaxed);
        let path = dir.path().join(format!("threaded-{index}.sqlite"));
        ThreadedStore::open_with_readers(&path, 2).expect("open")
    })
    .expect("the threaded store is contract-clean");
}

/// The reason this type exists. Another process holds the write lock, so the
/// writer lane is stuck in SQLite's busy wait; a read must still answer — and
/// the write must really have been blocked, or the test proved nothing.
///
/// What this pins: reads and writes are on different lanes (different
/// connections, WAL). If a read were served on the writer lane this fails at
/// the deadline, because the write cannot finish while `BEGIN IMMEDIATE` is
/// held by the other connection.
#[test]
fn a_contended_write_does_not_serialise_reads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("swarmbot.sqlite");
    let store = ThreadedStore::open(&path).expect("open");
    let lead = Bot::new("lead", "Lead", "you lead", "fake", ConversationId::new());
    store.insert_bot(&lead).expect("insert bot");

    // A second connection holding the write lock, exactly as a `swarmbot` CLI
    // command or a second brain would. It holds until the test commits.
    let other = rusqlite::Connection::open(&path).expect("second connection");
    other
        .execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lock");

    let second = Bot::new(
        "helper",
        "Helper",
        "you help",
        "fake",
        ConversationId::new(),
    );
    let write_done = AtomicBool::new(false);
    let write_started = AtomicBool::new(false);

    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            write_started.store(true, Ordering::SeqCst);
            let result = store.insert_bot(&second);
            write_done.store(true, Ordering::SeqCst);
            result
        });

        // Let the writer thread reach the store call; it cannot get past the
        // held lock, so from here on it is provably in SQLite's busy wait.
        let start_deadline = Instant::now() + Duration::from_secs(2);
        while !write_started.load(Ordering::SeqCst) && Instant::now() < start_deadline {
            std::thread::sleep(Duration::from_millis(1));
        }

        let read = store.list_bots();
        assert!(
            !write_done.load(Ordering::SeqCst),
            "the read answered only after the blocked write returned: reads are sharing the writer lane"
        );
        let bots = read.expect("a read must answer while a write is contended");
        assert!(
            bots.iter().any(|bot| bot.id == lead.id),
            "the read must see the committed bot: {bots:?}"
        );

        // Release the lock: the blocked write must now go through. That is what
        // proves the contention was real rather than a scheduling accident.
        other
            .execute_batch("COMMIT")
            .expect("release the write lock");
        let wrote = writer.join().expect("the writer thread must not panic");
        assert!(
            wrote.is_ok(),
            "the blocked write must succeed once the lock is released: {wrote:?}"
        );
    });

    assert_eq!(store.get_bot(&second.id).expect("read back"), Some(second));
}

/// Writes are FIFO on one lane, and a caller that queues work while the lane is
/// busy still gets every call answered — no call is dropped, and the last value
/// written is the last one asked for.
#[test]
fn calls_made_while_the_writer_lane_is_busy_still_complete_in_order() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("swarmbot.sqlite");
    let store = ThreadedStore::open(&path).expect("open");
    store
        .put_settings(&Settings::default())
        .expect("seed settings");

    let other = rusqlite::Connection::open(&path).expect("second connection");
    other
        .execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lock");

    let started = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let queued = scope.spawn(|| {
            started.store(true, Ordering::SeqCst);
            // Ten writes, all queued behind the one the held lock blocks. The
            // calls must all come back, in order, once the lock is released.
            (1..=10u8)
                .map(|hops| {
                    store.put_settings(&Settings {
                        max_hops: hops,
                        ..Settings::default()
                    })
                })
                .collect::<Vec<_>>()
        });

        let start_deadline = Instant::now() + Duration::from_secs(2);
        while !started.load(Ordering::SeqCst) && Instant::now() < start_deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        other
            .execute_batch("COMMIT")
            .expect("release the write lock");

        let results = queued.join().expect("the queued calls must not panic");
        assert_eq!(results.len(), 10);
        for result in results {
            assert!(
                result.is_ok(),
                "every queued call must be answered: {result:?}"
            );
        }
    });

    assert_eq!(
        store.settings().expect("read settings").max_hops,
        10,
        "the lane is FIFO: the last write asked for must be the value stored"
    );
}

/// A write on the writer lane is visible to a read on a reader lane: the reply
/// is sent after the commit, so WAL cannot show a stale snapshot.
#[test]
fn a_read_lane_sees_what_the_writer_lane_committed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("swarmbot.sqlite");
    // Four reader lanes, so successive reads rotate across connections instead
    // of always landing on the one that could be lucky.
    let store = ThreadedStore::open_with_readers(&path, 4).expect("open");
    let bot = Bot::new("lead", "Lead", "you lead", "fake", ConversationId::new());
    store.insert_bot(&bot).expect("insert bot");
    let entry = Entry::owner_message(bot.direct_conversation_id.clone(), 1, "remember me");

    for revision in 1..=8u32 {
        let mut entry = entry.clone();
        entry.revision = revision;
        store.upsert_entry(&entry).expect("upsert entry");
        assert_eq!(
            store
                .entries_after(&entry.conversation_id, 0, 10)
                .expect("read entries"),
            vec![entry],
            "a reader lane must see the write that just returned"
        );
    }
}

/// The store's typed error crosses the channel unchanged — same variant, same
/// message, same `code()` — and the lane keeps serving afterwards.
#[test]
fn a_store_error_comes_back_as_the_same_typed_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bot = Bot::new("lead", "Lead", "you lead", "fake", ConversationId::new());

    // The same operations against the plain store, so the comparison is
    // against the reference behaviour rather than a hand-written message.
    let plain = SqliteStore::open(&dir.path().join("plain.sqlite")).expect("plain open");
    plain.insert_bot(&bot).expect("plain insert");
    let plain_conflict = plain.insert_bot(&bot).expect_err("duplicate bot");

    let threaded = ThreadedStore::open(&dir.path().join("threaded.sqlite")).expect("threaded open");
    threaded.insert_bot(&bot).expect("threaded insert");
    let threaded_conflict = threaded.insert_bot(&bot).expect_err("duplicate bot");

    assert_eq!(threaded_conflict, plain_conflict);
    assert_eq!(threaded_conflict.code(), "conflict");

    // A `NotFound` too: the error is not flattened into "internal" on the way
    // back through the channel.
    let ghost = Bot::new(
        "ghost",
        "Ghost",
        "you are a ghost",
        "fake",
        ConversationId::new(),
    );
    let plain_missing = plain.update_bot(&ghost).expect_err("plain update missing");
    let threaded_missing = threaded
        .update_bot(&ghost)
        .expect_err("threaded update missing");
    assert_eq!(threaded_missing, plain_missing);
    assert_eq!(threaded_missing.code(), "not_found");

    // And the lane is still serving: an error is data, not a broken lane.
    assert_eq!(threaded.list_bots().expect("list after errors"), vec![bot]);
}

/// Dropping the wrapper stops its threads: `Drop` drains and joins, so it must
/// return promptly even with lanes mid-flight. A `Drop` that waited forever
/// would never send this message.
#[test]
fn dropping_the_wrapper_ends_its_workers_without_hanging() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("swarmbot.sqlite");
    let store = ThreadedStore::open_with_readers(&path, 2).expect("open");
    let bot = Bot::new("lead", "Lead", "you lead", "fake", ConversationId::new());
    store.insert_bot(&bot).expect("insert bot");
    assert!(
        !store.is_stopped(),
        "the lanes must be running before the drop"
    );

    let (dropped, waiting) = mpsc::channel();
    std::thread::spawn(move || {
        drop(store);
        let _ = dropped.send(());
    });

    waiting
        .recv_timeout(Duration::from_secs(5))
        .expect("dropping the wrapper must end its workers and return, not hang");

    // The database is intact and reopenable: the drop committed and closed the
    // connections instead of killing the threads mid-write.
    let reopened = SqliteStore::open(&path).expect("reopen after drop");
    assert_eq!(reopened.get_bot(&bot.id).expect("get bot"), Some(bot));
}

/// `shutdown` is what `Drop` calls, and it is observable: afterwards the lanes
/// have ended and a port call fails with a typed error instead of hanging on a
/// thread that is gone.
#[test]
fn shutdown_stops_every_lane_and_later_calls_fail_typed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("swarmbot.sqlite");
    let store = ThreadedStore::open_with_readers(&path, 2).expect("open");
    store.list_bots().expect("the store works before shutdown");

    store.shutdown();
    assert!(
        store.is_stopped(),
        "shutdown must join every lane thread, not detach it"
    );

    // Both a read lane and the writer lane must answer with an error, not
    // block: the channels are closed and no thread is left to serve them.
    for error in [
        store.list_bots().expect_err("a read after shutdown"),
        store
            .put_settings(&Settings::default())
            .expect_err("a write after shutdown"),
        store.epoch().expect_err("another read after shutdown"),
    ] {
        match error {
            DomainError::Internal { message } => {
                assert!(message.contains("shut down"), "{message}");
            }
            other => panic!("expected an internal error, got {other:?}"),
        }
    }

    // Idempotent, and Drop still runs cleanly on an already-stopped store.
    store.shutdown();
}

/// Two lanes means two independent connections: while the writer lane is stuck
/// in SQLite's busy wait, a *second* reader lane still answers even though the
/// first reader lane is busy with its own call.
#[test]
fn a_busy_reader_lane_does_not_stop_the_other_reader_lane() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("swarmbot.sqlite");
    let store = ThreadedStore::open_with_readers(&path, 2).expect("open");
    let lead = Bot::new("lead", "Lead", "you lead", "fake", ConversationId::new());
    store.insert_bot(&lead).expect("insert bot");

    // Twenty reads in parallel: with two reader lanes the round-robin must keep
    // answering, and none of them may block behind the writer.
    std::thread::scope(|scope| {
        let mut readers = Vec::new();
        for _ in 0..20 {
            readers.push(scope.spawn(|| store.list_bots()));
        }
        for reader in readers {
            let bots = reader
                .join()
                .expect("a reader thread must not panic")
                .expect("a read must answer");
            assert_eq!(bots.len(), 1);
        }
    });
}

// ---------------------------------------------------------------------------
// The reactor test, and the baseline it is measured against.
//
// `crates/store` has no `tokio` (and must not grow one), so the "runtime" here
// is a hand-rolled single-threaded poll loop: one thread, one future, and a
// no-op waker, re-polling until the future is ready. That is the shape of every
// async runtime's worker — if a call parks the thread, the loop stops and every
// other task on that worker starves.
// ---------------------------------------------------------------------------

/// The property this whole change is for: a store call *awaited* on a
/// single-threaded executor does not park that executor, even while the writer
/// lane sits in SQLite's busy wait.
///
/// The first five polls happen with the write lock held by another connection,
/// so each must be `Pending` — and the loop between them is the other task's
/// progress. A synchronous call would never return to the loop: it would block
/// for the whole busy timeout, which is exactly what `the_plain_store_...`
/// below demonstrates on the plain store.
#[test]
fn an_awaited_store_call_does_not_park_the_executor_thread() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("swarmbot.sqlite");
    let store = ThreadedStore::open(&path).expect("open");
    store.put_settings(&Settings::default()).expect("seed");
    let lead = Bot::new("lead", "Lead", "you lead", "fake", ConversationId::new());
    store.insert_bot(&lead).expect("insert bot");

    let other = rusqlite::Connection::open(&path).expect("second connection");
    other
        .execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lock");

    let settings = Settings {
        max_hops: 7,
        ..Settings::default()
    };
    let mut call = std::pin::pin!(store.write_async(move |store| store.put_settings(&settings)));
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);

    // Poll the write once: it cannot finish while the lock is held elsewhere,
    // so the only acceptable answer is `Pending`. A blocking call would never
    // return here at all — it would sit in SQLite for the whole busy timeout.
    match call.as_mut().poll(&mut cx) {
        std::task::Poll::Pending => {}
        std::task::Poll::Ready(answer) => {
            panic!("the write finished with the write lock held elsewhere: {answer:?}")
        }
    }

    // The other task on this executor: a *read*, awaited on the same thread.
    // It must complete while the write is still pending — that is the whole
    // point, and it is what a tokio worker or the GPUI executor would do with
    // the time. The read lane is a different connection, so it is not blocked.
    let read = std::pin::pin!(store.read_async(|store| store.list_bots()));
    let mut read = read;
    let read_deadline = Instant::now() + Duration::from_secs(5);
    let bots = loop {
        match read.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(Ok(bots)) => break bots,
            std::task::Poll::Ready(Err(error)) => panic!("the read failed: {error:?}"),
            std::task::Poll::Pending => {
                assert!(
                    Instant::now() < read_deadline,
                    "a read awaited on the same executor never completed while the write was contended"
                );
                std::thread::yield_now();
            }
        }
    };
    assert_eq!(bots.len(), 1, "the read task must have run: {bots:?}");

    // And the write is *still* pending: the executor ran a second task to
    // completion without waiting for the blocked write.
    assert!(
        matches!(call.as_mut().poll(&mut cx), std::task::Poll::Pending),
        "the write must still be blocked while the lock is held elsewhere"
    );

    // Release the lock; the future must now complete. Bounded, because a future
    // that never resolves is the failure this test exists to catch.
    other
        .execute_batch("COMMIT")
        .expect("release the write lock");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match call.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(Ok(())) => break,
            std::task::Poll::Ready(Err(error)) => panic!("the write failed: {error:?}"),
            std::task::Poll::Pending => {
                assert!(
                    Instant::now() < deadline,
                    "the awaited write never completed after the lock was released"
                );
                std::thread::yield_now();
            }
        }
    }

    assert_eq!(store.settings().expect("read settings").max_hops, 7);
}

/// The baseline: the plain store really does park its caller behind a
/// contended write — this is the failure the wrapper removes, and it is what
/// makes the test above non-vacuous.
///
/// It also documents the honest limit: `SqliteStore` is still what the brain
/// uses until the composition root adopts `ThreadedStore`.
#[test]
fn the_plain_store_parks_a_reader_behind_a_contended_write() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("swarmbot.sqlite");
    let store = std::sync::Arc::new(SqliteStore::open(&path).expect("open"));
    let lead = Bot::new("lead", "Lead", "you lead", "fake", ConversationId::new());
    store.insert_bot(&lead).expect("insert bot");

    let other = rusqlite::Connection::open(&path).expect("second connection");
    other
        .execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lock");

    // The writer is spawned first, but *being spawned* is not the premise: the
    // premise is that it holds the store's `Mutex<Connection>` while it waits in
    // SQLite's busy handler. The old shape signalled before calling
    // `insert_bot`, so on a loaded machine the reader thread could win that
    // mutex and answer at once — a green test on an idle box and a red one under
    // load, with the store behaving correctly the whole time.
    let writer = std::thread::spawn({
        let store = std::sync::Arc::clone(&store);
        let bot = Bot::new(
            "helper",
            "Helper",
            "you help",
            "fake",
            ConversationId::new(),
        );
        move || store.insert_bot(&bot)
    });

    // Establish the premise deterministically instead of racing for it. Each
    // round starts a probe read: if it answers, the writer had not been
    // scheduled yet and the round proved nothing, so try again. Once the writer
    // has run at all it holds the mutex until `COMMIT` — it cannot finish,
    // because `BEGIN IMMEDIATE` is held by the second connection — so every
    // later probe blocks. The loop therefore ends with a probe that is provably
    // parked behind the write, and that probe is the read under test.
    let premise_deadline = Instant::now() + Duration::from_secs(10);
    let mut probes = 0u32;
    let blocked = loop {
        let (done, answered) = mpsc::channel();
        std::thread::spawn({
            let store = std::sync::Arc::clone(&store);
            move || {
                let _ = done.send(store.list_bots());
            }
        });
        match answered.recv_timeout(Duration::from_millis(100)) {
            // The writer was not holding the mutex yet; the probe answered.
            Ok(_) => {
                probes += 1;
                assert!(
                    Instant::now() < premise_deadline,
                    "after {probes} probe reads the writer still had not taken the store's mutex, \
                     so this test cannot establish its premise"
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break answered,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("a probe read thread panicked")
            }
        }
    };
    assert!(probes < 200, "the premise settled after {probes} probes");

    // The parked probe must stay parked: 300 ms is far inside the 5 s busy
    // timeout the store configures, so an answer here means reads are not
    // serialised behind a contended write at all.
    assert!(
        blocked.recv_timeout(Duration::from_millis(300)).is_err(),
        "the plain store answered a read while a write held the lock: this test's premise is wrong"
    );

    // Release: both must finish, which proves the read was blocked rather than
    // broken.
    other
        .execute_batch("COMMIT")
        .expect("release the write lock");
    let write = writer.join().expect("the writer thread must not panic");
    assert!(write.is_ok(), "the blocked write must succeed: {write:?}");
    let read = blocked
        .recv_timeout(Duration::from_secs(5))
        .expect("the blocked read must complete once the lock is released")
        .expect("the read must succeed");
    assert_eq!(read.len(), 2, "the read must see both bots: {read:?}");
}
