//! The append-only JSONL transcript mirror.
//!
//! One file per conversation (`<dir>/<conversation id>.jsonl`), one compact
//! JSON object per line, written after the database committed — the database
//! stays authoritative, the mirror is the paper trail. Its reason to exist is
//! [`JsonlMirror::rebuild`]: when the SQLite file is lost (a bad upgrade, a
//! corrupted disk), replaying the transcripts into a fresh store recovers the
//! conversations, which is the part of the data an owner actually reads.
//!
//! **A rebuild has to restore the conversation row, not only its entries.**
//! The entries alone are not enough: every new entry takes its sequence from
//! the conversation's `last_seq`, so a store rebuilt from entries with no
//! conversation row cannot be appended to at all. Each file therefore starts
//! with one [`MirrorHeader`] line carrying the whole conversation; a file
//! written by an older build has no header, and the rebuild reconstructs a
//! best-effort row from the entry authors instead of pretending the file is
//! intact (see [`RebuildReport::inferred`]).
//!
//! Reads are deliberately forgiving: a crash mid-`write` can leave a torn last
//! line, and one bad line must never make a whole conversation unreadable —
//! it is skipped and counted in a warning instead.

use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use swarmbot_contracts::conversation::{Conversation, ConversationKind};
use swarmbot_contracts::entry::{Author, Entry};
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, ConversationId};
use swarmbot_contracts::repo::Persistence;

use crate::io_err;

/// The first line of a transcript file: the conversation row itself.
///
/// It is tagged with a `record` key rather than reusing `Entry`'s `kind` tag,
/// so a header can never be mistaken for an entry and an entry can never be
/// mistaken for a header — the discriminator is the field name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct MirrorHeader {
    record: HeaderTag,
    conversation: Conversation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HeaderTag {
    Conversation,
}

/// What one file turned out to be.
enum Line {
    Header(Conversation),
    Entry(Entry),
    /// Unparseable, skipped and counted.
    Junk,
}

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

    /// Write the header line for `conversation` unless the file already starts
    /// with one that says the same thing.
    ///
    /// Idempotent, and a *changed* row (a group renamed, a member added) writes
    /// a new header rather than rewriting the file: the mirror is append-only
    /// so that a crash can only ever lose the last line, and the rebuild takes
    /// the newest header it sees.
    pub fn ensure_header(&self, conversation: &Conversation) -> Result<(), DomainError> {
        let path = self.path_for(&conversation.id);
        match Self::first_line(&path)? {
            Some(Line::Header(existing)) if existing == *conversation => return Ok(()),
            // A header that no longer matches, or a file that has none: write
            // one now, before the next entry, so the newest header wins.
            _ => {}
        }
        let header = MirrorHeader {
            record: HeaderTag::Conversation,
            conversation: conversation.clone(),
        };
        self.append_line(
            &path,
            &serde_json::to_string(&header).map_err(crate::json_err)?,
        )
    }

    /// Append one committed entry. Called after `upsert_entry` succeeded, so
    /// the file may hold several revisions of the same id — [`read`] returns
    /// them all in file order and [`rebuild`] replays them in order, which
    /// the revision guard turns into "the newest one wins".
    pub fn append(&self, conversation: &ConversationId, entry: &Entry) -> Result<(), DomainError> {
        let path = self.path_for(conversation);
        self.append_line(
            &path,
            &serde_json::to_string(entry).map_err(crate::json_err)?,
        )
    }

    /// Append one already-serialized JSON object as a line.
    fn append_line(&self, path: &Path, json: &str) -> Result<(), DomainError> {
        std::fs::create_dir_all(&self.dir).map_err(io_err)?;
        let mut line = String::with_capacity(json.len() + 1);
        line.push_str(json);
        line.push('\n');
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(io_err)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(line.as_bytes()).map_err(io_err)?;
        // Flush before returning: the mirror's promise is "handed to the OS
        // when the call said ok", so a crash right after cannot lose the line
        // from the process's own buffers. (It is a flush, not an `fsync`: the
        // kernel may still hold it, and the database is authoritative anyway.)
        writer.flush().map_err(io_err)
    }

    /// Every parseable entry of one conversation, in file order. A missing
    /// file is an empty transcript; unparseable lines are skipped (warned).
    /// A header line is not an entry and is not returned.
    pub fn read(&self, conversation: &ConversationId) -> Result<Vec<Entry>, DomainError> {
        Ok(Self::read_file(&self.path_for(conversation))?
            .into_iter()
            .filter_map(|line| match line {
                Line::Entry(entry) => Some(entry),
                Line::Header(_) | Line::Junk => None,
            })
            .collect())
    }

    /// The first line of a file, without reading the rest.
    fn first_line(path: &Path) -> Result<Option<Line>, DomainError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_err(err)),
        };
        let Some(line) = text.lines().find(|line| !line.trim().is_empty()) else {
            return Ok(None);
        };
        Ok(Some(Self::parse_line(line)))
    }

    /// One line, classified. The header is tried first because its
    /// discriminator (`record`) cannot appear on an entry.
    fn parse_line(line: &str) -> Line {
        if let Ok(header) = serde_json::from_str::<MirrorHeader>(line) {
            return Line::Header(header.conversation);
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(entry) => Line::Entry(entry),
            Err(_) => Line::Junk,
        }
    }

    fn read_file(path: &Path) -> Result<Vec<Line>, DomainError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(io_err(err)),
        };
        let mut lines = Vec::new();
        let mut skipped = 0u64;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let parsed = Self::parse_line(line);
            if matches!(parsed, Line::Junk) {
                // A torn last line from a crash is expected, not fatal.
                skipped += 1;
            }
            lines.push(parsed);
        }
        if skipped > 0 {
            tracing::warn!(
                path = %path.display(),
                skipped,
                "skipping unparseable JSONL line(s); a torn write during a crash is the usual cause"
            );
        }
        Ok(lines)
    }

    /// Replay every `.jsonl` file under `dir` into `store`.
    ///
    /// This is the documented escape hatch when the database file is lost.
    /// For each file it restores, in order:
    ///
    /// 1. the **conversation row**, from the newest header line — without it
    ///    the entries could be read but never appended to, because every new
    ///    entry takes its sequence from the row's `last_seq`;
    /// 2. its **entries**, replayed through `upsert_entry` so the revision
    ///    guard decides what wins (an equal or newer revision is a conflict and
    ///    is skipped, which makes a rebuild idempotent);
    /// 3. `last_seq`, set to the highest sequence actually restored, so the
    ///    next append continues where the transcript stopped.
    ///
    /// A file with no header (written by a build before the header existed) is
    /// **inferred**: a `Direct` conversation when one bot authored everything,
    /// a `Group` when several did. That is a guess, and it is reported as
    /// [`RebuildReport::inferred`] rather than hidden — an inferred group loses
    /// its lead and member order, and an owner has to fix it by hand.
    pub fn rebuild(dir: &Path, store: &dyn Persistence) -> Result<RebuildReport, DomainError> {
        let mut report = RebuildReport::default();
        for path in jsonl_files(dir)? {
            let lines = Self::read_file(&path)?;
            let mut entries = Vec::new();
            let mut header: Option<Conversation> = None;
            for line in lines {
                match line {
                    Line::Header(conversation) => header = Some(conversation),
                    Line::Entry(entry) => entries.push(entry),
                    Line::Junk => {}
                }
            }

            // The file name is the conversation id, so an entry that names a
            // different conversation is a file that was moved or hand-edited.
            let from_header = header.is_some();
            let conversation = match header {
                Some(conversation) => conversation,
                None => {
                    let Some(inferred) = infer_conversation(&path, &entries) else {
                        continue;
                    };
                    inferred
                }
            };
            if !from_header {
                report.inferred += 1;
            }

            // Restore the row before its entries: `upsert_entry` may not need
            // it, but the next `next_seq` will.
            let mut row = conversation.clone();
            match store.insert_conversation(&row) {
                Ok(()) => {}
                Err(DomainError::Conflict { .. }) => {
                    // The store already has it (a partial rebuild): keep the
                    // existing row's identity and let the entries decide.
                    if let Some(existing) = store.get_conversation(&row.id)? {
                        row = existing;
                    }
                }
                Err(err) => return Err(err),
            }
            report.conversations += 1;

            for entry in &entries {
                match store.upsert_entry(entry) {
                    Ok(()) => {
                        report.entries += 1;
                        row.last_seq = row.last_seq.max(entry.seq);
                    }
                    Err(DomainError::Conflict { .. }) => {
                        tracing::debug!(entry = %entry.id, "rebuild skipped an entry the store already has");
                        row.last_seq = row.last_seq.max(entry.seq);
                    }
                    Err(err) => return Err(err),
                }
            }

            // Write the counter back: this is the line that makes a rebuilt
            // store appendable again.
            store.update_conversation(&row)?;
        }
        Ok(report)
    }
}

