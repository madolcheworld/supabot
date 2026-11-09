//! `grep` — content search over the ignore-aware walker.
//!
//! Behaviour a model can predict, because it is ripgrep-shaped:
//! `.gitignore`/`.ignore` respected, hidden files skipped, symlinks never
//! followed, binaries skipped. Output modes mirror the common need:
//! `content` (with optional `-N`/`+N` context), `files_with_matches` and
//! `count`. Match lines render `path:line:text`, context lines
//! `path-line-text`, and non-adjacent groups are separated by `--`, so a
//! model can lift the line numbers straight into `read_file` or
//! `edit_file`.

use std::path::Path;
use std::sync::Arc;

use regex::RegexBuilder;
use serde_json::Value;
use swarmbot_contracts::{Surface, ToolError, ToolResult, ToolSpec, Toolset};

use super::glob::{compile_glob, glob_matches, ignore_walk};
use super::{
    DEFAULT_GREP_LIMIT, TOOL_GREP, ToolEnv, blocking, display, forward_remote, gate_call, opt_bool,
    opt_str, opt_u32, req_str, stamp_meta, target_node,
};
use crate::handler::{NodeCall, ToolContext, ToolHandler};
use crate::policy::PathPolicy;

/// Files larger than this are skipped: a regex walk over multi-gigabyte logs
/// or blobs would stall the call, and such files are `shell` territory
/// (`grep -n … | head`), where the owner sees the command first.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

pub fn spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_GREP,
        "Search file contents for a regex (Rust regex syntax) or, with `literal: true`, for \
         exact text. Walks from `path` (default: the workspace) respecting .gitignore and \
         .ignore files; hidden files, symlinks and binary files are skipped. Output modes: \
         `content` (default) prints `path:line:text` per match — with `context_lines` the \
         surrounding lines are printed as `path-line-text` and `--` separates groups; \
         `files_with_matches` prints one path per file; `count` prints `path:matches`. The limit \
         (default 200) caps matches reported; narrow the pattern or add `glob` instead of \
         raising it.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Regex (or literal text with literal: true) to search for."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search; defaults to the workspace."
                },
                "glob": {
                    "type": "string",
                    "description": "Only search files matching this glob (e.g. `*.rs`); same semantics as the glob tool."
                },
                "ignore_case": {
                    "type": "boolean",
                    "description": "Case-insensitive matching; defaults to false."
                },
                "literal": {
                    "type": "boolean",
                    "description": "Match the pattern as literal text, not a regex; defaults to false."
                },
                "context_lines": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Lines of context around each match (content mode); defaults to 0."
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "What to report; defaults to content."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum matches to report; defaults to 200."
                },
                "node": {
                    "type": "string",
                    "description": "Node id (or \"local\"); defaults to the bot's host node."
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
        Toolset::Node,
    )
    .needs_node()
    .read_only()
}

pub struct GrepHandler {
    env: Arc<ToolEnv>,
}

impl GrepHandler {
    pub fn new(env: Arc<ToolEnv>) -> Self {
        Self { env }
    }
}

impl std::fmt::Debug for GrepHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrepHandler").finish_non_exhaustive()
    }
}

/// What to report per match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GrepMode {
    Content,
    FilesWithMatches,
    Count,
}

impl GrepMode {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, ToolError> {
        match value {
            None | Some("content") => Ok(Self::Content),
            Some("files_with_matches") => Ok(Self::FilesWithMatches),
            Some("count") => Ok(Self::Count),
            Some(other) => Err(ToolError::invalid(format!(
                "output_mode must be content, files_with_matches or count, not `{other}`"
            ))),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::FilesWithMatches => "files_with_matches",
            Self::Count => "count",
        }
    }
}

/// Everything `fs_grep` needs, parsed once so the blocking task owns a plain
/// value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GrepParams {
    pub pattern: String,
    pub glob: Option<String>,
    pub ignore_case: bool,
    pub literal: bool,
    pub context_lines: u32,
    pub output_mode: GrepMode,
    pub limit: u32,
}

fn parse_params(args: &Value) -> Result<GrepParams, ToolError> {
    Ok(GrepParams {
        pattern: req_str(args, "pattern")?,
        glob: opt_str(args, "glob"),
        ignore_case: opt_bool(args, "ignore_case")?.unwrap_or(false),
        literal: opt_bool(args, "literal")?.unwrap_or(false),
        context_lines: opt_u32(args, "context_lines")?.unwrap_or(0),
        output_mode: GrepMode::parse(args.get("output_mode").and_then(Value::as_str))?,
        limit: opt_u32(args, "limit")?.unwrap_or(DEFAULT_GREP_LIMIT),
    })
}

