//! `edit_file` — exact-string replacement with a uniqueness contract.
//!
//! Invariant: **a non-`replace_all` edit changes exactly one place, and the
//! caller proves which place it meant** by supplying an `old_string` that
//! occurs exactly once. Ambiguity is an error naming the match count, not a
//! silent "first wins" — a bot editing the wrong occurrence is the classic
//! way an autonomous agent corrupts a file. The result is a unified diff so
//! the owner (and reviewing bots) see precisely what changed.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use similar::TextDiff;
use swarmbot_contracts::{Surface, ToolError, ToolResult, ToolSpec, Toolset};

use super::write_file::{atomic_write, sha256_hex};
use super::{
    TOOL_EDIT_FILE, ToolEnv, blocking, display, forward_remote, gate_call, opt_bool, req_str,
    stamp_meta, target_node,
};
use crate::handler::{NodeCall, ToolContext, ToolHandler};

pub fn spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_EDIT_FILE,
        "Replace an exact string in a text file. `old_string` must match exactly one place \
         unless `replace_all: true` — include enough surrounding lines to make it unique; an \
         ambiguous match is refused with the number of matches. Whitespace and indentation must \
         match exactly (read_file first and copy from its output, without the line-number \
         gutter). Returns a unified diff plus the new sha256.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "File to edit; relative paths resolve against the workspace."
                },
                "old_string": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The exact text to replace; must occur exactly once unless replace_all is true."
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement text."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence instead of requiring a unique one; defaults to false."
                },
                "node": {
                    "type": "string",
                    "description": "Node id (or \"local\"); defaults to the bot's host node."
                }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        }),
        Toolset::Node,
    )
    .needs_node()
}

pub struct EditFileHandler {
    env: Arc<ToolEnv>,
}

impl EditFileHandler {
    pub fn new(env: Arc<ToolEnv>) -> Self {
        Self { env }
    }
}

impl std::fmt::Debug for EditFileHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditFileHandler").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl ToolHandler for EditFileHandler {
    fn spec(&self) -> ToolSpec {
        spec()
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let path = req_str(&args, "path")?;
        let old_string = req_str(&args, "old_string")?;
        // `new_string` may legitimately be empty (deleting the match), so it
        // is fetched without req_str's implicit shape checks.
        let new_string = args
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::invalid("`new_string` is required and must be a string"))?
            .to_string();
        let replace_all = opt_bool(&args, "replace_all")?.unwrap_or(false);
        let node = target_node(ctx, &args)?;

        if !node.is_local() {
            return forward_remote(
                ctx,
                &self.env,
                TOOL_EDIT_FILE,
                node,
                NodeCall::FsEdit {
                    path,
                    old_string,
                    new_string,
                    replace_all: Some(replace_all),
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
            Surface::EditFile,
            TOOL_EDIT_FILE,
            &target,
            false,
            None,
        )
        .await?;
        if let Some(mut result) = gate.blocked_result(TOOL_EDIT_FILE, &target) {
            stamp_meta(&mut result, &gate, Surface::EditFile, &target, &node);
            return Ok(result);
        }

        let mut result =
            blocking(move || fs_edit_file(&resolved, &old_string, &new_string, replace_all))
                .await?;
        stamp_meta(&mut result, &gate, Surface::EditFile, &target, &node);
        Ok(result)
    }
}

