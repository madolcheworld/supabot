//! [`LocalExecutor`] — the in-process [`NodeExecutor`]: what makes the brain's
//! own machine a node.
//!
//! A node daemon wires exactly this type as its `ctx.nodes`, and the brain can
//! do the same when it wants local calls to travel the same port as remote
//! ones. Either way the execution is the *shared implementation functions* of
//! [`crate::handlers`], so a daemon and the brain cannot drift apart.
//!
//! Policy split, stated precisely: ceilings and approvals are decided by the
//! **caller** before a [`NodeCall`] arrives (the brain's handlers run the
//! [`crate::PolicyEngine`] and the approval gate). The executor enforces what
//! only the executing machine can enforce: the [`crate::PathPolicy`] boundary,
//! and — when given an engine — its `Never` ceilings as a backstop, so even a
//! buggy or malicious brain cannot make a node do what its own policy says it
//! never does. `Ask` at this layer means "was asked upstream": with no human
//! attached to an executor, re-asking here would deadlock the call.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use swarmbot_contracts::{Invocation, NodeId, PolicyCeiling, Surface, ToolError, ToolResult};
use tokio_util::sync::CancellationToken;

use crate::handler::{NodeCall, NodeExecutor};
use crate::handlers::edit_file::fs_edit_file;
use crate::handlers::glob::fs_glob;
use crate::handlers::grep::{GrepMode, GrepParams, fs_grep};
use crate::handlers::list_dir::fs_list_dir;
use crate::handlers::read_file::fs_read_file;
use crate::handlers::shell::DEFAULT_SHELL_TIMEOUT_MS;
use crate::handlers::shell::exec_shell;
use crate::handlers::write_file::fs_write_file;
use crate::handlers::{DEFAULT_GLOB_LIMIT, DEFAULT_GREP_LIMIT, DEFAULT_LIST_LIMIT};
use crate::policy::{PathPolicy, PolicyEngine};

pub struct LocalExecutor {
    /// Default `cwd` for relative paths and shell commands: the workspace of
    /// whoever this executor serves.
    workspace: PathBuf,
    policy: Arc<PathPolicy>,
    /// Optional `Never`-only backstop; see the module docs.
    engine: Option<Arc<PolicyEngine>>,
    /// Tripped by the owner of this executor (daemon shutdown, brain stop);
    /// running shell commands are killed with it.
    cancel: CancellationToken,
    /// Which node this executor *is*. A brain's in-process executor is
    /// `nod_local`; a node daemon sets its own paired id, because a call
    /// addressed to itself is exactly what it must run.
    node_id: NodeId,
}

impl std::fmt::Debug for LocalExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalExecutor")
            .field("workspace", &self.workspace)
            .field("has_engine", &self.engine.is_some())
            .finish_non_exhaustive()
    }
}

impl LocalExecutor {
    pub fn new(workspace: impl Into<PathBuf>, policy: Arc<PathPolicy>) -> Self {
        Self {
            workspace: workspace.into(),
            policy,
            engine: None,
            cancel: CancellationToken::new(),
            node_id: NodeId::local(),
        }
    }

    /// Say which node this executor is. A node daemon passes its own paired id,
    /// so calls addressed to itself are its own to run; the default is the
    /// brain's in-process `nod_local`.
    pub fn for_node(mut self, node_id: NodeId) -> Self {
        self.node_id = node_id;
        self
    }

    /// The node this executor speaks for.
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    /// Add the node's own ceilings as a `Never` backstop.
    pub fn with_engine(mut self, engine: Arc<PolicyEngine>) -> Self {
        self.engine = Some(engine);
        self
    }

    /// The token whose cancellation kills in-flight shell commands. Hand it
    /// to the shutdown path of whatever embeds this executor.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Resolve one call path through this node's policy.
    fn resolve(&self, path: &str, write: bool) -> Result<PathBuf, ToolError> {
        self.policy.resolve(Path::new(path), &self.workspace, write)
    }