#[async_trait::async_trait]
impl ToolHandler for GrepHandler {
    fn spec(&self) -> ToolSpec {
        spec()
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let params = parse_params(&args)?;
        let path = opt_str(&args, "path");
        let node = target_node(ctx, &args)?;

        if !node.is_local() {
            return forward_remote(
                ctx,
                &self.env,
                TOOL_GREP,
                node,
                NodeCall::FsGrep {
                    pattern: params.pattern.clone(),
                    path: path.clone(),
                    glob: params.glob.clone(),
                    ignore_case: Some(params.ignore_case),
                    literal: Some(params.literal),
                    context_lines: Some(params.context_lines),
                    output_mode: Some(params.output_mode.as_str().to_string()),
                    limit: Some(params.limit),
                },
            )
            .await;
        }

        let base = match &path {
            Some(raw) => self
                .env
                .policy
                .resolve(Path::new(raw), &ctx.workspace, false)?,
            None => ctx.workspace.clone(),
        };
        let target = format!("/{}/ under {}", params.pattern, display(&base));
        let gate = gate_call(
            ctx,
            &self.env,
            Surface::ReadFile,
            TOOL_GREP,
            &target,
            true,
            None,
        )
        .await?;
        if let Some(mut result) = gate.blocked_result(TOOL_GREP, &target) {
            stamp_meta(&mut result, &gate, Surface::ReadFile, &target, &node);
            return Ok(result);
        }

        let policy = Arc::clone(&self.env.policy);
        let mut result = blocking(move || fs_grep(&policy, &base, &params)).await?;
        stamp_meta(&mut result, &gate, Surface::ReadFile, &target, &node);
        Ok(result)
    }
}

/// The shared implementation, used by the handler and by
/// [`crate::LocalExecutor`].
pub(crate) fn fs_grep(
    policy: &PathPolicy,
    base: &Path,
    params: &GrepParams,
) -> Result<ToolResult, ToolError> {
    let regex_source = if params.literal {
        regex::escape(&params.pattern)
    } else {
        params.pattern.clone()
    };
    let regex = RegexBuilder::new(&regex_source)
        .case_insensitive(params.ignore_case)
        .build()
        .map_err(|error| ToolError::invalid(format!("invalid pattern: {error}")))?;
    let filter = params.glob.as_deref().map(compile_glob).transpose()?;

    let mut out = String::new();
    let mut matches_reported: u32 = 0;
    let mut files_reported: u32 = 0;
    let mut skipped_binary: u32 = 0;
    let mut truncated = false;

    for item in ignore_walk(policy, base) {
        if truncated {
            break;
        }
        let Ok(entry) = item else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(filter) = &filter
            && !glob_matches(filter, params.glob.as_deref().unwrap_or(""), base, path)
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > MAX_FILE_BYTES {
            skipped_binary += 1;
            continue;
        }
        // read_to_string fails on invalid UTF-8, which is exactly the
        // "skip binaries" rule — one check, no separate sniffing pass.
        let Ok(content) = std::fs::read_to_string(path) else {
            skipped_binary += 1;
            continue;
        };

        let lines: Vec<&str> = content.lines().collect();
        let matched: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| regex.is_match(line))
            .map(|(index, _)| index)
            .collect();
        if matched.is_empty() {
            continue;
        }

        match params.output_mode {
            GrepMode::Content => {
                let context = params.context_lines as usize;
                for group in context_groups(&matched, context, lines.len()) {
                    if matches_reported > 0 {
                        out.push_str("--\n");
                    }
                    for index in group {
                        let is_match = matched.contains(&index);
                        let sep = if is_match { ':' } else { '-' };
                        out.push_str(&format!(
                            "{}{sep}{}{sep}{}\n",
                            path.display(),
                            index + 1,
                            lines[index]
                        ));
                        if is_match {
                            matches_reported += 1;
                            if matches_reported >= params.limit {
                                truncated = true;
                                break;
                            }
                        }
                    }
                    if truncated {
                        break;
                    }
                }
            }
            GrepMode::FilesWithMatches => {
                out.push_str(&format!("{}\n", path.display()));
                files_reported += 1;
                matches_reported += u32::try_from(matched.len()).unwrap_or(u32::MAX);
            }
            GrepMode::Count => {
                let count = u32::try_from(matched.len()).unwrap_or(u32::MAX);
                out.push_str(&format!("{}:{count}\n", path.display()));
                files_reported += 1;
                matches_reported += count;
            }
        }
        if matches_reported >= params.limit {
            truncated = true;
        }
    }

    if out.is_empty() {
        let mut text = format!("(no matches for `{}`)", params.pattern);
        if skipped_binary > 0 {
            text.push_str(&format!(
                " [skipped {skipped_binary} binary or unreadable files]"
            ));
        }
        return Ok(ToolResult::text(text)
            .with_meta("matches", 0u64)
            .with_meta("truncated", false));
    }
    if truncated {
        out.push_str(&format!(
            "[truncated at the limit of {} matches; narrow the pattern, add a glob filter, or raise limit]\n",
            params.limit
        ));
    }
    if skipped_binary > 0 {
        out.push_str(&format!(
            "[skipped {skipped_binary} binary or unreadable files]\n"
        ));
    }
    out.pop(); // the final newline
    Ok(ToolResult::text(out)
        .with_meta("matches", matches_reported)
        .with_meta("files", files_reported)
        .with_meta("truncated", truncated))
}

