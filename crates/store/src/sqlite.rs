//! The durable store: one SQLite file in WAL mode behind a mutex.
//!
//! Why the shape it has:
//!
//! * **payload + query columns**: every entity row stores the exact JSON of
//!   the domain object (`payload`) *and* real columns for each field a query
//!   filters or sorts on (ids, `seq`, `revision`, `status`, timestamps). A
//!   domain field may change shape without losing stored data, while every
//!   `WHERE`/`ORDER BY` still runs inside SQLite on indexed columns.
//! * **`Mutex<Connection>`**: `rusqlite::Connection` is `Send` but not `Sync`;
//!   the port demands a shareable store and every operation is microseconds,
//!   so one mutex around one connection is the whole concurrency story. Every
//!   method takes the lock, does one short unit of work, and releases it.
//! * **migrations in `PRAGMA user_version`**: `MIGRATIONS` is an ordered
//!   list; each step runs in its own transaction and bumps the version, so
//!   reopening an existing database is a no-op and a half-applied step cannot
//!   survive.
//! * **errors**: SQLite and serde failures become `DomainError::Internal`;
//!   uniqueness and revision violations become `DomainError::Conflict` with
//!   the same messages [`crate::MemoryStore`] produces.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use swarmbot_contracts::Seq;
use swarmbot_contracts::bot::{Bot, normalize_name};
use swarmbot_contracts::config::Settings;
use swarmbot_contracts::conversation::{Conversation, ConversationKind};
use swarmbot_contracts::delivery::{Delivery, DeliveryState};
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::error::{DomainError, not_found};
use swarmbot_contracts::id::{
    BotId, ConversationId, DeliveryId, EntryId, NodeId, RequestId, RoutineId, RuleId, RunId,
    TaskId, ToolCallId, TurnId,
};
use swarmbot_contracts::live::{LiveCursor, LiveEvent};
use swarmbot_contracts::node::{NodeInfo, PairingRecord};
use swarmbot_contracts::repo::{
    BotRepo, BotSessionRecord, ConversationRepo, DeliveryRepo, EntryRepo, EventLog, NodeRepo,
    RequestRepo, RoutineRepo, RuleRepo, SessionRepo, SettingsStore, TaskRepo, ToolCallRecord,
    ToolCallRepo, TurnRepo,
};
use swarmbot_contracts::request::{ApprovalRule, RuntimeRequest};
use swarmbot_contracts::routine::{Routine, RoutineRun, RoutineStatus, RunStatus};
use swarmbot_contracts::task::{Task, TaskStatus, TaskStep};
use swarmbot_contracts::turn::{Turn, TurnStatus};

use crate::{
    duplicate_bot_name, duplicate_id, io_err, json_err, seq_taken, stale_node_revision,
    stale_revision, ts,
};

/// Ordered schema migrations. `PRAGMA user_version` records how many have
/// run, so `open` on an existing database never re-runs or duplicates one.
const MIGRATIONS: &[&str] = &[MIGRATION_0001_INIT, MIGRATION_0002_ROUTINES];

const MIGRATION_0001_INIT: &str = r#"
CREATE TABLE meta (
    key   TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
);

-- The live-event epoch: stable across restarts, so clients may resume from a
-- stored cursor instead of taking a snapshot. A *fresh* data dir starts a new
-- log at epoch 1.
INSERT INTO meta (key, value) VALUES ('live_epoch', '1');

CREATE TABLE bots (
    id         TEXT NOT NULL PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at TEXT NOT NULL,
    payload    TEXT NOT NULL
);
-- bot_by_name must answer with exactly one bot, so handles are unique.
CREATE UNIQUE INDEX bots_name ON bots (name);

CREATE TABLE conversations (
    id            TEXT NOT NULL PRIMARY KEY,
    kind          TEXT NOT NULL,
    direct_bot_id TEXT,
    last_seq      INTEGER NOT NULL,
    archived      INTEGER NOT NULL,
    created_at    TEXT NOT NULL,
    payload       TEXT NOT NULL
);
CREATE INDEX conversations_direct_bot ON conversations (direct_bot_id);

-- Group membership, mirrored out of the payload so "which conversations is
-- this bot in" (and "which does it lead") is a query, not a scan over JSON.
CREATE TABLE conversation_members (
    conversation_id TEXT NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    position        INTEGER NOT NULL,
    bot_id          TEXT NOT NULL,
    is_lead         INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, position)
);
CREATE INDEX conversation_members_bot ON conversation_members (bot_id);

-- The transcript. `id` is the upsert key; `(conversation_id, seq)` is unique
-- because sequences are handed out once per conversation; `revision` guards
-- whole-entry upserts.
CREATE TABLE entries (
    id              TEXT NOT NULL PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    seq             INTEGER NOT NULL,
    revision        INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    payload         TEXT NOT NULL,
    UNIQUE (conversation_id, seq)
);

CREATE TABLE turns (
    id              TEXT NOT NULL PRIMARY KEY,
    bot_id          TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    status          TEXT NOT NULL,
    started_at      TEXT NOT NULL,
    payload         TEXT NOT NULL
);
CREATE INDEX turns_conversation ON turns (conversation_id, started_at, id);
CREATE INDEX turns_status ON turns (status);

CREATE TABLE deliveries (
    id              TEXT NOT NULL PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    bot_id          TEXT NOT NULL,
    state           TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    payload         TEXT NOT NULL
);
CREATE INDEX deliveries_bot_state ON deliveries (bot_id, state, created_at, id);
CREATE INDEX deliveries_conversation ON deliveries (conversation_id, state);

CREATE TABLE tasks (
    id         TEXT NOT NULL PRIMARY KEY,
    owner      TEXT NOT NULL,
    origin     TEXT NOT NULL,
    status     TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    payload    TEXT NOT NULL
);
CREATE INDEX tasks_owner ON tasks (owner, status);
CREATE INDEX tasks_origin ON tasks (origin, created_at, id);
CREATE INDEX tasks_status ON tasks (status, created_at, id);

-- A task's own progress lines; steps never become transcript entries.
CREATE TABLE task_steps (
    task_id TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (task_id, seq)
);

CREATE TABLE requests (
    id              TEXT NOT NULL PRIMARY KEY,
    bot_id          TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    turn_id         TEXT,
    status          TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    expires_at      TEXT,
    payload         TEXT NOT NULL
);
CREATE INDEX requests_status ON requests (status, created_at, id);
CREATE INDEX requests_conversation ON requests (conversation_id, created_at, id);
CREATE INDEX requests_turn ON requests (turn_id, status);

