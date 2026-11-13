//! The same port, served from dedicated threads: one writer lane, N WAL
//! reader lanes, and a channel instead of a mutex.
//!
//! ## Why this exists
//!
//! [`SqliteStore`](crate::SqliteStore) is a `Mutex<Connection>` with
//! `busy_timeout = 5000`. Every caller takes that one lock, so one caller that
//! lands in SQLite's busy wait (another process — a `swarmbot` CLI command, a
//! second brain — holding the write lock) parks **every** other caller behind
//! it, readers included, for up to five seconds. When those callers are tokio
//! workers that also means unrelated bots, live-socket subscribers and the node
//! gateway stall with them.
//!
//! [`ThreadedStore`] implements the same [`Persistence`] port, so no caller
//! changes, but the SQLite work happens on threads this type owns:
//!
//! * **one writer lane** — every mutating call goes to the same OS thread, in
//!   FIFO order, which is exactly the serialisation the port already implies;
//! * **N reader lanes** — each a second `SqliteStore` on the same file. WAL is
//!   what makes this safe: a reader sees the last committed snapshot and never
//!   waits for the writer, so a contended write no longer serialises reads.
//! * **a panic becomes a typed error.** A panicking store call used to poison
//!   the mutex, after which every later call failed with "lock poisoned". Here
//!   the panic is caught at the job boundary: that caller gets
//!   [`DomainError::Internal`] naming the panic, the lane stops rather than
//!   serving more work on a connection that may be mid-transaction, and later
//!   calls on that lane return a typed error instead of hanging.
//!
//! ## The honest limit, and the two ways round it
//!
//! The port is **synchronous** (`crates/contracts/src/repo.rs` says so on
//! purpose), so a port call still blocks its caller for as long as the call
//! takes. This wrapper moves the *SQLite* work onto its own threads; it cannot
//! make `fn get_bot(&self, …)` return before it has an answer. A tokio worker
//! that calls the port inline is therefore still parked — but on a channel
//! bounded by its own lane instead of a process-wide mutex, and a *different*
//! lane keeps making progress (that is what
//! `tests/threaded.rs::a_contended_write_does_not_serialise_reads` pins).
//!
//! Two ways to get the worker itself back:
//!
//! 1. [`ThreadedStore::read_async`] / [`ThreadedStore::write_async`] — a future
//!    that is `Pending` until the lane answers, so an executor can run other
//!    work meanwhile. This is the only form available without a new dependency:
//!    the crate has no `tokio` and must not grow one. It needs a *concrete*
//!    `ThreadedStore` handle, not `Arc<dyn Persistence>`, because the port has
//!    no async method — adopting it means holding both.
//! 2. `spawn_blocking` per call at the async boundary (a `tokio` dependency in
//!    this crate, i.e. a workspace-owner decision), which works through
//!    `Arc<dyn Persistence>` but needs every async call site to adopt a facade.
//!
//! Neither is wired into the composition root yet: it still builds
//! `Arc<dyn Persistence>` from `SqliteStore` (`crates/brain/src/compose.rs`,
//! `crates/core/src/compose.rs`). Swapping that one line to
//! [`ThreadedStore::open`] is the part that fixes the process-wide stall, and
//! it changes no caller.
//!
//! ## Invariants this type keeps
//!
//! 1. **Commit before reply.** A write's reply is sent only after the store
//!    call returned, so a caller that reads back through a *different* lane
//!    connection (WAL) still sees what it just wrote.
//! 2. **Lane FIFO.** One channel per lane, one thread per channel: calls on a
//!    lane are answered in the order they were sent.
//! 3. **The enqueue lock is never held across a store call.** It guards the
//!    sender list and nothing else; the send is on an unbounded channel, so it
//!    does not wait either.
//! 4. **Shutdown drains.** Dropping the senders is what ends a lane: `recv`
//!    only reports disconnect once the queue is empty, so a call already
//!    accepted is still answered. [`ThreadedStore::shutdown`] then joins every
//!    thread, so `Drop` cannot leave a detached thread behind.
//! 5. **No hangs from a dead lane.** If a lane ends without answering (a
//!    panic, or a shutdown racing a call), the caller's `recv` fails and it
//!    gets [`DomainError::Internal`] — never a blocked thread forever.

use std::any::Any;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;

use chrono::{DateTime, Utc};

use swarmbot_contracts::Seq;
use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::config::Settings;
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::delivery::Delivery;
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{
    BotId, ConversationId, DeliveryId, EntryId, NodeId, RequestId, RoutineId, RuleId, RunId,
    TaskId, ToolCallId, TurnId,
};
use swarmbot_contracts::live::{LiveCursor, LiveEvent};
use swarmbot_contracts::node::{NodeInfo, PairingRecord};
use swarmbot_contracts::repo::{
    BotRepo, BotSessionRecord, ConversationRepo, DeliveryRepo, EntryRepo, EventLog, NodeRepo,
    Persistence, RequestRepo, RoutineRepo, RuleRepo, SessionRepo, SettingsStore, TaskRepo,
    ToolCallRecord, ToolCallRepo, TurnRepo,
};
use swarmbot_contracts::request::{ApprovalRule, RuntimeRequest};
use swarmbot_contracts::routine::{Routine, RoutineRun};
use swarmbot_contracts::task::{Task, TaskStep};
use swarmbot_contracts::turn::Turn;

use crate::sqlite::SqliteStore;

