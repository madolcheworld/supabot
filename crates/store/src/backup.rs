//! Backup and restore-check of a whole data directory.
//!
//! Why this module exists: `<data>/swarmbot.sqlite` and `<data>/transcripts/`
//! are the only copy of an owner's bots, conversations and history, and until
//! now the only way to move or protect them was `cp`. Copying the database file
//! while the brain runs is **not** a backup: committed transactions may still
//! live in `swarmbot.sqlite-wal`, so the copy is either stale or torn, and a
//! torn SQLite file still opens and reports plausible-looking data. The
//! snapshot therefore goes through `VACUUM INTO`, which runs inside SQLite's
//! own locking and writes one self-contained database file containing every
//! committed transaction — including the ones still in the WAL.
//!
//! What a backup is:
//!
//! ```text
//! <out_dir>/swarmbot.sqlite     the snapshot (VACUUM INTO)
//! <out_dir>/transcripts/        the JSONL mirror, copied as-is
//! <out_dir>/MANIFEST.json       what the snapshot claims to contain
//! ```
//!
//! **A backup that never got its manifest is not a backup.** The manifest is
//! written last, through a temporary file and a rename, so a crash or a mid-way
//! failure leaves a directory that [`verify`] refuses instead of a directory
//! that merely looks complete. [`verify`] then re-derives every number from the
//! snapshot and the transcript tree instead of believing the manifest: a
//! manifest that lies is a detected failure, not a green check.
//!
//! This module deliberately does **not** use [`SqliteStore`](crate::SqliteStore)
//! or [`ThreadedStore`](crate::ThreadedStore). A backup is a file-level
//! operation on a data dir that may belong to another process (or a dead one),
//! and `ThreadedStore` owns lanes that must not be borrowed for it.
//!
//! Restoring is **not** in this module: replacing a live data dir has its own
//! safety rules (a running brain, a stale `-wal`, a way back), so it lives in
//! [`crate::restore`]. This module's half of that contract is only that a
//! snapshot is a *complete and verifiable* database, which is what
//! [`SqliteStore::open`](crate::SqliteStore::open) proving it restorable adds to.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The database file the brain keeps in its data dir.
pub const DATABASE_FILE: &str = "swarmbot.sqlite";

/// The transcript mirror directory (`JsonlMirror`), relative to the data dir.
pub const TRANSCRIPTS_DIR: &str = "transcripts";

/// The manifest written last; its absence marks an incomplete backup.
pub const MANIFEST_FILE: &str = "MANIFEST.json";

/// Bumped when the manifest shape changes incompatibly. A backup from a newer
/// build is refused rather than half-understood.
pub const MANIFEST_VERSION: u32 = 1;

/// The snapshot is written under the same name as the live database, so
/// restoring it is "put this file back as `<data>/swarmbot.sqlite`".
/// Public so `restore` names the same file instead of re-spelling it.
pub const SNAPSHOT_FILE: &str = DATABASE_FILE;

/// Temporary manifest name; the rename onto [`MANIFEST_FILE`] is the moment a
/// backup becomes a backup.
const MANIFEST_TMP: &str = ".MANIFEST.json.tmp";

/// How long to wait for the live brain's write lock. Five seconds matches
/// [`SqliteStore`](crate::SqliteStore)'s own `busy_timeout`: long enough for a
/// committing transaction, short enough that a wedged process is reported.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The tables a restore actually needs. Counts are recorded in the manifest and
/// re-checked from the snapshot by [`verify`]; a table an older schema never
/// had is simply absent from both.
const COUNTED_TABLES: &[&str] = &[
    "meta",
    "bots",
    "conversations",
    "conversation_members",
    "entries",
    "turns",
    "deliveries",
    "tasks",
    "task_steps",
    "requests",
    "approval_rules",
    "nodes",
    "node_keys",
    "pairings",
    "bot_sessions",
    "tool_calls",
    "settings",
    "events_log",
    "routines",
    "routine_runs",
];

/// The tables a restore re-counts, as a slice a caller can iterate.
///
/// Public because `restore` verifies a *copied* snapshot against the same list the
/// manifest was built from: two lists would drift, and the drift would show up as
/// a restore that "succeeded" with fewer rows than the backup held.
pub fn counted_tables() -> &'static [&'static str] {
    COUNTED_TABLES
}