CREATE TABLE approval_rules (
    id         TEXT NOT NULL PRIMARY KEY,
    created_at TEXT NOT NULL,
    payload    TEXT NOT NULL
);

CREATE TABLE nodes (
    id        TEXT NOT NULL PRIMARY KEY,
    paired_at TEXT NOT NULL,
    payload   TEXT NOT NULL
);

-- Pinned node public keys (base64url), kept apart from the roster so a
-- roster read never carries key material.
CREATE TABLE node_keys (
    node_id    TEXT NOT NULL PRIMARY KEY,
    public_key TEXT NOT NULL
);

CREATE TABLE pairings (
    code_hash  TEXT NOT NULL PRIMARY KEY,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    used       INTEGER NOT NULL,
    payload    TEXT NOT NULL
);
CREATE INDEX pairings_expiry ON pairings (used, expires_at);

CREATE TABLE bot_sessions (
    bot_id     TEXT NOT NULL PRIMARY KEY,
    updated_at TEXT NOT NULL,
    payload    TEXT NOT NULL
);

-- Tool-call idempotency: one row per (turn, call) the harness already ran.
CREATE TABLE tool_calls (
    turn_id    TEXT NOT NULL,
    call_id    TEXT NOT NULL,
    created_at TEXT NOT NULL,
    payload    TEXT NOT NULL,
    PRIMARY KEY (turn_id, call_id)
);

-- One row, always id = 1: the settings document.
CREATE TABLE settings (
    id      INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    payload TEXT NOT NULL
);

-- The live change log. AUTOINCREMENT so ids are never reused after a
-- truncate; clients see (epoch, sequence), never the id.
CREATE TABLE events_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    epoch      INTEGER NOT NULL,
    sequence   INTEGER NOT NULL,
    event      TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX events_cursor ON events_log (epoch, sequence);
"#;

const MIGRATION_0002_ROUTINES: &str = r#"
CREATE TABLE routines (
    id          TEXT NOT NULL PRIMARY KEY,
    bot_id      TEXT NOT NULL,
    status      TEXT NOT NULL,
    next_run_at TEXT,
    created_at  TEXT NOT NULL,
    payload     TEXT NOT NULL
);
CREATE INDEX routines_bot ON routines (bot_id, created_at, id);
CREATE INDEX routines_due ON routines (status, next_run_at, id);

CREATE TABLE routine_runs (
    id         TEXT NOT NULL PRIMARY KEY,
    routine_id TEXT NOT NULL REFERENCES routines (id) ON DELETE CASCADE,
    bot_id     TEXT NOT NULL,
    status     TEXT NOT NULL,
    started_at TEXT NOT NULL,
    payload    TEXT NOT NULL
);
CREATE INDEX routine_runs_routine ON routine_runs (routine_id, started_at DESC, id DESC);
"#;

/// The durable [`Persistence`](swarmbot_contracts::repo::Persistence) store:
/// SQLite in WAL mode, safe to share across threads behind one mutex.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (creating if needed) the database at `path`, creating missing
    /// parent directories — the brain owns `<data>/swarmbot.sqlite` and must
    /// not fail on a first boot with an empty data dir.
    pub fn open(path: &Path) -> Result<Self, DomainError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let conn = Connection::open(path).map_err(sql_err)?;
        Self::from_connection(conn)
    }

    /// A private temporary database — the same SQL, no file. Tests use this
    /// to run the full contract suite against the real query surface.
    pub fn open_in_memory() -> Result<Self, DomainError> {
        let conn = Connection::open_in_memory().map_err(sql_err)?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, DomainError> {
        // WAL: readers never block behind the writer, and the database
        // survives a power cut. NORMAL synchronous is the durability/speed
        // point WAL is designed for. busy_timeout covers the rare second
        // process (a CLI command touching the same file). foreign_keys makes
        // the member/step cascades real.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_err)?;
        conn.pragma_update(None, "busy_timeout", 5000)
            .map_err(sql_err)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(sql_err)?;
        let mut conn = conn;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// The single lock point: a poisoned mutex means a bug elsewhere already
    /// panicked, which for a store is an internal error, never a second
    /// panic.
    fn conn(&self) -> Result<MutexGuard<'_, Connection>, DomainError> {
        self.conn
            .lock()
            .map_err(|_| DomainError::internal("sqlite connection lock poisoned"))
    }

    /// What SQLite reports as its journal mode (`wal` for file databases).
    #[cfg(test)]
    fn journal_mode(&self) -> Result<String, DomainError> {
        let conn = self.conn()?;
        conn.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
            .map_err(sql_err)
    }

    /// The migration level recorded in `PRAGMA user_version`.
    #[cfg(test)]
    fn user_version(&self) -> Result<i64, DomainError> {
        let conn = self.conn()?;
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .map_err(sql_err)
    }
}

/// Apply every migration the database has not seen yet, one transaction per
/// step, recording progress in `user_version` — re-runnable and idempotent.
fn migrate(conn: &mut Connection) -> Result<(), DomainError> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(sql_err)?;
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let step = index as i64 + 1;
        if version >= step {
            continue;
        }
        let tx = conn.transaction().map_err(sql_err)?;
        tx.execute_batch(sql).map_err(sql_err)?;
        tx.pragma_update(None, "user_version", step)
            .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
    }
    Ok(())
}

fn sql_err(err: rusqlite::Error) -> DomainError {
    DomainError::internal(err.to_string())
}

/// Uniqueness violations are domain conflicts the caller can act on
/// (retry with a newer revision, pick another name); everything else is an
/// internal failure.
fn conflict_or(err: rusqlite::Error, conflict: DomainError) -> DomainError {
    match err.sqlite_error_code() {
        Some(rusqlite::ErrorCode::ConstraintViolation) => conflict,
        _ => sql_err(err),
    }
}

/// Serialize one entity into its `payload` column.
fn stored<T: Serialize>(value: &T) -> Result<String, DomainError> {
    serde_json::to_string(value).map_err(json_err)
}

/// Read one entity from a single-payload-column query.
fn entity<T: DeserializeOwned>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Option<T>, DomainError> {
    let row: Option<String> = conn
        .query_row(sql, params, |row| row.get(0))
        .optional()
        .map_err(sql_err)?;
    match row {
        Some(payload) => Ok(Some(serde_json::from_str(&payload).map_err(json_err)?)),
        None => Ok(None),
    }
}

/// Read a list of entities from a single-payload-column query.
fn entities<T: DeserializeOwned>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<T>, DomainError> {
    let mut stmt = conn.prepare(sql).map_err(sql_err)?;
    let rows = stmt
        .query_map(params, |row| row.get::<_, String>(0))
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        let payload = row.map_err(sql_err)?;
        out.push(serde_json::from_str(&payload).map_err(json_err)?);
    }
    Ok(out)
}