    /// The `Never` backstop: a surface this node refuses is refused here too,
    /// no matter who forwarded the call.
    fn check_ceiling(&self, surface: Surface, target: &str) -> Result<(), ToolError> {
        if let Some(engine) = &self.engine
            && engine.ceiling(surface) == PolicyCeiling::Never
        {
            return Err(ToolError::PolicyRefused {
                reason: format!("this node never allows `{surface}` actions (refused: {target})"),
                retry: false,
            });
        }
        Ok(())
    }

    /// Run one call. Blocking filesystem work happens on the blocking pool,
    /// exactly like it does inside the handlers.
    async fn run(&self, call: NodeCall) -> Result<ToolResult, ToolError> {
        match call {
            NodeCall::FsRead {
                path,
                offset,
                limit,
                encoding,
            } => {
                let resolved = self.resolve(&path, false)?;
                self.check_ceiling(Surface::ReadFile, &path)?;
                crate::handlers::blocking(move || {
                    fs_read_file(&resolved, offset, limit, encoding.as_deref())
                })
                .await
            }
            NodeCall::FsWrite {
                path,
                content,
                expected_sha256,
                create_parents,
            } => {
                let resolved = self.resolve(&path, true)?;
                self.check_ceiling(Surface::WriteFile, &path)?;
                crate::handlers::blocking(move || {
                    fs_write_file(
                        &resolved,
                        &content,
                        expected_sha256.as_deref(),
                        create_parents.unwrap_or(false),
                    )
                })
                .await
            }
            NodeCall::FsEdit {
                path,
                old_string,
                new_string,
                replace_all,
            } => {
                let resolved = self.resolve(&path, true)?;
                self.check_ceiling(Surface::EditFile, &path)?;
                crate::handlers::blocking(move || {
                    fs_edit_file(
                        &resolved,
                        &old_string,
                        &new_string,
                        replace_all.unwrap_or(false),
                    )
                })
                .await
            }
            NodeCall::FsStat { path } => {
                let resolved = self.resolve(&path, false)?;
                self.check_ceiling(Surface::ReadFile, &path)?;
                crate::handlers::blocking(move || fs_stat(&resolved)).await
            }
            NodeCall::FsList {
                path,
                depth,
                include_hidden,
                limit,
            } => {
                let resolved = self.resolve(&path, false)?;
                self.check_ceiling(Surface::ReadFile, &path)?;
                let policy = Arc::clone(&self.policy);
                crate::handlers::blocking(move || {
                    fs_list_dir(
                        &policy,
                        &resolved,
                        depth,
                        include_hidden.unwrap_or(false),
                        limit.unwrap_or(DEFAULT_LIST_LIMIT),
                    )
                })
                .await
            }
            NodeCall::FsGlob {
                pattern,
                path,
                limit,
            } => {
                let base = self.resolve(path.as_deref().unwrap_or("."), false)?;
                self.check_ceiling(Surface::ReadFile, &pattern)?;
                let policy = Arc::clone(&self.policy);
                crate::handlers::blocking(move || {
                    fs_glob(
                        &policy,
                        &base,
                        &pattern,
                        limit.unwrap_or(DEFAULT_GLOB_LIMIT),
                    )
                })
                .await
            }
            NodeCall::FsGrep {
                pattern,
                path,
                glob,
                ignore_case,
                literal,
                context_lines,
                output_mode,
                limit,
            } => {
                let base = self.resolve(path.as_deref().unwrap_or("."), false)?;
                self.check_ceiling(Surface::ReadFile, &pattern)?;
                let params = GrepParams {
                    pattern,
                    glob,
                    ignore_case: ignore_case.unwrap_or(false),
                    literal: literal.unwrap_or(false),
                    context_lines: context_lines.unwrap_or(0),
                    output_mode: GrepMode::parse(output_mode.as_deref())?,
                    limit: limit.unwrap_or(DEFAULT_GREP_LIMIT),
                };
                let policy = Arc::clone(&self.policy);
                crate::handlers::blocking(move || fs_grep(&policy, &base, &params)).await
            }
            NodeCall::FsMkdir { path, parents } => {
                let resolved = self.resolve(&path, true)?;
                self.check_ceiling(Surface::WriteFile, &path)?;
                crate::handlers::blocking(move || {
                    if parents.unwrap_or(true) {
                        // mkdir -p semantics by default: idempotent, which is
                        // what a bot retrying a failed step needs.
                        std::fs::create_dir_all(&resolved)
                    } else {
                        std::fs::create_dir(&resolved)
                    }
                    .map_err(|error| {
                        ToolError::failed(format!("cannot create {}: {error}", resolved.display()))
                    })?;
                    Ok(ToolResult::text(format!("created {}", resolved.display())))
                })
                .await
            }
            NodeCall::FsMove { from, to } => {
                let source = self.resolve(&from, true)?;
                let target = self.resolve(&to, true)?;
                self.check_ceiling(Surface::WriteFile, &from)?;
                crate::handlers::blocking(move || {
                    std::fs::rename(&source, &target).map_err(|error| {
                        ToolError::failed(format!(
                            "cannot move {} to {} (rename only works within one filesystem): {error}",
                            source.display(),
                            target.display()
                        ))
                    })?;
                    Ok(ToolResult::text(format!(
                        "moved {} to {}",
                        source.display(),
                        target.display()
                    )))
                })
                .await
            }
            NodeCall::FsRemove { path, recursive } => {
                let resolved = self.resolve(&path, true)?;
                self.check_ceiling(Surface::WriteFile, &path)?;
                crate::handlers::blocking(move || {
                    let meta = std::fs::symlink_metadata(&resolved).map_err(|error| {
                        ToolError::failed(format!("cannot remove {}: {error}", resolved.display()))
                    })?;
                    let result = if meta.is_dir() {
                        if recursive.unwrap_or(false) {
                            std::fs::remove_dir_all(&resolved)
                        } else {
                            std::fs::remove_dir(&resolved)
                        }
                    } else {
                        std::fs::remove_file(&resolved)
                    };
                    result.map_err(|error| {
                        ToolError::failed(format!("cannot remove {}: {error}", resolved.display()))
                    })?;
                    Ok(ToolResult::text(format!("removed {}", resolved.display())))
                })
                .await
            }
            NodeCall::ShellExec {
                command,
                cwd,
                timeout_ms,
                env,
            } => {
                let workdir = self.resolve(cwd.as_deref().unwrap_or("."), false)?;
                self.check_ceiling(Surface::Shell, &command)?;
                let env = env.unwrap_or_default();
                let cancel = self.cancel.clone();
                exec_shell(
                    &command,
                    &workdir,
                    timeout_ms.unwrap_or(DEFAULT_SHELL_TIMEOUT_MS),
                    &env,
                    &cancel,
                )
                .await
            }
        }
    }
}

