//! The store as an async caller should use it: a facade that runs a port call on
//! a blocking thread instead of on the runtime worker that asked for it.
//!
//! ## Why this exists
//!
//! [`Persistence`](swarmbot_contracts::repo::Persistence) is **synchronous on
//! purpose** (its module docs say so): a store call answers before it returns, so
//! there is no half-applied state to reason about and no async trait at a port
//! boundary. The cost is that whichever thread calls it is parked for the whole
//! call — and in a brain that thread is usually a tokio worker, so one call that
//! lands in SQLite's busy wait stalls the bots, the live socket and the node
//! gateway that happen to share that worker.
//!
//! [`ThreadedStore`](crate::ThreadedStore) fixes the *store* side: its own writer
//! thread and its own reader threads, so a contended write no longer serialises
//! reads. It does not fix the **caller** side, and its module docs list the two
//! ways round that, with the second one — "`spawn_blocking` per call at the async
//! boundary … needs every async call site to adopt a facade" — left unwired.
//! This module is that facade.
//!
//! ## What it does and does not promise
//!
//! `read`/`write` hand the closure to `tokio::task::spawn_blocking`, so the task
//! that awaited them keeps running other work while the store is busy. Three
//! things are worth being precise about:
//!
//! * **it is not a second store.** The facade wraps whatever
//!   `Arc<dyn Persistence>` the composition root built, so `ThreadedStore`'s lanes
//!   and `MemoryStore`'s mutex are both reached through it unchanged.
//! * **`spawn_blocking` is a bounded pool.** Enough concurrent calls saturate it,
//!   and a saturated pool queues rather than growing without limit — which is
//!   the intended behaviour, but it means this is *a* fix for a parked worker,
//!   not a promise of unlimited concurrency.
//! * **a panic in the closure is a typed error**, not a lost worker: the join
//!   error is converted into [`DomainError::Internal`] naming the panic, the same
//!   contract `ThreadedStore` keeps at its lane boundary.
//!
//! The closure must be `Send + 'static` (it runs on another thread): clone the id
//! into it rather than borrowing one.

use std::sync::Arc;

use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::repo::Persistence;

/// A [`Persistence`] handle for async callers.
///
/// Cheap to clone: it is one `Arc` plus nothing else.
#[derive(Clone)]
pub struct AsyncStore {
    store: Arc<dyn Persistence>,
}

impl std::fmt::Debug for AsyncStore {
    /// The port has no `Debug` worth printing (an `Arc<dyn Persistence>`), and a
    /// derived one would not compile; a stable placeholder keeps error messages
    /// readable without exposing the inside of a database handle.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AsyncStore(..)")
    }
}

impl AsyncStore {
    pub fn new(store: Arc<dyn Persistence>) -> Self {
        Self { store }
    }

    /// The synchronous handle, for the call sites that legitimately hold a thread
    /// — composition, a CLI command, a `spawn_blocking` body.
    ///
    /// Exposed rather than hidden: an async caller that reaches for it has made
    /// the parking its own problem, and a facade that pretended otherwise would be
    /// lying about what it wraps.
    pub fn blocking(&self) -> &dyn Persistence {
        self.store.as_ref()
    }

    /// The `Arc` itself, for a caller that needs to hand the store somewhere else
    /// (a runner's environment, a gateway session).
    pub fn handle(&self) -> Arc<dyn Persistence> {
        Arc::clone(&self.store)
    }