fn kind_column(kind: &ConversationKind) -> &'static str {
    if kind.is_group() { "group" } else { "direct" }
}

fn turn_status_column(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "running",
        TurnStatus::Completed => "completed",
        TurnStatus::Aborted => "aborted",
        TurnStatus::Interrupted => "interrupted",
        TurnStatus::Failed => "failed",
    }
}

fn delivery_state_column(state: DeliveryState) -> &'static str {
    match state {
        DeliveryState::Queued => "queued",
        DeliveryState::Running => "running",
        DeliveryState::Done => "done",
        DeliveryState::Dropped => "dropped",
    }
}

fn task_status_column(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::Succeeded => "succeeded",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Interrupted => "interrupted",
    }
}

fn request_status_column(request: &RuntimeRequest) -> &'static str {
    match request.status {
        swarmbot_contracts::request::RequestStatus::Pending => "pending",
        swarmbot_contracts::request::RequestStatus::Answered => "answered",
        swarmbot_contracts::request::RequestStatus::Expired => "expired",
        swarmbot_contracts::request::RequestStatus::Cancelled => "cancelled",
    }
}

/// A bot's handle is unique, so `bot_by_name` can only ever answer with one
/// bot; `exclude` lets an update keep its own name.
fn bot_name_taken(conn: &Connection, name: &str, exclude: &BotId) -> Result<bool, DomainError> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM bots WHERE name = ?1 AND id != ?2",
            params![name, exclude.as_str()],
            |_| Ok(1),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(found.is_some())
}

/// Rewrite the member mirror rows of a conversation (groups only; a DM has
/// exactly one participant and it lives in the `direct_bot_id` column).
fn sync_members(conn: &Connection, conversation: &Conversation) -> Result<(), DomainError> {
    conn.execute(
        "DELETE FROM conversation_members WHERE conversation_id = ?1",
        params![conversation.id.as_str()],
    )
    .map_err(sql_err)?;
    if let ConversationKind::Group { members, lead } = &conversation.kind {
        let mut stmt = conn
            .prepare(
                "INSERT INTO conversation_members (conversation_id, position, bot_id, is_lead)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .map_err(sql_err)?;
        for (position, member) in members.iter().enumerate() {
            stmt.execute(params![
                conversation.id.as_str(),
                position as i64,
                member.as_str(),
                member == lead
            ])
            .map_err(sql_err)?;
        }
    }
    Ok(())
}

/// The epoch is stamped once, at migration time, and only read afterwards:
/// resuming clients must see the same epoch across restarts.
fn read_epoch(conn: &Connection) -> Result<u64, DomainError> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'live_epoch'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    match value {
        Some(value) => value.parse::<u64>().map_err(|_| {
            DomainError::internal(format!("meta.live_epoch is not a number: {value}"))
        }),
        None => Ok(1),
    }
}

impl BotRepo for SqliteStore {
    fn insert_bot(&self, bot: &Bot) -> Result<(), DomainError> {
        let conn = self.conn()?;
        if bot_name_taken(&conn, &bot.name, &bot.id)? {
            return Err(duplicate_bot_name(&bot.name));
        }
        let payload = stored(bot)?;
        conn.execute(
            "INSERT INTO bots (id, name, created_at, payload) VALUES (?1, ?2, ?3, ?4)",
            params![bot.id.as_str(), bot.name, ts(&bot.created_at), payload],
        )
        .map_err(|err| conflict_or(err, duplicate_id("bot", &bot.id)))?;
        Ok(())
    }

    fn update_bot(&self, bot: &Bot) -> Result<(), DomainError> {
        let conn = self.conn()?;
        if bot_name_taken(&conn, &bot.name, &bot.id)? {
            return Err(duplicate_bot_name(&bot.name));
        }
        let payload = stored(bot)?;
        let changed = conn
            .execute(
                "UPDATE bots SET name = ?2, created_at = ?3, payload = ?4 WHERE id = ?1",
                params![bot.id.as_str(), bot.name, ts(&bot.created_at), payload],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(not_found::bot(&bot.id));
        }
        Ok(())
    }

    fn get_bot(&self, id: &BotId) -> Result<Option<Bot>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM bots WHERE id = ?1",
            params![id.as_str()],
        )
    }

    fn bot_by_name(&self, name: &str) -> Result<Option<Bot>, DomainError> {
        let conn = self.conn()?;
        // Mentions and tool arguments are free-form; the stored handle is
        // normalized, so the lookup normalizes too.
        let normalized = normalize_name(name);
        entity(
            &conn,
            "SELECT payload FROM bots WHERE name = ?1",
            params![normalized],
        )
    }

    fn list_bots(&self) -> Result<Vec<Bot>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM bots ORDER BY created_at, id",
            [],
        )
    }

    fn delete_bot(&self, id: &BotId) -> Result<(), DomainError> {
        let conn = self.conn()?;
        // Idempotent: deleting twice is not an error, the bot is gone either
        // way. Related rows (sessions, deliveries) are the caller's to
        // settle — recovery drops queues, reset drops sessions.
        conn.execute("DELETE FROM bots WHERE id = ?1", params![id.as_str()])
            .map_err(sql_err)?;
        Ok(())
    }
}

