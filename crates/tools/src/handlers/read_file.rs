//! `read_file` — UTF-8 text with 1-based line numbers.
//!
//! The line numbers are the contract: `edit_file` diffs and `grep` results
//! refer to them, so the format `  12→content` is fixed and every page a
//! model sees lines up with what the other tools tell it. `offset`/`limit`
//! are lines, not bytes, which is what makes paging a spilled file or a big
//! source file predictable.

use std::io::{BufRead as _, BufReader};
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use swarmbot_contracts::{Surface, ToolError, ToolResult, ToolSpec, Toolset};

use super::{
    DEFAULT_READ_LINE_LIMIT, TOOL_READ_FILE, ToolEnv, blocking, display, forward_remote, gate_call,
    opt_str, opt_u64, req_str, stamp_meta, target_node,
};
use crate::handler::{NodeCall, ToolContext, ToolHandler};

pub fn spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_READ_FILE,
        "Read a UTF-8 text file with 1-based line numbers (`  12→content`) — the same numbers \
         edit_file diffs and grep results refer to. `offset`/`limit` are lines (default limit \
         2000): use them to page big files, including spilled tool output. Binary or \
         non-UTF-8 files are refused with an error saying they are not text.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "File to read; relative paths resolve against the workspace, `~/` expands."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based first line to return; defaults to 1."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of lines to return; defaults to 2000."
                },
                "encoding": {
                    "type": "string",
                    "enum": ["utf-8", "utf8", "text"],
                    "description": "Only UTF-8 text is supported; the option exists so a model can say what it expects."
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

pub struct ReadFileHandler {
    env: Arc<ToolEnv>,
}

impl ReadFileHandler {
    pub fn new(env: Arc<ToolEnv>) -> Self {
        Self { env }
    }
}

impl std::fmt::Debug for ReadFileHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadFileHandler").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl ToolHandler for ReadFileHandler {
    fn spec(&self) -> ToolSpec {
        spec()
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let path = req_str(&args, "path")?;
        let offset = opt_u64(&args, "offset")?;
        let limit = opt_u64(&args, "limit")?;
        let encoding = opt_str(&args, "encoding");
        let node = target_node(ctx, &args)?;

        if !node.is_local() {
            return forward_remote(
                ctx,
                &self.env,
                TOOL_READ_FILE,
                node,
                NodeCall::FsRead {
                    path,
                    offset,
                    limit,
                    encoding,
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
            TOOL_READ_FILE,
            &target,
            true,
            None,
        )
        .await?;
        if let Some(mut result) = gate.blocked_result(TOOL_READ_FILE, &target) {
            stamp_meta(&mut result, &gate, Surface::ReadFile, &target, &node);
            return Ok(result);
        }

        let mut result =
            blocking(move || fs_read_file(&resolved, offset, limit, encoding.as_deref())).await?;
        stamp_meta(&mut result, &gate, Surface::ReadFile, &target, &node);
        Ok(result)
    }
}

/// The shared implementation, used by the handler and by
/// [`crate::LocalExecutor`].
pub(crate) fn fs_read_file(
    path: &Path,
    offset: Option<u64>,
    limit: Option<u64>,
    encoding: Option<&str>,
) -> Result<ToolResult, ToolError> {
    if let Some(encoding) = encoding {
        let normalized = encoding.to_ascii_lowercase().replace('_', "-");
        if !matches!(normalized.as_str(), "utf-8" | "utf8" | "text") {
            return Err(ToolError::invalid(format!(
                "encoding `{encoding}` is not supported; read_file reads UTF-8 text only"
            )));
        }
    }
    let offset = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(DEFAULT_READ_LINE_LIMIT).max(1);

    let file = std::fs::File::open(path)
        .map_err(|error| ToolError::failed(format!("cannot open {}: {error}", path.display())))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut total: u64 = 0;
    let mut text = String::new();
    let mut shown: u64 = 0;

    // Streaming with a window: only the requested lines are kept in memory,
    // so reading a huge log cannot blow up the process. Line counting
    // continues past the window to make the paging note honest.
    loop {
        line.clear();
        let read = reader.read_line(&mut line).map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                ToolError::failed(format!(
                    "{} is not valid UTF-8 — it looks like a binary file, and read_file only reads text",
                    path.display()
                ))
            } else {
                ToolError::failed(format!("cannot read {}: {error}", path.display()))
            }
        })?;
        if read == 0 {
            break;
        }
        total += 1;
        if total >= offset && shown < limit {
            shown += 1;
            let content = line.trim_end_matches('\n').trim_end_matches('\r');
            // Fixed-width gutter so columns line up in transcripts.
            text.push_str(&format!("{total:>4}→{content}\n"));
        }
    }

    if total == 0 {
        return Ok(ToolResult::text("(empty file)").with_meta("total_lines", 0u64));
    }
    if shown == 0 {
        return Ok(ToolResult::text(format!(
            "(offset {offset} is past the end; {} has {total} lines)",
            path.display()
        ))
        .with_meta("total_lines", total));
    }

    let first = offset;
    let last = offset + shown - 1;
    if total > last {
        text.push_str(&format!(
            "[showing lines {first}–{last} of {total}; read_file with offset={} for the next page]",
            last + 1
        ));
    } else {
        text.pop(); // the final newline
    }

    Ok(ToolResult::text(text)
        .with_meta("total_lines", total)
        .with_meta("first_line", first)
        .with_meta("last_line", last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::testkit::Rig;
    use serde_json::json;

    fn handler(rig: &Rig) -> ReadFileHandler {
        ReadFileHandler::new(Arc::clone(&rig.env))
    }

    #[tokio::test]
    async fn numbers_lines_from_one() {
        let rig = Rig::new();
        rig.write("notes.md", "alpha\nbeta\ngamma\n");
        let result = rig.call(&handler(&rig), json!({"path": "notes.md"})).await;
        assert!(!result.is_error);
        assert_eq!(result.text_only(), "   1→alpha\n   2→beta\n   3→gamma");
        assert_eq!(result.meta["total_lines"], 3);
    }

    #[tokio::test]
    async fn offset_and_limit_page_through_a_file() {
        let rig = Rig::new();
        let content: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        rig.write("ten.txt", &content);
        let result = rig
            .call(
                &handler(&rig),
                json!({"path": "ten.txt", "offset": 3, "limit": 4}),
            )
            .await;
        let text = result.text_only();
        assert!(text.starts_with("   3→line 3"), "{text}");
        assert!(text.contains("   6→line 6"), "{text}");
        assert!(!text.contains("line 7"), "{text}");
        // The paging note names the next offset — a model can follow it
        // without doing arithmetic.
        assert!(text.contains("offset=7 for the next page"), "{text}");
        assert!(text.contains("of 10"), "{text}");
    }

    #[tokio::test]
    async fn an_offset_past_the_end_says_so() {
        let rig = Rig::new();
        rig.write("short.txt", "one\ntwo\n");
        let result = rig
            .call(&handler(&rig), json!({"path": "short.txt", "offset": 99}))
            .await;
        assert!(!result.is_error);
        assert!(result.text_only().contains("past the end"), "{result:?}");
    }

    #[tokio::test]
    async fn binary_files_are_refused_with_a_clear_message() {
        let rig = Rig::new();
        let path = rig.workspace.join("blob.bin");
        std::fs::write(&path, [0xff_u8, 0xfe, 0x00, 0x80]).expect("write");
        let result = rig.call(&handler(&rig), json!({"path": "blob.bin"})).await;
        assert!(result.is_error);
        let text = result.text_only();
        assert!(text.starts_with("ERROR: "), "{text}");
        assert!(text.contains("not valid UTF-8"), "{text}");
        assert!(text.contains("binary"), "{text}");
    }

    #[tokio::test]
    async fn a_missing_file_reads_as_an_error() {
        let rig = Rig::new();
        let result = rig.call(&handler(&rig), json!({"path": "nope.txt"})).await;
        assert!(result.is_error);
        assert!(result.text_only().contains("cannot open"), "{result:?}");
    }

    #[tokio::test]
    async fn an_unsupported_encoding_is_invalid_args() {
        let rig = Rig::new();
        rig.write("f.txt", "x");
        let result = rig
            .call(
                &handler(&rig),
                json!({"path": "f.txt", "encoding": "utf-16"}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.text_only().contains("not supported"), "{result:?}");
    }

    #[tokio::test]
    async fn empty_files_say_they_are_empty() {
        let rig = Rig::new();
        rig.write("empty.txt", "");
        let result = rig.call(&handler(&rig), json!({"path": "empty.txt"})).await;
        assert_eq!(result.text_only(), "(empty file)");
    }
}
