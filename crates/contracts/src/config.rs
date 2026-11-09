//! Configuration shapes shared by the brain, the CLI and the UI.
//!
//! Parsing TOML lives in the brain; these types are the contract so a client
//! can show and edit exactly what the brain reads. Secrets are **never** part
//! of a config value: a backend names the environment variable its key comes
//! from, and the brain keeps the key in `<data>/secrets.json` (0600).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::delivery::HOP_LIMIT;
use crate::error::DomainError;
use crate::node::{PolicyCeiling, PolicyMap, Surface};
use crate::request::APPROVAL_EXPIRY_SECS;
use crate::task::{MAX_RUNNING_TASKS_PER_BOT, MAX_TASK_RESTARTS};
use crate::tool::Toolset;

/// How a bot reaches a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// Any OpenAI-compatible `/chat/completions` endpoint (SSE + tool calls).
    /// `rename_all` turns this into `open_ai_compat`, which nobody writes: the
    /// alias keeps a hand-written `kind = "openai_compat"` working, and it is
    /// the spelling the example config and the docs use.
    #[serde(alias = "openai_compat")]
    OpenAiCompat,
    /// `claude -p` with our MCP servers, using the owner's own login.
    ClaudeCli,
    /// `codex app-server` (JSON-RPC v2).
    CodexAppServer,
    /// Any agent that speaks the **Agent Client Protocol** (JSON-RPC 2.0 over
    /// stdio): `opencode acp`, `claude-code-acp` (Claude Code), `codex-acp`
    /// (Codex), and whatever speaks ACP next. The agent process to spawn comes
    /// from `options.command` + `options.args`; the protocol is the same for
    /// all of them, so one adapter covers every ACP agent.
    Acp,
    /// A deterministic scripted backend for tests and offline demos.
    Fake,
}

impl BackendKind {
    /// True when this backend executes tool calls itself (so the runner does
    /// not drive a tool loop). An ACP agent is a full agent: it runs its own
    /// tools (under its own sandbox and its own permission prompts, which our
    /// ACP client answers per `options.permission_mode`).
    pub fn owns_tool_loop(self) -> bool {
        matches!(self, Self::ClaudeCli | Self::CodexAppServer | Self::Acp)
    }
}

/// One configured LLM backend. Bots name one by `id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackendSpec {
    pub id: String,
    pub kind: BackendKind,
    /// e.g. `https://api.inferhub.dev/v1`. Required for `openai_compat`.
    pub base_url: Option<String>,
    /// The environment variable that holds the API key.
    pub api_key_env: Option<String>,
    /// Default model for bots that do not name one.
    pub model: Option<String>,
    /// Merged verbatim into the request body. This is where a gateway's own
    /// parameters go (for inferhub: its bid), without us knowing them.
    #[serde(default)]
    pub extra_body: Map<String, Value>,
    /// Extra request headers.
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Backend-specific options (`binary`, `config_dir`, `script`, …).
    #[serde(default)]
    pub options: Map<String, Value>,
}

/// A whole call, including retries inside it: long enough for a slow gateway,
/// short enough that a hung connection does not pin a bot's turn forever.
pub fn default_timeout_secs() -> u64 {
    180
}

/// 429s and 5xxs are worth retrying; three is enough that a dead gateway
/// surfaces as an error instead of a two-minute silence.
pub fn default_max_retries() -> u32 {
    3
}