impl ConversationRepo for SqliteStore {
    fn insert_conversation(&self, conversation: &Conversation) -> Result<(), DomainError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction().map_err(sql_err)?;
        let payload = stored(conversation)?;
        tx.execute(
            "INSERT INTO conversations (id, kind, direct_bot_id, last_seq, archived, created_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                conversation.id.as_str(),
                kind_column(&conversation.kind),
                conversation.direct_bot().map(BotId::as_str),
                conversation.last_seq as i64,
                conversation.archived,
                ts(&conversation.created_at),
                payload,
            ],
        )
        .map_err(|err| conflict_or(err, duplicate_id("conversation", &conversation.id)))?;
        sync_members(&tx, conversation)?;
        tx.commit().map_err(sql_err)?;
        Ok(())
    }

    fn update_conversation(&self, conversation: &Conversation) -> Result<(), DomainError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction().map_err(sql_err)?;
        let payload = stored(conversation)?;
        // `last_seq` is a real column because the sequence invariant (no
        // holes in a transcript) is checked by reads of this counter.
        let changed = tx
            .execute(
                "UPDATE conversations
                 SET kind = ?2, direct_bot_id = ?3, last_seq = ?4, archived = ?5, payload = ?6
                 WHERE id = ?1",
                params![
                    conversation.id.as_str(),
                    kind_column(&conversation.kind),
                    conversation.direct_bot().map(BotId::as_str),
                    conversation.last_seq as i64,
                    conversation.archived,
                    payload,
                ],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(not_found::conversation(&conversation.id));
        }
        sync_members(&tx, conversation)?;
        tx.commit().map_err(sql_err)?;
        Ok(())
    }

    fn get_conversation(&self, id: &ConversationId) -> Result<Option<Conversation>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM conversations WHERE id = ?1",
            params![id.as_str()],
        )
    }

    fn direct_conversation(&self, bot: &BotId) -> Result<Option<Conversation>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM conversations
             WHERE kind = 'direct' AND direct_bot_id = ?1
             ORDER BY created_at, id LIMIT 1",
            params![bot.as_str()],
        )
    }

    fn list_conversations(&self) -> Result<Vec<Conversation>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM conversations ORDER BY created_at, id",
            [],
        )
    }

    fn delete_conversation(&self, id: &ConversationId) -> Result<(), DomainError> {
        let conn = self.conn()?;
        // Member rows go with it (ON DELETE CASCADE); the transcript stays —
        // deleting a group must not shred the paper trail, and
        // `delete_entries` exists for the caller that really wants that.
        conn.execute(
            "DELETE FROM conversations WHERE id = ?1",
            params![id.as_str()],
        )
        .map_err(sql_err)?;
        Ok(())
    }
}

impl EntryRepo for SqliteStore {
    fn upsert_entry(&self, entry: &Entry) -> Result<(), DomainError> {
        let mut conn = self.conn()?;
        // One transaction for read-then-write: the revision guard is only
        // honest if no other write can slip between the check and the write.
        let tx = conn.transaction().map_err(sql_err)?;
        let revision: Option<i64> = tx
            .query_row(
                "SELECT revision FROM entries WHERE id = ?1",
                params![entry.id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some(revision) = revision
            && revision >= i64::from(entry.revision)
        {
            return Err(stale_revision(entry, revision.max(0) as u32));
        }
        let payload = stored(entry)?;
        let written = tx.execute(
            "INSERT INTO entries (id, conversation_id, seq, revision, created_at, updated_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (id) DO UPDATE SET
                 conversation_id = excluded.conversation_id,
                 seq             = excluded.seq,
                 revision        = excluded.revision,
                 created_at      = excluded.created_at,
                 updated_at      = excluded.updated_at,
                 payload         = excluded.payload",
            params![
                entry.id.as_str(),
                entry.conversation_id.as_str(),
                entry.seq as i64,
                i64::from(entry.revision),
                ts(&entry.created_at),
                ts(&entry.updated_at),
                payload,
            ],
        );
        match written {
            Ok(_) => {
                tx.commit().map_err(sql_err)?;
                Ok(())
            }
            // The only constraint left to trip is UNIQUE(conversation_id,
            // seq): someone else already holds this position.
            Err(err) => Err(conflict_or(
                err,
                seq_taken(&entry.conversation_id, entry.seq, &entry.id),
            )),
        }
    }

    fn get_entry(&self, id: &EntryId) -> Result<Option<Entry>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM entries WHERE id = ?1",
            params![id.as_str()],
        )
    }

    fn entries_after(
        &self,
        conversation: &ConversationId,
        after_seq: Seq,
        limit: u32,
    ) -> Result<Vec<Entry>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM entries
             WHERE conversation_id = ?1 AND seq > ?2
             ORDER BY seq LIMIT ?3",
            params![conversation.as_str(), after_seq as i64, i64::from(limit)],
        )
    }

    fn entry_tail(
        &self,
        conversation: &ConversationId,
        limit: u32,
    ) -> Result<Vec<Entry>, DomainError> {
        let conn = self.conn()?;
        // The newest window, re-sorted ascending: a transcript always reads
        // oldest-to-newest, even when it was selected newest-first.
        entities(
            &conn,
            "SELECT payload FROM (
                 SELECT payload, seq FROM entries
                 WHERE conversation_id = ?1
                 ORDER BY seq DESC LIMIT ?2
             ) ORDER BY seq ASC",
            params![conversation.as_str(), i64::from(limit)],
        )
    }

    fn delete_entries(&self, conversation: &ConversationId) -> Result<u64, DomainError> {
        let conn = self.conn()?;
        let deleted = conn
            .execute(
                "DELETE FROM entries WHERE conversation_id = ?1",
                params![conversation.as_str()],
            )
            .map_err(sql_err)?;
        Ok(deleted as u64)
    }
}

impl TurnRepo for SqliteStore {
    fn insert_turn(&self, turn: &Turn) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(turn)?;
        conn.execute(
            "INSERT INTO turns (id, bot_id, conversation_id, status, started_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                turn.id.as_str(),
                turn.bot_id.as_str(),
                turn.conversation_id.as_str(),
                turn_status_column(turn.status),
                ts(&turn.started_at),
                payload,
            ],
        )
        .map_err(|err| conflict_or(err, duplicate_id("turn", &turn.id)))?;
        Ok(())
    }

    fn update_turn(&self, turn: &Turn) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(turn)?;
        let changed = conn
            .execute(
                "UPDATE turns SET status = ?2, started_at = ?3, payload = ?4 WHERE id = ?1",
                params![
                    turn.id.as_str(),
                    turn_status_column(turn.status),
                    ts(&turn.started_at),
                    payload,
                ],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(not_found::turn(&turn.id));
        }
        Ok(())
    }

    fn get_turn(&self, id: &TurnId) -> Result<Option<Turn>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM turns WHERE id = ?1",
            params![id.as_str()],
        )
    }

    fn turns_in(
        &self,
        conversation: &ConversationId,
        limit: u32,
    ) -> Result<Vec<Turn>, DomainError> {
        let conn = self.conn()?;
        // The newest `limit` turns, returned oldest-first — a context window,
        // not a feed.
        entities(
            &conn,
            "SELECT payload FROM (
                 SELECT payload, started_at, id FROM turns
                 WHERE conversation_id = ?1
                 ORDER BY started_at DESC, id DESC LIMIT ?2
             ) ORDER BY started_at ASC, id ASC",
            params![conversation.as_str(), i64::from(limit)],
        )
    }

    fn running_turns(&self) -> Result<Vec<Turn>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM turns WHERE status = 'running' ORDER BY started_at, id",
            [],
        )
    }

    fn turns_since(&self, since: DateTime<Utc>) -> Result<Vec<Turn>, DomainError> {
        let conn = self.conn()?;
        // `>=` because the window is inclusive, and the same `(started_at, id)`
        // order the memory store sorts by — `ts` is fixed-width UTC, so SQLite's
        // text comparison is the chronological one.
        entities(
            &conn,
            "SELECT payload FROM turns WHERE started_at >= ?1 ORDER BY started_at, id",
            params![ts(&since)],
        )
    }
}