/// Thread names, so a stuck store shows up as a named thread in a backtrace
/// instead of an anonymous one. `comm` truncates to 15 bytes, which is exactly
/// the length of this prefix.
const THREAD_PREFIX: &str = "swarmbot-store-";

/// What one job hands back over the reply channel: the boxed return value, or
/// the store's own typed error.
///
/// `Any` is needed because one channel serves every port method. Only this
/// module boxes and unboxes, and an unbox mismatch is reported as
/// [`DomainError::Internal`] rather than panicking, so a bug here can never
/// take a caller's task down.
type Reply = Result<Box<dyn Any + Send>, DomainError>;

/// How a lane reacts to one finished job. A panic stops the lane; a domain
/// error does not, because an error is data the store produced, not a broken
/// connection.
enum Outcome {
    Served,
    Panicked(String),
}

type Job = Box<dyn FnOnce(&dyn Persistence) -> Outcome + Send>;

/// Which lane a call goes to. Reads never wait for a write: that is the whole
/// point of the type.
#[derive(Clone, Copy)]
enum Lane {
    Write,
    Read,
}

/// The two halves of a running wrapper. Kept in one `Option` so `shutdown`
/// can drop every sender atomically with respect to `dispatch`.
struct Lanes {
    writer: Sender<Job>,
    readers: Vec<Sender<Job>>,
}

/// A [`Persistence`] store that lives on its own threads.
///
/// Construct one with [`ThreadedStore::open`] and hand it to the composition
/// root as `Arc<dyn Persistence>` exactly as a `SqliteStore` would be. File
/// databases only: an in-memory SQLite database is private to its connection,
/// so a reader lane could not see the writer's rows (the honest fallback for
/// tests is [`MemoryStore`](crate::MemoryStore), which has no threads at all).
pub struct ThreadedStore {
    /// `None` after shutdown. Guarded because `std::sync::mpsc::Sender` is
    /// `Send` but not `Sync`, while every port method takes `&self`.
    lanes: Mutex<Option<Lanes>>,
    /// Joined by `shutdown`, so dropping the wrapper cannot leak a thread.
    workers: Mutex<Option<Vec<JoinHandle<()>>>>,
    /// Threads still inside their lane loop. Test-facing observability: it is
    /// the only way to assert that a drop really ended them.
    alive: Arc<AtomicUsize>,
    /// Round-robin over the reader lanes.
    next_reader: AtomicUsize,
}

impl ThreadedStore {
    /// A wrapper with one writer lane and one reader lane.
    pub fn open(path: &Path) -> Result<Self, DomainError> {
        Self::open_with_readers(path, 1)
    }

    /// A wrapper with `readers` reader lanes (clamped to at least one).
    ///
    /// More than one reader only helps if concurrent reads are expected: reads
    /// are microseconds, so one lane is normally enough and each extra lane is
    /// one more SQLite connection on the same file.
    pub fn open_with_readers(path: &Path, readers: usize) -> Result<Self, DomainError> {
        let readers = readers.max(1);
        // The writer connection opens first: `SqliteStore::open` runs the
        // migrations, so the reader lanes open an already-migrated database
        // instead of racing the migration on three connections.
        let writer_store = SqliteStore::open(path)?;
        let mut reader_stores = Vec::with_capacity(readers);
        for _ in 0..readers {
            reader_stores.push(SqliteStore::open(path)?);
        }

        let alive = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(readers + 1);

        let (writer, writer_jobs) = mpsc::channel::<Job>();
        handles.push(spawn_lane(
            format!("{THREAD_PREFIX}writer"),
            Box::new(writer_store),
            writer_jobs,
            Arc::clone(&alive),
        )?);

        let mut reader_senders = Vec::with_capacity(readers);
        for (index, store) in reader_stores.into_iter().enumerate() {
            let (sender, jobs) = mpsc::channel::<Job>();
            handles.push(spawn_lane(
                format!("{THREAD_PREFIX}reader-{index}"),
                Box::new(store),
                jobs,
                Arc::clone(&alive),
            )?);
            reader_senders.push(sender);
        }

        Ok(Self {
            lanes: Mutex::new(Some(Lanes {
                writer,
                readers: reader_senders,
            })),
            workers: Mutex::new(Some(handles)),
            alive,
            next_reader: AtomicUsize::new(0),
        })
    }

    /// Stop every lane and wait for its thread to end. Idempotent, and also
    /// called by `Drop`.
    ///
    /// Dropping the senders is what ends a lane (see invariant 4); the join is
    /// what makes "the worker thread is gone" a fact rather than a hope.
    ///
    /// A call already accepted is allowed to finish rather than being cut off,
    /// so `Drop` waits for at most one in-flight call per lane — bounded by
    /// that call, which for SQLite is at worst its `busy_timeout`. Callers that
    /// must not wait should stop issuing calls first; this is not a way to
    /// abort a running statement (SQLite has no cancel from another thread).
    pub fn shutdown(&self) {
        let lanes = self
            .lanes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(lanes);
        let workers = self
            .workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(workers) = workers {
            for worker in workers {
                // A join error means the thread itself panicked, which the job
                // boundary already turned into a typed error for its caller.
                // It is logged because it is still a bug, and it must not take
                // the dropping thread down with it.
                if worker.join().is_err() {
                    tracing::error!("a store worker thread ended by panicking");
                }
            }
        }
    }