impl BackendSpec {
    pub fn fake(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: BackendKind::Fake,
            base_url: None,
            api_key_env: None,
            model: Some("fake".into()),
            extra_body: Map::new(),
            extra_headers: BTreeMap::new(),
            timeout_secs: 30,
            max_retries: 0,
            options: Map::new(),
        }
    }

    pub fn openai_compat(
        id: impl Into<String>,
        base_url: impl Into<String>,
        api_key_env: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: BackendKind::OpenAiCompat,
            base_url: Some(base_url.into()),
            api_key_env: Some(api_key_env.into()),
            model: Some(model.into()),
            extra_body: Map::new(),
            extra_headers: BTreeMap::new(),
            timeout_secs: 180,
            max_retries: 3,
            options: Map::new(),
        }
    }

    /// A string option, for backend-specific knobs.
    pub fn option_str(&self, key: &str) -> Option<&str> {
        self.options.get(key).and_then(Value::as_str)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        if self.id.trim().is_empty() {
            return Err(DomainError::invalid("a backend needs an id"));
        }
        match self.kind {
            BackendKind::OpenAiCompat => {
                if self.base_url.is_none() {
                    return Err(DomainError::invalid(format!(
                        "backend `{}`: openai_compat needs a base_url",
                        self.id
                    )));
                }
            }
            BackendKind::Acp => {
                // The one thing an ACP backend cannot invent is which agent to
                // spawn: fail at config load, not on a bot's first turn.
                let has_command = self
                    .option_str("command")
                    .is_some_and(|command| !command.trim().is_empty());
                if !has_command {
                    return Err(DomainError::invalid(format!(
                        "backend `{}`: acp needs options.command (e.g. command = \"opencode\", args = [\"acp\"])",
                        self.id
                    )));
                }
            }
            BackendKind::ClaudeCli | BackendKind::CodexAppServer | BackendKind::Fake => {}
        }
        Ok(())
    }

    /// Redacted view for logs and the UI (never shows a key, only its source).
    ///
    /// `options` rides along because a Settings screen must be able to show
    /// *how* a backend connects (an ACP agent's command line, a harness's
    /// binary) — but redacted: the `env` map an ACP backend injects into its
    /// child keeps its variable **names** and loses its values (one of them
    /// may be an API key), and any option whose *name* looks secret-bearing
    /// loses its value too.
    pub fn public_view(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "kind": self.kind,
            "base_url": self.base_url,
            "api_key_env": self.api_key_env,
            "model": self.model,
            "extra_body": self.extra_body,
            "timeout_secs": self.timeout_secs,
            "options": self.public_options(),
        })
    }

    /// The `options` map as a client may see it. Values are configuration, not
    /// secrets — except inside `env` and under secret-looking names, which are
    /// masked to `"[hidden]"`.
    pub fn public_options(&self) -> Value {
        let mut out = Map::new();
        for (name, value) in &self.options {
            let masked = match value {
                // The ACP child-environment map: names are diagnostics,
                // values may be keys.
                Value::Object(entries) if name == "env" => Value::Object(
                    entries
                        .keys()
                        .map(|key| (key.clone(), Value::from("[hidden]")))
                        .collect(),
                ),
                Value::String(_) if looks_secret(name) => Value::from("[hidden]"),
                other => other.clone(),
            };
            out.insert(name.clone(), masked);
        }
        Value::Object(out)
    }
}

/// True for option names whose *values* must never be rendered. Deliberately
/// broad, mirroring the header rule in `swarmbot_llm::types`: leaking a key is
/// worse than masking a harmless value.
fn looks_secret(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("key") || n.contains("token") || n.contains("secret") || n.contains("password")
}

/// Brain-wide behaviour the owner can change at runtime.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// The global kill switch for bot-to-bot messaging.
    pub bot_messaging: bool,
    pub max_hops: u8,
    /// Tool-call rounds inside one turn before it is cut off.
    pub max_tool_rounds: u32,
    pub max_running_tasks_per_bot: u32,
    pub max_task_restarts: u8,
    pub approval_expiry_secs: u64,
    /// Brain-side ceilings. A node's own ceiling always wins by minimum.
    pub safety: PolicyMap,
    /// Read-only tools run without a card.
    pub auto_approve_read_only: bool,
    /// Mirror every committed entry into `<data>/transcripts/*.jsonl`.
    pub transcript_mirror: bool,
    /// Compact a bot's session once its history passes this fraction of the
    /// backend's context window.
    pub compact_at_fraction: f32,
}

impl Default for Settings {
    fn default() -> Self {
        let mut safety = PolicyMap::default();
        // Reading inside the roots is free; everything else asks.
        safety.set(Surface::ReadFile, PolicyCeiling::Always);
        Self {
            bot_messaging: true,
            max_hops: HOP_LIMIT,
            max_tool_rounds: 32,
            max_running_tasks_per_bot: MAX_RUNNING_TASKS_PER_BOT,
            max_task_restarts: MAX_TASK_RESTARTS,
            approval_expiry_secs: APPROVAL_EXPIRY_SECS,
            safety,
            auto_approve_read_only: true,
            transcript_mirror: true,
            compact_at_fraction: 0.6,
        }
    }
}