impl DeliveryRepo for SqliteStore {
    fn insert_delivery(&self, delivery: &Delivery) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(delivery)?;
        conn.execute(
            "INSERT INTO deliveries (id, conversation_id, bot_id, state, created_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                delivery.id.as_str(),
                delivery.conversation_id.as_str(),
                delivery.bot_id.as_str(),
                delivery_state_column(delivery.state),
                ts(&delivery.created_at),
                payload,
            ],
        )
        .map_err(|err| conflict_or(err, duplicate_id("delivery", &delivery.id)))?;
        Ok(())
    }

    fn update_delivery(&self, delivery: &Delivery) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(delivery)?;
        let changed = conn
            .execute(
                "UPDATE deliveries SET state = ?2, created_at = ?3, payload = ?4 WHERE id = ?1",
                params![
                    delivery.id.as_str(),
                    delivery_state_column(delivery.state),
                    ts(&delivery.created_at),
                    payload,
                ],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(DomainError::not_found("delivery", &delivery.id));
        }
        Ok(())
    }

    fn get_delivery(&self, id: &DeliveryId) -> Result<Option<Delivery>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM deliveries WHERE id = ?1",
            params![id.as_str()],
        )
    }

    fn queued_deliveries(&self, bot: &BotId) -> Result<Vec<Delivery>, DomainError> {
        let conn = self.conn()?;
        // FIFO is the fairness rule: created_at first, id breaks ties (ids
        // are time-sortable, so equal timestamps stay in arrival order).
        entities(
            &conn,
            "SELECT payload FROM deliveries
             WHERE bot_id = ?1 AND state = 'queued'
             ORDER BY created_at, id",
            params![bot.as_str()],
        )
    }

    fn drop_deliveries(
        &self,
        conversation: &ConversationId,
        reason: &str,
    ) -> Result<u64, DomainError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction().map_err(sql_err)?;
        let payloads: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT payload FROM deliveries
                     WHERE conversation_id = ?1 AND state = 'queued'
                     ORDER BY created_at, id",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![conversation.as_str()], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(sql_err)?);
            }
            out
        };
        // Marked dropped, not deleted: a dropped delivery is still a fact
        // about the conversation (recovery reports it, the audit keeps it).
        let now = Utc::now();
        let mut count = 0u64;
        for payload in payloads {
            let mut delivery: Delivery = serde_json::from_str(&payload).map_err(json_err)?;
            delivery.mark_dropped(now);
            let updated = stored(&delivery)?;
            tx.execute(
                "UPDATE deliveries SET state = 'dropped', payload = ?2 WHERE id = ?1",
                params![delivery.id.as_str(), updated],
            )
            .map_err(sql_err)?;
            count += 1;
        }
        tx.commit().map_err(sql_err)?;
        tracing::debug!(
            conversation = %conversation,
            count,
            reason,
            "dropped queued deliveries"
        );
        Ok(count)
    }
}

impl TaskRepo for SqliteStore {
    fn insert_task(&self, task: &Task) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(task)?;
        conn.execute(
            "INSERT INTO tasks (id, owner, origin, status, created_at, updated_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                task.id.as_str(),
                task.owner.as_str(),
                task.origin.as_str(),
                task_status_column(task.status),
                ts(&task.created_at),
                ts(&task.updated_at),
                payload,
            ],
        )
        .map_err(|err| conflict_or(err, duplicate_id("task", &task.id)))?;
        Ok(())
    }

    fn update_task(&self, task: &Task) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(task)?;
        let changed = conn
            .execute(
                "UPDATE tasks SET status = ?2, updated_at = ?3, payload = ?4 WHERE id = ?1",
                params![
                    task.id.as_str(),
                    task_status_column(task.status),
                    ts(&task.updated_at),
                    payload,
                ],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(not_found::task(&task.id));
        }
        Ok(())
    }

    fn get_task(&self, id: &TaskId) -> Result<Option<Task>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM tasks WHERE id = ?1",
            params![id.as_str()],
        )
    }

    fn tasks_of(&self, owner: &BotId) -> Result<Vec<Task>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM tasks WHERE owner = ?1 ORDER BY created_at, id",
            params![owner.as_str()],
        )
    }

    fn tasks_in(&self, origin: &ConversationId) -> Result<Vec<Task>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM tasks WHERE origin = ?1 ORDER BY created_at, id",
            params![origin.as_str()],
        )
    }

    fn queued_tasks(&self) -> Result<Vec<Task>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM tasks WHERE status = 'queued' ORDER BY created_at, id",
            [],
        )
    }

    fn running_tasks(&self) -> Result<Vec<Task>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM tasks WHERE status = 'running' ORDER BY created_at, id",
            [],
        )
    }

    fn running_task_count(&self, owner: &BotId) -> Result<u32, DomainError> {
        let conn = self.conn()?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE owner = ?1 AND status = 'running'",
                params![owner.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        Ok(count.max(0) as u32)
    }

    fn append_task_step(&self, step: &TaskStep) -> Result<(), DomainError> {
        let conn = self.conn()?;
        // Steps belong to a task; a step for a missing task is a bug upstream
        // and must not create an orphan row (the FK would reject it anyway —
        // this check reports it as the domain error it is).
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM tasks WHERE id = ?1",
                params![step.task_id.as_str()],
                |_| Ok(1),
            )
            .optional()
            .map_err(sql_err)?;
        if exists.is_none() {
            return Err(not_found::task(&step.task_id));
        }
        let payload = stored(step)?;
        conn.execute(
            "INSERT INTO task_steps (task_id, seq, payload) VALUES (?1, ?2, ?3)
             ON CONFLICT (task_id, seq) DO UPDATE SET payload = excluded.payload",
            params![step.task_id.as_str(), i64::from(step.seq), payload],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    fn task_steps(&self, task: &TaskId) -> Result<Vec<TaskStep>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM task_steps WHERE task_id = ?1 ORDER BY seq",
            params![task.as_str()],
        )
    }
}

