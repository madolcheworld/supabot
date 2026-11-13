//! SwarmBot storage: one port, two indistinguishable doubles, and a paper
//! trail.
//!
//! The ports live in [`swarmbot_contracts::repo`]; this crate implements them
//! twice — [`SqliteStore`] (durable, WAL, migrations) and [`MemoryStore`]
//! (tests) — plus [`JsonlMirror`], the append-only transcript mirror that can
//! rebuild a store when the database file is lost.
//!
//! [`ThreadedStore`] is the same durable store behind dedicated threads: the
//! port is synchronous, so a caller cannot stop waiting for its own answer,
//! but with one writer lane and WAL reader lanes a contended write no longer
//! serialises *reads* — see the module docs for exactly what that does and
//! does not fix.
//!
//! Both stores must be **observably identical** through the port: same
//! ordering, same conflict rules, same counts. Two mechanisms keep them so:
//!
//! * [`run_contract_suite`] — one body of assertions run against both
//!   implementations (see [`contract`]);
//! * the small shared helpers below — one timestamp format and one set of
//!   error constructors, so the two stores cannot drift on the details the
//!   suite compares.
//!
//! Nothing here ever panics: every failure is a
//! [`DomainError`](swarmbot_contracts::DomainError).

pub mod backup;
pub mod contract;
pub mod memory;
pub mod mirror;
pub mod sqlite;
pub mod threaded;

pub use backup::{BackupError, BackupReport, Manifest, VerifyReport, backup, verify};
pub use contract::run_contract_suite;
pub use memory::MemoryStore;
pub use mirror::JsonlMirror;
pub use sqlite::SqliteStore;
pub use threaded::ThreadedStore;

use chrono::{DateTime, SecondsFormat, Utc};
use swarmbot_contracts::Seq;
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{ConversationId, EntryId};

/// The one timestamp format both stores write and compare.
///
/// Fixed width, UTC (`Z`), microsecond precision: lexicographic order equals
/// chronological order in SQLite `ORDER BY`, and sub-microsecond differences
/// collapse the *same way* in both stores — so [`MemoryStore`] sorting by this
/// string can never disagree with SQLite sorting by the stored column.
pub(crate) fn ts(value: &DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Stored bytes that no longer parse mean a corrupt store, not a domain fault.
pub(crate) fn json_err(err: serde_json::Error) -> DomainError {
    DomainError::internal(format!("stored data is corrupt: {err}"))
}

/// I/O failures (a missing data dir, a full disk) are internal errors: the
/// caller cannot fix them by changing the request.
pub(crate) fn io_err(err: std::io::Error) -> DomainError {
    DomainError::internal(err.to_string())
}

/// Both stores report the *same* message for the same violation, so a test
/// can pin behaviour without knowing which double it is holding.
pub(crate) fn duplicate_id(what: &str, id: &dyn std::fmt::Display) -> DomainError {
    DomainError::conflict(format!("{what} {id} already exists"))
}

/// `bot_by_name` must answer with exactly one bot, so handles are unique.
pub(crate) fn duplicate_bot_name(name: &str) -> DomainError {
    DomainError::conflict(format!("a bot named `{name}` already exists"))
}

/// The whole-entry-upsert invariant: a write whose revision is not greater
/// than the stored one is a conflict, never a silent overwrite.
pub(crate) fn stale_revision(entry: &Entry, stored: u32) -> DomainError {
    DomainError::conflict(format!(
        "entry {} revision {} is not newer than the stored revision {stored}",
        entry.id, entry.revision
    ))
}

/// Sequences are per conversation and handed out by the conversation row, so
/// a second entry claiming the same `(conversation, seq)` is a bug upstream —
/// reported as a conflict, never stored.
pub(crate) fn seq_taken(conversation: &ConversationId, seq: Seq, id: &EntryId) -> DomainError {
    DomainError::conflict(format!(
        "conversation {conversation} already holds an entry at seq {seq} (tried to write {id})"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_sort_lexicographically_like_time() {
        let base = Utc::now();
        let earlier = ts(&(base - chrono::Duration::microseconds(1)));
        let later = ts(&base);
        assert!(earlier < later, "{earlier} should sort before {later}");
        // Fixed width: no fractional-part length may change the order.
        assert_eq!(earlier.len(), later.len());
        assert!(later.ends_with('Z'));
    }
}