    /// Run a read on a blocking thread.
    ///
    /// Named `read` because a reader is the common case, but nothing enforces
    /// that: the port has one handle, and a mislabelled write would still be one
    /// write. The name exists so a reviewer can see which call sites expect to
    /// change nothing.
    pub async fn read<R, F>(&self, f: F) -> Result<R, DomainError>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Persistence) -> Result<R, DomainError> + Send + 'static,
    {
        self.run(f).await
    }

    /// Run a write on a blocking thread.
    pub async fn write<R, F>(&self, f: F) -> Result<R, DomainError>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Persistence) -> Result<R, DomainError> + Send + 'static,
    {
        self.run(f).await
    }

    async fn run<R, F>(&self, f: F) -> Result<R, DomainError>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Persistence) -> Result<R, DomainError> + Send + 'static,
    {
        let store = Arc::clone(&self.store);
        // `spawn_blocking` hands the closure to the blocking pool. The join error
        // is a panic (or a cancelled pool), and it becomes a typed error naming
        // what happened — a panic that reached the caller as a `JoinError` would
        // carry no domain meaning at all.
        match tokio::task::spawn_blocking(move || f(store.as_ref())).await {
            Ok(result) => result,
            Err(error) => Err(DomainError::internal(format!(
                "the store call did not finish: {error}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use swarmbot_contracts::repo::BotRepo;
    use swarmbot_contracts::{Bot, BotId, ConversationId};

    fn store() -> Arc<dyn Persistence> {
        let store = crate::MemoryStore::new();
        let bot = Bot::new("lead", "Lead", "you lead", "fake", ConversationId::new());
        store.insert_bot(&bot).expect("a bot");
        Arc::new(store)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_store_call_does_not_park_the_task_that_awaited_it() {
        // The whole point: while a slow store call is in flight, another task on
        // the same runtime keeps running. With a bare synchronous call this test
        // would deadlock — the timer task could never be polled.
        let facade = AsyncStore::new(store());
        let ran_while_waiting = Arc::new(AtomicBool::new(false));

        let ticker = {
            let flag = Arc::clone(&ran_while_waiting);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                flag.store(true, Ordering::SeqCst);
            })
        };
        let bots = facade
            .read(|store| {
                // A store call that takes real time, standing in for SQLite's
                // busy wait.
                std::thread::sleep(Duration::from_millis(80));
                store.list_bots()
            })
            .await
            .expect("the read answers");
        assert_eq!(bots.len(), 1, "the call did its work");
        ticker.await.expect("the ticker task finishes");
        assert!(
            ran_while_waiting.load(Ordering::SeqCst),
            "another task made progress while the store call was in flight"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_domain_error_crosses_the_boundary_unchanged() {
        let facade = AsyncStore::new(store());
        let missing = BotId::new();
        let error = facade
            .read(move |store| store.get_bot(&missing))
            .await
            .expect("a lookup is a Result");
        assert!(error.is_none(), "an unknown bot is None, not an error");

        // A real error is passed through as itself, not reformatted: a caller
        // matches on `DomainError::Conflict` and that has to survive the hop.
        let error = facade
            .write(|store| {
                let mut bot = Bot::new("x", "X", "you x", "fake", ConversationId::new());
                bot.name = "lead".into();
                // A second bot with the taken handle is a conflict in both stores.
                store.insert_bot(&bot)
            })
            .await
            .expect_err("a duplicate handle is a conflict");
        assert!(
            matches!(error, DomainError::Conflict { .. }),
            "the error came back as itself: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_panicking_store_call_becomes_a_typed_error() {
        // A panic must not surface as a bare `JoinError`: the caller's error type
        // is `DomainError`, and "the task panicked" has to arrive in that shape or
        // the API layer has nothing to map it to.
        let facade = AsyncStore::new(store());
        let error = facade
            .read(|_store| -> Result<(), DomainError> { panic!("the store exploded") })
            .await
            .expect_err("a panic is an error");
        match error {
            DomainError::Internal { message } => {
                assert!(
                    message.contains("did not finish"),
                    "the message says what happened: {message}"
                );
            }
            other => panic!("expected a typed internal error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_facade_hands_back_the_same_store_it_wraps() {
        let inner = store();
        let facade = AsyncStore::new(Arc::clone(&inner));
        // A write through the facade is visible through the original handle: the
        // facade is a boundary, not a copy.
        let bot = Bot::new("scout", "Scout", "you scout", "fake", ConversationId::new());
        facade
            .write(move |store| store.insert_bot(&bot))
            .await
            .expect("the write lands");
        assert_eq!(inner.list_bots().expect("bots").len(), 2);
        assert_eq!(
            facade.blocking().list_bots().expect("bots").len(),
            2,
            "the blocking handle sees the same rows"
        );
    }
}
