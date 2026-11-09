//! The append-only JSONL transcript mirror.
//!
//! One file per conversation (`<dir>/<conversation id>.jsonl`), one compact
//! JSON object per line, written after the database committed — the database
//! stays authoritative, the mirror is the paper trail. Its reason to exist is
//! [`JsonlMirror::rebuild`]: when the SQLite file is lost (a bad upgrade, a
//! corrupted disk), replaying the transcripts into a fresh store recovers the
//! conversations, which is the part of the data an owner actually reads.
//!
//! Reads are deliberately forgiving: a crash mid-`write` can leave a torn last
//! line, and one bad line must never make a whole conversation unreadable —
//! it is skipped and counted in a warning instead.

use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};

use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::ConversationId;
use swarmbot_contracts::repo::Persistence;

use crate::io_err;

/// An append-only JSON Lines mirror of the transcripts under `dir`.
pub struct JsonlMirror {
    dir: PathBuf,
}

impl JsonlMirror {
    /// A mirror writing into `dir` (created on the first append).
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The directory this mirror writes into.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file holding one conversation's transcript.
    pub fn path_for(&self, conversation: &ConversationId) -> PathBuf {
        self.dir.join(format!("{conversation}.jsonl"))
    }

    /// Append one committed entry. Called after `upsert_entry` succeeded, so
    /// the file may hold several revisions of the same id — [`read`] returns
    /// them all in file order and [`rebuild`] replays them in order, which
    /// the revision guard turns into "the newest one wins".
    pub fn append(&self, conversation: &ConversationId, entry: &Entry) -> Result<(), DomainError> {
        std::fs::create_dir_all(&self.dir).map_err(io_err)?;
        let mut line = serde_json::to_string(entry).map_err(crate::json_err)?;
        line.push('\n');
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path_for(conversation))
            .map_err(io_err)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(line.as_bytes()).map_err(io_err)?;
        // Flush before returning: the mirror's promise is "on disk when the
        // call said ok", so a crash right after cannot lose the line.
        writer.flush().map_err(io_err)
    }

    /// Every parseable entry of one conversation, in file order. A missing
    /// file is an empty transcript; unparseable lines are skipped (warned).
    pub fn read(&self, conversation: &ConversationId) -> Result<Vec<Entry>, DomainError> {
        Self::read_file(&self.path_for(conversation))
    }

    fn read_file(path: &Path) -> Result<Vec<Entry>, DomainError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(io_err(err)),
        };
        let mut entries = Vec::new();
        let mut skipped = 0u64;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(line) {
                Ok(entry) => entries.push(entry),
                // A torn last line from a crash is expected, not fatal.
                Err(_) => skipped += 1,
            }
        }
        if skipped > 0 {
            tracing::warn!(
                path = %path.display(),
                skipped,
                "skipping unparseable JSONL line(s); a torn write during a crash is the usual cause"
            );
        }
        Ok(entries)
    }

    /// Replay every `.jsonl` file under `dir` into `store`, returning the
    /// number of entries written.
    ///
    /// This is the documented escape hatch when the database file is lost:
    /// transcripts are whole entries, so `upsert_entry` is all the rebuild
    /// needs. Entries already present with an equal or newer revision come
    /// back as conflicts and are skipped, which makes a rebuild idempotent —
    /// running it twice never duplicates or reverts anything.
    pub fn rebuild(dir: &Path, store: &dyn Persistence) -> Result<u64, DomainError> {
        let mut written = 0u64;
        for path in jsonl_files(dir)? {
            for entry in Self::read_file(&path)? {
                match store.upsert_entry(&entry) {
                    Ok(()) => written += 1,
                    Err(DomainError::Conflict { .. }) => {
                        tracing::debug!(entry = %entry.id, "rebuild skipped an entry the store already has");
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        Ok(written)
    }
}

/// The `.jsonl` files under `dir`, sorted by path so a rebuild replays
/// conversations in a deterministic order. A missing directory holds no
/// transcripts and is not an error.
fn jsonl_files(dir: &Path) -> Result<Vec<PathBuf>, DomainError> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io_err(err)),
    };
    let mut paths = Vec::new();
    for item in read {
        let path = item.map_err(io_err)?.path();
        if path.extension().is_some_and(|ext| ext == "jsonl") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}