/// Merge match line indices into `(start..=end)` windows padded by `context`
/// lines, so overlapping contexts print one contiguous group instead of
/// repeating lines.
fn context_groups(matched: &[usize], context: usize, total: usize) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for &index in matched {
        let start = index.saturating_sub(context);
        let end = (index + context).min(total.saturating_sub(1));
        match groups.last_mut() {
            // Contiguous or overlapping with the previous window: extend it.
            Some(last) if start <= last.last().copied().unwrap_or(0) + 1 => {
                for candidate in (*last.last().unwrap_or(&start))..=end {
                    if !last.contains(&candidate) {
                        last.push(candidate);
                    }
                }
            }
            _ => groups.push((start..=end).collect()),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::testkit::Rig;
    use serde_json::json;

    fn handler(rig: &Rig) -> GrepHandler {
        GrepHandler::new(Arc::clone(&rig.env))
    }

    #[tokio::test]
    async fn content_mode_prints_path_line_text() {
        let rig = Rig::new();
        rig.write("src/a.rs", "fn needle_one() {}\nother\n");
        rig.write("src/b.rs", "nothing here\n");
        let result = rig.call(&handler(&rig), json!({"pattern": "needle"})).await;
        assert!(!result.is_error, "{result:?}");
        let text = result.text_only();
        let line = text.lines().next().expect("one match line");
        let expected_suffix = "src/a.rs:1:fn needle_one() {}";
        assert!(line.ends_with(expected_suffix), "{line}");
        assert_eq!(result.meta["matches"], 1);
    }

    #[tokio::test]
    async fn context_lines_are_marked_and_grouped() {
        let rig = Rig::new();
        rig.write("f.txt", "l1\nl2\nhit\nl4\nl5\nfar\nl7\nhit\nl9\n");
        let result = rig
            .call(
                &handler(&rig),
                json!({"pattern": "hit", "context_lines": 1}),
            )
            .await;
        let text = result.text_only();
        // Match lines use ':', context lines use '-'.
        assert!(text.contains(":3:hit"), "{text}");
        assert!(text.contains("-2-l2"), "{text}");
        assert!(text.contains("-4-l4"), "{text}");
        // Two distant groups are separated.
        assert!(text.contains("\n--\n"), "{text}");
        assert!(text.contains(":8:hit"), "{text}");
    }

    #[tokio::test]
    async fn files_with_matches_and_count_modes() {
        let rig = Rig::new();
        rig.write("a.txt", "x\nx\n");
        rig.write("b.txt", "x\n");
        let files = rig
            .call(
                &handler(&rig),
                json!({"pattern": "x", "output_mode": "files_with_matches"}),
            )
            .await;
        let text = files.text_only();
        assert!(text.contains("a.txt"), "{text}");
        assert!(text.contains("b.txt"), "{text}");
        assert_eq!(text.lines().count(), 2, "{text}");

        let counts = rig
            .call(
                &handler(&rig),
                json!({"pattern": "x", "output_mode": "count"}),
            )
            .await;
        let text = counts.text_only();
        assert!(text.contains("a.txt:2"), "{text}");
        assert!(text.contains("b.txt:1"), "{text}");
    }

    #[tokio::test]
    async fn ignore_files_are_respected() {
        let rig = Rig::new();
        rig.write(".gitignore", "ignored.txt\n");
        rig.write("ignored.txt", "needle\n");
        rig.write("kept.txt", "needle\n");
        let result = rig.call(&handler(&rig), json!({"pattern": "needle"})).await;
        let text = result.text_only();
        assert!(text.contains("kept.txt"), "{text}");
        assert!(!text.contains("ignored.txt"), "{text}");
    }

    #[tokio::test]
    async fn binary_files_are_skipped_not_fatal() {
        let rig = Rig::new();
        rig.write("good.txt", "needle\n");
        std::fs::write(rig.workspace.join("blob.bin"), [0x6e_u8, 0xff, 0xfe, 0x00]).expect("write");
        let result = rig.call(&handler(&rig), json!({"pattern": "n"})).await;
        assert!(!result.is_error, "{result:?}");
        let text = result.text_only();
        assert!(text.contains("good.txt"), "{text}");
    }

    #[tokio::test]
    async fn literal_and_ignore_case_flags() {
        let rig = Rig::new();
        rig.write("f.txt", "Hello (World)\n");
        // As a regex, the parens would group; as a literal they must match.
        let literal = rig
            .call(
                &handler(&rig),
                json!({"pattern": "(World)", "literal": true}),
            )
            .await;
        assert!(literal.text_only().contains("(World)"), "{literal:?}");
        let caseless = rig
            .call(
                &handler(&rig),
                json!({"pattern": "HELLO", "ignore_case": true}),
            )
            .await;
        assert!(caseless.text_only().contains("Hello"), "{caseless:?}");
        let sensitive = rig.call(&handler(&rig), json!({"pattern": "HELLO"})).await;
        assert!(
            sensitive.text_only().contains("no matches"),
            "{sensitive:?}"
        );
    }

    #[tokio::test]
    async fn the_glob_filter_narrows_the_walk() {
        let rig = Rig::new();
        rig.write("a.rs", "needle\n");
        rig.write("a.txt", "needle\n");
        let result = rig
            .call(&handler(&rig), json!({"pattern": "needle", "glob": "*.rs"}))
            .await;
        let text = result.text_only();
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("a.txt"), "{text}");
    }

    #[tokio::test]
    async fn the_limit_truncates_matches() {
        let rig = Rig::new();
        let content: String = (0..10).map(|i| format!("hit {i}\n")).collect();
        rig.write("many.txt", &content);
        let result = rig
            .call(&handler(&rig), json!({"pattern": "hit", "limit": 3}))
            .await;
        let text = result.text_only();
        assert!(text.contains("truncated at the limit of 3"), "{text}");
        assert_eq!(result.meta["truncated"], true);
        assert_eq!(text.matches(":hit ").count(), 3, "{text}");
    }

    #[tokio::test]
    async fn an_invalid_regex_is_invalid_args() {
        let rig = Rig::new();
        let result = rig
            .call(&handler(&rig), json!({"pattern": "unbalanced("}))
            .await;
        assert!(result.is_error);
        assert!(result.text_only().contains("invalid pattern"), "{result:?}");
    }

    #[tokio::test]
    async fn a_single_file_can_be_the_search_target() {
        let rig = Rig::new();
        rig.write("only.txt", "needle\n");
        rig.write("other.txt", "needle\n");
        let result = rig
            .call(
                &handler(&rig),
                json!({"pattern": "needle", "path": "only.txt"}),
            )
            .await;
        let text = result.text_only();
        assert!(text.contains("only.txt"), "{text}");
        assert!(!text.contains("other.txt"), "{text}");
    }

    #[test]
    fn context_groups_merge_overlapping_windows() {
        // Matches at 2 and 4 with context 1 → one window 1..=5.
        let groups = context_groups(&[2, 4], 1, 10);
        assert_eq!(groups, vec![vec![1, 2, 3, 4, 5]]);
        // Distant matches stay separate.
        let groups = context_groups(&[0, 8], 1, 10);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], vec![0, 1]);
        assert_eq!(groups[1], vec![7, 8, 9]);
    }
}
