//! Restoring a backup into a data dir — including the case that makes restore
//! dangerous: **a brain that is still running.**
//!
//! Backing up is safe to do under a live store (`VACUUM INTO` runs inside
//! SQLite's locking). Restoring is the opposite: it replaces the file the live
//! process has open. Three things go wrong if it is done naively, and each of
//! them is silent:
//!
//! 1. **The running brain keeps writing to the old inode.** Its open file
//!    descriptor still points at the pre-restore database, so the brain happily
//!    continues with the old data while the new file sits on disk; the next
//!    checkpoint or write can then overwrite the restored file with the old
//!    pages. Nothing errors. This module therefore **refuses** when another
//!    process holds the database open, and says which one.
//! 2. **A stale `-wal` replays the old database onto the new one.** Copying the
//!    snapshot over `swarmbot.sqlite` and leaving `swarmbot.sqlite-wal` behind
//!    means SQLite applies the *old* WAL to the *new* database at the next open.
//!    The result opens, reports plausible data, and is not the backup the owner
//!    restored. So the sidecar files are removed as part of the switch, never
//!    left for SQLite to interpret.
//! 3. **A restore that cannot be undone.** Every restore takes a safety backup
//!    of the data dir it is about to replace, before it touches anything, and
//!    reports where it went. An owner who restores the wrong snapshot has a way
//!    back; a restore that only deletes is data loss with a log line.
//!
//! The order below is chosen so that nothing destructive happens until
//! everything has been checked: verify the backup, verify the copy that will be
//! switched in, **then** remove the sidecars and rename.
//!
//! Detecting a live holder is Linux-specific (`/proc/<pid>/fd`). That is stated
//! rather than hidden: on a platform without `/proc` the probe returns "cannot
//! tell" and the restore proceeds, which is the same behaviour `cp` would have —
//! so the *procedure* documented for non-Linux owners is "stop the brain first".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{Connection, OpenFlags};
use thiserror::Error;

use crate::backup::{
    self, BackupError, DATABASE_FILE, MANIFEST_FILE, Manifest, SNAPSHOT_FILE, TRANSCRIPTS_DIR,
    VerifyReport,
};

/// SQLite's write-ahead log file, next to the database.
pub const WAL_SUFFIX: &str = "-wal";
/// SQLite's shared-memory file, next to the database.
pub const SHM_SUFFIX: &str = "-shm";
/// The temporary name a snapshot is copied to before the rename that switches it
/// in. Same directory on purpose: a rename across filesystems is a copy, which is
/// not atomic.
const RESTORE_TMP: &str = ".swarmbot.sqlite.restore-tmp";
/// Prefix of the safety backup a restore writes before it replaces anything.
pub const SAFETY_PREFIX: &str = "pre-restore";

/// What [`restore`] did, so a CLI can print it and an owner can undo it.
#[derive(Clone, Debug, PartialEq)]
pub struct RestoreReport {
    /// The data dir that was replaced.
    pub data_dir: PathBuf,
    /// The backup that was restored.
    pub from: PathBuf,
    /// The backup's manifest, as verified.
    pub manifest: Manifest,
    /// Where the previous data dir went, when there was one. `None` on a restore
    /// into a fresh directory — there was nothing to lose.
    pub safety: Option<PathBuf>,
    /// Whether a stale `-wal` was removed as part of the switch. It almost always
    /// was, and the count is reported because its presence is exactly the thing
    /// that silently un-does a restore.
    pub removed_wal: bool,
    /// Whether a `-shm` was removed.
    pub removed_shm: bool,
    /// The row counts read back from the **restored** database.
    pub tables: BTreeMap<String, i64>,
}

/// Everything a restore can refuse to do, each naming the path that caused it.
#[derive(Debug, Error)]
pub enum RestoreError {
    /// The backup itself is not a backup: missing manifest, bad integrity,
    /// manifest that disagrees with the snapshot.
    #[error(transparent)]
    Backup(#[from] BackupError),
    /// A process still holds the database open. Restoring now would fight it and
    /// lose silently — see the module docs.
    #[error(
        "{path} is open in pid {pids:?}; a running brain would keep writing to the old file and \
         can overwrite what was just restored. Stop the brain, then restore"
    )]
    BrainIsRunning { path: PathBuf, pids: Vec<u32> },
    #[error("data directory {path} is not a directory")]
    DataDirNotADirectory { path: PathBuf },
    #[error("could not create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not copy {from} to {to}: {source}")]
    Copy {
        from: PathBuf,
        to: PathBuf,
        source: std::io::Error,
    },
    #[error("could not remove {path}: {source}")]
    Remove {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not switch {from} into {to}: {source}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        source: std::io::Error,
    },
    #[error("could not read back the restored database at {path}: {detail}")]
    RestoredUnreadable { path: PathBuf, detail: String },
    #[error(
        "the copy that was about to be restored holds {found} rows in `{table}`, but the backup's \
         manifest says {expected}; nothing was changed"
    )]
    CopyDoesNotMatch {
        table: String,
        found: i64,
        expected: i64,
    },
    #[error(
        "cannot take a safety backup of {path} before restoring: {detail}. Nothing was changed — a \
         restore with no way back is not one this build performs"
    )]
    SafetyBackupFailed { path: PathBuf, detail: String },
}