/// What [`backup`] wrote, and the manifest it wrote last.
#[derive(Clone, Debug, PartialEq)]
pub struct BackupReport {
    /// The directory the backup was written into.
    pub out_dir: PathBuf,
    /// The snapshot database (`out_dir/swarmbot.sqlite`).
    pub snapshot: PathBuf,
    /// The manifest (`out_dir/MANIFEST.json`) — proof the backup finished.
    pub manifest_path: PathBuf,
    /// Exactly what the manifest says; also on disk.
    pub manifest: Manifest,
}

/// `MANIFEST.json`: what one backup contains, as it was written.
///
/// Every count here is a claim [`verify`] re-checks against the snapshot, so a
/// hand-edited manifest is a detected failure rather than an accepted backup.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Manifest shape; see [`MANIFEST_VERSION`].
    pub manifest_version: u32,
    /// When the snapshot was taken (UTC, fixed width — the store's `ts` format).
    pub created_at: String,
    /// The `swarmbot-store` version that wrote the backup.
    pub crate_version: String,
    /// `PRAGMA user_version` of the snapshot (how many migrations ran).
    pub schema_version: i64,
    /// The snapshot's file name inside the backup directory.
    pub database: String,
    /// Snapshot size in bytes.
    pub database_bytes: u64,
    /// The transcript directory's name inside the backup directory.
    pub transcripts: String,
    /// Regular files copied from the transcript mirror.
    pub transcript_files: u64,
    /// Their total size in bytes.
    pub transcript_bytes: u64,
    /// Row count per counted table, read from the snapshot itself.
    pub tables: BTreeMap<String, i64>,
}

/// What [`verify`] observed, re-derived from the backup (never from the
/// manifest alone).
#[derive(Clone, Debug, PartialEq)]
pub struct VerifyReport {
    /// The directory that was verified.
    pub out_dir: PathBuf,
    /// The manifest as read from disk.
    pub manifest: Manifest,
    /// What `PRAGMA integrity_check` said; `"ok"` on success.
    pub integrity_check: String,
    /// Row counts read from the snapshot, not copied from the manifest.
    pub tables: BTreeMap<String, i64>,
    /// Transcript files found in the backup.
    pub transcript_files: u64,
}

