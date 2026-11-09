//! `list_dir` — what is actually on disk, as sorted `type size path` lines.
//!
//! Deliberately dumb: it lists what is there and honours no ignore file.
//! That is the honest division of labour with `glob`/`grep`, which *do*
//! respect `.gitignore` and friends; a model that wonders why a build
//! artifact shows up here but not in a grep result finds the answer in the
//! tool descriptions.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use swarmbot_contracts::{Surface, ToolError, ToolResult, ToolSpec, Toolset};
use walkdir::{DirEntry, WalkDir};

use super::{
    DEFAULT_LIST_LIMIT, TOOL_LIST_DIR, ToolEnv, blocking, display, forward_remote, gate_call,
    opt_bool, opt_u32, req_str, stamp_meta, target_node,
};
use crate::handler::{NodeCall, ToolContext, ToolHandler};
use crate::policy::PathPolicy;

pub fn spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_LIST_DIR,
        "List a directory as sorted `type size path` lines (type: d=directory, f=file, \
         l=symlink; size in bytes, `-` for directories). `depth` is how many levels to descend \
         (default 1 = immediate children; 0 = unlimited). Hidden entries are skipped unless \
         `include_hidden`. Shows what is on disk — .gitignore is NOT respected here; use glob or \
         grep when you want ignore files honoured.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Directory to list; relative paths resolve against the workspace."
                },
                "depth": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Levels to descend; 1 (default) lists immediate children, 0 is unlimited."
                },
                "include_hidden": {
                    "type": "boolean",
                    "description": "Include dotfiles and descend into dot-directories; defaults to false."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum entries to return; defaults to 1000."
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

pub struct ListDirHandler {
    env: Arc<ToolEnv>,
}

impl ListDirHandler {
    pub fn new(env: Arc<ToolEnv>) -> Self {
        Self { env }
    }
}

impl std::fmt::Debug for ListDirHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListDirHandler").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl ToolHandler for ListDirHandler {
    fn spec(&self) -> ToolSpec {
        spec()
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let path = req_str(&args, "path")?;
        let depth = opt_u32(&args, "depth")?;
        let include_hidden = opt_bool(&args, "include_hidden")?.unwrap_or(false);
        let limit = opt_u32(&args, "limit")?.unwrap_or(DEFAULT_LIST_LIMIT);
        let node = target_node(ctx, &args)?;

        if !node.is_local() {
            return forward_remote(
                ctx,
                &self.env,
                TOOL_LIST_DIR,
                node,
                NodeCall::FsList {
                    path,
                    depth,
                    include_hidden: Some(include_hidden),
                    limit: Some(limit),
                },
            )
            .await;
        }

        let resolved = self
            .env
            .policy
            .resolve(Path::new(&path), &ctx.workspace, false)?;
        let target = display(&resolved);
        let gate = gate_call(
            ctx,
            &self.env,
            Surface::ReadFile,
            TOOL_LIST_DIR,
            &target,
            true,
            None,
        )
        .await?;
        if let Some(mut result) = gate.blocked_result(TOOL_LIST_DIR, &target) {
            stamp_meta(&mut result, &gate, Surface::ReadFile, &target, &node);
            return Ok(result);
        }

        let policy = Arc::clone(&self.env.policy);
        let base = resolved.clone();
        let mut result =
            blocking(move || fs_list_dir(&policy, &base, depth, include_hidden, limit)).await?;
        stamp_meta(&mut result, &gate, Surface::ReadFile, &target, &node);
        Ok(result)
    }
}