/// Restore the backup in `from` into the data dir `data_dir`.
///
/// Refuses in every case where the result would be a lie: a backup that does not
/// verify, a database another process is using, or a data dir it cannot first
/// back up. On success the data dir holds the backup's database and transcripts,
/// and `safety` points at the previous contents.
pub fn restore(from: &Path, data_dir: &Path) -> Result<RestoreReport, RestoreError> {
    // 1. The backup must be a backup. Everything after this trusts its numbers.
    let verified: VerifyReport = backup::verify(from)?;
    let manifest = verified.manifest.clone();

    // 2. A live holder is a refusal, not a race to lose.
    let database = data_dir.join(DATABASE_FILE);
    if database.exists() {
        let holders = open_holders(&database);
        if !holders.is_empty() {
            return Err(RestoreError::BrainIsRunning {
                path: database,
                pids: holders,
            });
        }
    }

    if data_dir.exists() && !data_dir.is_dir() {
        return Err(RestoreError::DataDirNotADirectory {
            path: data_dir.to_path_buf(),
        });
    }
    std::fs::create_dir_all(data_dir).map_err(|source| RestoreError::CreateDir {
        path: data_dir.to_path_buf(),
        source,
    })?;

    // 3. Copy the snapshot next to its destination and prove *the copy* opens
    //    and holds the manifest's rows. Nothing has been destroyed yet.
    let tmp = data_dir.join(RESTORE_TMP);
    let snapshot = from.join(&manifest.database);
    std::fs::copy(&snapshot, &tmp).map_err(|source| RestoreError::Copy {
        from: snapshot.clone(),
        to: tmp.clone(),
        source,
    })?;
    let tables = match read_tables(&tmp) {
        Ok(tables) => tables,
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
    };
    // The copy is only as good as its agreement with the manifest; a mismatch
    // here means the disk lied, and restoring it would be restoring something
    // nobody verified.
    for (table, expected) in &manifest.tables {
        let found = tables.get(table).copied().unwrap_or(0);
        if found != *expected {
            let _ = std::fs::remove_file(&tmp);
            return Err(RestoreError::CopyDoesNotMatch {
                table: table.clone(),
                found,
                expected: *expected,
            });
        }
    }

    // 4. Safety backup of what is about to be replaced. Taken from the *live*
    //    files (the ones the switch is about to remove), through the ordinary
    //    `VACUUM INTO` path, so a WAL that has not been checkpointed is folded
    //    into it rather than lost.
    let safety = if database.exists() {
        let dir = safety_dir(data_dir);
        backup::backup(data_dir, &dir).map_err(|error| RestoreError::SafetyBackupFailed {
            path: data_dir.to_path_buf(),
            detail: error.to_string(),
        })?;
        Some(dir)
    } else {
        None
    };

    // 5. The switch. Sidecars first: a `-wal` left next to a replaced database is
    //    replayed onto it at the next open, which is the silent failure this
    //    whole module exists to prevent.
    let removed_wal = remove_if_present(&data_dir.join(format!("{DATABASE_FILE}{WAL_SUFFIX}")))?;
    let removed_shm = remove_if_present(&data_dir.join(format!("{DATABASE_FILE}{SHM_SUFFIX}")))?;
    std::fs::rename(&tmp, &database).map_err(|source| RestoreError::Rename {
        from: tmp.clone(),
        to: database.clone(),
        source,
    })?;

    // 6. Transcripts: the backup's tree replaces the data dir's, so a restore can
    //    never leave a transcript from the old data dir pointing at entries the
    //    restored database does not have.
    let restored_transcripts = data_dir.join(TRANSCRIPTS_DIR);
    if restored_transcripts.exists() {
        std::fs::remove_dir_all(&restored_transcripts).map_err(|source| RestoreError::Remove {
            path: restored_transcripts.clone(),
            source,
        })?;
    }
    let backup_transcripts = from.join(&manifest.transcripts);
    if backup_transcripts.is_dir() {
        copy_tree(&backup_transcripts, &restored_transcripts)?;
    }

    Ok(RestoreReport {
        data_dir: data_dir.to_path_buf(),
        from: from.to_path_buf(),
        manifest,
        safety,
        removed_wal,
        removed_shm,
        tables,
    })
}

