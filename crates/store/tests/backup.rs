//! Backing up a data directory: what the snapshot contains, whether it can be
//! opened as a real store, and every way a dishonest backup must be refused.
//!
//! The point of a backup is that it can be **restored**, so the strongest test
//! here opens the snapshot with [`SqliteStore::open`] and reads the owner's data
//! back out of it — a backup nobody ever opened is a guess. The second point is
//! that the live case is the one `cp` gets wrong: a store still running with an
//! un-checkpointed WAL must snapshot whole, which `VACUUM INTO` does and a file
//! copy does not.

use std::path::{Path, PathBuf};

use rusqlite::OpenFlags;

use swarmbot_contracts::repo::{BotRepo, ConversationRepo, EntryRepo, SettingsStore, TurnRepo};
use swarmbot_contracts::{
    Bot, Conversation, ConversationId, Entry, Settings, Turn, TurnInitiator, TurnStatus, Usage,
};
use swarmbot_store::{BackupError, JsonlMirror, SqliteStore, backup, verify};

/// One seeded data dir: the store plus the rows a restore has to reproduce.
struct Seeded {
    dir: PathBuf,
    store: SqliteStore,
    lead: Bot,
    scout: Bot,
    dm: Conversation,
    group: Conversation,
    entries: Vec<Entry>,
    turn: Turn,
}

impl Seeded {
    /// The database path inside the data dir.
    fn database(&self) -> PathBuf {
        self.dir.join("swarmbot.sqlite")
    }
}

/// A data dir with two bots, a DM and a group, four entries, one finished turn
/// with usage, and a JSONL mirror holding the DM's transcript.
///
/// Two bots and two conversations on purpose: a snapshot that only happens to
/// carry one row of each would still look fine to a weak test.
fn seed(dir: &Path) -> Seeded {
    std::fs::create_dir_all(dir).expect("data dir");
    let store = SqliteStore::open(&dir.join("swarmbot.sqlite")).expect("open store");

    let mut lead = Bot::new("lead", "Lead", "you lead", "fake", ConversationId::new());
    let scout = Bot::new(
        "scout",
        "Scout",
        "you explore",
        "fake",
        ConversationId::new(),
    );
    let dm = Conversation::direct(lead.id.clone());
    lead.direct_conversation_id = dm.id.clone();
    let group = Conversation::group(
        "ops",
        vec![lead.id.clone(), scout.id.clone()],
        lead.id.clone(),
    )
    .expect("valid group");

    store.insert_bot(&lead).expect("insert lead");
    store.insert_bot(&scout).expect("insert scout");
    store.insert_conversation(&dm).expect("insert dm");
    store.insert_conversation(&group).expect("insert group");

    let entries = vec![
        Entry::owner_message(dm.id.clone(), 1, "remember me"),
        Entry::bot_message(dm.id.clone(), 2, lead.id.clone(), "on it", None),
        Entry::owner_message(group.id.clone(), 1, "@scout look at the logs"),
        Entry::bot_message(group.id.clone(), 2, scout.id.clone(), "checking", None),
    ];
    for entry in &entries {
        store.upsert_entry(entry).expect("upsert entry");
    }

    let mut turn = Turn::start(
        lead.id.clone(),
        dm.id.clone(),
        TurnInitiator::Owner,
        None,
        None,
    );
    turn.status = TurnStatus::Completed;
    turn.usage = Usage {
        input_tokens: 120,
        output_tokens: 40,
        llm_calls: 1,
        tool_calls: 2,
    };
    store.insert_turn(&turn).expect("insert turn");

    store
        .put_settings(&Settings {
            max_hops: 3,
            ..Settings::default()
        })
        .expect("put settings");

    // The paper trail: one header line plus the DM's two entries.
    let mirror = JsonlMirror::new(dir.join("transcripts"));
    mirror.ensure_header(&dm).expect("mirror header");
    for entry in entries.iter().filter(|e| e.conversation_id == dm.id) {
        mirror.append(&dm.id, entry).expect("mirror entry");
    }

    Seeded {
        dir: dir.to_path_buf(),
        store,
        lead,
        scout,
        dm,
        group,
        entries,
        turn,
    }
}

