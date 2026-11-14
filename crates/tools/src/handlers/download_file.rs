//! `download_file` — bring a file from a node onto the brain's machine.
//!
//! This is the caller the data-stream half of the node protocol was missing
//! (D9.21 built the transport and left it unreachable from any turn —
//! `docs/node-protocol.md` §13 said so out loud). A bot that has been told "the
//! report is on the build box" can now fetch it instead of asking a human to
//! copy it, and the bytes arrive through the same digest-checked transfer the
//! `real_brain` test pins.
//!
//! Three decisions worth stating, because each one is where this tool would
//! otherwise lie:
//!
//! * **It lands in the bot's own workspace**, under a `.downloads/` directory,
//!   and the model is told the path. Not the spool directory (that is for
//!   truncated *output*), and not wherever the caller asks: a tool that writes to
//!   an arbitrary path on the brain's machine is a remote-write surface, which is
//!   exactly what the node's own roots exist to bound.
//! * **It is a read on the node and a write here.** The gate is asked about the
//!   *source* (`Surface::ReadFile`) because that is the permission being
//!   exercised on the far machine; the destination is chosen by this tool, inside
//!   a workspace the owner already granted, so it needs no second card.
//! * **Local is not a special case with different semantics.** A local download
//!   copies the file through the same policy (`LocalExecutor::download`) and
//!   reports the same `{bytes, sha256}` shape, so a bot cannot tell — from its
//!   result — which transport carried the file. That also keeps the test suite
//!   honest: the local path exercises the handler, the policy and the result
//!   shape without needing a paired node.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use swarmbot_contracts::{Surface, ToolError, ToolResult, ToolSpec, Toolset};

use super::{
    TOOL_DOWNLOAD_FILE, ToolEnv, display, gate_call, opt_str, req_str, stamp_meta, target_node,
};
use crate::handler::{ToolContext, ToolHandler};

/// Where a downloaded file lands inside the bot's workspace.
///
/// A subdirectory rather than the workspace root for two reasons: a file whose
/// name collides with the bot's own work must not silently replace it, and an
/// operator looking at a workspace can tell what a bot *fetched* from what a bot
/// *made*.
pub const DOWNLOAD_DIR: &str = ".downloads";

pub fn spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_DOWNLOAD_FILE,
        "Copy a file from a node (this machine or a paired remote one) into your own workspace, \
         under `.downloads/`. Use it when a file is on another machine; for a file on the node you \
         are already working on, `read_file` is cheaper. The result names the destination path, \
         the size and the sha256 of what arrived — check it if the file matters.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The file to fetch, as a path on the node (relative paths resolve against that node's workspace)."
                },
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "description": "File name to save as, inside `.downloads/`. Defaults to the source's file name; a path separator is refused."
                },
                "node": {
                    "type": "string",
                    "description": "Node id (or \"local\"); defaults to the bot's host node."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        Toolset::Node,
    )
    .needs_node()
    .read_only()
}

pub struct DownloadFileHandler {
    env: Arc<ToolEnv>,
}

impl DownloadFileHandler {
    pub fn new(env: Arc<ToolEnv>) -> Self {
        Self { env }
    }
}

impl std::fmt::Debug for DownloadFileHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadFileHandler")
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl ToolHandler for DownloadFileHandler {
    fn spec(&self) -> ToolSpec {
        spec()
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let path = req_str(&args, "path")?;
        let node = target_node(ctx, &args)?;
        let name = match opt_str(&args, "name") {
            Some(name) => name,
            None => source_name(&path)?,
        };
        let dest = destination(ctx, &name)?;

        // One gate, on the source: the far machine is what is being read, and
        // that is the permission the card is about. The destination needs none —
        // this tool chose it, inside a workspace the owner already granted.
        let target = format!("{path} (from {node})");
        let gate = gate_call(
            ctx,
            &self.env,
            Surface::ReadFile,
            TOOL_DOWNLOAD_FILE,
            &target,
            true,
            None,
        )
        .await?;
        let mut result = match gate.blocked_result(TOOL_DOWNLOAD_FILE, &target) {
            Some(blocked) => blocked,
            None => {
                ctx.nodes
                    .download(&node, &ctx.invocation, &path, &dest)
                    .await?
            }
        };
        stamp_meta(&mut result, &gate, Surface::ReadFile, &target, &node);
        Ok(result)
    }
}

/// The name a source path would be saved under.
///
/// Refuses a path with no file name (`/`, `..`), because those have no name to
/// take and guessing one would put a file somewhere the model cannot predict.
fn source_name(path: &str) -> Result<String, ToolError> {
    let trimmed = path.trim_end_matches('/');
    let name = Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .filter(|name| !name.is_empty() && name != "." && name != "..")
        .ok_or_else(|| {
            ToolError::invalid(format!(
                "`path` ({path}) has no file name to save as; pass `name` explicitly"
            ))
        })?;
    Ok(name)
}

/// The absolute destination inside the bot's workspace.
///
/// Refuses anything that is not a plain file name: a `name` carrying a separator
/// is a request to write outside `.downloads/`, and `..` is the same request
/// spelled differently.
fn destination(ctx: &ToolContext, name: &str) -> Result<PathBuf, ToolError> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ToolError::invalid(format!(
            "`name` ({name}) must be a plain file name: a download lands in `{DOWNLOAD_DIR}/` \
             inside your workspace, and a path here would put it somewhere else"
        )));
    }
    let dir = ctx.workspace.join(DOWNLOAD_DIR);
    std::fs::create_dir_all(&dir).map_err(|error| {
        ToolError::failed(format!(
            "cannot create the download directory {}: {error}",
            display(&dir)
        ))
    })?;
    Ok(dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_name_is_the_last_segment_and_a_bare_directory_is_refused() {
        assert_eq!(source_name("/srv/reports/q3.csv").expect("name"), "q3.csv");
        assert_eq!(source_name("notes.md").expect("name"), "notes.md");
        assert_eq!(source_name("/srv/dir/").expect("name"), "dir");
        for bad in ["/", "..", "a/.."] {
            assert!(
                source_name(bad).is_err(),
                "{bad} has no file name and must not be guessed"
            );
        }
    }

    #[test]
    fn a_destination_never_escapes_the_download_directory() {
        let rig = crate::handlers::testkit::Rig::new();
        let ctx = &rig.ctx;
        for bad in ["../escape.txt", "sub/dir.txt", "a\\b.txt", ".."] {
            let refused = destination(ctx, bad);
            assert!(refused.is_err(), "{bad} must be refused, got {refused:?}");
        }
        let inside = destination(ctx, "report.csv").expect("a plain name");
        assert!(
            inside.starts_with(ctx.workspace.join(DOWNLOAD_DIR)),
            "{}",
            inside.display()
        );
    }
}
