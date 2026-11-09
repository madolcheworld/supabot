//! `write_file` — atomic, optionally conditional writes.
//!
//! Two invariants:
//!
//! * **atomicity**: a reader (another tool call, a crashed turn's replay)
//!   never sees a half-written file. The bytes go to a temp file in the
//!   *same directory* — same filesystem, so the rename is atomic — are
//!   fsynced, then renamed over the target;
//! * **conditional writes**: `expected_sha256` makes a write fail instead of
//!   clobbering concurrent work, and `expected_sha256: ""` means "create, but
//!   never overwrite". The check runs before anything is written.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use sha2::Digest as _;
use swarmbot_contracts::{Surface, ToolError, ToolResult, ToolSpec, Toolset};

use super::{
    TOOL_WRITE_FILE, ToolEnv, blocking, display, forward_remote, gate_call, opt_bool, opt_str,
    req_str, stamp_meta, target_node,
};
use crate::handler::{NodeCall, ToolContext, ToolHandler};

pub fn spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_WRITE_FILE,
        "Write text to a file atomically (temp file in the same directory, fsync, rename). \
         Pass `expected_sha256` to make the write conditional: the current sha256 of the file, \
         or \"\" to require that the file does not exist yet — on mismatch nothing is written. \
         Returns the new sha256 and size. `create_parents: true` creates missing parent \
         directories.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "File to write; relative paths resolve against the workspace."
                },
                "content": {
                    "type": "string",
                    "description": "Full new content of the file."
                },
                "expected_sha256": {
                    "type": "string",
                    "description": "Hex sha256 the file must currently have, or \"\" meaning it must not exist. Omit for an unconditional write."
                },
                "create_parents": {
                    "type": "boolean",
                    "description": "Create missing parent directories; defaults to false."
                },
                "node": {
                    "type": "string",
                    "description": "Node id (or \"local\"); defaults to the bot's host node."
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
        Toolset::Node,
    )
    .needs_node()
}

pub struct WriteFileHandler {
    env: Arc<ToolEnv>,
}

impl WriteFileHandler {
    pub fn new(env: Arc<ToolEnv>) -> Self {
        Self { env }
    }
}

impl std::fmt::Debug for WriteFileHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteFileHandler").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl ToolHandler for WriteFileHandler {
    fn spec(&self) -> ToolSpec {
        spec()
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let path = req_str(&args, "path")?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::invalid("`content` is required and must be a string"))?
            .to_string();
        let expected_sha256 = opt_str(&args, "expected_sha256").or_else(|| {
            // `""` is meaningful ("must not exist"), so opt_str's empty
            // filter must not swallow it; recover it here.
            args.get("expected_sha256")
                .and_then(Value::as_str)
                .filter(|s| s.is_empty())
                .map(str::to_string)
        });
        let create_parents = opt_bool(&args, "create_parents")?.unwrap_or(false);
        let node = target_node(ctx, &args)?;

        if !node.is_local() {
            return forward_remote(
                ctx,
                &self.env,
                TOOL_WRITE_FILE,
                node,
                NodeCall::FsWrite {
                    path,
                    content,
                    expected_sha256,
                    create_parents: Some(create_parents),
                },
            )
            .await;
        }

        let resolved = self
            .env
            .policy
            .resolve(Path::new(&path), &ctx.workspace, true)?;
        let target = display(&resolved);
        let gate = gate_call(
            ctx,
            &self.env,
            Surface::WriteFile,
            TOOL_WRITE_FILE,
            &target,
            false,
            None,
        )
        .await?;
        if let Some(mut result) = gate.blocked_result(TOOL_WRITE_FILE, &target) {
            stamp_meta(&mut result, &gate, Surface::WriteFile, &target, &node);
            return Ok(result);
        }

        let mut result = blocking(move || {
            fs_write_file(
                &resolved,
                &content,
                expected_sha256.as_deref(),
                create_parents,
            )
        })
        .await?;
        stamp_meta(&mut result, &gate, Surface::WriteFile, &target, &node);
        Ok(result)
    }
}

/// Lowercase hex sha256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Write `bytes` to `path` atomically: temp file in the same directory,
/// fsync, rename. The temp name carries a uuid v7 so two concurrent writers
/// to the same target cannot share a temp file — one of the two renames wins
/// and the loser's file is already gone or gets cleaned up.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = parent.join(format!(".{name}.tmp-{}", uuid::Uuid::now_v7().simple()));

    fn write_tmp(tmp: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::File::create(tmp)?;
        file.write_all(bytes)?;
        // Durability before visibility: the rename must never expose a file
        // whose contents are still in flight.
        file.sync_all()?;
        std::fs::rename(tmp, target)?;
        Ok(())
    }

    if let Err(error) = write_tmp(&tmp, path, bytes) {
        // Best effort: a leftover temp file is litter, not corruption.
        let _ = std::fs::remove_file(&tmp);
        return Err(ToolError::failed(format!(
            "cannot write {}: {error}",
            path.display()
        )));
    }
    Ok(())
}