#[test]
fn a_backup_snapshots_the_data_and_verify_rederives_the_same_numbers() {
    let root = tempfile::tempdir().expect("temp dir");
    let data = root.path().join("data");
    let seeded = seed(&data);
    let out = root.path().join("backups").join("first");

    let report = backup(&seeded.dir, &out).expect("backup succeeds");

    // The three pieces, and the manifest is what makes it a backup.
    assert!(report.snapshot.is_file(), "the snapshot exists");
    assert!(out.join("MANIFEST.json").is_file(), "the manifest exists");
    assert!(out.join("transcripts").is_dir(), "transcripts were copied");

    let manifest = &report.manifest;
    assert_eq!(manifest.manifest_version, 1);
    assert_eq!(manifest.database, "swarmbot.sqlite");
    assert_eq!(manifest.transcripts, "transcripts");
    // The schema version is read from `PRAGMA user_version` of the snapshot,
    // not from a constant: compare against what the snapshot itself reports so
    // adding a migration does not break this test, while a hardcoded version
    // would.
    let snapshot =
        rusqlite::Connection::open_with_flags(&report.snapshot, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open the snapshot read-only");
    let raw_user_version: i64 = snapshot
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version");
    assert!(
        raw_user_version >= 1,
        "a snapshot of a migrated database records its migration level"
    );
    assert_eq!(
        manifest.schema_version, raw_user_version,
        "the manifest reports the snapshot's own user_version"
    );
    assert_eq!(manifest.crate_version, env!("CARGO_PKG_VERSION"));
    assert!(
        manifest.created_at.ends_with('Z') && manifest.created_at.contains('T'),
        "the backup time is the store's fixed-width UTC format, got {:?}",
        manifest.created_at
    );
    assert_eq!(manifest.tables.get("bots"), Some(&2), "two bots");
    assert_eq!(
        manifest.tables.get("conversations"),
        Some(&2),
        "two conversations"
    );
    assert_eq!(manifest.tables.get("entries"), Some(&4), "four entries");
    assert_eq!(manifest.tables.get("turns"), Some(&1), "one turn");
    assert_eq!(
        manifest.tables.get("settings"),
        Some(&1),
        "one settings row"
    );
    assert_eq!(manifest.transcript_files, 1, "one transcript file");
    assert!(manifest.transcript_bytes > 0);
    assert!(manifest.database_bytes > 0);

    // Verify re-derives every number from the snapshot and the transcript
    // tree, and finds them consistent.
    let verified = verify(&out).expect("the fresh backup verifies");
    assert_eq!(verified.integrity_check, "ok");
    assert_eq!(verified.tables, manifest.tables);
    assert_eq!(verified.transcript_files, 1);
    assert_eq!(verified.manifest, *manifest);

    // The manifest on disk is the same document the report described, so a
    // caller may read either.
    let on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("MANIFEST.json")).expect("read manifest"))
            .expect("manifest is JSON");
    assert_eq!(
        on_disk["schema_version"],
        serde_json::json!(raw_user_version)
    );
    assert_eq!(on_disk["tables"]["entries"], serde_json::json!(4));
}

#[test]
fn the_snapshot_opens_as_a_store_and_serves_the_owners_data() {
    let root = tempfile::tempdir().expect("temp dir");
    let data = root.path().join("data");
    let seeded = seed(&data);
    let out = root.path().join("backup");

    let report = backup(&seeded.dir, &out).expect("backup succeeds");

    // The whole point: the snapshot is a *database*, not a blob. Opening it
    // with the real store proves the restore path works without a bespoke
    // restore routine — "put this file back as <data>/swarmbot.sqlite".
    let restored = SqliteStore::open(&report.snapshot).expect("the snapshot opens as a store");

    assert_eq!(
        restored.list_bots().expect("list bots").len(),
        2,
        "both bots survived the snapshot"
    );
    assert_eq!(
        restored.get_bot(&seeded.lead.id).expect("get bot"),
        Some(seeded.lead.clone()),
        "the lead bot reads back field for field"
    );
    assert_eq!(
        restored.get_bot(&seeded.scout.id).expect("get bot"),
        Some(seeded.scout.clone())
    );
    assert_eq!(
        restored
            .entries_after(&seeded.dm.id, 0, 10)
            .expect("dm entries"),
        seeded
            .entries
            .iter()
            .filter(|e| e.conversation_id == seeded.dm.id)
            .cloned()
            .collect::<Vec<_>>(),
        "the DM transcript reads back in order"
    );
    assert_eq!(
        restored
            .entries_after(&seeded.group.id, 0, 10)
            .expect("group entries")
            .len(),
        2,
        "the group transcript survived too"
    );
    assert_eq!(
        restored.turns_in(&seeded.dm.id, 10).expect("turns"),
        vec![seeded.turn.clone()],
        "the turn and its usage read back"
    );
    assert_eq!(
        restored.settings().expect("settings").max_hops,
        3,
        "settings are in the snapshot"
    );
    // A group's members are mirrored out of the payload; a snapshot that lost
    // them would restore a group nobody can address.
    let group = restored
        .get_conversation(&seeded.group.id)
        .expect("get group")
        .expect("the group row is there");
    assert_eq!(group.kind, seeded.group.kind);
    assert_eq!(
        restored
            .list_conversations()
            .expect("list conversations")
            .len(),
        2
    );
}