/// Everything that can go wrong while backing up or verifying.
///
/// Every message names the path and what failed: the caller is usually a CLI
/// command whose whole job is to tell the owner which file is wrong.
#[derive(Debug, Error)]
pub enum BackupError {
    #[error("data directory {path} does not exist; there is nothing to back up")]
    MissingDataDir { path: PathBuf },
    #[error("data directory {path} is not a directory")]
    DataDirNotADirectory { path: PathBuf },
    #[error("no database at {path}; a data dir must have booted once before it can be backed up")]
    MissingDatabase { path: PathBuf },
    #[error("backup target {path} is not a directory")]
    TargetNotADirectory { path: PathBuf },
    #[error(
        "backup target {path} is not empty; refusing to mix a new backup with what is already there"
    )]
    TargetNotEmpty { path: PathBuf },
    #[error("could not create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not list {path}: {source}")]
    ListDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not read {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    WriteFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not copy transcript {from} to {to}: {source}")]
    CopyTranscript {
        from: PathBuf,
        to: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot back up transcript entry {path}: {detail}")]
    TranscriptEntry { path: PathBuf, detail: String },
    #[error("could not open the database at {path}: {detail}")]
    OpenDatabase { path: PathBuf, detail: String },
    #[error("VACUUM INTO from {from} to {to} failed: {detail}")]
    Snapshot {
        from: PathBuf,
        to: PathBuf,
        detail: String,
    },
    #[error("could not query the snapshot at {path}: {detail}")]
    QuerySnapshot { path: PathBuf, detail: String },
    #[error("snapshot {path} has no `bots` table; it is not a SwarmBot database")]
    NotASwarmbotDatabase { path: PathBuf },
    #[error("path {path} is not valid UTF-8, so SQLite cannot open it")]
    NonUtf8Path { path: PathBuf },
    #[error("could not encode the backup manifest for {path}: {detail}")]
    EncodeManifest { path: PathBuf, detail: String },
    #[error("backup manifest {path} is missing or unreadable: {source}")]
    UnreadableManifest {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("backup manifest {path} is not valid JSON for this build: {source}")]
    InvalidManifest {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "backup manifest {path} declares format version {found}, but this build understands {expected}"
    )]
    UnsupportedManifest {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
    #[error("backup manifest {path} names database {found:?}, expected {expected:?}")]
    ManifestDatabaseName {
        path: PathBuf,
        found: String,
        expected: String,
    },
    #[error("backup manifest {path} names transcript directory {found:?}, expected {expected:?}")]
    ManifestTranscriptsName {
        path: PathBuf,
        found: String,
        expected: String,
    },
    #[error("backup directory {path} does not exist")]
    MissingBackup { path: PathBuf },
    #[error("backup {path} has no snapshot at {snapshot}")]
    MissingSnapshot { path: PathBuf, snapshot: PathBuf },
    #[error("snapshot {path} failed integrity_check: {detail}")]
    CorruptSnapshot { path: PathBuf, detail: String },
    #[error("snapshot {path} reports schema version {found}, but the manifest says {expected}")]
    SchemaMismatch {
        path: PathBuf,
        found: i64,
        expected: i64,
    },
    #[error("snapshot {path} is {found} bytes, but the manifest says {expected}")]
    DatabaseSizeMismatch {
        path: PathBuf,
        found: u64,
        expected: u64,
    },
    #[error("snapshot {path} is missing table `{table}` that the manifest claims")]
    MissingTable { path: PathBuf, table: String },
    #[error("snapshot {path} holds table `{table}`, but the manifest omits it")]
    UnlistedTable { path: PathBuf, table: String },
    #[error("snapshot {path} holds {found} rows in `{table}`, but the manifest says {expected}")]
    RowCountMismatch {
        path: PathBuf,
        table: String,
        found: i64,
        expected: i64,
    },
    #[error("transcript tree {dir} holds {found} files, but the manifest says {expected}")]
    TranscriptCountMismatch {
        dir: PathBuf,
        found: u64,
        expected: u64,
    },
    #[error("transcript tree {dir} holds {found} bytes, but the manifest says {expected}")]
    TranscriptBytesMismatch {
        dir: PathBuf,
        found: u64,
        expected: u64,
    },
}

/// Snapshot `data_dir` into `out_dir`.
///
/// `out_dir` must be absent or empty. A non-empty target is refused rather than
/// overwritten: a directory holding two snapshots is a directory nobody can
/// verify, and silently replacing an owner's older backup is data loss. A
/// failure leaves the target without a manifest, so [`verify`] will not accept
/// the half-finished result.
///
/// The caller may keep using the data dir: the snapshot goes through SQLite's
/// locking, so it is consistent even while a live store is writing (that is the
/// case `cp` gets wrong, and the test suite pins it).
pub fn backup(data_dir: &Path, out_dir: &Path) -> Result<BackupReport, BackupError> {
    prepare_target(data_dir, out_dir)?;
    match write_backup(data_dir, out_dir) {
        Ok(report) => Ok(report),
        Err(error) => {
            discard_partial(out_dir);
            Err(error)
        }
    }
}

/// Verify that `out_dir` holds a finished, internally consistent backup.
///
/// Reads the manifest (which must exist), opens the snapshot, runs
/// `PRAGMA integrity_check`, and then **re-counts** every table and every
/// transcript byte from the backup itself. A manifest whose numbers disagree
/// with the snapshot is an error, not a warning: that is exactly what a
/// half-restored or tampered backup looks like.
pub fn verify(out_dir: &Path) -> Result<VerifyReport, BackupError> {
    if !out_dir.is_dir() {
        return Err(BackupError::MissingBackup {
            path: out_dir.to_path_buf(),
        });
    }

    let manifest_path = out_dir.join(MANIFEST_FILE);
    let manifest = read_manifest(&manifest_path)?;
    if manifest.manifest_version != MANIFEST_VERSION {
        return Err(BackupError::UnsupportedManifest {
            path: manifest_path,
            found: manifest.manifest_version,
            expected: MANIFEST_VERSION,
        });
    }
    // The manifest names the files; a name that is not the one this layout
    // writes is either a tampered manifest or a backup from another tool.
    if manifest.database != SNAPSHOT_FILE {
        return Err(BackupError::ManifestDatabaseName {
            path: manifest_path,
            found: manifest.database,
            expected: SNAPSHOT_FILE.to_string(),
        });
    }
    if manifest.transcripts != TRANSCRIPTS_DIR {
        return Err(BackupError::ManifestTranscriptsName {
            path: manifest_path,
            found: manifest.transcripts,
            expected: TRANSCRIPTS_DIR.to_string(),
        });
    }

    let snapshot = out_dir.join(SNAPSHOT_FILE);
    if !snapshot.is_file() {
        return Err(BackupError::MissingSnapshot {
            path: out_dir.to_path_buf(),
            snapshot,
        });
    }

    let conn = open_readonly(&snapshot)?;
    let integrity_check: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|source| query_err(&snapshot, "PRAGMA integrity_check", &source))?;
    if integrity_check != "ok" {
        return Err(BackupError::CorruptSnapshot {
            path: snapshot,
            detail: integrity_check,
        });
    }

    let schema_version = read_user_version(&conn, &snapshot)?;
    if schema_version != manifest.schema_version {
        return Err(BackupError::SchemaMismatch {
            path: snapshot,
            found: schema_version,
            expected: manifest.schema_version,
        });
    }

    let database_bytes = std::fs::metadata(&snapshot)
        .map_err(|source| BackupError::ReadFile {
            path: snapshot.clone(),
            source,
        })?
        .len();
    if database_bytes != manifest.database_bytes {
        return Err(BackupError::DatabaseSizeMismatch {
            path: snapshot,
            found: database_bytes,
            expected: manifest.database_bytes,
        });
    }

    // Count from the snapshot, table by table, and require the manifest's table
    // set to match the snapshot's exactly: a manifest that drops a table is as
    // dishonest as one that inflates a count.
    let present = table_names(&conn, &snapshot)?;
    let mut tables = BTreeMap::new();
    for (table, expected) in &manifest.tables {
        if !present.contains(table) {
            return Err(BackupError::MissingTable {
                path: snapshot,
                table: table.clone(),
            });
        }
        let found = count_rows(&conn, &snapshot, table)?;
        if found != *expected {
            return Err(BackupError::RowCountMismatch {
                path: snapshot,
                table: table.clone(),
                found,
                expected: *expected,
            });
        }
        tables.insert(table.clone(), found);
    }
    for table in COUNTED_TABLES {
        if present.contains(*table) && !manifest.tables.contains_key(*table) {
            return Err(BackupError::UnlistedTable {
                path: snapshot,
                table: (*table).to_string(),
            });
        }
    }

    let transcripts_dir = out_dir.join(TRANSCRIPTS_DIR);
    let transcripts = count_transcript_tree(&transcripts_dir)?;
    if transcripts.files != manifest.transcript_files {
        return Err(BackupError::TranscriptCountMismatch {
            dir: transcripts_dir,
            found: transcripts.files,
            expected: manifest.transcript_files,
        });
    }
    if transcripts.bytes != manifest.transcript_bytes {
        return Err(BackupError::TranscriptBytesMismatch {
            dir: transcripts_dir,
            found: transcripts.bytes,
            expected: manifest.transcript_bytes,
        });
    }

    Ok(VerifyReport {
        out_dir: out_dir.to_path_buf(),
        manifest,
        integrity_check,
        tables,
        transcript_files: transcripts.files,
    })
}