#[async_trait::async_trait]
impl NodeExecutor for LocalExecutor {
    /// Runs the call in this process. The `node` argument is informational:
    /// whoever wired this executor decided the call belongs to this machine,
    /// and refusing by id would only break the daemon's own node id (which is
    /// not `nod_local`). A mismatch is logged, not enforced.
    ///
    /// `_invocation` is deliberately unused: this type has no audit log and
    /// raises no approval card, so the only thing it could do with attribution is
    /// ignore it. The layer above records the call against the invocation it was
    /// given — the brain's registry before the forward, the node daemon's
    /// `fs`/`shell` handlers around the execution — and that is where the fact is
    /// checked, not here.
    async fn exec(
        &self,
        node: &NodeId,
        _invocation: &Invocation,
        call: NodeCall,
    ) -> Result<ToolResult, ToolError> {
        if node != &self.node_id {
            // Refuse, do not absorb. Running another node's call here would
            // execute it on the brain's machine, inside the brain's roots and
            // under the brain's ceilings — a placement lie with the owner's
            // files on the line, and invisible in the transcript (the tool-call
            // entry would still name the remote node). A brain with real remote
            // nodes puts a dispatcher in front of this executor and only the
            // local calls reach here; a brain without them has no business
            // being asked, and saying so is cheaper than guessing.
            tracing::warn!(
                %node,
                "refusing a call addressed to another node: this executor only runs local ones"
            );
            return Err(ToolError::PolicyRefused {
                reason: format!(
                    "node {node} is not this executor (it serves {}), and a call for another \
                     machine must not run here",
                    self.node_id
                ),
                retry: false,
            });
        }
        self.run(call).await
    }
}