#[test]
fn a_non_empty_target_is_refused_instead_of_overwritten() {
    let root = tempfile::tempdir().expect("temp dir");
    let seeded = seed(&root.path().join("data"));
    let out = root.path().join("backup");
    std::fs::create_dir_all(&out).expect("target dir");
    // Something the owner put there — an older backup, or anything else. A
    // silent overwrite is data loss, so this is a typed refusal naming the path.
    std::fs::write(out.join("MANIFEST.json"), "{}").expect("existing file");

    let error = backup(&seeded.dir, &out).expect_err("a non-empty target is refused");
    match &error {
        BackupError::TargetNotEmpty { path } => {
            assert_eq!(path, &out, "the error names the target it refused");
        }
        other => panic!("expected TargetNotEmpty, got {other:?}"),
    }
    // Nothing was touched: the pre-existing file is byte for byte what it was.
    assert_eq!(
        std::fs::read_to_string(out.join("MANIFEST.json")).expect("read"),
        "{}"
    );
    assert!(
        !out.join("swarmbot.sqlite").exists(),
        "no snapshot was written into an occupied target"
    );
}

#[test]
fn a_live_store_with_an_uncheckpointed_wal_is_snapshotted_whole() {
    let root = tempfile::tempdir().expect("temp dir");
    let data = root.path().join("data");
    // Keep the store alive across the backup: this is the case the owner hits
    // while the brain is running, and the one `cp` gets wrong.
    let seeded = seed(&data);
    let wal = seeded.dir.join("swarmbot.sqlite-wal");
    assert!(
        wal.is_file() && std::fs::metadata(&wal).expect("wal metadata").len() > 0,
        "the test needs a live, un-checkpointed WAL to be meaningful: {}",
        wal.display()
    );

    // What a file copy would have produced, for contrast: the main database
    // file without its WAL. Opening it either fails outright or shows data
    // that is missing the newest commits — either way it is not a backup.
    let copied = root.path().join("cp-copy.sqlite");
    std::fs::copy(seeded.database(), &copied).expect("copy the main file");
    let copied_rows =
        rusqlite::Connection::open_with_flags(&copied, OpenFlags::SQLITE_OPEN_READ_ONLY).and_then(
            |conn| conn.query_row("SELECT COUNT(*) FROM bots", [], |row| row.get::<_, i64>(0)),
        );
    // A copied WAL-mode database that will not even open — observed here as
    // "no such table: bots", because the whole schema still lives in the WAL —
    // is the other way `cp` loses data, and just as much proof. So only a copy
    // that opens *and* holds every commit would break this test's premise.
    if let Ok(rows) = copied_rows {
        assert!(
            rows < 2,
            "a plain file copy saw every commit, so the WAL case did not reproduce; \
             this test would no longer prove anything (rows = {rows})"
        );
    }

    // The backup, taken while the store is still open.
    let out = root.path().join("backup");
    let report = backup(&seeded.dir, &out).expect("backup of a live store succeeds");
    assert!(
        seeded
            .store
            .list_bots()
            .expect("the store still works")
            .len()
            == 2,
        "the live store is untouched by the backup"
    );

    // `VACUUM INTO` runs under SQLite's locking and sees the WAL, so every
    // committed row is in the snapshot — including the ones `cp` would miss.
    let restored = SqliteStore::open(&report.snapshot).expect("the snapshot opens");
    assert_eq!(
        restored.get_bot(&seeded.lead.id).expect("get bot"),
        Some(seeded.lead.clone()),
        "the newest committed bot is in the snapshot, not stranded in the WAL"
    );
    assert_eq!(
        restored
            .entries_after(&seeded.group.id, 0, 10)
            .expect("entries")
            .len(),
        2
    );
    let verified = verify(&out).expect("the live snapshot verifies");
    assert_eq!(verified.integrity_check, "ok");
    assert_eq!(verified.tables.get("bots"), Some(&2));
}

