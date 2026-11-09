//! `fs.*` through the handler: the payload → `NodeCall` bridge, this node's own
//! path policy, and the typed errors a denial becomes.
//!
//! Every path in here is inside a `TempDir`, and nothing runs a shell.

mod common;

use common::*;
use serde_json::{Value, json};
use swarmbot_node_protocol::TypedError;
use swarmbot_node_protocol::rpc;

/// The text of a successful `ToolResult` value.
fn text(value: &Value) -> String {
    value["content"]
        .as_array()
        .and_then(|content| content.first())
        .and_then(|item| item["text"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn is_error(value: &Value) -> bool {
    value["is_error"].as_bool().unwrap_or(false)
}

async fn call(brain: &BrainPeer, method: &str, params: Value) -> Value {
    brain
        .call(method, params)
        .await
        .unwrap_or_else(|error| panic!("{method} failed: {error}"))
}

#[tokio::test]
async fn read_write_edit_stat_round_trip_through_the_handler() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    // write, into a directory that does not exist yet
    let wrote = call(
        &brain,
        rpc::FS_WRITE,
        json!({"inv": inv_value(), "path": "notes/a.txt", "content": "hello world",
               "createParents": true}),
    )
    .await;
    assert!(!is_error(&wrote), "{wrote}");
    assert!(text(&wrote).contains("wrote 11 bytes"), "{wrote}");
    assert!(
        side.root.join("notes/a.txt").exists(),
        "the file is on disk"
    );
    assert_eq!(wrote["meta"]["size"], json!(11));
    assert!(wrote["meta"]["sha256"].as_str().is_some(), "{wrote}");

    // read
    let read = call(&brain, rpc::FS_READ, fs_read("notes/a.txt")).await;
    assert!(!is_error(&read), "{read}");
    assert_eq!(text(&read), "   1→hello world");
    assert_eq!(read["meta"]["total_lines"], json!(1));

    // read a window
    side.write("notes/many.txt", "one\ntwo\nthree\nfour\n");
    let page = call(
        &brain,
        rpc::FS_READ,
        json!({"inv": inv_value(), "path": "notes/many.txt", "offset": 2, "limit": 2}),
    )
    .await;
    assert!(
        text(&page).contains("two") && text(&page).contains("three"),
        "{page}"
    );
    assert!(!text(&page).contains("one\n"), "{page}");
    assert_eq!(page["meta"]["first_line"], json!(2));
    assert_eq!(page["meta"]["last_line"], json!(3));

    // edit
    let edited = call(
        &brain,
        rpc::FS_EDIT,
        json!({"inv": inv_value(), "path": "notes/a.txt", "oldString": "world", "newString": "bot"}),
    )
    .await;
    assert!(!is_error(&edited), "{edited}");
    assert!(text(&edited).contains("+hello bot"), "{edited}");
    assert_eq!(edited["meta"]["replacements"], json!(1));
    assert_eq!(
        std::fs::read_to_string(side.root.join("notes/a.txt")).expect("reads"),
        "hello bot"
    );

    // stat
    let stat = call(&brain, rpc::FS_STAT, fs_read("notes/a.txt")).await;
    assert!(text(&stat).contains("type: file"), "{stat}");
    assert!(text(&stat).contains("size: 9"), "{stat}");
}

#[tokio::test]
async fn writing_into_a_missing_directory_says_which_flag_creates_it() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    let error = brain
        .call(rpc::FS_WRITE, fs_write("missing/dir/a.txt", "x"))
        .await
        .expect_err("the parent directory is not there");
    match typed(&error) {
        TypedError::EditFailed { reason, .. } => {
            assert!(reason.contains("create_parents"), "{reason}");
        }
        other => panic!("expected EditFailed, got {other:?}"),
    }
    assert!(!side.root.join("missing").exists(), "nothing was created");
}

#[tokio::test]
async fn stat_of_a_missing_path_is_null_not_an_error() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;
    let value = call(&brain, rpc::FS_STAT, fs_read("definitely/not/here")).await;
    assert_eq!(value, json!(null), "§5: a missing path is null");
}

