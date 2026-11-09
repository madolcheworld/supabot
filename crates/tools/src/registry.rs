//! The tool registry: what a bot sees, and what happens to every call.
//!
//! `dispatch` is the single funnel every tool call goes through, so the
//! result rules from `contracts/tool.rs` are enforced **here**, not in each
//! handler: arguments are schema-validated before any handler runs, errors
//! come back as `ERROR: …` text, results are capped at
//! [`TOOL_RESULT_LIMIT_BYTES`], and anything larger is spilled to a file the
//! model can page. Handlers can therefore be written as if output size were
//! somebody else's problem — it is.
//!
//! Idempotency is deliberately *not* done here: the caller owns retry
//! decisions (it knows whether a call was already sent); the registry only
//! guarantees one audit line and one capped result per dispatch.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use jsonschema::Validator;
use serde_json::Value;
use swarmbot_contracts::{
    Capabilities, InvocationId, TOOL_RESULT_LIMIT_BYTES, ToolCallRequest, ToolError, ToolResult,
    ToolSpec, Toolset,
};

use crate::audit::{AuditDecision, AuditEntry, AuditLog, AuditOutcome};
use crate::handler::{ToolContext, ToolHandler};
use crate::handlers::TOOL_SHELL;

/// Task-management tools. A task's own session must not see them: a task that
/// could start or cancel tasks would be one confused model away from a
/// recursion bomb.
pub const TASK_TOOLS: &[&str] = &[
    "start_task",
    "list_tasks",
    "check_task",
    "cancel_task",
    "update_task",
];

/// The async asks. Inside a task there is no owner watching a conversation to
/// answer them, so they are hidden instead of expiring unseen.
pub const ASK_TOOLS: &[&str] = &["ask_user", "request_approval"];