#[test]
fn a_missing_manifest_is_refused() {
    let root = tempfile::tempdir().expect("temp dir");
    let seeded = seed(&root.path().join("data"));
    let out = root.path().join("backup");
    backup(&seeded.dir, &out).expect("backup succeeds");

    // A directory without a manifest is a half-finished backup: the snapshot
    // may be perfect, but nothing proves the transcript copy finished.
    std::fs::remove_file(out.join("MANIFEST.json")).expect("remove manifest");
    let error = verify(&out).expect_err("no manifest, no valid backup");
    match &error {
        BackupError::UnreadableManifest { path, .. } => {
            assert_eq!(path, &out.join("MANIFEST.json"));
        }
        other => panic!("expected UnreadableManifest, got {other:?}"),
    }
}

#[test]
fn a_corrupt_manifest_is_refused() {
    let root = tempfile::tempdir().expect("temp dir");
    let seeded = seed(&root.path().join("data"));
    let out = root.path().join("backup");
    backup(&seeded.dir, &out).expect("backup succeeds");

    std::fs::write(
        out.join("MANIFEST.json"),
        "{\"manifest_version\": 1, \"trunca",
    )
    .expect("truncate manifest");
    let error = verify(&out).expect_err("a torn manifest is not a backup");
    assert!(
        matches!(error, BackupError::InvalidManifest { .. }),
        "expected InvalidManifest, got {error:?}"
    );

    // Valid JSON of the wrong shape is refused too, rather than defaulted.
    std::fs::write(out.join("MANIFEST.json"), "{\"unexpected\": true}").expect("write");
    let error = verify(&out).expect_err("a manifest of the wrong shape is refused");
    assert!(
        matches!(error, BackupError::InvalidManifest { .. }),
        "expected InvalidManifest, got {error:?}"
    );
}

#[test]
fn a_manifest_that_lies_about_row_counts_is_caught() {
    let root = tempfile::tempdir().expect("temp dir");
    let seeded = seed(&root.path().join("data"));
    let out = root.path().join("backup");
    backup(&seeded.dir, &out).expect("backup succeeds");

    // The manifest is not trusted: `verify` re-counts the snapshot itself, so
    // a manifest that claims more (or fewer) rows than the snapshot holds is a
    // detected failure, not a green check.
    let manifest_path = out.join("MANIFEST.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read manifest"))
            .expect("manifest is JSON");
    manifest["tables"]["entries"] = serde_json::json!(999);
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("re-encode"),
    )
    .expect("write lying manifest");

    let error = verify(&out).expect_err("a manifest that inflates a count is refused");
    match &error {
        BackupError::RowCountMismatch {
            table,
            found,
            expected,
            ..
        } => {
            assert_eq!(table, "entries");
            assert_eq!(*found, 4, "the count comes from the snapshot");
            assert_eq!(*expected, 999, "the claim comes from the manifest");
        }
        other => panic!("expected RowCountMismatch, got {other:?}"),
    }
}

#[test]
fn a_manifest_that_omits_a_table_is_caught() {
    let root = tempfile::tempdir().expect("temp dir");
    let seeded = seed(&root.path().join("data"));
    let out = root.path().join("backup");
    backup(&seeded.dir, &out).expect("backup succeeds");

    let manifest_path = out.join("MANIFEST.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read manifest"))
            .expect("manifest is JSON");
    manifest["tables"]
        .as_object_mut()
        .expect("tables is an object")
        .remove("entries");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("re-encode"),
    )
    .expect("write");

    let error = verify(&out).expect_err("a manifest that hides a table is refused");
    match &error {
        BackupError::UnlistedTable { table, .. } => assert_eq!(table, "entries"),
        other => panic!("expected UnlistedTable, got {other:?}"),
    }
}