#[tokio::test]
async fn list_glob_and_grep_report_what_is_in_the_root() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;
    side.write("src/a.rs", "needle here\n");
    side.write("src/b.txt", "nothing\n");

    let listed = call(
        &brain,
        rpc::FS_LIST,
        json!({"inv": inv_value(), "path": "src", "depth": 1}),
    )
    .await;
    let listing = text(&listed);
    assert!(
        listing.contains("a.rs") && listing.contains("b.txt"),
        "{listing}"
    );

    let globbed = call(
        &brain,
        rpc::FS_GLOB,
        json!({"inv": inv_value(), "pattern": "*.rs", "cwd": "src"}),
    )
    .await;
    assert!(text(&globbed).contains("a.rs"), "{globbed}");
    assert!(!text(&globbed).contains("b.txt"), "{globbed}");

    // `cwd` is required on the wire; an empty one means the workspace.
    let from_workspace = call(
        &brain,
        rpc::FS_GLOB,
        json!({"inv": inv_value(), "pattern": "**/*.rs", "cwd": ""}),
    )
    .await;
    assert!(text(&from_workspace).contains("a.rs"), "{from_workspace}");

    let grepped = call(
        &brain,
        rpc::FS_GREP,
        json!({"inv": inv_value(), "pattern": "needle", "path": "src",
               "outputMode": "files_with_matches"}),
    )
    .await;
    assert!(text(&grepped).contains("a.rs"), "{grepped}");
    assert!(!text(&grepped).contains("b.txt"), "{grepped}");
}

#[tokio::test]
async fn mkdir_move_and_remove_work() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;
    side.write("loose.txt", "x");

    call(
        &brain,
        rpc::FS_MKDIR,
        json!({"inv": inv_value(), "path": "archive/deep", "recursive": true}),
    )
    .await;
    assert!(side.root.join("archive/deep").is_dir());

    call(
        &brain,
        rpc::FS_MOVE,
        json!({"inv": inv_value(), "from": "loose.txt", "to": "archive/deep/loose.txt"}),
    )
    .await;
    assert!(side.root.join("archive/deep/loose.txt").exists());
    assert!(!side.root.join("loose.txt").exists());

    call(
        &brain,
        rpc::FS_REMOVE,
        json!({"inv": inv_value(), "path": "archive", "recursive": true}),
    )
    .await;
    assert!(
        !side.root.join("archive").exists(),
        "removed inside the root"
    );
}

#[tokio::test]
async fn a_path_outside_the_roots_is_denied_with_a_reason_that_reads_alone() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    for (method, params) in [
        (rpc::FS_READ, fs_read("/etc/passwd")),
        (rpc::FS_STAT, fs_read("/etc/passwd")),
        (rpc::FS_WRITE, fs_write("/etc/definitely-not-writable", "x")),
        (rpc::FS_LIST, json!({"inv": inv_value(), "path": "/etc"})),
    ] {
        let error = brain
            .call(method, params)
            .await
            .expect_err("a path outside the roots is a refusal, not an answer");
        match typed(&error) {
            TypedError::PathDenied { path, reason } => {
                assert_eq!(reason, "outside roots", "{method}");
                assert!(path.contains("/etc"), "{method}: {path}");
            }
            other => panic!("{method}: expected PathDenied, got {other:?}"),
        }
        // It reads on its own, which is the point of a typed denial.
        assert!(error.to_string().contains("outside roots"), "{error}");
    }
}