/// A bot as a roster file describes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BotSpec {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub instructions: String,
    /// Backend id; falls back to `BrainConfig::default_backend`.
    pub backend: Option<String>,
    pub model: Option<String>,
    pub color: Option<String>,
    /// Node label or id; defaults to the local node.
    pub node: Option<String>,
    pub toolsets: Option<Vec<Toolset>>,
    #[serde(default)]
    pub skills: Vec<String>,
}

/// A group as a roster file describes it, by bot `name`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupSpec {
    pub title: String,
    pub members: Vec<String>,
    pub lead: String,
}

/// The team: which bots exist and which groups they share.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TeamSpec {
    #[serde(default)]
    pub bots: Vec<BotSpec>,
    #[serde(default)]
    pub groups: Vec<GroupSpec>,
}

impl TeamSpec {
    pub fn validate(&self) -> Result<(), DomainError> {
        let mut seen = std::collections::BTreeSet::new();
        for bot in &self.bots {
            if !seen.insert(bot.name.to_lowercase()) {
                return Err(DomainError::invalid(format!(
                    "duplicate bot name `{}`",
                    bot.name
                )));
            }
        }
        for group in &self.groups {
            for member in &group.members {
                if !seen.contains(&member.to_lowercase()) {
                    return Err(DomainError::invalid(format!(
                        "group `{}` names an unknown bot `{member}`",
                        group.title
                    )));
                }
            }
            if !group.members.contains(&group.lead) {
                return Err(DomainError::invalid(format!(
                    "group `{}` has a lead `{}` that is not a member",
                    group.title, group.lead
                )));
            }
        }
        Ok(())
    }
}

/// The whole brain configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrainConfig {
    pub data_dir: String,
    /// Public listener: REST, live WebSocket, node gateway.
    pub public_addr: String,
    /// Internal listener: MCP gateway. `None` disables it.
    pub internal_addr: Option<String>,
    /// Login URL printed at first boot.
    pub public_origin: Option<String>,
    pub default_backend: String,
    #[serde(default)]
    pub backends: Vec<BackendSpec>,
    #[serde(default)]
    pub team: Option<TeamSpec>,
    #[serde(default)]
    pub settings: Settings,
}

impl Default for BrainConfig {
    fn default() -> Self {
        Self {
            data_dir: "data".into(),
            public_addr: "127.0.0.1:8787".into(),
            internal_addr: Some("127.0.0.1:0".into()),
            public_origin: None,
            default_backend: "fake".into(),
            backends: vec![BackendSpec::fake("fake")],
            team: None,
            settings: Settings::default(),
        }
    }
}

impl BrainConfig {
    pub fn backend(&self, id: &str) -> Option<&BackendSpec> {
        self.backends.iter().find(|b| b.id == id)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        if self.backends.is_empty() {
            return Err(DomainError::invalid("no LLM backends configured"));
        }
        for backend in &self.backends {
            backend.validate()?;
        }
        if self.backend(&self.default_backend).is_none() {
            return Err(DomainError::invalid(format!(
                "default_backend `{}` is not among the configured backends",
                self.default_backend
            )));
        }
        if let Some(team) = &self.team {
            team.validate()?;
            for bot in &team.bots {
                if let Some(backend) = &bot.backend
                    && self.backend(backend).is_none()
                {
                    return Err(DomainError::invalid(format!(
                        "bot `{}` names unknown backend `{backend}`",
                        bot.name
                    )));
                }
            }
        }
        if self.settings.max_hops == 0 {
            return Err(DomainError::invalid("max_hops must be at least 1"));
        }
        if self.settings.max_tool_rounds == 0 {
            return Err(DomainError::invalid("max_tool_rounds must be at least 1"));
        }
        if !(0.05..=0.95).contains(&self.settings.compact_at_fraction) {
            return Err(DomainError::invalid(
                "compact_at_fraction must be between 0.05 and 0.95",
            ));
        }
        Ok(())
    }