impl RequestRepo for SqliteStore {
    fn insert_request(&self, request: &RuntimeRequest) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(request)?;
        conn.execute(
            "INSERT INTO requests (id, bot_id, conversation_id, turn_id, status, created_at, expires_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                request.id.as_str(),
                request.bot_id.as_str(),
                request.conversation_id.as_str(),
                request.turn_id.as_ref().map(TurnId::as_str),
                request_status_column(request),
                ts(&request.created_at),
                request.expires_at.as_ref().map(ts),
                payload,
            ],
        )
        .map_err(|err| conflict_or(err, duplicate_id("request", &request.id)))?;
        Ok(())
    }

    fn update_request(&self, request: &RuntimeRequest) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(request)?;
        let changed = conn
            .execute(
                "UPDATE requests SET status = ?2, expires_at = ?3, payload = ?4 WHERE id = ?1",
                params![
                    request.id.as_str(),
                    request_status_column(request),
                    request.expires_at.as_ref().map(ts),
                    payload,
                ],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(not_found::request(&request.id));
        }
        Ok(())
    }

    fn get_request(&self, id: &RequestId) -> Result<Option<RuntimeRequest>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM requests WHERE id = ?1",
            params![id.as_str()],
        )
    }

    fn pending_requests(&self) -> Result<Vec<RuntimeRequest>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM requests WHERE status = 'pending' ORDER BY created_at, id",
            [],
        )
    }

    fn requests_in(
        &self,
        conversation: &ConversationId,
    ) -> Result<Vec<RuntimeRequest>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM requests WHERE conversation_id = ?1 ORDER BY created_at, id",
            params![conversation.as_str()],
        )
    }

    fn cancel_requests_of_turn(&self, turn: &TurnId, reason: &str) -> Result<u64, DomainError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction().map_err(sql_err)?;
        let payloads: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT payload FROM requests
                     WHERE turn_id = ?1 AND status = 'pending'
                     ORDER BY created_at, id",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![turn.as_str()], |row| row.get::<_, String>(0))
                .map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(sql_err)?);
            }
            out
        };
        // A card must not outlive the turn that raised it; settled requests
        // are left exactly as they were (they are history, not state).
        let now = Utc::now();
        let mut count = 0u64;
        for payload in payloads {
            let mut request: RuntimeRequest = serde_json::from_str(&payload).map_err(json_err)?;
            if request.cancel(now, reason) {
                let updated = stored(&request)?;
                tx.execute(
                    "UPDATE requests SET status = 'cancelled', payload = ?2 WHERE id = ?1",
                    params![request.id.as_str(), updated],
                )
                .map_err(sql_err)?;
                count += 1;
            }
        }
        tx.commit().map_err(sql_err)?;
        Ok(count)
    }
}

impl RuleRepo for SqliteStore {
    fn insert_rule(&self, rule: &ApprovalRule) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(rule)?;
        conn.execute(
            "INSERT INTO approval_rules (id, created_at, payload) VALUES (?1, ?2, ?3)",
            params![rule.id.as_str(), ts(&rule.created_at), payload],
        )
        .map_err(|err| conflict_or(err, duplicate_id("rule", &rule.id)))?;
        Ok(())
    }

    fn list_rules(&self) -> Result<Vec<ApprovalRule>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM approval_rules ORDER BY created_at, id",
            [],
        )
    }

    fn delete_rule(&self, id: &RuleId) -> Result<(), DomainError> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM approval_rules WHERE id = ?1",
            params![id.as_str()],
        )
        .map_err(sql_err)?;
        Ok(())
    }
}

impl NodeRepo for SqliteStore {
    fn insert_node(&self, node: &NodeInfo) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(node)?;
        conn.execute(
            "INSERT INTO nodes (id, paired_at, payload) VALUES (?1, ?2, ?3)",
            params![node.id.as_str(), ts(&node.paired_at), payload],
        )
        .map_err(|err| conflict_or(err, duplicate_id("node", &node.id)))?;
        Ok(())
    }

    fn update_node(&self, node: &NodeInfo) -> Result<(), DomainError> {
        let mut conn = self.conn()?;
        // One transaction for read-then-write, exactly like `upsert_entry`: the
        // revision guard is only honest if no other writer can slip between the
        // check and the write, and the roster has several concurrent ones.
        let tx = conn.transaction().map_err(sql_err)?;
        // The revision travels inside `payload` rather than in its own column,
        // so checking it costs a `json_extract`. That is affordable here — the
        // roster is a handful of rows and the frequent writer is a heartbeat
        // every 15 s — and it keeps this change out of the migration list, so a
        // database written before the field existed still opens. `COALESCE` is
        // what makes that true: such a payload has no `$.revision` path, and
        // without the coalesce the check would see NULL and reject every write
        // to a pre-existing row instead of treating it as generation 0.
        let stored_revision: Option<i64> = tx
            .query_row(
                "SELECT COALESCE(CAST(json_extract(payload, '$.revision') AS INTEGER), 0)
                 FROM nodes WHERE id = ?1",
                params![node.id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let Some(stored_revision) = stored_revision else {
            return Err(not_found::node(&node.id));
        };
        if stored_revision >= node.revision as i64 {
            return Err(stale_node_revision(node, stored_revision.max(0) as u64));
        }
        let payload = stored(node)?;
        let changed = tx
            .execute(
                "UPDATE nodes SET paired_at = ?2, payload = ?3 WHERE id = ?1",
                params![node.id.as_str(), ts(&node.paired_at), payload],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(not_found::node(&node.id));
        }
        tx.commit().map_err(sql_err)
    }

    fn get_node(&self, id: &NodeId) -> Result<Option<NodeInfo>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM nodes WHERE id = ?1",
            params![id.as_str()],
        )
    }

    fn list_nodes(&self) -> Result<Vec<NodeInfo>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM nodes ORDER BY paired_at, id",
            [],
        )
    }

    fn insert_pairing(&self, record: &PairingRecord) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(record)?;
        conn.execute(
            "INSERT INTO pairings (code_hash, created_at, expires_at, used, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.code_hash,
                ts(&record.created_at),
                ts(&record.expires_at),
                record.used,
                payload,
            ],
        )
        .map_err(|err| conflict_or(err, duplicate_id("pairing", &record.code_hash)))?;
        Ok(())
    }

    fn get_pairing(&self, code_hash: &str) -> Result<Option<PairingRecord>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM pairings WHERE code_hash = ?1",
            params![code_hash],
        )
    }

    fn update_pairing(&self, record: &PairingRecord) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(record)?;
        let changed = conn
            .execute(
                "UPDATE pairings SET used = ?2, expires_at = ?3, payload = ?4 WHERE code_hash = ?1",
                params![
                    record.code_hash,
                    record.used,
                    ts(&record.expires_at),
                    payload,
                ],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(DomainError::not_found("pairing", &record.code_hash));
        }
        Ok(())
    }

    fn expired_pairings(&self, now: DateTime<Utc>) -> Result<Vec<PairingRecord>, DomainError> {
        let conn = self.conn()?;
        // Only *unused* codes expire — a spent code is history. Comparing the
        // canonical timestamp strings compares the instants.
        entities(
            &conn,
            "SELECT payload FROM pairings
             WHERE used = 0 AND expires_at <= ?1
             ORDER BY created_at, code_hash",
            params![ts(&now)],
        )
    }

    fn put_node_key(&self, node: &NodeId, public_key_base64url: &str) -> Result<(), DomainError> {
        let conn = self.conn()?;
        // Upsert: re-pairing a node replaces its pinned key.
        conn.execute(
            "INSERT INTO node_keys (node_id, public_key) VALUES (?1, ?2)
             ON CONFLICT (node_id) DO UPDATE SET public_key = excluded.public_key",
            params![node.as_str(), public_key_base64url],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    fn node_key(&self, node: &NodeId) -> Result<Option<String>, DomainError> {
        let conn = self.conn()?;
        conn.query_row(
            "SELECT public_key FROM node_keys WHERE node_id = ?1",
            params![node.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)
    }

    fn list_node_keys(&self) -> Result<Vec<(NodeId, String)>, DomainError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare("SELECT node_id, public_key FROM node_keys ORDER BY node_id")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (node_id, key) = row.map_err(sql_err)?;
            out.push((node_id_from_column(&node_id)?, key));
        }
        Ok(out)
    }
}