/// Validate both ends and make sure `out_dir` is ours to write.
fn prepare_target(data_dir: &Path, out_dir: &Path) -> Result<(), BackupError> {
    if !data_dir.exists() {
        return Err(BackupError::MissingDataDir {
            path: data_dir.to_path_buf(),
        });
    }
    if !data_dir.is_dir() {
        return Err(BackupError::DataDirNotADirectory {
            path: data_dir.to_path_buf(),
        });
    }
    let database = data_dir.join(DATABASE_FILE);
    if !database.is_file() {
        return Err(BackupError::MissingDatabase { path: database });
    }

    if out_dir.exists() && !out_dir.is_dir() {
        return Err(BackupError::TargetNotADirectory {
            path: out_dir.to_path_buf(),
        });
    }
    if out_dir.is_dir() {
        let mut entries = std::fs::read_dir(out_dir).map_err(|source| BackupError::ListDir {
            path: out_dir.to_path_buf(),
            source,
        })?;
        // `read_dir` may yield an error as its first item; that still means the
        // directory is not empty, and the error is what the caller needs to see.
        if let Some(entry) = entries.next() {
            entry.map_err(|source| BackupError::ListDir {
                path: out_dir.to_path_buf(),
                source,
            })?;
            return Err(BackupError::TargetNotEmpty {
                path: out_dir.to_path_buf(),
            });
        }
    }
    std::fs::create_dir_all(out_dir).map_err(|source| BackupError::CreateDir {
        path: out_dir.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Do the work; the caller cleans up after a failure.
fn write_backup(data_dir: &Path, out_dir: &Path) -> Result<BackupReport, BackupError> {
    let source = data_dir.join(DATABASE_FILE);
    let snapshot = out_dir.join(SNAPSHOT_FILE);

    // The source connection is dropped before the snapshot is opened: holding
    // two handles on the same live database is fine, but there is no reason to.
    {
        let conn = open_source(&source)?;
        vacuum_into(&conn, &source, &snapshot)?;
    }

    let (schema_version, tables) = {
        let conn = open_readonly(&snapshot)?;
        let schema_version = read_user_version(&conn, &snapshot)?;
        let tables = count_tables(&conn, &snapshot)?;
        (schema_version, tables)
    };
    if !tables.contains_key("bots") {
        return Err(BackupError::NotASwarmbotDatabase { path: snapshot });
    }

    let transcripts = copy_transcripts(data_dir, out_dir)?;
    let database_bytes = std::fs::metadata(&snapshot)
        .map_err(|source| BackupError::ReadFile {
            path: snapshot.clone(),
            source,
        })?
        .len();

    let manifest = Manifest {
        manifest_version: MANIFEST_VERSION,
        created_at: crate::ts(&chrono::Utc::now()),
        crate_version: env!("CARGO_PKG_VERSION").to_string(),
        schema_version,
        database: DATABASE_FILE.to_string(),
        database_bytes,
        transcripts: TRANSCRIPTS_DIR.to_string(),
        transcript_files: transcripts.files,
        transcript_bytes: transcripts.bytes,
        tables,
    };

    // Last, and atomically: before this rename the directory is not a backup.
    let manifest_path = out_dir.join(MANIFEST_FILE);
    write_manifest(&manifest_path, &manifest)?;

    Ok(BackupReport {
        out_dir: out_dir.to_path_buf(),
        snapshot,
        manifest_path,
        manifest,
    })
}

/// Open the *live* database for reading it into a snapshot.
///
/// Read-write, not read-only: a WAL database whose `-wal` still needs recovering
/// cannot be opened read-only, and `VACUUM INTO` only reads — it never writes
/// the source. `SQLITE_OPEN_READ_WRITE` without `CREATE` means a missing file is
/// an error instead of a brand-new empty database.
fn open_source(path: &Path) -> Result<Connection, BackupError> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(|source| {
            BackupError::OpenDatabase {
                path: path.to_path_buf(),
                detail: source.to_string(),
            }
        })?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|source| BackupError::OpenDatabase {
            path: path.to_path_buf(),
            detail: source.to_string(),
        })?;
    Ok(conn)
}

/// Open a snapshot without any chance of writing to it.
fn open_readonly(path: &Path) -> Result<Connection, BackupError> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|source| {
            BackupError::OpenDatabase {
                path: path.to_path_buf(),
                detail: source.to_string(),
            }
        })?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|source| BackupError::OpenDatabase {
            path: path.to_path_buf(),
            detail: source.to_string(),
        })?;
    Ok(conn)
}