/// `FsStat`: the small metadata summary a bot needs to decide between
/// read, list and "leave it alone".
fn fs_stat(path: &Path) -> Result<ToolResult, ToolError> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|error| ToolError::failed(format!("cannot stat {}: {error}", path.display())))?;
    let kind = if meta.is_dir() {
        "dir"
    } else if meta.is_symlink() {
        "symlink"
    } else {
        "file"
    };
    let modified = meta
        .modified()
        .ok()
        .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339())
        .unwrap_or_else(|| "unknown".to_string());
    Ok(ToolResult::text(format!(
        "path: {}\ntype: {kind}\nsize: {}\nmodified: {modified}",
        path.display(),
        meta.len()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::testkit::{Rig, test_invocation};
    use swarmbot_contracts::{NodeRoot, PolicyMap};

    /// The regression test for a placement lie: `LocalExecutor` used to log a
    /// mismatch and run the call anyway, so a bot bound to a remote node had its
    /// tools executed on the brain's machine, inside the brain's roots.
    #[tokio::test]
    async fn a_call_for_another_node_is_refused_not_absorbed() {
        let rig = Rig::new();
        rig.write("notes.txt", "local only\n");
        let executor = LocalExecutor::new(rig.workspace.clone(), Arc::clone(&rig.env.policy));
        let remote = NodeId::new();
        assert!(!remote.is_local());
        let err = executor
            .exec(
                &remote,
                &test_invocation(),
                NodeCall::FsRead {
                    path: "notes.txt".into(),
                    offset: None,
                    limit: None,
                    encoding: None,
                },
            )
            .await
            .expect_err("a remote node is not this machine");
        match err {
            ToolError::PolicyRefused { reason, retry } => {
                assert!(!retry, "retrying here cannot help");
                assert!(reason.contains(&remote.to_string()), "{reason}");
                // The refusal names both nodes, so an operator can see which
                // machine was asked and which one answered.
                assert!(reason.contains("nod_local"), "{reason}");
                assert!(reason.contains("must not run here"), "{reason}");
            }
            other => panic!("expected a typed refusal, got {other:?}"),
        }
        // The local node still works, so this is a refusal and not a shutdown.
        let ok = executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsRead {
                    path: "notes.txt".into(),
                    offset: None,
                    limit: None,
                    encoding: None,
                },
            )
            .await
            .expect("a local call still runs");
        assert!(!ok.is_error, "{}", ok.text_only());
        assert!(ok.text_only().contains("local only"));
    }

    #[tokio::test]
    async fn runs_fs_calls_in_process() {
        let rig = Rig::new();
        let executor = LocalExecutor::new(rig.workspace.clone(), Arc::clone(&rig.env.policy));

        // Write through the executor...
        let wrote = executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsWrite {
                    path: "hello.txt".into(),
                    content: "hello world".into(),
                    expected_sha256: None,
                    create_parents: None,
                },
            )
            .await
            .expect("write succeeds");
        assert!(wrote.text_only().contains("wrote 11 bytes"), "{wrote:?}");
        assert!(rig.workspace.join("hello.txt").exists());

        // ...read it back...
        let read = executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsRead {
                    path: "hello.txt".into(),
                    offset: None,
                    limit: None,
                    encoding: None,
                },
            )
            .await
            .expect("read succeeds");
        assert_eq!(read.text_only(), "   1→hello world");

        // ...stat, edit, move and remove it.
        let stat = executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsStat {
                    path: "hello.txt".into(),
                },
            )
            .await
            .expect("stat succeeds");
        assert!(stat.text_only().contains("type: file"), "{stat:?}");

        let edited = executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsEdit {
                    path: "hello.txt".into(),
                    old_string: "world".into(),
                    new_string: "bot".into(),
                    replace_all: None,
                },
            )
            .await
            .expect("edit succeeds");
        assert!(edited.text_only().contains("+hello bot"), "{edited:?}");

        executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsMkdir {
                    path: "archive".into(),
                    parents: None,
                },
            )
            .await
            .expect("mkdir succeeds");
        executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsMove {
                    from: "hello.txt".into(),
                    to: "archive/hello.txt".into(),
                },
            )
            .await
            .expect("move succeeds");
        assert!(rig.workspace.join("archive/hello.txt").exists());
        executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsRemove {
                    path: "archive/hello.txt".into(),
                    recursive: None,
                },
            )
            .await
            .expect("remove succeeds");
        assert!(!rig.workspace.join("archive/hello.txt").exists());
    }

    #[tokio::test]
    async fn runs_shell_calls_and_honours_cancellation() {
        let rig = Rig::new();
        let executor = LocalExecutor::new(rig.workspace.clone(), Arc::clone(&rig.env.policy));
        let result = executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::ShellExec {
                    command: "echo from-executor".into(),
                    cwd: None,
                    timeout_ms: Some(30_000),
                    env: None,
                },
            )
            .await
            .expect("shell succeeds");
        assert!(result.text_only().contains("from-executor"), "{result:?}");

        // Tripping the executor's token kills a running command.
        let token = executor.cancel_token();
        token.cancel();
        let cancelled = executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::ShellExec {
                    command: "sleep 30".into(),
                    cwd: None,
                    timeout_ms: None,
                    env: None,
                },
            )
            .await;
        assert_eq!(cancelled, Err(ToolError::Cancelled));
    }

    #[tokio::test]
    async fn paths_stay_inside_the_policy() {
        let rig = Rig::new();
        let executor = LocalExecutor::new(rig.workspace.clone(), Arc::clone(&rig.env.policy));
        let error = executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsRead {
                    path: "/etc/passwd".into(),
                    offset: None,
                    limit: None,
                    encoding: None,
                },
            )
            .await
            .expect_err("outside the roots");
        assert!(matches!(error, ToolError::PathDenied { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_never_ceiling_refuses_even_when_forwarded() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).expect("mkdir");
        let policy = Arc::new(PathPolicy::new(
            vec![NodeRoot::read_write(
                "tmp",
                tmp.path().display().to_string(),
            )],
            Vec::new(),
        ));
        let mut node_policy = PolicyMap::uniform(PolicyCeiling::Always);
        node_policy.set(Surface::Shell, PolicyCeiling::Never);
        let engine = Arc::new(PolicyEngine::new(
            PolicyMap::uniform(PolicyCeiling::Always),
            node_policy,
            true,
        ));
        let executor = LocalExecutor::new(workspace, Arc::clone(&policy)).with_engine(engine);
        let error = executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::ShellExec {
                    command: "echo never".into(),
                    cwd: None,
                    timeout_ms: None,
                    env: None,
                },
            )
            .await
            .expect_err("the node never allows shell");
        match error {
            ToolError::PolicyRefused { reason, retry } => {
                assert!(reason.contains("never allows `shell`"), "{reason}");
                assert!(!retry);
            }
            other => panic!("expected PolicyRefused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn glob_grep_and_list_travel_as_data() {
        let rig = Rig::new();
        rig.write("src/a.rs", "needle\n");
        rig.write("src/b.txt", "nothing\n");
        let executor = LocalExecutor::new(rig.workspace.clone(), Arc::clone(&rig.env.policy));

        let globbed = executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsGlob {
                    pattern: "*.rs".into(),
                    path: None,
                    limit: Some(DEFAULT_GLOB_LIMIT),
                },
            )
            .await
            .expect("glob succeeds");
        assert!(globbed.text_only().contains("src/a.rs"), "{globbed:?}");

        let grepped = executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsGrep {
                    pattern: "needle".into(),
                    path: None,
                    glob: None,
                    ignore_case: None,
                    literal: None,
                    context_lines: None,
                    output_mode: Some("files_with_matches".into()),
                    limit: None,
                },
            )
            .await
            .expect("grep succeeds");
        assert!(grepped.text_only().contains("a.rs"), "{grepped:?}");

        let listed = executor
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsList {
                    path: "src".into(),
                    depth: Some(1),
                    include_hidden: None,
                    limit: None,
                },
            )
            .await
            .expect("list succeeds");
        let text = listed.text_only();
        assert!(text.contains("a.rs") && text.contains("b.txt"), "{text}");
    }
}