    /// Whether every lane has ended. Used by the shutdown test; a caller that
    /// gets `true` here will get [`DomainError::Internal`] from any port call.
    pub fn is_stopped(&self) -> bool {
        self.alive.load(Ordering::SeqCst) == 0
    }

    /// A call that must run on the writer lane.
    fn write<R, F>(&self, f: F) -> Result<R, DomainError>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Persistence) -> Result<R, DomainError> + Send + 'static,
    {
        self.enqueue(Lane::Write, f)
    }

    /// A call that may run on any reader lane.
    fn read<R, F>(&self, f: F) -> Result<R, DomainError>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Persistence) -> Result<R, DomainError> + Send + 'static,
    {
        self.enqueue(Lane::Read, f)
    }

    /// Run a mutating call on the writer lane **without blocking the caller's
    /// thread**: the returned future is ready when the lane has answered.
    ///
    /// This is the piece that takes an async executor off the hook. The port is
    /// synchronous, so [`ThreadedStore::write`] (and every port method that
    /// forwards to it) still parks whoever calls it; this entry point does not.
    /// A tokio worker, the GPUI executor or any other runtime can `await` it
    /// and keep running other work while the store is busy.
    ///
    /// The closure runs **on the lane thread**, so everything it captures must
    /// be `Send + 'static`: clone the id into it rather than borrowing.
    ///
    /// ```ignore
    /// store.write_async(move |store| store.put_settings(&settings)).await?;
    /// ```
    pub async fn write_async<R, F>(&self, f: F) -> Result<R, DomainError>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Persistence) -> Result<R, DomainError> + Send + 'static,
    {
        self.enqueue_async(Lane::Write, f)?.await
    }

    /// The read-side twin of [`ThreadedStore::write_async`]:
    ///
    /// ```ignore
    /// let bot = store.read_async(move |store| store.get_bot(&id)).await?;
    /// ```
    pub async fn read_async<R, F>(&self, f: F) -> Result<R, DomainError>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Persistence) -> Result<R, DomainError> + Send + 'static,
    {
        self.enqueue_async(Lane::Read, f)?.await
    }

    /// The one round-trip every port method uses.
    fn enqueue<R, F>(&self, lane: Lane, f: F) -> Result<R, DomainError>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Persistence) -> Result<R, DomainError> + Send + 'static,
    {
        let (reply, answer) = mpsc::channel::<Reply>();
        self.dispatch(lane, job(reply, None, f))?;
        let boxed = answer.recv().map_err(lane_ended)??;
        unbox(boxed)
    }

    /// Dispatch a call and hand back a future for its answer. The job is queued
    /// before this returns, so a busy lane delays only the completion, never
    /// the caller's poll.
    fn enqueue_async<R, F>(&self, lane: Lane, f: F) -> Result<LaneCall<R>, DomainError>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Persistence) -> Result<R, DomainError> + Send + 'static,
    {
        let (reply, answer) = mpsc::channel::<Reply>();
        // The waker slot is filled by the first poll and emptied by the lane
        // when it answers. Poll fills it *before* checking the channel, so a
        // wakeup cannot be lost: either the answer is already there, or the
        // lane will find the waker and wake it.
        let waker: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));
        self.dispatch(lane, job(reply, Some(Arc::clone(&waker)), f))?;
        Ok(LaneCall {
            reply: answer,
            waker,
            _value: std::marker::PhantomData,
        })
    }

    /// Put a job on a lane. The lock is held only for the send.
    fn dispatch(&self, lane: Lane, work: Job) -> Result<(), DomainError> {
        let guard = self
            .lanes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        {
            let lanes = guard.as_ref().ok_or_else(|| {
                DomainError::internal("the store has been shut down; this call was not run")
            })?;
            let sender = match lane {
                Lane::Write => &lanes.writer,
                // `readers` is built non-empty, but a modulo by zero would be
                // a panic, so the writer is the documented fallback: a read on
                // the writer lane is slow, never wrong.
                Lane::Read => lanes
                    .readers
                    .get(
                        self.next_reader.fetch_add(1, Ordering::Relaxed)
                            % lanes.readers.len().max(1),
                    )
                    .unwrap_or(&lanes.writer),
            };
            sender.send(work).map_err(|_| {
                DomainError::internal("the store lane for this call is gone; it was not run")
            })?;
        }
        Ok(())
    }
}