/// The snapshot itself.
///
/// `VACUUM INTO` runs under SQLite's locking, sees every committed transaction
/// (WAL included), and writes one self-contained, checkpointed file. `cp` of
/// the main file would miss the WAL pages and produce a file that opens but has
/// lost the newest data — the worst kind of corrupt.
fn vacuum_into(conn: &Connection, from: &Path, to: &Path) -> Result<(), BackupError> {
    let destination = to.to_str().ok_or_else(|| BackupError::NonUtf8Path {
        path: to.to_path_buf(),
    })?;
    conn.execute("VACUUM INTO ?1", params![destination])
        .map_err(|source| BackupError::Snapshot {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            detail: source.to_string(),
        })?;
    Ok(())
}

/// Remove what a failed backup left behind.
///
/// Best effort: the manifest is written last, so even if this cleanup itself
/// fails, the directory still has no manifest and [`verify`] refuses it.
fn discard_partial(out_dir: &Path) {
    for path in [
        out_dir.join(MANIFEST_TMP),
        out_dir.join(MANIFEST_FILE),
        out_dir.join(SNAPSHOT_FILE),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not remove a partial backup file");
            }
        }
    }
    let transcripts = out_dir.join(TRANSCRIPTS_DIR);
    match std::fs::remove_dir_all(&transcripts) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %transcripts.display(), %error, "could not remove partial transcripts");
        }
    }
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<(), BackupError> {
    let json =
        serde_json::to_vec_pretty(manifest).map_err(|source| BackupError::EncodeManifest {
            path: path.to_path_buf(),
            detail: source.to_string(),
        })?;
    let tmp = path.with_file_name(MANIFEST_TMP);
    std::fs::write(&tmp, json).map_err(|source| BackupError::WriteFile {
        path: tmp.clone(),
        source,
    })?;
    // A rename within one directory is atomic: a reader sees either no
    // manifest or the complete one, never half a JSON document.
    std::fs::rename(&tmp, path).map_err(|source| BackupError::WriteFile {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn read_manifest(path: &Path) -> Result<Manifest, BackupError> {
    let bytes = std::fs::read(path).map_err(|source| BackupError::UnreadableManifest {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| BackupError::InvalidManifest {
        path: path.to_path_buf(),
        source,
    })
}

fn query_err(path: &Path, what: &str, source: &rusqlite::Error) -> BackupError {
    BackupError::QuerySnapshot {
        path: path.to_path_buf(),
        detail: format!("{what}: {source}"),
    }
}

fn read_user_version(conn: &Connection, path: &Path) -> Result<i64, BackupError> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|source| query_err(path, "PRAGMA user_version", &source))
}

/// The user tables actually present in a database, from `sqlite_master`.
fn table_names(conn: &Connection, path: &Path) -> Result<HashSet<String>, BackupError> {
    let mut statement = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .map_err(|source| query_err(path, "listing sqlite_master", &source))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| query_err(path, "listing sqlite_master", &source))?;
    let mut names = HashSet::new();
    for row in rows {
        names.insert(row.map_err(|source| query_err(path, "reading sqlite_master", &source))?);
    }
    Ok(names)
}

/// Counts for the counted tables the snapshot actually has.
fn count_tables(conn: &Connection, path: &Path) -> Result<BTreeMap<String, i64>, BackupError> {
    let present = table_names(conn, path)?;
    let mut tables = BTreeMap::new();
    for table in COUNTED_TABLES {
        if present.contains(*table) {
            tables.insert((*table).to_string(), count_rows(conn, path, table)?);
        }
    }
    Ok(tables)
}

/// `table` comes from [`COUNTED_TABLES`] or from `sqlite_master`, never from a
/// caller — and the identifier is quoted anyway so a table named with a quote
/// cannot turn into SQL.
fn count_rows(conn: &Connection, path: &Path, table: &str) -> Result<i64, BackupError> {
    let quoted = table.replace('"', "\"\"");
    conn.query_row(&format!("SELECT COUNT(*) FROM \"{quoted}\""), [], |row| {
        row.get(0)
    })
    .map_err(|source| query_err(path, &format!("counting `{table}`"), &source))
}

/// Files and bytes copied out of a transcript tree.
#[derive(Default)]
struct TranscriptCopy {
    files: u64,
    bytes: u64,
}

/// Copy `<data>/transcripts` into `<out>/transcripts`, if the data dir has one.
fn copy_transcripts(data_dir: &Path, out_dir: &Path) -> Result<TranscriptCopy, BackupError> {
    let from = data_dir.join(TRANSCRIPTS_DIR);
    let to = out_dir.join(TRANSCRIPTS_DIR);
    let mut copy = TranscriptCopy::default();

    let metadata = match std::fs::symlink_metadata(&from) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(copy),
        Err(source) => {
            return Err(BackupError::ReadFile { path: from, source });
        }
    };
    if !metadata.is_dir() {
        return Err(BackupError::TranscriptEntry {
            path: from,
            detail: "the transcript path is not a plain directory".to_string(),
        });
    }
    copy_dir(&from, &to, &mut copy)?;
    Ok(copy)
}