/// The sibling directory a safety backup goes into: `<data_dir>.pre-restore-<utc>`.
///
/// A sibling rather than a child so it cannot be mistaken for part of the data
/// dir (the brain reads `<data>/config.toml`, `<data>/owner-token`, …), and
/// timestamped so two restores in a row cannot collide.
pub fn safety_dir(data_dir: &Path) -> PathBuf {
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let name = data_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "data".to_string());
    let parent = data_dir.parent().unwrap_or_else(|| Path::new("."));
    let base = parent.join(format!("{name}.{SAFETY_PREFIX}-{stamp}"));
    // Two restores inside the same second is not hypothetical — rolling a restore
    // back is the obvious next thing an owner does, and it happens immediately.
    // A second-resolution stamp alone would make the rollback's safety backup
    // refuse to write into the directory the forward restore just created, so the
    // suffix makes the name unique instead of the restore failing.
    if !base.exists() {
        return base;
    }
    for suffix in 2..1000u32 {
        let candidate = parent.join(format!("{name}.{SAFETY_PREFIX}-{stamp}-{suffix}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // A thousand collisions in one second means something else is writing here;
    // a unique name is still what the caller needs, and the timestamp plus pid is
    // as unique as this process can be without reserving the directory itself.
    parent.join(format!(
        "{name}.{SAFETY_PREFIX}-{stamp}-{}",
        std::process::id()
    ))
}

/// Every pid (other than this process) that holds `path` open.
///
/// `/proc/<pid>/fd` is the one place the kernel tells the truth about who has a
/// file open, and an open file descriptor is exactly what makes a restore unsafe
/// (the running brain keeps writing to the old inode). Best effort by design: a
/// process that exits mid-scan, or a pid we may not inspect, is skipped rather
/// than reported as an error — this probe decides whether to *refuse*, and a
/// probe that cannot see cannot accuse.
pub fn open_holders(path: &Path) -> Vec<u32> {
    let wanted = match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) => path.to_path_buf(),
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        // No /proc: this platform cannot answer, and the caller's documented
        // procedure is "stop the brain first".
        return Vec::new();
    };
    let own = std::process::id();
    let mut holders = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own {
            // Our own descriptors (the probe's, the test's) say nothing about
            // another brain holding the file.
            continue;
        }
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let mut held = false;
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            // Compare the file names as well as the full path: a database opened
            // through a symlink shows the resolved path, and both spellings are
            // the same file.
            if target == wanted
                || target.file_name() == wanted.file_name()
                    && target.parent().and_then(|p| std::fs::canonicalize(p).ok())
                        == wanted.parent().and_then(|p| std::fs::canonicalize(p).ok())
            {
                held = true;
                break;
            }
        }
        if held {
            holders.push(pid);
        }
    }
    holders.sort_unstable();
    holders
}