impl Drop for ThreadedStore {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Turn one port call into a job: catch a panic, box the value, answer, and
/// wake the caller if it is waiting on a future rather than on `recv`.
fn job<R, F>(reply: Sender<Reply>, waker: Option<Arc<Mutex<Option<Waker>>>>, f: F) -> Job
where
    R: Send + 'static,
    F: FnOnce(&dyn Persistence) -> Result<R, DomainError> + Send + 'static,
{
    Box::new(
        move |store: &dyn Persistence| match catch_unwind(AssertUnwindSafe(|| f(store))) {
            Ok(Ok(value)) => {
                let _ = reply.send(Ok(Box::new(value) as Box<dyn Any + Send>));
                wake(waker);
                Outcome::Served
            }
            Ok(Err(error)) => {
                // The store's own typed error crosses the channel unchanged: a
                // Conflict stays a Conflict with the same message.
                let _ = reply.send(Err(error));
                wake(waker);
                Outcome::Served
            }
            Err(payload) => {
                let message = panic_text(payload);
                let _ = reply.send(Err(DomainError::internal(message.clone())));
                wake(waker);
                Outcome::Panicked(message)
            }
        },
    )
}

/// Wake the future waiting for this job, if any. Sent *after* the reply, so a
/// woken task that polls again always finds its answer.
///
/// A panicking waker is the *executor's* bug and must not take this lane down:
/// the caller's answer is already in its channel, so the panic is contained and
/// logged, and the lane keeps serving.
fn wake(waker: Option<Arc<Mutex<Option<Waker>>>>) {
    let Some(slot) = waker else { return };
    let taken = slot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(waker) = taken
        && catch_unwind(AssertUnwindSafe(|| waker.wake())).is_err()
    {
        tracing::error!("a waker panicked while being woken for a finished store call");
    }
}

/// A future for one store call, resolved by its lane.
///
/// `Unpin` for every `R`: the type parameter is a function pointer in the
/// marker, so the future can be polled through a `Pin<&mut _>` without
/// `unsafe`, without boxing, and without an `R: Unpin` bound leaking into the
/// public API.
struct LaneCall<R> {
    reply: Receiver<Reply>,
    waker: Arc<Mutex<Option<Waker>>>,
    _value: std::marker::PhantomData<fn() -> R>,
}

impl<R: Send + 'static> Future for LaneCall<R> {
    type Output = Result<R, DomainError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        *this
            .waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(cx.waker().clone());
        match this.reply.try_recv() {
            Ok(answer) => Poll::Ready(answer.and_then(unbox)),
            Err(TryRecvError::Empty) => Poll::Pending,
            Err(TryRecvError::Disconnected) => Poll::Ready(Err(lane_ended(mpsc::RecvError))),
        }
    }
}

/// The lane ended between accepting a call and answering it: a panic inside
/// another call, or a shutdown. Reported, never waited on — a caller must not
/// park forever on a dead lane.
fn lane_ended(_: mpsc::RecvError) -> DomainError {
    DomainError::internal(
        "the store lane ended before answering this call (a store call panicked or the store was shut down)",
    )
}

/// Unbox a lane answer. A type mismatch is a bug in this module, reported as an
/// error rather than a panic, because it must not take a caller's task down.
fn unbox<R: Send + 'static>(boxed: Box<dyn Any + Send>) -> Result<R, DomainError> {
    boxed.downcast::<R>().map(|value| *value).map_err(|_| {
        DomainError::internal(
            "the store answered with a different type than the port method returns",
        )
    })
}

/// One lane: receive a job, run it against this lane's connection, repeat.
///
/// A panicking job ends the lane (its connection may be mid-transaction). The
/// receiver is owned here, so returning drops every job still queued behind
/// it — each one's reply channel disconnects, and its caller gets a typed
/// error instead of waiting for an answer that will never come.
fn run_lane(store: &dyn Persistence, jobs: Receiver<Job>) {
    while let Ok(work) = jobs.recv() {
        if let Outcome::Panicked(message) = work(store) {
            tracing::error!(
                panic = %message,
                "a store call panicked; stopping this lane instead of serving more work on a connection that may be mid-transaction"
            );
            return;
        }
    }
}

/// Decrements the live-lane count when the thread ends — including on an
/// unwind, so `is_stopped` can never report a thread that is not there.
struct AliveGuard(Arc<AtomicUsize>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Spawn one lane thread. `alive` is incremented before the spawn and
/// decremented as the thread's last act, so `is_stopped` cannot report a
/// thread that never started.
fn spawn_lane(
    name: String,
    store: Box<dyn Persistence>,
    jobs: Receiver<Job>,
    alive: Arc<AtomicUsize>,
) -> Result<JoinHandle<()>, DomainError> {
    alive.fetch_add(1, Ordering::SeqCst);
    let lane_alive = AliveGuard(Arc::clone(&alive));
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        let _alive = lane_alive;
        run_lane(store.as_ref(), jobs);
    });
    match spawned {
        Ok(handle) => Ok(handle),
        Err(error) => {
            // The closure (and the store with it) was dropped by the failed
            // spawn, so there is nothing to stop — only the count to undo.
            alive.fetch_sub(1, Ordering::SeqCst);
            Err(DomainError::internal(format!(
                "could not start a store worker thread: {error}"
            )))
        }
    }
}

/// A panic payload is `&str` or `String` in practice; anything else still gets
/// a message, because the caller is owed a reason.
fn panic_text(payload: Box<dyn Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        format!("a store call panicked: {text}")
    } else if let Some(text) = payload.downcast_ref::<String>() {
        format!("a store call panicked: {text}")
    } else {
        "a store call panicked with a payload that is not a string".to_string()
    }
}

// ---------------------------------------------------------------------------
// The port. Every method is a forwarding call; the only judgement each one
// makes is read lane vs writer lane, and the rules are stated per group:
// anything that changes state is a write, everything else is a read.
// ---------------------------------------------------------------------------

impl BotRepo for ThreadedStore {
    fn insert_bot(&self, bot: &Bot) -> Result<(), DomainError> {
        let bot = bot.clone();
        self.write(move |store| store.insert_bot(&bot))
    }

    fn update_bot(&self, bot: &Bot) -> Result<(), DomainError> {
        let bot = bot.clone();
        self.write(move |store| store.update_bot(&bot))
    }