/// Recursive copy. Transcripts are one file per conversation in practice, so
/// the recursion is shallow; symlinks are refused rather than followed, because
/// following one can escape the data dir or loop forever.
fn copy_dir(from: &Path, to: &Path, copy: &mut TranscriptCopy) -> Result<(), BackupError> {
    std::fs::create_dir_all(to).map_err(|source| BackupError::CreateDir {
        path: to.to_path_buf(),
        source,
    })?;
    let entries = std::fs::read_dir(from).map_err(|source| BackupError::ListDir {
        path: from.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| BackupError::ListDir {
            path: from.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| BackupError::ReadFile {
            path: path.clone(),
            source,
        })?;
        let target = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&path, &target, copy)?;
        } else if file_type.is_file() {
            let bytes =
                std::fs::copy(&path, &target).map_err(|source| BackupError::CopyTranscript {
                    from: path.clone(),
                    to: target.clone(),
                    source,
                })?;
            copy.files += 1;
            copy.bytes += bytes;
        } else {
            return Err(BackupError::TranscriptEntry {
                path,
                detail: "only regular files and directories are copied; symlinks and special \
                         files are refused rather than followed"
                    .to_string(),
            });
        }
    }
    Ok(())
}

/// Count what the backup's transcript tree holds, without copying anything.
fn count_transcript_tree(dir: &Path) -> Result<TranscriptCopy, BackupError> {
    let mut count = TranscriptCopy::default();
    let metadata = match std::fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(count),
        Err(source) => {
            return Err(BackupError::ReadFile {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.is_dir() {
        return Err(BackupError::TranscriptEntry {
            path: dir.to_path_buf(),
            detail: "the transcript path is not a plain directory".to_string(),
        });
    }
    count_dir(dir, &mut count)?;
    Ok(count)
}

fn count_dir(dir: &Path, count: &mut TranscriptCopy) -> Result<(), BackupError> {
    let entries = std::fs::read_dir(dir).map_err(|source| BackupError::ListDir {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| BackupError::ListDir {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| BackupError::ReadFile {
            path: path.clone(),
            source,
        })?;
        if file_type.is_dir() {
            count_dir(&path, count)?;
        } else if file_type.is_file() {
            let bytes = std::fs::metadata(&path)
                .map_err(|source| BackupError::ReadFile {
                    path: path.clone(),
                    source,
                })?
                .len();
            count.files += 1;
            count.bytes += bytes;
        } else {
            return Err(BackupError::TranscriptEntry {
                path,
                detail: "symlinks and special files are not part of a transcript tree".to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counted_tables_are_a_subset_of_the_known_schema() {
        // The list is hand-maintained; a table added to a migration and not
        // here would silently stop being verified. This test cannot see the
        // migrations (they are private to `sqlite`), so it pins the obvious
        // invariant instead: no duplicates, all lowercase identifiers.
        let mut seen = HashSet::new();
        for table in COUNTED_TABLES {
            assert!(seen.insert(*table), "`{table}` is listed twice");
            assert!(
                table.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "`{table}` is not a plain SQLite identifier"
            );
        }
        assert!(COUNTED_TABLES.contains(&"bots"));
        assert!(COUNTED_TABLES.contains(&"entries"));
        assert!(!COUNTED_TABLES.contains(&"transcripts"));
    }

    #[test]
    fn a_manifest_names_the_files_this_layout_writes() {
        assert_eq!(SNAPSHOT_FILE, DATABASE_FILE);
        assert_eq!(MANIFEST_FILE, "MANIFEST.json");
        assert_eq!(MANIFEST_VERSION, 1);
    }
}