/// Rebuild a `NodeId` from its stored string. `NodeId::parse` is too strict
/// here: the fixed local node (`nod_local`) has no 32-hex suffix, and the
/// column only ever holds ids this store wrote via `as_str`.
fn node_id_from_column(value: &str) -> Result<NodeId, DomainError> {
    value
        .strip_prefix("nod_")
        .map(NodeId::from_suffix)
        .ok_or_else(|| DomainError::internal(format!("stored node id `{value}` lost its prefix")))
}

impl SessionRepo for SqliteStore {
    fn get_session(&self, bot: &BotId) -> Result<Option<BotSessionRecord>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM bot_sessions WHERE bot_id = ?1",
            params![bot.as_str()],
        )
    }

    fn put_session(&self, record: &BotSessionRecord) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(record)?;
        // `put` is the port's word for upsert: the session is written whole
        // after every turn, whether or not one existed before.
        conn.execute(
            "INSERT INTO bot_sessions (bot_id, updated_at, payload) VALUES (?1, ?2, ?3)
             ON CONFLICT (bot_id) DO UPDATE SET updated_at = excluded.updated_at, payload = excluded.payload",
            params![record.bot_id.as_str(), ts(&record.updated_at), payload],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    fn delete_session(&self, bot: &BotId) -> Result<(), DomainError> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM bot_sessions WHERE bot_id = ?1",
            params![bot.as_str()],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    fn reset_session(&self, bot: &BotId) -> Result<(), DomainError> {
        let conn = self.conn()?;
        // "New session" means: the backend forgets, the bot does not go blind
        // in its groups — the `seen` cursors survive so no conversation
        // re-shows old messages after the reset.
        let existing: Option<BotSessionRecord> = entity(
            &conn,
            "SELECT payload FROM bot_sessions WHERE bot_id = ?1",
            params![bot.as_str()],
        )?;
        let Some(mut record) = existing else {
            return Ok(());
        };
        record.native_session_id = None;
        record.history = Value::Array(Vec::new());
        record.token_estimate = 0;
        record.updated_at = Utc::now();
        let payload = stored(&record)?;
        conn.execute(
            "UPDATE bot_sessions SET updated_at = ?2, payload = ?3 WHERE bot_id = ?1",
            params![bot.as_str(), ts(&record.updated_at), payload],
        )
        .map_err(sql_err)?;
        Ok(())
    }
}

impl ToolCallRepo for SqliteStore {
    fn find_tool_call(
        &self,
        turn: &TurnId,
        call: &ToolCallId,
    ) -> Result<Option<ToolCallRecord>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM tool_calls WHERE turn_id = ?1 AND call_id = ?2",
            params![turn.as_str(), call.as_str()],
        )
    }

    fn insert_tool_call(&self, record: &ToolCallRecord) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(record)?;
        // Keyed by (turn, call): a harness may replay a call id after a
        // resume, and re-recording the same call must not fail the turn.
        conn.execute(
            "INSERT INTO tool_calls (turn_id, call_id, created_at, payload) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (turn_id, call_id) DO UPDATE SET
                 created_at = excluded.created_at,
                 payload    = excluded.payload",
            params![
                record.turn_id.as_str(),
                record.call_id.as_str(),
                ts(&record.created_at),
                payload,
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }
}

impl EventLog for SqliteStore {
    fn epoch(&self) -> Result<u64, DomainError> {
        let conn = self.conn()?;
        read_epoch(&conn)
    }

    fn current_cursor(&self) -> Result<LiveCursor, DomainError> {
        let conn = self.conn()?;
        let epoch = read_epoch(&conn)?;
        let sequence: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) FROM events_log WHERE epoch = ?1",
                params![epoch as i64],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        Ok(LiveCursor::new(epoch, sequence.max(0) as u64))
    }

    fn append_event(&self, event: &LiveEvent) -> Result<LiveCursor, DomainError> {
        let mut conn = self.conn()?;
        // MAX+INSERT in one transaction: two appenders can never draw the
        // same sequence number. Sequences grow unbounded within an epoch by
        // design — a u64 counter needs no wrap-around, and inventing epoch
        // bumps would force spurious client resyncs.
        let tx = conn.transaction().map_err(sql_err)?;
        let epoch = read_epoch(&tx)?;
        let sequence: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) + 1 FROM events_log WHERE epoch = ?1",
                params![epoch as i64],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        let json = serde_json::to_string(event).map_err(json_err)?;
        tx.execute(
            "INSERT INTO events_log (epoch, sequence, event, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![epoch as i64, sequence, json, ts(&Utc::now())],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(LiveCursor::new(epoch, sequence.max(0) as u64))
    }

    fn events_since(
        &self,
        cursor: LiveCursor,
        limit: u32,
    ) -> Result<Vec<(LiveCursor, LiveEvent)>, DomainError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT epoch, sequence, event FROM events_log
                 WHERE epoch > ?1 OR (epoch = ?2 AND sequence > ?3)
                 ORDER BY epoch, sequence LIMIT ?4",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(
                params![
                    cursor.epoch as i64,
                    cursor.epoch as i64,
                    cursor.sequence as i64,
                    i64::from(limit),
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (epoch, sequence, event) = row.map_err(sql_err)?;
            let event: LiveEvent = serde_json::from_str(&event).map_err(json_err)?;
            out.push((
                LiveCursor::new(epoch.max(0) as u64, sequence.max(0) as u64),
                event,
            ));
        }
        Ok(out)
    }

    fn truncate_events(&self, keep: u64) -> Result<u64, DomainError> {
        let conn = self.conn()?;
        // The ring is bounded on disk too: keep the newest `keep` by cursor
        // order, delete the rest, and report exactly how many went.
        let deleted = conn
            .execute(
                "DELETE FROM events_log WHERE id NOT IN (
                     SELECT id FROM events_log ORDER BY epoch DESC, sequence DESC LIMIT ?1
                 )",
                params![keep as i64],
            )
            .map_err(sql_err)?;
        Ok(deleted as u64)
    }
}