    fn get_bot(&self, id: &BotId) -> Result<Option<Bot>, DomainError> {
        let id = id.clone();
        self.read(move |store| store.get_bot(&id))
    }

    fn bot_by_name(&self, name: &str) -> Result<Option<Bot>, DomainError> {
        let name = name.to_string();
        self.read(move |store| store.bot_by_name(&name))
    }

    fn list_bots(&self) -> Result<Vec<Bot>, DomainError> {
        self.read(|store| store.list_bots())
    }

    fn delete_bot(&self, id: &BotId) -> Result<(), DomainError> {
        let id = id.clone();
        self.write(move |store| store.delete_bot(&id))
    }
}

impl ConversationRepo for ThreadedStore {
    fn insert_conversation(&self, conversation: &Conversation) -> Result<(), DomainError> {
        let conversation = conversation.clone();
        self.write(move |store| store.insert_conversation(&conversation))
    }

    fn update_conversation(&self, conversation: &Conversation) -> Result<(), DomainError> {
        let conversation = conversation.clone();
        self.write(move |store| store.update_conversation(&conversation))
    }

    fn get_conversation(&self, id: &ConversationId) -> Result<Option<Conversation>, DomainError> {
        let id = id.clone();
        self.read(move |store| store.get_conversation(&id))
    }

    fn direct_conversation(&self, bot: &BotId) -> Result<Option<Conversation>, DomainError> {
        let bot = bot.clone();
        self.read(move |store| store.direct_conversation(&bot))
    }

    fn list_conversations(&self) -> Result<Vec<Conversation>, DomainError> {
        self.read(|store| store.list_conversations())
    }

    fn delete_conversation(&self, id: &ConversationId) -> Result<(), DomainError> {
        let id = id.clone();
        self.write(move |store| store.delete_conversation(&id))
    }
}

impl EntryRepo for ThreadedStore {
    fn upsert_entry(&self, entry: &Entry) -> Result<(), DomainError> {
        let entry = entry.clone();
        self.write(move |store| store.upsert_entry(&entry))
    }

    fn get_entry(&self, id: &EntryId) -> Result<Option<Entry>, DomainError> {
        let id = id.clone();
        self.read(move |store| store.get_entry(&id))
    }

    fn entries_after(
        &self,
        conversation: &ConversationId,
        after_seq: Seq,
        limit: u32,
    ) -> Result<Vec<Entry>, DomainError> {
        let conversation = conversation.clone();
        self.read(move |store| store.entries_after(&conversation, after_seq, limit))
    }

    fn entry_tail(
        &self,
        conversation: &ConversationId,
        limit: u32,
    ) -> Result<Vec<Entry>, DomainError> {
        let conversation = conversation.clone();
        self.read(move |store| store.entry_tail(&conversation, limit))
    }

    fn delete_entries(&self, conversation: &ConversationId) -> Result<u64, DomainError> {
        let conversation = conversation.clone();
        self.write(move |store| store.delete_entries(&conversation))
    }
}

impl TurnRepo for ThreadedStore {
    fn insert_turn(&self, turn: &Turn) -> Result<(), DomainError> {
        let turn = turn.clone();
        self.write(move |store| store.insert_turn(&turn))
    }

    fn update_turn(&self, turn: &Turn) -> Result<(), DomainError> {
        let turn = turn.clone();
        self.write(move |store| store.update_turn(&turn))
    }

    fn get_turn(&self, id: &TurnId) -> Result<Option<Turn>, DomainError> {
        let id = id.clone();
        self.read(move |store| store.get_turn(&id))
    }

    fn turns_in(
        &self,
        conversation: &ConversationId,
        limit: u32,
    ) -> Result<Vec<Turn>, DomainError> {
        let conversation = conversation.clone();
        self.read(move |store| store.turns_in(&conversation, limit))
    }

    fn running_turns(&self) -> Result<Vec<Turn>, DomainError> {
        self.read(|store| store.running_turns())
    }

    fn turns_since(&self, since: DateTime<Utc>) -> Result<Vec<Turn>, DomainError> {
        self.read(move |store| store.turns_since(since))
    }
}

impl DeliveryRepo for ThreadedStore {
    fn insert_delivery(&self, delivery: &Delivery) -> Result<(), DomainError> {
        let delivery = delivery.clone();
        self.write(move |store| store.insert_delivery(&delivery))
    }

    fn update_delivery(&self, delivery: &Delivery) -> Result<(), DomainError> {
        let delivery = delivery.clone();
        self.write(move |store| store.update_delivery(&delivery))
    }

    fn get_delivery(&self, id: &DeliveryId) -> Result<Option<Delivery>, DomainError> {
        let id = id.clone();
        self.read(move |store| store.get_delivery(&id))
    }

    fn queued_deliveries(&self, bot: &BotId) -> Result<Vec<Delivery>, DomainError> {
        let bot = bot.clone();
        self.read(move |store| store.queued_deliveries(&bot))
    }

    fn drop_deliveries(
        &self,
        conversation: &ConversationId,
        reason: &str,
    ) -> Result<u64, DomainError> {
        let conversation = conversation.clone();
        let reason = reason.to_string();
        self.write(move |store| store.drop_deliveries(&conversation, &reason))
    }
}

impl TaskRepo for ThreadedStore {
    fn insert_task(&self, task: &Task) -> Result<(), DomainError> {
        let task = task.clone();
        self.write(move |store| store.insert_task(&task))
    }