/// Open `path` read-only and count the rows the manifest cares about.
fn read_tables(path: &Path) -> Result<BTreeMap<String, i64>, RestoreError> {
    let opened = path
        .to_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| RestoreError::RestoredUnreadable {
            path: path.to_path_buf(),
            detail: "the path is not valid UTF-8, so SQLite cannot open it".to_string(),
        })?;
    let connection = Connection::open_with_flags(
        opened,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| RestoreError::RestoredUnreadable {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    let mut tables = BTreeMap::new();
    for table in crate::backup::counted_tables() {
        let sql = format!("SELECT COUNT(*) FROM \"{table}\"");
        match connection.query_row(&sql, [], |row| row.get::<_, i64>(0)) {
            Ok(count) => {
                tables.insert((*table).to_string(), count);
            }
            // A table an older schema never had is absent from both, which the
            // manifest agrees with; anything else is a real failure to report.
            Err(rusqlite::Error::SqliteFailure(_, _)) => {}
            Err(error) => {
                return Err(RestoreError::RestoredUnreadable {
                    path: path.to_path_buf(),
                    detail: format!("cannot count `{table}`: {error}"),
                });
            }
        }
    }
    if tables.is_empty() {
        return Err(RestoreError::RestoredUnreadable {
            path: path.to_path_buf(),
            detail: "the copy has none of the tables a SwarmBot database has".to_string(),
        });
    }
    Ok(tables)
}

fn remove_if_present(path: &Path) -> Result<bool, RestoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(RestoreError::Remove {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Copy a directory tree one level deep — the transcript mirror is
/// `<transcripts>/<conversation>/<file>.jsonl`, so a recursive copy is the honest
/// shape even though the current layout is shallow.
fn copy_tree(from: &Path, to: &Path) -> Result<(), RestoreError> {
    std::fs::create_dir_all(to).map_err(|source| RestoreError::CreateDir {
        path: to.to_path_buf(),
        source,
    })?;
    let entries = std::fs::read_dir(from).map_err(|source| RestoreError::Copy {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
        source,
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).map_err(|source| RestoreError::Copy {
                from: path.clone(),
                to: target.clone(),
                source,
            })?;
        }
    }
    Ok(())
}

/// Re-exported so a caller can name the snapshot file without reaching into
/// `backup`'s private constants: `restore` reads `<from>/<that name>`.
pub const RESTORED_DATABASE: &str = SNAPSHOT_FILE;

/// Re-exported for the same reason: the manifest file name a restored backup has.
pub const RESTORED_MANIFEST: &str = MANIFEST_FILE;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::{self, DATABASE_FILE};
    use swarmbot_contracts::repo::{BotRepo, SettingsStore};
    use swarmbot_contracts::{Bot, ConversationId, Settings};

    /// A data dir with one bot, so a restored copy is distinguishable from an
    /// empty one by something a test can name.
    fn seed(dir: &Path, name: &str) -> crate::SqliteStore {
        std::fs::create_dir_all(dir).expect("data dir");
        let store = crate::SqliteStore::open(&dir.join(DATABASE_FILE)).expect("open the store");
        let bot = Bot::new(name, name, "you are a test", "fake", ConversationId::new());
        store.insert_bot(&bot).expect("insert a bot");
        store
    }

    fn backup_of(dir: &Path, out: &Path) {
        backup::backup(dir, out).expect("a backup is written");
    }

    #[test]
    fn a_restore_replaces_the_database_and_reports_the_safety_copy() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let source = tmp.path().join("source");
        seed(&source, "from-the-backup");
        let snapshot = tmp.path().join("snapshot");
        backup_of(&source, &snapshot);

        // The target holds different data.
        let target = tmp.path().join("target");
        seed(&target, "the-old-one");

        let report = restore(&snapshot, &target).expect("the restore happens");
        assert!(report.safety.is_some(), "the old data dir was preserved");
        assert_eq!(report.tables.get("bots"), Some(&1));

        // The restored store serves the backup's data, not the old data.
        let restored = crate::SqliteStore::open(&target.join(DATABASE_FILE))
            .expect("the restored database opens as a store");
        let bots = restored.list_bots().expect("bots read");
        assert_eq!(bots.len(), 1);
        assert_eq!(bots[0].name, "from-the-backup");

        // And the safety copy can be restored back, which is the whole point of
        // taking it.
        let safety = report.safety.expect("a safety dir");
        let back = restore(&safety, &target).expect("the safety copy restores");
        let restored = crate::SqliteStore::open(&target.join(DATABASE_FILE))
            .expect("the rolled-back database opens");
        let bots = restored.list_bots().expect("bots read");
        assert_eq!(bots[0].name, "the-old-one");
        assert!(back.safety.is_some());
    }

    #[test]
    fn a_stale_wal_is_removed_by_the_switch_instead_of_being_replayed() {
        // The silent failure this module exists to prevent: leave the old
        // database's WAL next to a replaced database and SQLite replays the old
        // pages onto the new file at the next open.
        let tmp = tempfile::tempdir().expect("temp dir");
        let source = tmp.path().join("source");
        seed(&source, "from-the-backup");
        let snapshot = tmp.path().join("snapshot");
        backup_of(&source, &snapshot);

        let target = tmp.path().join("target");
        // The store is kept **alive** across the restore on purpose: that is the
        // shape the whole module is about. A brain still running owns the old
        // inode even after its file is replaced.
        let live = seed(&target, "the-old-one");
        live.put_settings(&Settings::default())
            .expect("settings written");
        let wal = target.join(format!("{DATABASE_FILE}{WAL_SUFFIX}"));
        assert!(wal.exists(), "the live store left a WAL behind: {wal:?}");

        let report = restore(&snapshot, &target).expect("the restore happens");
        assert!(report.removed_wal, "the stale WAL was named and removed");
        assert!(
            !wal.exists(),
            "no WAL is left for SQLite to replay onto the restored file"
        );

        let restored = crate::SqliteStore::open(&target.join(DATABASE_FILE))
            .expect("the restored database opens");
        let bots = restored.list_bots().expect("bots read");
        assert_eq!(
            bots.iter().map(|bot| bot.name.as_str()).collect::<Vec<_>>(),
            vec!["from-the-backup"],
            "the restored rows are the backup's, not a replay of the old WAL"
        );
    }

    #[test]
    fn a_restore_into_a_fresh_directory_has_no_safety_copy_and_says_so() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let source = tmp.path().join("source");
        seed(&source, "from-the-backup");
        let snapshot = tmp.path().join("snapshot");
        backup_of(&source, &snapshot);

        let fresh = tmp.path().join("brand-new");
        let report = restore(&snapshot, &fresh).expect("the restore happens");
        assert!(
            report.safety.is_none(),
            "there was nothing to lose, and the report says so rather than inventing a directory"
        );
        let restored = crate::SqliteStore::open(&fresh.join(DATABASE_FILE)).expect("opens");
        assert_eq!(restored.list_bots().expect("bots").len(), 1);
    }

    #[test]
    fn a_backup_that_does_not_verify_is_refused_before_anything_is_touched() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let source = tmp.path().join("source");
        seed(&source, "from-the-backup");
        let snapshot = tmp.path().join("snapshot");
        backup_of(&source, &snapshot);

        let target = tmp.path().join("target");
        seed(&target, "the-old-one");
        let before = std::fs::read(target.join(DATABASE_FILE)).expect("read the database");

        // A manifest that claims a different row count is not a backup any more.
        let manifest_path = snapshot.join(MANIFEST_FILE);
        let mut manifest: Manifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("read the manifest"))
                .expect("a manifest");
        manifest.tables.insert("bots".to_string(), 99);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("encode"),
        )
        .expect("write the manifest");

        let error = restore(&snapshot, &target).expect_err("a lying manifest is refused");
        assert!(
            matches!(error, RestoreError::Backup(_)),
            "the refusal comes from verify: {error}"
        );
        let after = std::fs::read(target.join(DATABASE_FILE)).expect("read the database");
        assert_eq!(before, after, "nothing was changed by a refused restore");
    }

    /// A second process holding the database open is the case that makes a
    /// restore dangerous, and the one a single-process test cannot fake: the
    /// probe deliberately ignores *this* process's own descriptors, so proving the
    /// refusal needs a real foreign holder.
    #[test]
    fn a_database_held_by_another_process_is_a_refusal_not_a_race() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let source = tmp.path().join("source");
        seed(&source, "from-the-backup");
        let snapshot = tmp.path().join("snapshot");
        backup_of(&source, &snapshot);

        let target = tmp.path().join("target");
        seed(&target, "the-old-one");
        let database = target.join(DATABASE_FILE);

        // `exec sleep 30 < db` keeps one descriptor open on the database for the
        // life of the child. `sh` is used so the redirection is the child's own.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exec sleep 30 < \"$1\"")
            .arg("sh")
            .arg(&database)
            .spawn()
            .expect("a holder process starts");

        // The fork happens before the redirect lands, so wait for the descriptor
        // to show up rather than assuming it is instant.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut holders = open_holders(&database);
        while holders.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            holders = open_holders(&database);
        }
        let refused = restore(&snapshot, &target);
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            !holders.is_empty(),
            "the probe found the child holding {database:?}"
        );
        assert!(
            holders.contains(&child.id()),
            "the probe names the pid that holds it: {holders:?} vs {}",
            child.id()
        );
        match refused {
            Err(RestoreError::BrainIsRunning { pids, .. }) => {
                assert!(pids.contains(&child.id()), "{pids:?}");
            }
            other => panic!("a held database must be refused, got {other:?}"),
        }
        // And the refusal left the target alone.
        let still = crate::SqliteStore::open(&database).expect("the target still opens");
        assert_eq!(
            still
                .list_bots()
                .expect("bots")
                .first()
                .map(|b| b.name.as_str()),
            Some("the-old-one"),
            "a refused restore changes nothing"
        );
    }

    #[test]
    fn the_safety_directory_is_a_sibling_never_a_child_of_the_data_dir() {
        // A child would be read as part of the data dir by every `Paths::new`
        // lookup and by the next backup.
        let dir = Path::new("/var/lib/swarmbot");
        let safety = safety_dir(dir);
        assert_eq!(safety.parent(), Some(Path::new("/var/lib")));
        assert!(
            safety
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("swarmbot.pre-restore-"))
        );
    }
}