impl SettingsStore for SqliteStore {
    fn settings(&self) -> Result<Settings, DomainError> {
        let conn = self.conn()?;
        // An empty table means "nobody changed anything", which is exactly
        // what the defaults say — a first boot must not need a settings row.
        let stored: Option<Settings> =
            entity(&conn, "SELECT payload FROM settings WHERE id = 1", [])?;
        Ok(stored.unwrap_or_default())
    }

    fn put_settings(&self, settings: &Settings) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(settings)?;
        conn.execute(
            "INSERT INTO settings (id, payload) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET payload = excluded.payload",
            params![payload],
        )
        .map_err(sql_err)?;
        Ok(())
    }
}

fn routine_status_column(status: RoutineStatus) -> &'static str {
    match status {
        RoutineStatus::Active => "active",
        RoutineStatus::Paused => "paused",
        RoutineStatus::Completed => "completed",
    }
}

fn run_status_column(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "running",
        RunStatus::Success => "success",
        RunStatus::Failed => "failed",
    }
}

impl RoutineRepo for SqliteStore {
    fn insert_routine(&self, routine: &Routine) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(routine)?;
        let next_run = routine.next_run_at.as_ref().map(ts);
        conn.execute(
            "INSERT INTO routines (id, bot_id, status, next_run_at, created_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                routine.id.as_str(),
                routine.bot_id.as_str(),
                routine_status_column(routine.status),
                next_run,
                ts(&routine.created_at),
                payload,
            ],
        )
        .map_err(|err| conflict_or(err, duplicate_id("routine", &routine.id)))?;
        Ok(())
    }

    fn update_routine(&self, routine: &Routine) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(routine)?;
        let next_run = routine.next_run_at.as_ref().map(ts);
        let changed = conn
            .execute(
                "UPDATE routines SET status = ?2, next_run_at = ?3, payload = ?4 WHERE id = ?1",
                params![
                    routine.id.as_str(),
                    routine_status_column(routine.status),
                    next_run,
                    payload,
                ],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(not_found::routine(&routine.id));
        }
        Ok(())
    }

    fn get_routine(&self, id: &RoutineId) -> Result<Option<Routine>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM routines WHERE id = ?1",
            params![id.as_str()],
        )
    }

    fn list_routines(&self) -> Result<Vec<Routine>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM routines ORDER BY created_at, id",
            [],
        )
    }

    fn list_routines_for_bot(&self, bot: &BotId) -> Result<Vec<Routine>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM routines WHERE bot_id = ?1 ORDER BY created_at, id",
            params![bot.as_str()],
        )
    }

    fn due_routines(&self, now: DateTime<Utc>) -> Result<Vec<Routine>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM routines WHERE status = 'active' AND next_run_at IS NOT NULL AND next_run_at <= ?1 ORDER BY next_run_at, id",
            params![ts(&now)],
        )
    }

    fn delete_routine(&self, id: &RoutineId) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let changed = conn
            .execute("DELETE FROM routines WHERE id = ?1", params![id.as_str()])
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(not_found::routine(id));
        }
        Ok(())
    }

    fn insert_run(&self, run: &RoutineRun) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(run)?;
        conn.execute(
            "INSERT INTO routine_runs (id, routine_id, bot_id, status, started_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run.id.as_str(),
                run.routine_id.as_str(),
                run.bot_id.as_str(),
                run_status_column(run.status),
                ts(&run.started_at),
                payload,
            ],
        )
        .map_err(|err| conflict_or(err, duplicate_id("routine_run", &run.id)))?;
        Ok(())
    }

    fn update_run(&self, run: &RoutineRun) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let payload = stored(run)?;
        let changed = conn
            .execute(
                "UPDATE routine_runs SET status = ?2, payload = ?3 WHERE id = ?1",
                params![run.id.as_str(), run_status_column(run.status), payload,],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(not_found::routine_run(&run.id));
        }
        Ok(())
    }

    fn get_run(&self, id: &RunId) -> Result<Option<RoutineRun>, DomainError> {
        let conn = self.conn()?;
        entity(
            &conn,
            "SELECT payload FROM routine_runs WHERE id = ?1",
            params![id.as_str()],
        )
    }

    fn list_runs(
        &self,
        routine_id: &RoutineId,
        limit: u32,
    ) -> Result<Vec<RoutineRun>, DomainError> {
        let conn = self.conn()?;
        entities(
            &conn,
            "SELECT payload FROM routine_runs WHERE routine_id = ?1 ORDER BY started_at DESC, id DESC LIMIT ?2",
            params![routine_id.as_str(), limit],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_a_file_creates_missing_parents() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("a/b/c/swarmbot.sqlite");
        let store = SqliteStore::open(&path).expect("open creates parents");
        assert!(path.exists());
        assert_eq!(store.user_version().unwrap(), MIGRATIONS.len() as i64);
    }

    #[test]
    fn file_stores_run_in_wal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&dir.path().join("swarmbot.sqlite")).unwrap();
        assert_eq!(store.journal_mode().unwrap(), "wal");
    }

    #[test]
    fn reopening_reruns_nothing_and_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("swarmbot.sqlite");
        let bot = Bot::new("lead", "Lead", "you lead", "fake", ConversationId::new());
        {
            let store = SqliteStore::open(&path).unwrap();
            store.insert_bot(&bot).unwrap();
            assert_eq!(store.user_version().unwrap(), MIGRATIONS.len() as i64);
        }
        // `open` on an existing database must not fail, duplicate migrations
        // or lose rows.
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(reopened.user_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(reopened.get_bot(&bot.id).unwrap(), Some(bot));
        let again = SqliteStore::open(&path).unwrap();
        assert_eq!(again.list_bots().unwrap().len(), 1);
    }

    #[test]
    fn in_memory_stores_are_ready_to_use() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(store.list_bots().unwrap(), vec![]);
        assert_eq!(store.epoch().unwrap(), 1);
        assert_eq!(store.settings().unwrap(), Settings::default());
    }
}