    fn update_task(&self, task: &Task) -> Result<(), DomainError> {
        let task = task.clone();
        self.write(move |store| store.update_task(&task))
    }

    fn get_task(&self, id: &TaskId) -> Result<Option<Task>, DomainError> {
        let id = id.clone();
        self.read(move |store| store.get_task(&id))
    }

    fn tasks_of(&self, owner: &BotId) -> Result<Vec<Task>, DomainError> {
        let owner = owner.clone();
        self.read(move |store| store.tasks_of(&owner))
    }

    fn tasks_in(&self, origin: &ConversationId) -> Result<Vec<Task>, DomainError> {
        let origin = origin.clone();
        self.read(move |store| store.tasks_in(&origin))
    }

    fn queued_tasks(&self) -> Result<Vec<Task>, DomainError> {
        self.read(|store| store.queued_tasks())
    }

    fn running_tasks(&self) -> Result<Vec<Task>, DomainError> {
        self.read(|store| store.running_tasks())
    }

    fn running_task_count(&self, owner: &BotId) -> Result<u32, DomainError> {
        let owner = owner.clone();
        self.read(move |store| store.running_task_count(&owner))
    }

    fn append_task_step(&self, step: &TaskStep) -> Result<(), DomainError> {
        let step = step.clone();
        self.write(move |store| store.append_task_step(&step))
    }

    fn task_steps(&self, task: &TaskId) -> Result<Vec<TaskStep>, DomainError> {
        let task = task.clone();
        self.read(move |store| store.task_steps(&task))
    }
}

impl RequestRepo for ThreadedStore {
    fn insert_request(&self, request: &RuntimeRequest) -> Result<(), DomainError> {
        let request = request.clone();
        self.write(move |store| store.insert_request(&request))
    }

    fn update_request(&self, request: &RuntimeRequest) -> Result<(), DomainError> {
        let request = request.clone();
        self.write(move |store| store.update_request(&request))
    }

    fn get_request(&self, id: &RequestId) -> Result<Option<RuntimeRequest>, DomainError> {
        let id = id.clone();
        self.read(move |store| store.get_request(&id))
    }

    fn pending_requests(&self) -> Result<Vec<RuntimeRequest>, DomainError> {
        self.read(|store| store.pending_requests())
    }

    fn requests_in(
        &self,
        conversation: &ConversationId,
    ) -> Result<Vec<RuntimeRequest>, DomainError> {
        let conversation = conversation.clone();
        self.read(move |store| store.requests_in(&conversation))
    }

    fn cancel_requests_of_turn(&self, turn: &TurnId, reason: &str) -> Result<u64, DomainError> {
        let turn = turn.clone();
        let reason = reason.to_string();
        self.write(move |store| store.cancel_requests_of_turn(&turn, &reason))
    }
}

impl RuleRepo for ThreadedStore {
    fn insert_rule(&self, rule: &ApprovalRule) -> Result<(), DomainError> {
        let rule = rule.clone();
        self.write(move |store| store.insert_rule(&rule))
    }

    fn list_rules(&self) -> Result<Vec<ApprovalRule>, DomainError> {
        self.read(|store| store.list_rules())
    }

    fn delete_rule(&self, id: &RuleId) -> Result<(), DomainError> {
        let id = id.clone();
        self.write(move |store| store.delete_rule(&id))
    }
}

impl NodeRepo for ThreadedStore {
    fn insert_node(&self, node: &NodeInfo) -> Result<(), DomainError> {
        let node = node.clone();
        self.write(move |store| store.insert_node(&node))
    }

    fn update_node(&self, node: &NodeInfo) -> Result<(), DomainError> {
        let node = node.clone();
        self.write(move |store| store.update_node(&node))
    }

    fn get_node(&self, id: &NodeId) -> Result<Option<NodeInfo>, DomainError> {
        let id = id.clone();
        self.read(move |store| store.get_node(&id))
    }

    fn list_nodes(&self) -> Result<Vec<NodeInfo>, DomainError> {
        self.read(|store| store.list_nodes())
    }

    fn insert_pairing(&self, record: &PairingRecord) -> Result<(), DomainError> {
        let record = record.clone();
        self.write(move |store| store.insert_pairing(&record))
    }

    fn get_pairing(&self, code_hash: &str) -> Result<Option<PairingRecord>, DomainError> {
        let code_hash = code_hash.to_string();
        self.read(move |store| store.get_pairing(&code_hash))
    }

    fn update_pairing(&self, record: &PairingRecord) -> Result<(), DomainError> {
        let record = record.clone();
        self.write(move |store| store.update_pairing(&record))
    }

    fn expired_pairings(&self, now: DateTime<Utc>) -> Result<Vec<PairingRecord>, DomainError> {
        self.read(move |store| store.expired_pairings(now))
    }

    fn put_node_key(&self, node: &NodeId, public_key_base64url: &str) -> Result<(), DomainError> {
        let node = node.clone();
        let key = public_key_base64url.to_string();
        self.write(move |store| store.put_node_key(&node, &key))
    }

    fn node_key(&self, node: &NodeId) -> Result<Option<String>, DomainError> {
        let node = node.clone();
        self.read(move |store| store.node_key(&node))
    }

    fn list_node_keys(&self) -> Result<Vec<(NodeId, String)>, DomainError> {
        self.read(|store| store.list_node_keys())
    }
}