#[test]
fn a_failed_backup_leaves_no_manifest_behind() {
    let root = tempfile::tempdir().expect("temp dir");
    let seeded = seed(&root.path().join("data"));

    // A symlink in the transcript tree is refused rather than followed, which
    // fails the copy *after* the snapshot succeeded — exactly the half-finished
    // state that must never look like a backup.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/hostname", seeded.dir.join("transcripts/evil.jsonl"))
            .expect("symlink");
        let out = root.path().join("backup");
        let error = backup(&seeded.dir, &out).expect_err("a symlinked transcript is refused");
        assert!(
            matches!(error, BackupError::TranscriptEntry { .. }),
            "expected TranscriptEntry, got {error:?}"
        );

        assert!(
            !out.join("MANIFEST.json").exists(),
            "a failed backup must not leave a manifest, or verify would accept it"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&out)
            .expect("out dir still exists")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert!(
            leftovers.is_empty(),
            "the partial snapshot was cleaned up, found {leftovers:?}"
        );
        let error = verify(&out).expect_err("a failed backup never verifies");
        assert!(
            matches!(error, BackupError::UnreadableManifest { .. }),
            "expected UnreadableManifest, got {error:?}"
        );
    }
}

#[test]
fn a_data_dir_without_a_database_or_without_transcripts_is_handled_honestly() {
    let root = tempfile::tempdir().expect("temp dir");

    // No database at all: an empty data dir cannot be backed up, and the error
    // says which path is missing instead of producing an empty "backup".
    let empty = root.path().join("empty-data");
    std::fs::create_dir_all(&empty).expect("empty data dir");
    let error = backup(&empty, &root.path().join("b1")).expect_err("nothing to back up");
    assert!(
        matches!(error, BackupError::MissingDatabase { .. }),
        "expected MissingDatabase, got {error:?}"
    );

    // A data dir that never mirrored a transcript is a normal data dir (the
    // mirror is optional), so the backup succeeds with zero transcript files.
    let seeded = seed(&root.path().join("data"));
    std::fs::remove_dir_all(seeded.dir.join("transcripts")).expect("drop transcripts");
    let out = root.path().join("b2");
    let report = backup(&seeded.dir, &out).expect("a data dir with no transcripts still backs up");
    assert_eq!(report.manifest.transcript_files, 0);
    assert_eq!(report.manifest.transcript_bytes, 0);
    let verified = verify(&out).expect("verifies");
    assert_eq!(verified.integrity_check, "ok");
    assert_eq!(verified.tables.get("bots"), Some(&2));
}

#[test]
fn a_damaged_snapshot_is_refused_even_with_an_honest_manifest() {
    let root = tempfile::tempdir().expect("temp dir");
    let seeded = seed(&root.path().join("data"));
    let out = root.path().join("backup");
    backup(&seeded.dir, &out).expect("backup succeeds");

    // The manifest is untouched and still truthful about the *original*
    // snapshot; only the snapshot is damaged. `verify` has to notice, which it
    // can only do by opening the file instead of trusting the manifest.
    let snapshot = out.join("swarmbot.sqlite");
    let mut bytes = std::fs::read(&snapshot).expect("read snapshot");
    for byte in bytes.iter_mut().skip(100) {
        *byte = 0;
    }
    std::fs::write(&snapshot, &bytes).expect("damage snapshot");

    let error = verify(&out).expect_err("a damaged snapshot is not a backup");
    let message = error.to_string();
    assert!(
        message.contains("swarmbot.sqlite"),
        "the error names the damaged file: {message}"
    );
    assert!(
        matches!(
            error,
            BackupError::CorruptSnapshot { .. }
                | BackupError::QuerySnapshot { .. }
                | BackupError::DatabaseSizeMismatch { .. }
        ),
        "expected a snapshot-level failure, got {error:?}"
    );
}

#[test]
fn verifying_a_directory_that_is_not_a_backup_is_refused() {
    let root = tempfile::tempdir().expect("temp dir");
    let error = verify(&root.path().join("never-backed-up")).expect_err("no such backup");
    assert!(
        matches!(error, BackupError::MissingBackup { .. }),
        "expected MissingBackup, got {error:?}"
    );

    // A directory that exists but has no snapshot: the manifest cannot make a
    // missing database look present.
    let out = root.path().join("hollow");
    std::fs::create_dir_all(&out).expect("dir");
    std::fs::write(out.join("MANIFEST.json"), "{}").expect("write");
    let error = verify(&out).expect_err("no snapshot");
    assert!(
        matches!(error, BackupError::InvalidManifest { .. }),
        "expected InvalidManifest for a placeholder manifest, got {error:?}"
    );
}