/// The shared implementation, used by the handler and by
/// [`crate::LocalExecutor`]. `path` must already be resolved through the
/// executing node's path policy.
pub(crate) fn fs_write_file(
    path: &Path,
    content: &str,
    expected_sha256: Option<&str>,
    create_parents: bool,
) -> Result<ToolResult, ToolError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && !parent.is_dir()
    {
        if create_parents {
            std::fs::create_dir_all(parent).map_err(|error| {
                ToolError::failed(format!(
                    "cannot create parent directory {}: {error}",
                    parent.display()
                ))
            })?;
        } else {
            return Err(ToolError::failed(format!(
                "parent directory {} does not exist; pass create_parents=true to create it",
                parent.display()
            )));
        }
    }

    // The conditional check runs before any write: a mismatch must leave the
    // file exactly as it was.
    if let Some(expected) = expected_sha256 {
        let expected = expected.trim().to_ascii_lowercase();
        if expected.is_empty() {
            if let Ok(existing) = std::fs::read(path) {
                return Err(ToolError::failed(format!(
                    "{} already exists (sha256 {}), but expected_sha256=\"\" requires that it does not; refusing to overwrite",
                    path.display(),
                    sha256_hex(&existing)
                )));
            }
        } else {
            let existing = std::fs::read(path).map_err(|_| {
                ToolError::failed(format!(
                    "{} does not exist, but expected_sha256 {expected} was given; write it without expected_sha256 to create it",
                    path.display()
                ))
            })?;
            let actual = sha256_hex(&existing);
            if actual != expected {
                return Err(ToolError::failed(format!(
                    "expected_sha256 mismatch for {}: the file is {actual} but the call expected {expected}; re-read the file and retry from its current content",
                    path.display()
                )));
            }
        }
    }

    atomic_write(path, content.as_bytes())?;
    let hash = sha256_hex(content.as_bytes());
    let size = content.len();
    Ok(ToolResult::text(format!(
        "wrote {size} bytes to {}\nsha256: {hash}",
        path.display()
    ))
    .with_meta("sha256", hash.clone())
    .with_meta("size", size)
    .with_meta("path", path.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::testkit::Rig;
    use serde_json::json;

    fn handler(rig: &Rig) -> WriteFileHandler {
        WriteFileHandler::new(Arc::clone(&rig.env))
    }

    #[tokio::test]
    async fn writes_and_reports_hash_and_size() {
        let rig = Rig::new();
        let result = rig
            .call(
                &handler(&rig),
                json!({"path": "out.txt", "content": "hello"}),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        let text = result.text_only();
        assert!(text.contains("wrote 5 bytes"), "{text}");
        let expected = sha256_hex(b"hello");
        assert!(text.contains(&expected), "{text}");
        assert_eq!(result.meta["sha256"], expected);
        assert_eq!(result.meta["size"], 5);
        assert_eq!(
            std::fs::read_to_string(rig.workspace.join("out.txt")).expect("file"),
            "hello"
        );
        // No temp-file litter left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&rig.workspace)
            .expect("dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[tokio::test]
    async fn a_missing_parent_needs_create_parents() {
        let rig = Rig::new();
        let refused = rig
            .call(
                &handler(&rig),
                json!({"path": "deep/dir/out.txt", "content": "x"}),
            )
            .await;
        assert!(refused.is_error);
        assert!(
            refused.text_only().contains("create_parents=true"),
            "{refused:?}"
        );
        let ok = rig
            .call(
                &handler(&rig),
                json!({"path": "deep/dir/out.txt", "content": "x", "create_parents": true}),
            )
            .await;
        assert!(!ok.is_error, "{ok:?}");
        assert!(rig.workspace.join("deep/dir/out.txt").exists());
    }

    #[tokio::test]
    async fn expected_sha256_mismatch_writes_nothing() {
        let rig = Rig::new();
        rig.write("f.txt", "current");
        let result = rig
            .call(
                &handler(&rig),
                json!({"path": "f.txt", "content": "new", "expected_sha256": sha256_hex(b"stale")}),
            )
            .await;
        assert!(result.is_error);
        let text = result.text_only();
        assert!(text.contains("expected_sha256 mismatch"), "{text}");
        assert!(text.contains(&sha256_hex(b"current")), "{text}");
        // The file is untouched: the conditional check precedes the write.
        assert_eq!(
            std::fs::read_to_string(rig.workspace.join("f.txt")).expect("file"),
            "current"
        );
    }

    #[tokio::test]
    async fn expected_sha256_match_allows_the_write() {
        let rig = Rig::new();
        rig.write("f.txt", "current");
        let result = rig
            .call(
                &handler(&rig),
                json!({"path": "f.txt", "content": "new", "expected_sha256": sha256_hex(b"current")}),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(rig.workspace.join("f.txt")).expect("file"),
            "new"
        );
    }

    #[tokio::test]
    async fn empty_expected_sha256_means_must_not_exist() {
        let rig = Rig::new();
        rig.write("exists.txt", "x");
        let refused = rig
            .call(
                &handler(&rig),
                json!({"path": "exists.txt", "content": "y", "expected_sha256": ""}),
            )
            .await;
        assert!(refused.is_error);
        assert!(
            refused.text_only().contains("refusing to overwrite"),
            "{refused:?}"
        );
        let created = rig
            .call(
                &handler(&rig),
                json!({"path": "fresh.txt", "content": "y", "expected_sha256": ""}),
            )
            .await;
        assert!(!created.is_error, "{created:?}");
    }

    #[tokio::test]
    async fn a_read_only_root_refuses_the_write() {
        let rig = Rig::new();
        let ro = rig.tmp.path().join("ro");
        std::fs::create_dir(&ro).expect("mkdir");
        let policy = Arc::new(crate::policy::PathPolicy::new(
            vec![
                swarmbot_contracts::NodeRoot::read_write(
                    "tmp",
                    rig.tmp.path().display().to_string(),
                ),
                swarmbot_contracts::NodeRoot::read_only("ro", ro.display().to_string()),
            ],
            Vec::new(),
        ));
        let env = Arc::new(ToolEnv::shared(policy, Arc::clone(&rig.env.engine)));
        let handler = WriteFileHandler::new(env);
        let result = rig
            .call(
                &handler,
                json!({"path": ro.join("x.txt").display().to_string(), "content": "y"}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.text_only().contains("read-only root"), "{result:?}");
        assert!(!ro.join("x.txt").exists());
    }
}