impl SessionRepo for ThreadedStore {
    fn get_session(&self, bot: &BotId) -> Result<Option<BotSessionRecord>, DomainError> {
        let bot = bot.clone();
        self.read(move |store| store.get_session(&bot))
    }

    fn put_session(&self, record: &BotSessionRecord) -> Result<(), DomainError> {
        let record = record.clone();
        self.write(move |store| store.put_session(&record))
    }

    fn delete_session(&self, bot: &BotId) -> Result<(), DomainError> {
        let bot = bot.clone();
        self.write(move |store| store.delete_session(&bot))
    }

    fn reset_session(&self, bot: &BotId) -> Result<(), DomainError> {
        let bot = bot.clone();
        self.write(move |store| store.reset_session(&bot))
    }
}

impl ToolCallRepo for ThreadedStore {
    fn find_tool_call(
        &self,
        turn: &TurnId,
        call: &ToolCallId,
    ) -> Result<Option<ToolCallRecord>, DomainError> {
        let turn = turn.clone();
        let call = call.clone();
        self.read(move |store| store.find_tool_call(&turn, &call))
    }

    fn insert_tool_call(&self, record: &ToolCallRecord) -> Result<(), DomainError> {
        let record = record.clone();
        self.write(move |store| store.insert_tool_call(&record))
    }
}

impl EventLog for ThreadedStore {
    fn epoch(&self) -> Result<u64, DomainError> {
        self.read(|store| store.epoch())
    }

    fn current_cursor(&self) -> Result<LiveCursor, DomainError> {
        self.read(|store| store.current_cursor())
    }

    fn append_event(&self, event: &LiveEvent) -> Result<LiveCursor, DomainError> {
        let event = event.clone();
        self.write(move |store| store.append_event(&event))
    }

    fn events_since(
        &self,
        cursor: LiveCursor,
        limit: u32,
    ) -> Result<Vec<(LiveCursor, LiveEvent)>, DomainError> {
        self.read(move |store| store.events_since(cursor, limit))
    }

    fn truncate_events(&self, keep: u64) -> Result<u64, DomainError> {
        self.write(move |store| store.truncate_events(keep))
    }
}

impl SettingsStore for ThreadedStore {
    fn settings(&self) -> Result<Settings, DomainError> {
        self.read(|store| store.settings())
    }

    fn put_settings(&self, settings: &Settings) -> Result<(), DomainError> {
        let settings = settings.clone();
        self.write(move |store| store.put_settings(&settings))
    }
}

impl RoutineRepo for ThreadedStore {
    fn insert_routine(&self, routine: &Routine) -> Result<(), DomainError> {
        let routine = routine.clone();
        self.write(move |store| store.insert_routine(&routine))
    }

    fn update_routine(&self, routine: &Routine) -> Result<(), DomainError> {
        let routine = routine.clone();
        self.write(move |store| store.update_routine(&routine))
    }

    fn get_routine(&self, id: &RoutineId) -> Result<Option<Routine>, DomainError> {
        let id = id.clone();
        self.read(move |store| store.get_routine(&id))
    }

    fn list_routines(&self) -> Result<Vec<Routine>, DomainError> {
        self.read(|store| store.list_routines())
    }

    fn list_routines_for_bot(&self, bot: &BotId) -> Result<Vec<Routine>, DomainError> {
        let bot = bot.clone();
        self.read(move |store| store.list_routines_for_bot(&bot))
    }

    fn due_routines(&self, now: DateTime<Utc>) -> Result<Vec<Routine>, DomainError> {
        self.read(move |store| store.due_routines(now))
    }

    fn delete_routine(&self, id: &RoutineId) -> Result<(), DomainError> {
        let id = id.clone();
        self.write(move |store| store.delete_routine(&id))
    }

    fn insert_run(&self, run: &RoutineRun) -> Result<(), DomainError> {
        let run = run.clone();
        self.write(move |store| store.insert_run(&run))
    }

    fn update_run(&self, run: &RoutineRun) -> Result<(), DomainError> {
        let run = run.clone();
        self.write(move |store| store.update_run(&run))
    }

    fn get_run(&self, id: &RunId) -> Result<Option<RoutineRun>, DomainError> {
        let id = id.clone();
        self.read(move |store| store.get_run(&id))
    }

