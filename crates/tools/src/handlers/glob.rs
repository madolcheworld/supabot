//! `glob` — find paths by pattern, honouring ignore files.
//!
//! The walker is the `ignore` crate's (the one ripgrep uses), so
//! `.gitignore`/`.ignore` are respected and hidden files stay out unless the
//! pattern explicitly asks for them. Patterns without a `/` match the file
//! name at any depth (`*.rs` finds `src/main.rs`), which is the gitignore
//! intuition models already have; patterns with a `/` match the path
//! relative to the search root.

use std::path::Path;
use std::sync::Arc;

use globset::GlobBuilder;
use serde_json::Value;
use swarmbot_contracts::{Surface, ToolError, ToolResult, ToolSpec, Toolset};

use super::{
    DEFAULT_GLOB_LIMIT, TOOL_GLOB, ToolEnv, blocking, display, forward_remote, gate_call, opt_str,
    opt_u32, req_str, stamp_meta, target_node,
};
use crate::handler::{NodeCall, ToolContext, ToolHandler};
use crate::policy::PathPolicy;

pub fn spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_GLOB,
        "Find files by glob pattern (e.g. `src/**/*.rs`). A pattern without `/` matches the \
         file name at any depth (`*.rs` finds `src/main.rs`); a pattern with `/` matches paths \
         relative to `path`. Walks from `path` (default: the workspace) respecting .gitignore \
         and .ignore files, skipping hidden files; returns matching paths relative to `path`, \
         sorted. `truncated` in the result says whether the limit (default 200) cut the list — \
         narrow the pattern instead of raising the limit blindly.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Glob pattern; supports *, ?, [...] and **."
                },
                "path": {
                    "type": "string",
                    "description": "Directory to walk; defaults to the workspace."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum paths to return; defaults to 200."
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

pub struct GlobHandler {
    env: Arc<ToolEnv>,
}

impl GlobHandler {
    pub fn new(env: Arc<ToolEnv>) -> Self {
        Self { env }
    }
}

impl std::fmt::Debug for GlobHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlobHandler").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl ToolHandler for GlobHandler {
    fn spec(&self) -> ToolSpec {
        spec()
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let pattern = req_str(&args, "pattern")?;
        let path = opt_str(&args, "path");
        let limit = opt_u32(&args, "limit")?.unwrap_or(DEFAULT_GLOB_LIMIT);
        let node = target_node(ctx, &args)?;

        if !node.is_local() {
            return forward_remote(
                ctx,
                &self.env,
                TOOL_GLOB,
                node,
                NodeCall::FsGlob {
                    pattern,
                    path,
                    limit: Some(limit),
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
        let target = format!("{pattern} under {}", display(&base));
        let gate = gate_call(
            ctx,
            &self.env,
            Surface::ReadFile,
            TOOL_GLOB,
            &target,
            true,
            None,
        )
        .await?;
        if let Some(mut result) = gate.blocked_result(TOOL_GLOB, &target) {
            stamp_meta(&mut result, &gate, Surface::ReadFile, &target, &node);
            return Ok(result);
        }

        let policy = Arc::clone(&self.env.policy);
        let mut result = blocking(move || fs_glob(&policy, &base, &pattern, limit)).await?;
        stamp_meta(&mut result, &gate, Surface::ReadFile, &target, &node);
        Ok(result)
    }
}

/// Compile one glob with path semantics: `/` in the pattern must be a real
/// separator (`*` does not cross directories).
pub(crate) fn compile_glob(pattern: &str) -> Result<globset::GlobMatcher, ToolError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|error| ToolError::invalid(format!("invalid glob pattern `{pattern}`: {error}")))
        .map(|glob| glob.compile_matcher())
}

/// True when the entry matches: the relative path, or — for patterns without
/// a `/` — the bare file name, so `*.rs` works at any depth. The file-name
/// fallback reads from `path`, not from `relative`, so a single-file search
/// root (relative = "") still matches its own name.
pub(crate) fn glob_matches(
    matcher: &globset::GlobMatcher,
    pattern: &str,
    base: &Path,
    path: &Path,
) -> bool {
    let relative = path.strip_prefix(base).unwrap_or(path);
    if matcher.is_match(relative) {
        return true;
    }
    if !pattern.contains('/')
        && let Some(name) = path.file_name()
    {
        return matcher.is_match(name);
    }
    false
}

/// The shared implementation, used by the handler and by
/// [`crate::LocalExecutor`].
pub(crate) fn fs_glob(
    policy: &PathPolicy,
    base: &Path,
    pattern: &str,
    limit: u32,
) -> Result<ToolResult, ToolError> {
    let matcher = compile_glob(pattern)?;
    let walker = ignore_walk(policy, base);

    let mut matched: Vec<String> = Vec::new();
    for item in walker {
        let Ok(entry) = item else { continue };
        let path = entry.path();
        if path == base {
            continue;
        }
        // Symlinks are reported but never followed; the walker's
        // follow_links(false) already guarantees we do not descend through
        // directory links.
        if entry.file_type().is_some_and(|t| t.is_symlink()) {
            continue;
        }
        if !glob_matches(&matcher, pattern, base, path) {
            continue;
        }
        let relative = path
            .strip_prefix(base)
            .unwrap_or(path)
            .display()
            .to_string();
        let relative = if entry.file_type().is_some_and(|t| t.is_dir()) {
            format!("{relative}/")
        } else {
            relative
        };
        matched.push(relative);
    }
    matched.sort();
    matched.dedup();

    let total = matched.len();
    let truncated = total > limit as usize;
    matched.truncate(limit as usize);

    if matched.is_empty() {
        return Ok(ToolResult::text(format!("(no matches for `{pattern}`)"))
            .with_meta("count", 0u64)
            .with_meta("truncated", false));
    }
    let mut text = matched.join("\n");
    if truncated {
        text.push_str(&format!(
            "\n[truncated: showing the first {limit} of {total} matches; narrow the pattern]"
        ));
    }
    Ok(ToolResult::text(text)
        .with_meta("count", matched.len())
        .with_meta("truncated", truncated))
}

/// The walker every ignore-respecting tool shares: gitignore/ignore files
/// honoured even outside a git repository, hidden files skipped, symlinks not
/// followed, deny-listed subtrees pruned.
pub(crate) fn ignore_walk(policy: &PathPolicy, base: &Path) -> ignore::Walk {
    let policy = policy.clone();
    let base_owned = base.to_path_buf();
    ignore::WalkBuilder::new(base)
        // Respect ignore files even without a .git directory: the behaviour
        // must not depend on whether somebody ran `git init`, or tests and
        // plain folders would silently differ.
        .require_git(false)
        .hidden(true)
        .follow_links(false)
        .filter_entry(move |entry| {
            if entry.path() == base_owned.as_path() {
                return true;
            }
            // Only directories need the deny check here: pruning a denied
            // directory removes its whole subtree, and file entries under an
            // allowed, canonical base are checked by the callers that open
            // them (grep skips symlinks; read/write go through resolve()).
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                return !policy.is_denied(entry.path());
            }
            true
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::testkit::Rig;
    use serde_json::json;

    fn handler(rig: &Rig) -> GlobHandler {
        GlobHandler::new(Arc::clone(&rig.env))
    }

    #[tokio::test]
    async fn patterns_match_relative_paths() {
        let rig = Rig::new();
        rig.write("src/main.rs", "fn main() {}");
        rig.write("src/lib/util.rs", "pub fn f() {}");
        rig.write("README.md", "# hi");

        let deep = rig
            .call(&handler(&rig), json!({"pattern": "**/*.rs"}))
            .await;
        let text = deep.text_only();
        assert!(text.contains("src/main.rs"), "{text}");
        assert!(text.contains("src/lib/util.rs"), "{text}");
        assert!(!text.contains("README.md"), "{text}");
        // Results are relative to the search root and sorted.
        assert!(
            text.lines().next().expect("a line").starts_with("src/"),
            "{text}"
        );

        // A pattern without `/` matches the file name at any depth.
        let bare = rig.call(&handler(&rig), json!({"pattern": "*.rs"})).await;
        let text = bare.text_only();
        assert!(text.contains("src/main.rs"), "{text}");
        assert!(text.contains("src/lib/util.rs"), "{text}");
    }

    #[tokio::test]
    async fn ignore_files_are_respected() {
        let rig = Rig::new();
        rig.write(".gitignore", "generated/\n*.log\n");
        rig.write("generated/out.rs", "x");
        rig.write("notes.log", "x");
        rig.write("keep.rs", "x");
        let result = rig.call(&handler(&rig), json!({"pattern": "**/*"})).await;
        let text = result.text_only();
        assert!(text.contains("keep.rs"), "{text}");
        assert!(!text.contains("generated/out.rs"), "{text}");
        assert!(!text.contains("notes.log"), "{text}");
    }

    #[tokio::test]
    async fn the_limit_truncates_with_a_flag() {
        let rig = Rig::new();
        for i in 0..5 {
            rig.write(&format!("f{i}.txt"), "x");
        }
        let result = rig
            .call(&handler(&rig), json!({"pattern": "*.txt", "limit": 2}))
            .await;
        let text = result.text_only();
        assert!(text.contains("truncated"), "{text}");
        assert!(text.contains("first 2 of 5"), "{text}");
        assert_eq!(result.meta["truncated"], true);
        assert_eq!(result.meta["count"], 2);
    }

    #[tokio::test]
    async fn no_matches_says_so_without_being_an_error() {
        let rig = Rig::new();
        let result = rig.call(&handler(&rig), json!({"pattern": "*.nope"})).await;
        assert!(!result.is_error);
        assert!(result.text_only().contains("no matches"), "{result:?}");
    }

    #[tokio::test]
    async fn an_invalid_pattern_is_invalid_args() {
        let rig = Rig::new();
        let result = rig
            .call(&handler(&rig), json!({"pattern": "unclosed["}))
            .await;
        assert!(result.is_error);
        assert!(
            result.text_only().contains("invalid glob pattern"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_subdirectory_can_be_the_search_root() {
        let rig = Rig::new();
        rig.write("src/a.rs", "x");
        rig.write("other/b.rs", "y");
        let result = rig
            .call(&handler(&rig), json!({"pattern": "*.rs", "path": "src"}))
            .await;
        let text = result.text_only();
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("b.rs"), "{text}");
    }
}