/// The shared implementation, used by the handler and by
/// [`crate::LocalExecutor`]. `path` must already be resolved through the
/// executing node's path policy.
pub(crate) fn fs_edit_file(
    path: &Path,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<ToolResult, ToolError> {
    if old_string.is_empty() {
        return Err(ToolError::invalid(
            "old_string must not be empty; use write_file to create or replace a whole file",
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| ToolError::failed(format!("cannot read {}: {error}", path.display())))?;
    let content = String::from_utf8(bytes).map_err(|_| {
        ToolError::failed(format!(
            "{} is not valid UTF-8 — edit_file only edits text files",
            path.display()
        ))
    })?;

    let matches = content.matches(old_string).count();
    if matches == 0 {
        return Err(ToolError::failed(format!(
            "old_string not found in {} — read_file it and copy the exact text; whitespace and indentation matter",
            path.display()
        )));
    }
    if matches > 1 && !replace_all {
        return Err(ToolError::failed(format!(
            "old_string has {matches} matches in {}; extend it with surrounding lines until it is unique, or pass replace_all=true to change all {matches}",
            path.display()
        )));
    }

    let updated = if replace_all {
        content.replace(old_string, new_string)
    } else {
        content.replacen(old_string, new_string, 1)
    };
    atomic_write(path, updated.as_bytes())?;

    let hash = sha256_hex(updated.as_bytes());
    let replacements = if replace_all { matches } else { 1 };
    let text = if updated == content {
        format!("no change (old_string equals new_string)\nsha256: {hash}")
    } else {
        let mut diff = unified_diff(&content, &updated, path);
        if !diff.ends_with('\n') {
            diff.push('\n');
        }
        diff.push_str(&format!("sha256: {hash}"));
        diff
    };
    Ok(ToolResult::text(text)
        .with_meta("sha256", hash)
        .with_meta("replacements", replacements)
        .with_meta("path", path.display().to_string()))
}

fn unified_diff(old: &str, new: &str, path: &Path) -> String {
    let name = path.display().to_string();
    let diff = TextDiff::from_lines(old, new);
    diff.unified_diff()
        .header(&format!("a/{name}"), &format!("b/{name}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::testkit::Rig;
    use serde_json::json;

    fn handler(rig: &Rig) -> EditFileHandler {
        EditFileHandler::new(Arc::clone(&rig.env))
    }

    #[tokio::test]
    async fn edits_a_unique_match_and_returns_a_diff() {
        let rig = Rig::new();
        rig.write("app.rs", "fn main() {\n    println!(\"hi\");\n}\n");
        let result = rig
            .call(
                &handler(&rig),
                json!({
                    "path": "app.rs",
                    "old_string": "println!(\"hi\");",
                    "new_string": "println!(\"hello\");",
                }),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        let text = result.text_only();
        assert!(text.contains("-    println!(\"hi\");"), "{text}");
        assert!(text.contains("+    println!(\"hello\");"), "{text}");
        assert!(text.contains("--- a/"), "{text}");
        assert!(text.contains("+++ b/"), "{text}");
        assert!(text.contains("sha256: "), "{text}");
        assert_eq!(result.meta["replacements"], 1);
        let updated = std::fs::read_to_string(rig.workspace.join("app.rs")).expect("file");
        assert_eq!(updated, "fn main() {\n    println!(\"hello\");\n}\n");
        assert_eq!(result.meta["sha256"], sha256_hex(updated.as_bytes()));
    }

    #[tokio::test]
    async fn an_ambiguous_match_is_refused_with_the_count() {
        let rig = Rig::new();
        rig.write("dup.txt", "todo: fix\nmiddle\ntodo: fix\n");
        let result = rig
            .call(
                &handler(&rig),
                json!({"path": "dup.txt", "old_string": "todo: fix", "new_string": "done"}),
            )
            .await;
        assert!(result.is_error);
        let text = result.text_only();
        assert!(text.starts_with("ERROR: "), "{text}");
        assert!(text.contains("2 matches"), "{text}");
        assert!(text.contains("replace_all"), "{text}");
        // Nothing was written.
        assert_eq!(
            std::fs::read_to_string(rig.workspace.join("dup.txt")).expect("file"),
            "todo: fix\nmiddle\ntodo: fix\n"
        );
    }

    #[tokio::test]
    async fn replace_all_changes_every_occurrence() {
        let rig = Rig::new();
        rig.write("dup.txt", "aXa X bXb\n");
        let result = rig
            .call(
                &handler(&rig),
                json!({"path": "dup.txt", "old_string": "X", "new_string": "Y", "replace_all": true}),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.meta["replacements"], 3);
        assert_eq!(
            std::fs::read_to_string(rig.workspace.join("dup.txt")).expect("file"),
            "aYa Y bYb\n"
        );
    }

    #[tokio::test]
    async fn a_missing_match_names_the_file() {
        let rig = Rig::new();
        rig.write("f.txt", "content\n");
        let result = rig
            .call(
                &handler(&rig),
                json!({"path": "f.txt", "old_string": "absent", "new_string": "x"}),
            )
            .await;
        assert!(result.is_error);
        let text = result.text_only();
        assert!(text.contains("old_string not found"), "{text}");
        assert!(text.contains("f.txt"), "{text}");
    }

    #[tokio::test]
    async fn an_empty_old_string_is_invalid() {
        let rig = Rig::new();
        rig.write("f.txt", "content\n");
        let result = rig
            .call(
                &handler(&rig),
                json!({"path": "f.txt", "old_string": "", "new_string": "x"}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.text_only().contains("must not be empty"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_noop_edit_says_so() {
        let rig = Rig::new();
        rig.write("f.txt", "same\n");
        let result = rig
            .call(
                &handler(&rig),
                json!({"path": "f.txt", "old_string": "same", "new_string": "same"}),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.text_only().contains("no change"), "{result:?}");
    }
}