    fn list_runs(
        &self,
        routine_id: &RoutineId,
        limit: u32,
    ) -> Result<Vec<RoutineRun>, DomainError> {
        let routine_id = routine_id.clone();
        self.read(move |store| store.list_runs(&routine_id, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    /// How long a test waits for an answer that must arrive. Long enough that a
    /// loaded machine cannot trip it, short enough that a regression reports a
    /// failure instead of stalling the gate.
    const BOUND: std::time::Duration = std::time::Duration::from_secs(5);

    /// A panicking store call used to poison the mutex and take the caller
    /// down. At the job boundary it is caught, answered with a typed error
    /// naming the panic, and reported as `Panicked` so the lane can stop.
    #[test]
    fn a_panicking_call_is_answered_with_a_typed_error() {
        let store = MemoryStore::new();
        let (reply, answer) = mpsc::channel::<Reply>();
        let work = job(
            reply,
            None,
            |_store: &dyn Persistence| -> Result<(), DomainError> {
                panic!("the connection exploded")
            },
        );

        assert!(matches!(work(&store), Outcome::Panicked(_)));

        let answer = answer
            .recv_timeout(BOUND)
            .expect("a panicking job still answers");
        match answer {
            Err(DomainError::Internal { message }) => {
                assert!(message.contains("the connection exploded"), "{message}");
            }
            other => panic!("expected an internal error, got {other:?}"),
        }
    }

    /// A lane that ran a panicking call stops, and the calls behind it must
    /// **fail typed rather than hang** — both the ones already queued and the
    /// ones that arrive after the lane is gone.
    ///
    /// This test used to hang forever, which is the worst possible shape: a
    /// hung test blocks the whole gate instead of reporting a failure. Two
    /// things were wrong and both are pinned here:
    ///
    /// * the sender was still alive while the assertions ran, so the queued
    ///   job stayed in the channel buffer and its reply sender was never
    ///   dropped — the caller parked on a channel nobody could answer. Every
    ///   wait below is therefore **bounded** (`recv_timeout`): a regression
    ///   fails in a second instead of hanging the gate;
    /// * the test never exercised the case its name promises — a call that
    ///   arrives *after* the lane stopped. That path is `dispatch` seeing a
    ///   disconnected sender, and it is asserted below.
    #[test]
    fn a_panicking_call_stops_the_lane_and_later_calls_fail_typed() {
        let store = MemoryStore::new();
        let (jobs, queue) = mpsc::channel::<Job>();

        let (first_reply, first_answer) = mpsc::channel::<Reply>();
        jobs.send(job(
            first_reply,
            None,
            |_store: &dyn Persistence| -> Result<(), DomainError> { panic!("boom") },
        ))
        .expect("queue the panicking job");

        let (second_reply, second_answer) = mpsc::channel::<Reply>();
        jobs.send(job(second_reply, None, |store: &dyn Persistence| {
            store.list_bots()
        }))
        .expect("queue a normal job behind it");

        // The lane returns after the panic. Dropping the sender first is what
        // lets the receiver's buffer actually release the queued job — with a
        // live sender, `std::sync::mpsc` keeps the buffer and the second
        // caller's reply channel stays open forever.
        drop(jobs);
        run_lane(&store, queue);

        let first = first_answer
            .recv_timeout(BOUND)
            .expect("the panicking caller must still be answered");
        assert!(
            matches!(first, Err(DomainError::Internal { .. })),
            "the panic is reported to the caller as an internal error"
        );

        assert!(
            second_answer.recv_timeout(BOUND).is_err(),
            "a call queued behind a panicking call must fail, not wait forever"
        );

        // The production path the name promises: a call that arrives after the
        // lane is gone. `dispatch` must refuse it with a typed error, never
        // park the caller on a channel no lane will read.
        let (third_reply, third_answer) = mpsc::channel::<Reply>();
        let dead_lane = std::sync::mpsc::channel::<Job>();
        drop(dead_lane.1);
        let refused = dead_lane
            .0
            .send(job(third_reply, None, |store: &dyn Persistence| {
                store.list_bots()
            }))
            .is_err();
        assert!(
            refused,
            "a send to a stopped lane is refused, which is what makes the caller \
             fail typed instead of waiting"
        );
        assert!(
            third_answer.recv_timeout(BOUND).is_err(),
            "a refused call's reply channel is closed"
        );
    }

    /// A panicking waker belongs to the *executor*, not to the store: the
    /// answer is already in the channel, so the panic must be contained and the
    /// lane must keep serving rather than dying under a caller's bad waker.
    #[test]
    fn a_panicking_waker_does_not_take_the_lane_down() {
        struct PanickingWaker;
        impl std::task::Wake for PanickingWaker {
            fn wake(self: std::sync::Arc<Self>) {
                panic!("this executor's waker is broken")
            }
        }

        let store = MemoryStore::new();
        let slot: Arc<Mutex<Option<Waker>>> =
            Arc::new(Mutex::new(Some(Waker::from(Arc::new(PanickingWaker)))));
        let (reply, answer) = mpsc::channel::<Reply>();
        let work = job(reply, Some(slot), |store: &dyn Persistence| {
            store.list_bots()
        });

        assert!(
            matches!(work(&store), Outcome::Served),
            "a panicking waker must not be reported as a panicking store call"
        );
        assert!(
            matches!(answer.recv_timeout(BOUND), Ok(Ok(_))),
            "the caller's answer must still be delivered"
        );
    }

    /// An error the store *produced* is not a broken lane: it crosses the
    /// channel unchanged and the lane keeps serving.
    #[test]
    fn a_domain_error_does_not_stop_the_lane() {
        let store = MemoryStore::new();
        let (jobs, queue) = mpsc::channel::<Job>();

        let (first_reply, first_answer) = mpsc::channel::<Reply>();
        jobs.send(job(first_reply, None, |_store: &dyn Persistence| {
            Err::<(), DomainError>(DomainError::conflict("nope"))
        }))
        .expect("queue the failing job");

        let (second_reply, second_answer) = mpsc::channel::<Reply>();
        jobs.send(job(second_reply, None, |store: &dyn Persistence| {
            store.list_bots()
        }))
        .expect("queue a normal job behind it");
        drop(jobs);

        run_lane(&store, queue);

        assert!(matches!(
            first_answer.recv_timeout(BOUND),
            Ok(Err(DomainError::Conflict { .. }))
        ));
        assert!(
            matches!(second_answer.recv_timeout(BOUND), Ok(Ok(_))),
            "the lane must keep serving after a domain error"
        );
    }
}