pub struct ToolRegistry {
    handlers: Vec<Arc<dyn ToolHandler>>,
    audit: Option<Arc<AuditLog>>,
    /// Compiled argument schemas, keyed by tool name. Compiling a JSON Schema
    /// per call would show up in every turn's latency; the cache is cleared
    /// per name on `register` so a re-registered tool cannot run against a
    /// stale schema.
    validators: Mutex<HashMap<String, Validator>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tool_names())
            .field("audit", &self.audit.as_ref().map(|log| log.path()))
            .finish_non_exhaustive()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
            audit: None,
            validators: Mutex::new(HashMap::new()),
        }
    }

    /// A registry holding the seven built-in node tools.
    pub fn with_builtins(
        policy: Arc<crate::policy::PathPolicy>,
        engine: Arc<crate::policy::PolicyEngine>,
    ) -> Self {
        let mut registry = Self::new();
        for handler in crate::handlers::builtin_handlers(policy, engine) {
            registry.register(handler);
        }
        registry
    }

    /// Attach the audit log. Without one, dispatch still runs — auditing
    /// must never be a hard dependency of acting (see `audit` module docs).
    pub fn with_audit(mut self, audit: Arc<AuditLog>) -> Self {
        self.audit = Some(audit);
        self
    }

    pub fn register(&mut self, handler: Arc<dyn ToolHandler>) {
        let name = handler.spec().name;
        // A re-registered name replaces the old handler: hot-swapping a tool
        // must not leave two candidates for one name.
        if let Some(slot) = self
            .handlers
            .iter()
            .position(|existing| existing.spec().name == name)
        {
            self.handlers[slot] = handler;
        } else {
            self.handlers.push(handler);
        }
        if let Ok(mut cache) = self.validators.lock() {
            cache.remove(&name);
        }
    }

    /// Every registered spec, in registration order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.handlers.iter().map(|handler| handler.spec()).collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.handlers
            .iter()
            .find(|handler| handler.spec().name == name)
            .cloned()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.handlers
            .iter()
            .map(|handler| handler.spec().name)
            .collect()
    }

    /// The specs one bot actually sees.
    ///
    /// Three gates, in order:
    ///
    /// * **toolset** — the bot's placement (`Bot`, `Node`, `Owner`, `Task`);
    /// * **node capabilities** — a `Node` tool is hidden when the target node
    ///   lacks `shell@1` (for `shell`) or `fs@1` (for the filesystem tools).
    ///   `None` means "capabilities unknown" and does not gate: a bot without
    ///   a bound node still sees its tools and gets a clear error at call
    ///   time rather than silently losing half its hands;
    /// * **task context** — inside a task, task management and the async asks
    ///   are hidden (see [`TASK_TOOLS`] / [`ASK_TOOLS`]).
    pub fn visible_specs(
        &self,
        toolsets: &[Toolset],
        node_caps: Option<&Capabilities>,
        in_task: bool,
    ) -> Vec<ToolSpec> {
        self.specs()
            .into_iter()
            .filter(|spec| {
                if !toolsets.contains(&spec.toolset) {
                    return false;
                }
                if in_task && is_task_hidden(&spec.name) {
                    return false;
                }
                if spec.toolset == Toolset::Node
                    && let Some(caps) = node_caps
                {
                    let required = if spec.name == TOOL_SHELL {
                        "shell@1"
                    } else {
                        "fs@1"
                    };
                    if !caps.has(required) {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    /// Run one tool call end to end: validate, execute, apply the result
    /// rules, audit. Never fails — every outcome is a [`ToolResult`].
    pub async fn dispatch(&self, ctx: &ToolContext, request: ToolCallRequest) -> ToolResult {
        let started = Instant::now();
        let (mut result, decision) = self.run(ctx, &request).await;
        self.apply_result_rules(&mut result, &request.name, ctx);
        if let Some(audit) = &self.audit {
            let surface = result
                .meta
                .get("surface")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let target = result
                .meta
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            audit.record(
                AuditEntry::builder(&ctx.invocation, &request.name)
                    .node(ctx.node_id.clone())
                    .surface(surface)
                    .target(target)
                    .decision(decision)
                    .outcome(if result.is_error {
                        AuditOutcome::Error
                    } else {
                        AuditOutcome::Ok
                    })
                    .duration_ms(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
                    .result_bytes(result.text_only().len())
                    .build(),
            );
        }
        result
    }

    /// The execution half of `dispatch`: everything that can go wrong before
    /// a handler runs, plus the handler itself. Returns the raw result and
    /// the audit decision label.
    async fn run(
        &self,
        ctx: &ToolContext,
        request: &ToolCallRequest,
    ) -> (ToolResult, AuditDecision) {
        // A stopped turn must not act, not even "just" read.
        if ctx.cancel.is_cancelled() {
            return (ToolError::Cancelled.to_result(), AuditDecision::Run);
        }
        let Some(handler) = self.get(&request.name) else {
            return (
                ToolError::UnknownTool {
                    tool: request.name.clone(),
                }
                .to_result(),
                AuditDecision::Run,
            );
        };

        // Models that call a no-argument tool often omit `arguments`
        // entirely; an empty object is what the schema expects.
        let args = if request.args.is_null() {
            Value::Object(serde_json::Map::new())
        } else {
            request.args.clone()
        };
        if let Err(error) = self.validate_args(&request.name, &handler.spec().parameters, &args) {
            return (error.to_result(), AuditDecision::Run);
        }

        match handler.call(ctx, args).await {
            Ok(result) => {
                // Handlers stamp their gate outcome into meta; anything that
                // did not ask ran.
                let decision = result
                    .meta
                    .get("decision")
                    .and_then(Value::as_str)
                    .map(AuditDecision::from_label)
                    .unwrap_or(AuditDecision::Run);
                (result, decision)
            }
            Err(error) => {
                // The error kind is itself the decision record for the two
                // ways a call can be stopped by policy.
                let decision = match &error {
                    ToolError::PolicyRefused { .. } => AuditDecision::Refuse,
                    ToolError::Denied { .. } => AuditDecision::Deny,
                    _ => AuditDecision::Run,
                };
                (error.to_result(), decision)
            }
        }
    }

    fn validate_args(&self, tool: &str, schema: &Value, args: &Value) -> Result<(), ToolError> {
        let validator = self.validator_for(tool, schema)?;
        let errors: Vec<String> = validator
            .iter_errors(args)
            .take(3)
            .map(|error| format!("{error} (at {})", error.instance_path()))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ToolError::invalid(errors.join("; ")))
        }
    }

    fn validator_for(&self, tool: &str, schema: &Value) -> Result<Validator, ToolError> {
        {
            let cache = self
                .validators
                .lock()
                .map_err(|_| ToolError::failed("internal error: the schema cache is poisoned"))?;
            if let Some(validator) = cache.get(tool) {
                return Ok(validator.clone());
            }
        }
        let validator = jsonschema::validator_for(schema).map_err(|error| {
            ToolError::failed(format!("the schema of `{tool}` is invalid: {error}"))
        })?;
        if let Ok(mut cache) = self.validators.lock() {
            cache.insert(tool.to_string(), validator.clone());
        }
        Ok(validator)
    }

    /// The result rules, applied to *every* result whatever the handler did:
    /// text over the cap is spilled to `<spool_dir>/<tool>-<invocation>.txt`
    /// and replaced by a capped head plus paging instructions; if the spill
    /// itself fails (no spool dir, permissions) the result is still capped,
    /// because blowing up a context is worse than losing the tail.
    fn apply_result_rules(&self, result: &mut ToolResult, tool: &str, ctx: &ToolContext) {
        let text = result.text_only();
        if text.len() > TOOL_RESULT_LIMIT_BYTES {
            match spill(&text, tool, &ctx.invocation.id, &ctx.spool_dir) {
                Ok(path) => {
                    let note = format!(
                        "\n[output too large: {} bytes total. The full output was spilled to {}. \
                         Page it with read_file {{\"path\": \"{}\", \"offset\": 1, \"limit\": 200}} \
                         or search it with grep.]",
                        text.len(),
                        path.display(),
                        path.display()
                    );
                    let keep = floor_char_boundary(
                        &text,
                        TOOL_RESULT_LIMIT_BYTES.saturating_sub(note.len()),
                    );
                    let mut head = text[..keep].to_string();
                    head.push_str(&note);
                    result.content = vec![swarmbot_contracts::Content::Text { text: head }];
                    result
                        .meta
                        .insert("spill_path".into(), Value::from(path.display().to_string()));
                    result
                        .meta
                        .insert("spilled_bytes".into(), Value::from(text.len()));
                    return;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        tool,
                        spool = %ctx.spool_dir.display(),
                        "cannot spill oversized tool output; truncating instead"
                    );
                }
            }
        }
        // Caps the result even when spilling was unnecessary or impossible.
        result.truncate_to_limit();
    }
}

fn is_task_hidden(name: &str) -> bool {
    TASK_TOOLS.contains(&name) || ASK_TOOLS.contains(&name)
}

/// Write the full text to `<spool_dir>/<tool>-<invocation>.txt`. The
/// invocation id makes the name unique per call, so a turn that spills twice
/// from the same tool keeps both files.
fn spill(
    text: &str,
    tool: &str,
    invocation: &InvocationId,
    spool_dir: &Path,
) -> Result<std::path::PathBuf, std::io::Error> {
    std::fs::create_dir_all(spool_dir)?;
    let path = spool_dir.join(format!("{tool}-{invocation}.txt"));
    std::fs::write(&path, text)?;
    Ok(path)
}

/// Shrink `s` to at most `limit` bytes without splitting a UTF-8 sequence.
/// (`str::floor_char_boundary` is still unstable; contracts keeps a private
/// copy for the same reason.)
fn floor_char_boundary(s: &str, limit: usize) -> usize {
    if s.len() <= limit {
        return s.len();
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditLog;
    use crate::handlers::builtin_handlers;
    use crate::handlers::testkit::Rig;
    use crate::policy::{PathPolicy, PolicyEngine};
    use serde_json::json;
    use swarmbot_contracts::{
        BotId, ConversationId, Invocation, NodeRoot, PolicyCeiling, PolicyMap, ToolCallId,
    };
    use tempfile::TempDir;

    fn request(name: &str, args: Value) -> ToolCallRequest {
        ToolCallRequest {
            id: ToolCallId::new(),
            name: name.to_string(),
            args,
        }
    }

    fn registry_from(rig: &Rig) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for handler in builtin_handlers(Arc::clone(&rig.env.policy), Arc::clone(&rig.env.engine)) {
            registry.register(handler);
        }
        registry
    }

    #[tokio::test]
    async fn a_missing_required_field_is_invalid_args() {
        let rig = Rig::new();
        let registry = registry_from(&rig);
        let result = registry
            .dispatch(&rig.ctx, request("shell", json!({})))
            .await;
        assert!(result.is_error);
        let text = result.text_only();
        assert!(text.starts_with("ERROR: invalid arguments"), "{text}");
        assert!(text.contains("command"), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_field_is_invalid_args() {
        let rig = Rig::new();
        let registry = registry_from(&rig);
        let result = registry
            .dispatch(
                &rig.ctx,
                request("read_file", json!({"path": "x.txt", "bogus": 1})),
            )
            .await;
        assert!(result.is_error);
        let text = result.text_only();
        assert!(text.starts_with("ERROR: invalid arguments"), "{text}");
        assert!(text.contains("bogus"), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_tool_is_named() {
        let rig = Rig::new();
        let registry = registry_from(&rig);
        let result = registry
            .dispatch(&rig.ctx, request("frobnicate", json!({})))
            .await;
        assert!(result.is_error);
        assert_eq!(result.text_only(), "ERROR: unknown tool: frobnicate");
    }

    #[tokio::test]
    async fn a_tripped_cancel_token_stops_the_call() {
        let rig = Rig::new();
        let registry = registry_from(&rig);
        rig.ctx.cancel.cancel();
        let result = registry
            .dispatch(&rig.ctx, request("shell", json!({"command": "echo hi"})))
            .await;
        assert!(result.is_error);
        assert_eq!(result.text_only(), "ERROR: cancelled");
    }

    #[tokio::test]
    async fn oversized_output_is_spilled_capped_and_referenced() {
        let rig = Rig::new();
        let registry = registry_from(&rig);
        // A file whose full read is far above the 10 KiB result cap.
        let content: String = (1..=2000)
            .map(|i| format!("line {i:04} padding padding\n"))
            .collect();
        assert!(content.len() > TOOL_RESULT_LIMIT_BYTES);
        rig.write("big.txt", &content);

        let result = registry
            .dispatch(
                &rig.ctx,
                request("read_file", json!({"path": "big.txt", "limit": 2000})),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        let text = result.text_only();
        // Capped...
        assert!(
            text.len() <= TOOL_RESULT_LIMIT_BYTES,
            "capped: {} bytes",
            text.len()
        );
        // ...spilled...
        let spill_path = result.meta["spill_path"]
            .as_str()
            .expect("a spill path in meta");
        let spill_path = std::path::PathBuf::from(spill_path);
        assert!(spill_path.exists(), "spill file exists");
        assert!(
            spill_path.starts_with(&rig.spool),
            "the spill lands in the spool dir: {}",
            spill_path.display()
        );
        assert!(
            spill_path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .starts_with(&format!("read_file-{}", rig.ctx.invocation.id))
        );
        // ...with the full text...
        let spilled = std::fs::read_to_string(&spill_path).expect("readable");
        assert!(spilled.contains("line 2000"), "the tail is in the file");
        // ...and paging instructions the model can follow.
        assert!(text.contains("spilled to"), "{text}");
        assert!(text.contains("read_file"), "{text}");
        assert!(text.contains("offset"), "{text}");
    }

    #[tokio::test]
    async fn results_under_the_cap_are_left_alone() {
        let rig = Rig::new();
        let registry = registry_from(&rig);
        rig.write("small.txt", "hello\n");
        let result = registry
            .dispatch(&rig.ctx, request("read_file", json!({"path": "small.txt"})))
            .await;
        assert_eq!(result.text_only(), "   1→hello");
        assert!(result.meta.get("spill_path").is_none());
    }

    #[tokio::test]
    async fn visible_specs_gate_by_toolset_caps_and_task() {
        let rig = Rig::new();
        let registry = registry_from(&rig);

        // The seven node tools, full caps.
        let caps = Capabilities::new(["fs@1", "shell@1"]);
        let visible = registry.visible_specs(&[Toolset::Node], Some(&caps), false);
        assert_eq!(visible.len(), 7, "{visible:?}");

        // A node without shell loses exactly the shell tool.
        let fs_only = Capabilities::new(["fs@1"]);
        let visible = registry.visible_specs(&[Toolset::Node], Some(&fs_only), false);
        assert_eq!(visible.len(), 6);
        assert!(visible.iter().all(|spec| spec.name != "shell"));

        // No node toolset → nothing.
        assert!(
            registry
                .visible_specs(&[Toolset::Bot], Some(&caps), false)
                .is_empty()
        );
        // Unknown caps do not gate.
        assert_eq!(
            registry.visible_specs(&[Toolset::Node], None, false).len(),
            7
        );

        // Task and ask tools vanish inside a task.
        let mut with_tasks = ToolRegistry::new();
        with_tasks.register(Arc::new(StubHandler::new("start_task", Toolset::Task)));
        with_tasks.register(Arc::new(StubHandler::new("ask_user", Toolset::Bot)));
        with_tasks.register(Arc::new(StubHandler::new("message_bot", Toolset::Bot)));
        for handler in builtin_handlers(Arc::clone(&rig.env.policy), Arc::clone(&rig.env.engine)) {
            with_tasks.register(handler);
        }
        let outside = with_tasks.visible_specs(
            &[Toolset::Bot, Toolset::Node, Toolset::Task],
            Some(&caps),
            false,
        );
        assert!(outside.iter().any(|s| s.name == "start_task"));
        assert!(outside.iter().any(|s| s.name == "ask_user"));
        let inside = with_tasks.visible_specs(
            &[Toolset::Bot, Toolset::Node, Toolset::Task],
            Some(&caps),
            true,
        );
        assert!(!inside.iter().any(|s| s.name == "start_task"));
        assert!(!inside.iter().any(|s| s.name == "ask_user"));
        // A normal bot tool survives the task filter.
        assert!(inside.iter().any(|s| s.name == "message_bot"));
    }

    #[tokio::test]
    async fn dispatch_writes_one_audit_line_per_call() {
        let rig = Rig::new();
        let audit_path = rig.tmp.path().join("audit").join("nod_local.jsonl");
        let registry = registry_from(&rig).with_audit(Arc::new(AuditLog::new(audit_path.clone())));
        rig.write("f.txt", "hello\n");
        registry
            .dispatch(&rig.ctx, request("read_file", json!({"path": "f.txt"})))
            .await;
        registry
            .dispatch(&rig.ctx, request("nope", json!({})))
            .await;

        let content = std::fs::read_to_string(&audit_path).expect("audit file");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "one line per dispatch");
        let first: Value = serde_json::from_str(lines[0]).expect("json");
        assert_eq!(first["tool"], "read_file");
        assert_eq!(first["surface"], "read_file");
        assert_eq!(first["decision"], "run");
        assert_eq!(first["outcome"], "ok");
        assert!(first["duration_ms"].as_u64().is_some());
        assert!(first["result_bytes"].as_u64().expect("bytes") > 0);
        assert!(first["target"].as_str().expect("target").contains("f.txt"));
        let second: Value = serde_json::from_str(lines[1]).expect("json");
        assert_eq!(second["tool"], "nope");
        assert_eq!(second["outcome"], "error");
    }

    #[tokio::test]
    async fn a_refused_call_audits_as_refuse_and_tells_the_model_not_to_retry() {
        // WriteFile: Never on the node side; the brain saying Always cannot
        // raise it.
        let mut node = PolicyMap::uniform(PolicyCeiling::Always);
        node.set(swarmbot_contracts::Surface::WriteFile, PolicyCeiling::Never);
        let rig = Rig::with_engine(PolicyMap::uniform(PolicyCeiling::Always), node, true);
        let audit_path = rig.tmp.path().join("audit").join("nod_local.jsonl");
        let registry = registry_from(&rig).with_audit(Arc::new(AuditLog::new(audit_path.clone())));

        let result = registry
            .dispatch(
                &rig.ctx,
                request("write_file", json!({"path": "x.txt", "content": "y"})),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.text_only().contains("refused by policy"),
            "{result:?}"
        );
        let content = std::fs::read_to_string(&audit_path).expect("audit file");
        let line: Value =
            serde_json::from_str(content.lines().next().expect("a line")).expect("json");
        assert_eq!(line["decision"], "refuse");
        assert_eq!(line["outcome"], "error");
        assert_eq!(line["surface"], "write_file");
    }

    #[tokio::test]
    async fn an_asked_and_allowed_call_audits_as_allow() {
        let rig = Rig::with_engine(
            PolicyMap::uniform(PolicyCeiling::Ask),
            PolicyMap::uniform(PolicyCeiling::Ask),
            false,
        );
        let audit_path = rig.tmp.path().join("audit").join("nod_local.jsonl");
        let registry = registry_from(&rig).with_audit(Arc::new(AuditLog::new(audit_path.clone())));
        let result = registry
            .dispatch(
                &rig.ctx,
                request("write_file", json!({"path": "x.txt", "content": "y"})),
            )
            .await;
        // The rig's gate allows.
        assert!(!result.is_error, "{result:?}");
        assert_eq!(rig.gate.ask_count(), 1);
        let content = std::fs::read_to_string(&audit_path).expect("audit file");
        let line: Value =
            serde_json::from_str(content.lines().next().expect("a line")).expect("json");
        assert_eq!(line["decision"], "allow");
        assert_eq!(line["outcome"], "ok");
    }

    #[tokio::test]
    async fn an_unwritable_audit_path_does_not_fail_the_call() {
        let tmp = TempDir::new().expect("tempdir");
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "file, not a directory").expect("write");
        let policy = Arc::new(PathPolicy::new(
            vec![NodeRoot::read_write(
                "tmp",
                tmp.path().display().to_string(),
            )],
            Vec::new(),
        ));
        let engine = Arc::new(PolicyEngine::new(
            PolicyMap::uniform(PolicyCeiling::Always),
            PolicyMap::uniform(PolicyCeiling::Always),
            true,
        ));
        let registry = ToolRegistry::with_builtins(policy, engine).with_audit(Arc::new(
            AuditLog::new(blocker.join("audit").join("nod_local.jsonl")),
        ));
        let ctx = ToolContext::local(
            Invocation::new(
                BotId::from_suffix(&"b".repeat(32)),
                ConversationId::from_suffix(&"c".repeat(32)),
            ),
            tmp.path(),
            crate::handlers::testkit::TestGate::allowing(),
            crate::handlers::testkit::TestNodes::new(),
            tmp.path().join("spool"),
        );
        std::fs::write(tmp.path().join("f.txt"), "hello\n").expect("write");
        let result = registry
            .dispatch(&ctx, request("read_file", json!({"path": "f.txt"})))
            .await;
        // The audit append failed (blocker is a file); the tool call did not.
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.text_only(), "   1→hello");
    }

    #[tokio::test]
    async fn null_arguments_are_treated_as_an_empty_object() {
        let rig = Rig::new();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubHandler::new("ping", Toolset::Bot)));
        let result = registry
            .dispatch(&rig.ctx, request("ping", Value::Null))
            .await;
        assert_eq!(result.text_only(), "pong");
    }

    /// A minimal non-node handler, so registry tests can exercise gating and
    /// dispatch without the filesystem.
    struct StubHandler {
        spec: ToolSpec,
    }

    impl StubHandler {
        fn new(name: &str, toolset: Toolset) -> Self {
            Self {
                spec: ToolSpec::new(
                    name,
                    format!("stub {name}"),
                    json!({"type": "object", "properties": {}, "additionalProperties": false}),
                    toolset,
                ),
            }
        }
    }

    impl std::fmt::Debug for StubHandler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("StubHandler")
                .field("name", &self.spec.name)
                .finish()
        }
    }

    #[async_trait::async_trait]
    impl ToolHandler for StubHandler {
        fn spec(&self) -> ToolSpec {
            self.spec.clone()
        }

        async fn call(&self, _ctx: &ToolContext, _args: Value) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("pong"))
        }
    }
}