/// The shared implementation, used by the handler and by
/// [`crate::LocalExecutor`].
pub(crate) fn fs_list_dir(
    policy: &PathPolicy,
    root: &Path,
    depth: Option<u32>,
    include_hidden: bool,
    limit: u32,
) -> Result<ToolResult, ToolError> {
    if !root.is_dir() {
        return Err(ToolError::failed(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    // depth: None → 1 (immediate children, the documented default);
    // Some(0) → unlimited; Some(n) → n levels below the root (walkdir
    // counts the root itself as depth 0).
    let max_depth = match depth {
        None => Some(1),
        Some(0) => None,
        Some(n) => Some(n as usize),
    };

    let mut entries: Vec<(char, Option<u64>, String)> = Vec::new();
    // Prune whole subtrees that are hidden or deny-listed: cheaper than
    // filtering leaves and it keeps the walker out of `.git` object stores.
    let iter = WalkDir::new(root)
        .max_depth(max_depth.unwrap_or(usize::MAX))
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if !include_hidden && is_hidden(entry) {
                return false;
            }
            !policy.is_denied(entry.path())
        });
    for item in iter {
        // A walk error mid-tree (a vanished directory, permissions) skips
        // that entry instead of failing the whole listing.
        let Ok(entry) = item else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let kind = if entry.file_type().is_dir() {
            'd'
        } else if entry.file_type().is_symlink() {
            'l'
        } else {
            'f'
        };
        let size = if kind == 'f' {
            entry.metadata().ok().map(|meta| meta.len())
        } else {
            None
        };
        entries.push((kind, size, entry.path().display().to_string()));
    }

    entries.sort_by(|a, b| a.2.cmp(&b.2));
    let total = entries.len();
    let truncated = total > limit as usize;
    entries.truncate(limit as usize);

    if entries.is_empty() {
        return Ok(ToolResult::text("(empty directory)").with_meta("count", 0u64));
    }
    let mut text = String::new();
    for (kind, size, path) in &entries {
        let size_text = size
            .map(|size| size.to_string())
            .unwrap_or_else(|| "-".to_string());
        text.push_str(&format!("{kind} {size_text:>10} {path}\n"));
    }
    if truncated {
        text.push_str(&format!(
            "[showing the first {limit} of {total} entries; pass a larger limit or use glob to narrow down]"
        ));
    } else {
        text.pop();
    }
    Ok(ToolResult::text(text)
        .with_meta("count", entries.len())
        .with_meta("truncated", truncated))
}

fn is_hidden(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| name.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::testkit::Rig;
    use serde_json::json;

    fn handler(rig: &Rig) -> ListDirHandler {
        ListDirHandler::new(Arc::clone(&rig.env))
    }

    #[tokio::test]
    async fn lists_entries_sorted_with_type_and_size() {
        let rig = Rig::new();
        rig.write("b.txt", "12345");
        rig.write("a.txt", "x");
        std::fs::create_dir(rig.workspace.join("sub")).expect("mkdir");
        let result = rig.call(&handler(&rig), json!({"path": "."})).await;
        assert!(!result.is_error, "{result:?}");
        let text = result.text_only();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        // Sorted by path: a.txt, b.txt, sub.
        assert!(lines[0].starts_with('f'), "{text}");
        assert!(lines[0].contains("a.txt"), "{text}");
        assert!(lines[1].contains("b.txt"), "{text}");
        assert!(
            lines[1].contains('5'),
            "the size column shows 5 bytes: {text}"
        );
        assert!(lines[2].starts_with('d'), "{text}");
        assert!(lines[2].contains("sub"), "{text}");
        // Default depth 1: the child of `sub` is not listed.
        rig.write("sub/inner.txt", "y");
        let shallow = rig.call(&handler(&rig), json!({"path": "."})).await;
        assert!(!shallow.text_only().contains("inner.txt"));
        let deep = rig
            .call(&handler(&rig), json!({"path": ".", "depth": 2}))
            .await;
        assert!(deep.text_only().contains("inner.txt"), "{deep:?}");
    }

    #[tokio::test]
    async fn hidden_entries_need_the_flag() {
        let rig = Rig::new();
        rig.write(".envrc", "x=1");
        rig.write("visible.txt", "y");
        let plain = rig.call(&handler(&rig), json!({"path": "."})).await;
        assert!(!plain.text_only().contains(".envrc"), "{plain:?}");
        let all = rig
            .call(&handler(&rig), json!({"path": ".", "include_hidden": true}))
            .await;
        assert!(all.text_only().contains(".envrc"), "{all:?}");
    }

    #[tokio::test]
    async fn the_limit_truncates_and_says_so() {
        let rig = Rig::new();
        for i in 0..5 {
            rig.write(&format!("f{i}.txt"), "x");
        }
        let result = rig
            .call(&handler(&rig), json!({"path": ".", "limit": 2}))
            .await;
        let text = result.text_only();
        assert!(text.contains("first 2 of 5"), "{text}");
        assert_eq!(result.meta["truncated"], true);
        assert_eq!(result.meta["count"], 2);
    }

    #[tokio::test]
    async fn a_file_is_not_a_directory() {
        let rig = Rig::new();
        rig.write("f.txt", "x");
        let result = rig.call(&handler(&rig), json!({"path": "f.txt"})).await;
        assert!(result.is_error);
        assert!(result.text_only().contains("not a directory"), "{result:?}");
    }
}