/// Reconstruct a conversation row from a headerless file's entries.
///
/// Best effort by construction: the file records who wrote what, not what the
/// conversation *was*. One author means a DM (a group of one is not a group);
/// several mean a group whose lead is the first bot that spoke. A file with no
/// bot author at all yields a DM with the id parsed from the file name and no
/// participants, which the rebuild reports as inferred so the owner knows.
fn infer_conversation(path: &Path, entries: &[Entry]) -> Option<Conversation> {
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| ConversationId::parse(stem).ok())?;

    let mut authors: Vec<BotId> = Vec::new();
    for entry in entries {
        if let Author::Bot { id } = &entry.author
            && !authors.contains(id)
        {
            authors.push(id.clone());
        }
    }

    let kind = match authors.len() {
        0 | 1 => ConversationKind::Direct {
            bot_id: authors.first().cloned().unwrap_or_else(BotId::new),
        },
        _ => ConversationKind::Group {
            lead: authors[0].clone(),
            members: authors,
        },
    };
    let (created_at, updated_at) = match (entries.first(), entries.last()) {
        (Some(first), Some(last)) => (first.created_at, last.updated_at),
        _ => (chrono::Utc::now(), chrono::Utc::now()),
    };
    Some(Conversation {
        id,
        kind,
        title: None,
        created_at,
        updated_at,
        last_seq: entries.iter().map(|entry| entry.seq).max().unwrap_or(0),
        archived: false,
    })
}

/// What a rebuild restored, and how much of it is a guess.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RebuildReport {
    /// Conversation rows written.
    pub conversations: u64,
    /// Entries written (a conflict is not counted: it was already there).
    pub entries: u64,
    /// Files that had no header, so their conversation row was inferred from
    /// the entry authors. Non-zero means the owner should check those
    /// conversations by hand.
    pub inferred: u64,
}

impl RebuildReport {
    pub fn is_empty(&self) -> bool {
        self.conversations == 0 && self.entries == 0 && self.inferred == 0
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