    /// The four-bot team used by the demo and by `swarmbot demo --offline`.
    /// Roles mirror a small engineering team; every bot gets the same tools,
    /// only their instructions differ.
    pub fn demo_team(backend: &str) -> TeamSpec {
        TeamSpec {
            bots: vec![
                BotSpec {
                    name: "lead".into(),
                    label: "Lead".into(),
                    instructions: "You lead a small team. Break the owner's request into work for \
                                   your teammates, hand each piece to the right one, and write the \
                                   final answer yourself. Never do research or edits yourself when \
                                   a teammate owns that."
                        .into(),
                    backend: Some(backend.into()),
                    model: None,
                    color: Some("#599ce7".into()),
                    node: None,
                    toolsets: None,
                    skills: Vec::new(),
                },
                BotSpec {
                    name: "scout".into(),
                    label: "Scout".into(),
                    instructions:
                        "You explore. Read files, list directories, grep, and report what \
                                   you found, with paths and line numbers. You never modify \
                                   anything."
                            .into(),
                    backend: Some(backend.into()),
                    model: None,
                    color: Some("#a78bfa".into()),
                    node: None,
                    toolsets: None,
                    skills: Vec::new(),
                },
                BotSpec {
                    name: "dev".into(),
                    label: "Dev".into(),
                    instructions: "You write and change files, and you run commands to check your \
                                   work. Report what you changed and what the tests said."
                        .into(),
                    backend: Some(backend.into()),
                    model: None,
                    color: Some("#34d399".into()),
                    node: None,
                    toolsets: None,
                    skills: Vec::new(),
                },
                BotSpec {
                    name: "sentinel".into(),
                    label: "Sentinel".into(),
                    instructions:
                        "You review. Check diffs for mistakes and risky commands, and say \
                                   plainly whether the work is safe to keep."
                            .into(),
                    backend: Some(backend.into()),
                    model: None,
                    color: Some("#fbbf24".into()),
                    node: None,
                    toolsets: None,
                    skills: Vec::new(),
                },
            ],
            groups: vec![GroupSpec {
                title: "Team".into(),
                members: vec![
                    "lead".into(),
                    "scout".into(),
                    "dev".into(),
                    "sentinel".into(),
                ],
                lead: "lead".into(),
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe_and_consistent() {
        let config = BrainConfig::default();
        config.validate().expect("the default config is valid");
        assert_eq!(config.settings.max_hops, HOP_LIMIT);
        assert!(config.settings.bot_messaging);
        assert!(!config.settings.safety.get(Surface::Shell).allows_auto());
        assert_eq!(
            config.settings.safety.get(Surface::ReadFile),
            PolicyCeiling::Always
        );
    }

    #[test]
    fn a_default_backend_must_exist() {
        let config = BrainConfig {
            default_backend: "inferhub".into(),
            ..BrainConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("default_backend"), "{err}");
    }

    #[test]
    fn openai_compat_needs_a_base_url() {
        let mut backend = BackendSpec::openai_compat(
            "inferhub",
            "https://api.inferhub.dev/v1",
            "INFERHUB_API_KEY",
            "ali/qwen3.8-flash",
        );
        backend.validate().expect("complete");
        // A gateway's own parameters travel in extra_body, untouched.
        backend
            .extra_body
            .insert("max_bid_per_mtok".into(), Value::from(0.5));
        assert_eq!(backend.extra_body["max_bid_per_mtok"], 0.5);

        backend.base_url = None;
        let err = backend.validate().unwrap_err();
        assert!(err.to_string().contains("base_url"), "{err}");
    }

    #[test]
    fn public_view_names_the_key_source_but_never_a_key() {
        let backend = BackendSpec::openai_compat("inferhub", "https://x/v1", "KEY_ENV", "m");
        let view = backend.public_view();
        let obj = view.as_object().expect("an object");
        // `api_key_env` says where the key comes from; there is no `api_key`.
        assert_eq!(obj["api_key_env"], "KEY_ENV");
        assert!(!obj.contains_key("api_key"));
        assert!(!obj.contains_key("extra_headers"));
        assert!(obj.contains_key("extra_body"));
    }

    #[test]
    fn public_view_shows_how_a_backend_connects_without_its_secrets() {
        // An ACP backend's options are the connection story the Settings
        // screen must be able to tell: command, args, cwd, permission mode.
        let mut backend = BackendSpec::openai_compat("claude", "https://x/v1", "KEY_ENV", "m");
        backend.kind = BackendKind::Acp;
        backend.options = serde_json::from_value(serde_json::json!({
            "command": "claude-code-acp",
            "args": ["--flag"],
            "permission_mode": "reject",
            "env": {"ANTHROPIC_API_KEY": "sk-super-secret", "OTHER": "also-secret"},
            "some_token": "tok-secret",
        }))
        .expect("options");
        let view = backend.public_view();
        let options = &view["options"];
        assert_eq!(options["command"], "claude-code-acp");
        assert_eq!(options["args"][0], "--flag");
        assert_eq!(options["permission_mode"], "reject");
        // The env map keeps its names and loses its values…
        assert_eq!(options["env"]["ANTHROPIC_API_KEY"], "[hidden]");
        assert_eq!(options["env"]["OTHER"], "[hidden]");
        // …and a secret-looking option name is masked whole.
        assert_eq!(options["some_token"], "[hidden]");
        let rendered = view.to_string();
        assert!(!rendered.contains("sk-super-secret"), "{rendered}");
        assert!(!rendered.contains("also-secret"), "{rendered}");
        assert!(!rendered.contains("tok-secret"), "{rendered}");
    }

    #[test]
    fn harness_backends_own_their_tool_loop() {
        assert!(BackendKind::ClaudeCli.owns_tool_loop());
        assert!(BackendKind::CodexAppServer.owns_tool_loop());
        assert!(BackendKind::Acp.owns_tool_loop());
        assert!(!BackendKind::OpenAiCompat.owns_tool_loop());
        assert!(!BackendKind::Fake.owns_tool_loop());
    }

    #[test]
    fn acp_backends_need_a_command_to_spawn() {
        // The config spelling round-trips: `kind = "acp"` (the brain's TOML
        // and this JSON hit the same serde representation).
        let spec: BackendSpec = serde_json::from_value(serde_json::json!({
            "id": "opencode",
            "kind": "acp",
            "options": {"command": "opencode", "args": ["acp"]},
        }))
        .expect("kind = \"acp\" loads");
        assert_eq!(spec.kind, BackendKind::Acp);
        assert_eq!(
            serde_json::to_value(spec.kind).expect("serializable"),
            Value::from("acp")
        );
        spec.validate().expect("command present");

        let mut missing = spec.clone();
        missing.options.clear();
        let err = missing.validate().unwrap_err();
        assert!(err.to_string().contains("options.command"), "{err}");

        let mut blank = spec.clone();
        blank.options.insert("command".into(), Value::from("  "));
        assert!(blank.validate().is_err());
    }

    #[test]
    fn teams_are_validated_by_name() {
        let team = BrainConfig::demo_team("fake");
        team.validate().expect("the demo team is valid");
        assert_eq!(team.bots.len(), 4);
        assert_eq!(team.groups[0].lead, "lead");

        let mut broken = team.clone();
        broken.bots.push(BotSpec {
            name: "lead".into(),
            label: "Second Lead".into(),
            instructions: String::new(),
            backend: None,
            model: None,
            color: None,
            node: None,
            toolsets: None,
            skills: Vec::new(),
        });
        assert!(
            broken
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );

        let mut ghost = team.clone();
        ghost.groups[0].members.push("nobody".into());
        assert!(
            ghost
                .validate()
                .unwrap_err()
                .to_string()
                .contains("unknown bot")
        );

        let mut leadless = team;
        leadless.groups[0].lead = "scout".into();
        leadless.groups[0].members = vec!["lead".into(), "dev".into()];
        assert!(
            leadless
                .validate()
                .unwrap_err()
                .to_string()
                .contains("lead")
        );
    }

    #[test]
    fn config_survives_json_round_trip() {
        let mut config = BrainConfig {
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        config
            .settings
            .safety
            .set(Surface::Shell, PolicyCeiling::Always);
        let json = serde_json::to_value(&config).unwrap();
        let back: BrainConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back, config);
        back.validate().unwrap();
    }
}