#[tokio::test]
async fn a_symlink_inside_the_root_cannot_escape_it() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;
    std::os::unix::fs::symlink("/etc", side.root.join("escape")).expect("symlink");

    // `escape/passwd` *spells* a contained path but canonicalizes to /etc/passwd.
    let error = brain
        .call(rpc::FS_READ, fs_read("escape/passwd"))
        .await
        .expect_err("the symlink target is outside the root");
    match typed(&error) {
        TypedError::PathDenied { path, reason } => {
            assert_eq!(reason, "outside roots");
            assert!(path.contains("/etc/passwd"), "{path}");
        }
        other => panic!("expected PathDenied, got {other:?}"),
    }

    // A symlink that stays inside the root still works: the policy is about
    // where a path lands, not about links being suspicious.
    side.write("real.txt", "inside\n");
    std::os::unix::fs::symlink(side.root.join("real.txt"), side.root.join("inner"))
        .expect("symlink");
    let value = call(&brain, rpc::FS_READ, fs_read("inner")).await;
    assert!(text(&value).contains("inside"), "{value}");
}

#[tokio::test]
async fn the_deny_list_wins_over_a_root_that_contains_it() {
    let (side, socket) = node_pair(|dir, root| {
        let mut config = permissive_config(dir, root);
        // An owner may point a root at a whole tree; the deny list is what
        // keeps the credential-shaped part of it out of reach.
        config.deny = vec![root.join("secrets")];
        config
    });
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;
    side.write("secrets/token.txt", "hunter2\n");

    let error = brain
        .call(rpc::FS_READ, fs_read("secrets/token.txt"))
        .await
        .expect_err("deny-listed");
    match typed(&error) {
        TypedError::PathDenied { reason, .. } => assert_eq!(reason, "deny-list"),
        other => panic!("expected PathDenied, got {other:?}"),
    }

    // The node's own identity is unreachable either way: it sits outside every
    // configured root here, and at the default data dir it is additionally on
    // the tools crate's default deny list (see `config`'s unit tests). A bot
    // must not be able to read the key that signs this node's upgrades.
    let identity_path = side.runtime.paths.identity();
    let error = brain
        .call(
            rpc::FS_READ,
            json!({"inv": inv_value(), "path": identity_path.display().to_string()}),
        )
        .await
        .expect_err("the identity is outside the roots");
    assert!(
        matches!(typed(&error), TypedError::PathDenied { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_default_data_dir_identity_is_on_the_default_deny_list() {
    // The other half of the same guarantee, for a node installed the normal way.
    let default_identity = swarmbot_node::NodePaths::default_dir().join("identity.json");
    let policy = swarmbot_node::config::default_ceilings();
    let _ = policy;
    let config = swarmbot_node::NodeConfig::default();
    assert!(
        config.path_policy().is_denied(&default_identity),
        "{} must be deny-listed by default",
        default_identity.display()
    );
}

#[tokio::test]
async fn a_read_only_root_refuses_a_write_but_allows_a_read() {
    let (side, socket) = node_pair(read_only_root);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    let hello = authenticate(&brain, &side).await;
    assert_eq!(root_mode(&hello, "reference").as_deref(), Some("read_only"));

    let reference = side.tmp.path().join("reference");
    std::fs::write(reference.join("spec.md"), "read me\n").expect("seed the read-only root");

    let value = call(
        &brain,
        rpc::FS_READ,
        json!({"inv": inv_value(), "path": reference.join("spec.md").display().to_string()}),
    )
    .await;
    assert!(text(&value).contains("read me"), "{value}");

    let error = brain
        .call(
            rpc::FS_WRITE,
            json!({"inv": inv_value(), "path": reference.join("nope.md").display().to_string(),
                   "content": "x"}),
        )
        .await
        .expect_err("a read-only root refuses a write");
    match typed(&error) {
        TypedError::PathDenied { reason, .. } => assert_eq!(reason, "read-only root"),
        other => panic!("expected PathDenied, got {other:?}"),
    }
}

#[tokio::test]
async fn a_conditional_write_is_enforced_and_a_stale_one_fails_as_an_edit() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    // `expectedSha256: ""` means "must not exist".
    side.write("exists.txt", "already here");
    let error = brain
        .call(
            rpc::FS_WRITE,
            json!({"inv": inv_value(), "path": "exists.txt", "content": "clobber",
                   "expectedSha256": ""}),
        )
        .await
        .expect_err("the file exists, so the condition cannot hold");
    match typed(&error) {
        TypedError::EditFailed { path, reason } => {
            assert!(path.contains("exists.txt"), "{path}");
            assert!(reason.contains("expected_sha256"), "{reason}");
        }
        other => panic!("expected EditFailed, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(side.root.join("exists.txt")).expect("reads"),
        "already here",
        "a refused conditional write must leave the file exactly as it was"
    );
}

#[tokio::test]
async fn a_conditional_edit_is_refused_because_the_local_call_cannot_carry_it() {
    // §5.1: `NodeCall::FsEdit` has no `expected_sha256`, so honouring the wire's
    // condition would mean dropping it — which turns a stale-write check into a
    // clobber. Refusing is the honest answer.
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;
    side.write("a.txt", "one");

    let error = brain
        .call(
            rpc::FS_EDIT,
            json!({"inv": inv_value(), "path": "a.txt", "oldString": "one", "newString": "two",
                   "expectedSha256": "deadbeef"}),
        )
        .await
        .expect_err("refused rather than silently unconditional");
    match typed(&error) {
        TypedError::PolicyRefused { reason, retry, .. } => {
            assert!(reason.contains("expectedSha256"), "{reason}");
            assert!(!retry);
        }
        other => panic!("expected PolicyRefused, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(side.root.join("a.txt")).expect("reads"),
        "one",
        "nothing was edited"
    );
}

#[tokio::test]
async fn a_base64_write_body_is_refused_because_it_cannot_be_carried() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;
    let error = brain
        .call(
            rpc::FS_WRITE,
            json!({"inv": inv_value(), "path": "bin.dat", "content": "AAEC", "encoding": "base64"}),
        )
        .await
        .expect_err("refused");
    match typed(&error) {
        TypedError::PolicyRefused { reason, retry, .. } => {
            assert!(reason.contains("base64"), "{reason}");
            assert!(!retry);
        }
        other => panic!("expected PolicyRefused, got {other:?}"),
    }
    assert!(!side.root.join("bin.dat").exists(), "nothing was written");
}

#[tokio::test]
async fn a_move_that_promised_not_to_clobber_refuses_and_moves_nothing() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;
    side.write("from.txt", "new");
    side.write("to.txt", "existing");

    let error = brain
        .call(
            rpc::FS_MOVE,
            json!({"inv": inv_value(), "from": "from.txt", "to": "to.txt", "overwrite": false}),
        )
        .await
        .expect_err("`overwrite: false` and the target is there");
    match typed(&error) {
        TypedError::FsError { code, reason, .. } => {
            assert_eq!(code, "EEXIST");
            assert!(reason.contains("nothing was moved"), "{reason}");
        }
        other => panic!("expected FsError, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(side.root.join("to.txt")).expect("reads"),
        "existing",
        "the target is untouched"
    );
    assert!(side.root.join("from.txt").exists(), "so is the source");

    // `overwrite: true` is what `rename` does anyway, so it goes through.
    call(
        &brain,
        rpc::FS_MOVE,
        json!({"inv": inv_value(), "from": "from.txt", "to": "to.txt", "overwrite": true}),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(side.root.join("to.txt")).expect("reads"),
        "new"
    );
}

#[tokio::test]
async fn a_missing_file_is_an_fs_error_with_an_errno_code() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;
    let error = brain
        .call(rpc::FS_READ, fs_read("nope.txt"))
        .await
        .expect_err("there is no such file");
    match typed(&error) {
        TypedError::FsError {
            op, code, reason, ..
        } => {
            assert_eq!(op, rpc::FS_READ);
            assert_eq!(code, "ENOENT", "the errno is read out of the tools message");
            assert!(reason.contains("nope.txt"), "{reason}");
        }
        other => panic!("expected FsError, got {other:?}"),
    }
}

#[tokio::test]
async fn a_malformed_payload_is_a_typed_refusal() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;
    // A missing `inv`, a missing `path`, a wrong-typed `path`, and a `fs.glob`
    // without its required `cwd`: each is the brain's mistake, so each is a
    // non-retryable refusal that names the method.
    for (method, params) in [
        (rpc::FS_READ, json!({"path": "a.txt"})),
        (rpc::FS_READ, json!({"inv": inv_value()})),
        (rpc::FS_READ, json!({"inv": inv_value(), "path": 3})),
        (rpc::FS_GLOB, json!({"inv": inv_value(), "pattern": "*.rs"})),
        (rpc::FS_GREP, json!({"inv": inv_value(), "pattern": "x"})),
    ] {
        let error = brain
            .call(method, params.clone())
            .await
            .expect_err("a payload the brain got wrong is a refusal, not an answer");
        match typed(&error) {
            TypedError::PolicyRefused {
                action,
                reason,
                retry,
            } => {
                assert_eq!(action, method);
                assert!(reason.contains("malformed params"), "{method}: {reason}");
                assert!(!retry, "the brain built the payload; retrying cannot help");
            }
            other => panic!("{method}: expected PolicyRefused, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn every_invocation_lands_in_the_audit_log() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    call(&brain, rpc::FS_WRITE, fs_write("audit.txt", "x")).await;
    call(&brain, rpc::FS_READ, fs_read("audit.txt")).await;
    let _ = brain
        .call(rpc::FS_READ, fs_read("/etc/passwd"))
        .await
        .expect_err("denied");

    let path = side
        .runtime
        .paths
        .audit()
        .join(format!("{}.jsonl", side.node_id()));
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} was not written: {error}", path.display()));
    let lines: Vec<Value> = content
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON object per line"))
        .collect();
    assert_eq!(lines.len(), 3, "one line per invocation: {content}");

    let write = &lines[0];
    assert_eq!(write["tool"], json!("write_file"));
    assert_eq!(write["surface"], json!("write_file"));
    assert_eq!(
        write["decision"],
        json!("run"),
        "an `always` ceiling does not ask"
    );
    assert_eq!(write["outcome"], json!("ok"));
    assert_eq!(write["node_id"], json!(side.node_id().to_string()));
    assert!(
        write["target"]
            .as_str()
            .is_some_and(|t| t.contains("audit.txt")),
        "{write}"
    );
    assert!(!write["target"].as_str().is_some_and(|t| t.is_empty()));

    let read = &lines[1];
    assert_eq!(read["tool"], json!("read_file"));
    assert_eq!(read["surface"], json!("read_file"));
    assert!(read["duration_ms"].as_u64().is_some(), "{read}");
    assert!(read["result_bytes"].as_u64().is_some(), "{read}");

    // A denial is audited too, and says so.
    let denied = &lines[2];
    assert_eq!(denied["outcome"], json!("error"));
    assert!(
        denied["target"]
            .as_str()
            .is_some_and(|t| t.contains("/etc/passwd")),
        "{denied}"
    );
    // The audit line never carries file contents.
    assert!(!content.contains("hunter2"));
}

#[tokio::test]
async fn a_failed_audit_append_does_not_fail_the_call() {
    // AGENTS.md: the audit log never fails a tool call, and a dropped line is
    // logged loudly rather than silently. Put a regular file where the audit
    // *directory* has to be, so every append must fail.
    let (side, socket) = node_pair(permissive_config);
    let audit_path = side
        .runtime
        .paths
        .audit()
        .join(format!("{}.jsonl", side.node_id()));
    let _ = std::fs::remove_file(&audit_path);
    // A directory where the append-only file has to be: every `open` for append
    // must fail with EISDIR, and `record` must absorb it.
    std::fs::create_dir_all(&audit_path).expect("blocker");
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    side.write("still-works.txt", "yes");
    let value = call(&brain, rpc::FS_READ, fs_read("still-works.txt")).await;
    assert!(text(&value).contains("yes"), "{value}");
}
