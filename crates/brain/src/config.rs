//! Configuration: the CLI, the TOML file, the environment, and the data dir.
//!
//! Precedence is fixed and deliberately boring, because "which value won?" has
//! to be answerable without reading code: **CLI > environment > config file >
//! built-in default**.
//!
//! A config file may be *partial*: it is parsed into a JSON overlay and merged
//! key by key over the built-in defaults, so a two-line file is as valid as the
//! shipped example, and a new field with a sensible default never breaks an
//! existing file. Lists (`[[backends]]`, the team's bots and groups) replace
//! wholesale, because appending to a list the defaults already filled is never
//! what a config file means.
//!
//! This is deliberately the *same* algorithm `swarmbot`'s own loader uses
//! (`crates/cli/src/config.rs`), including the per-kind backend defaults and the
//! secrets precedence. One config file has to mean one thing whichever of the two
//! binaries reads it; if this function and that one ever disagree, that is the
//! bug to fix.
//!
//! No secret is ever part of a config value: a backend names the *environment
//! variable* its key lives in, and the key itself comes from
//! `<data>/secrets.json` (mode 0600) or the process environment.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use clap::Parser;
use serde_json::{Value, json};
use tracing::{info, warn};

use swarmbot_contracts::config::{BackendSpec, BrainConfig, Settings, TeamSpec};
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::node::NodeRoot;

/// The data directory layout, in one place.
///
/// Everything the daemon persists lives under one directory so a brain can be
/// copied, backed up or deleted as a unit — and so "where does the brain keep
/// X?" has exactly one answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The default config file location (`--config` overrides it).
    pub fn config(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// API keys, as `{"ENV_NAME": "value"}`. Mode 0600.
    pub fn secrets(&self) -> PathBuf {
        self.root.join("secrets.json")
    }

    /// The owner's bearer token. Mode 0600, minted at first boot.
    pub fn owner_token(&self) -> PathBuf {
        self.root.join("owner-token")
    }

    /// The brain's Ed25519 identity, which signs `node.hello` and whose public
    /// half is handed out at pairing. The path is `swarmbot-node-protocol`'s.
    pub fn brain_identity(&self) -> PathBuf {
        self.root.join("brain-identity.json")
    }

    pub fn database(&self) -> PathBuf {
        self.root.join("swarmbot.sqlite")
    }

    pub fn transcripts(&self) -> PathBuf {
        self.root.join("transcripts")
    }

    pub fn workspaces(&self) -> PathBuf {
        self.root.join("workspaces")
    }

    pub fn audit(&self) -> PathBuf {
        self.root.join("audit")
    }

    /// The MCP server configuration file for Claude CLI or ACP harnesses.
    pub fn mcp_config(&self) -> PathBuf {
        self.root.join("swarmbot-mcp.json")
    }
}

/// The daemon's command line.
///
/// Every long option that names a config value also reads an environment
/// variable, so a brain under a supervisor needs no wrapper script.
#[derive(Parser, Clone, Debug)]
#[command(
    name = "swarmbot-brain",
    version,
    about = "Run the SwarmBot brain: the orchestration, the HTTP/WS API and the local node.",
    long_about = "Run the SwarmBot brain daemon.\n\n\
                  Configuration is read from --config, else <data-dir>/config.toml, else the \
                  built-in defaults; environment variables and the command line override it, in \
                  that order. Secrets are never part of a config file: a backend names the \
                  environment variable its key lives in, and the value comes from \
                  <data-dir>/secrets.json (mode 0600) or the process environment."
)]
pub struct Cli {
    /// TOML config file [default: <data-dir>/config.toml when it exists].
    #[arg(long, value_name = "FILE", env = "SWARMBOT_CONFIG")]
    pub config: Option<PathBuf>,

    /// Where the brain keeps everything: database, transcripts, workspaces, secrets.
    #[arg(long, value_name = "PATH", env = "SWARMBOT_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Public listener address: REST, the live WebSocket and the node gateway.
    #[arg(long, value_name = "HOST:PORT", env = "SWARMBOT_PUBLIC_ADDR")]
    pub addr: Option<String>,

    /// Listen on this port instead of the configured one (keeps the host).
    #[arg(long, value_name = "PORT", env = "SWARMBOT_PORT")]
    pub port: Option<u16>,

    /// Internal listener address for the MCP gateway ("" disables it).
    #[arg(long, value_name = "HOST:PORT", env = "SWARMBOT_INTERNAL_ADDR")]
    pub internal_addr: Option<String>,

    /// Origin used for the one-time login URL and the WebSocket Origin allow-list.
    #[arg(long, value_name = "ORIGIN", env = "SWARMBOT_PUBLIC_ORIGIN")]
    pub public_origin: Option<String>,

    /// Load the team roster from this TOML file instead of the config's [team].
    #[arg(long, value_name = "FILE")]
    pub team: Option<PathBuf>,

    /// Default LLM backend for bots that do not name one.
    #[arg(long, value_name = "ID", env = "SWARMBOT_DEFAULT_BACKEND")]
    pub backend: Option<String>,

    /// Extra read-write root for the local node (repeatable).
    #[arg(long = "root", value_name = "PATH")]
    pub roots: Vec<PathBuf>,

    /// Do not start the task that expires unattended approval requests.
    #[arg(long)]
    pub no_ticker: bool,

    /// Force the scripted `fake` backend: no model call ever leaves the process.
    #[arg(long)]
    pub offline: bool,

    /// Print the resolved configuration (secrets redacted) as JSON and exit.
    #[arg(long)]
    pub print_config: bool,

    /// Validate the configuration, open the store, and exit without serving.
    #[arg(long)]
    pub check: bool,
}

/// A configuration with every override applied, ready to compose a swarm from.
///
/// `Debug` is hand-written rather than derived: a `BackendSpec` carries
/// `extra_headers`, which is exactly where an operator would put a gateway's
/// `Authorization` value, and a derived impl would print it into any `expect_err`
/// message, log line or test failure. The same shape as the CLI's `Loaded`.
#[derive(Clone)]
pub struct Resolved {
    /// The domain configuration the swarm is built from.
    pub config: BrainConfig,
    /// Where the daemon's files live (already expanded and absolute-ish).
    pub data_dir: PathBuf,
    pub paths: Paths,
    /// The file the config came from, when there was one.
    pub config_path: Option<PathBuf>,
    /// The file the roster came from, when `--team` was used.
    pub team_path: Option<PathBuf>,
    /// Extra roots for the local node, from `--root`.
    pub roots: Vec<NodeRoot>,
    /// Whether to start the approval-expiry ticker.
    pub expiry_ticker: bool,
    /// Whether `--offline` forced the fake backend.
    pub offline: bool,
    /// True when the database did not exist yet. A fresh brain adopts the
    /// config's `[settings]`; an existing one keeps the settings its owner last
    /// set through the API, because a restart must not silently undo them.
    pub fresh: bool,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("data_dir", &self.data_dir)
            .field(
                "config_path",
                &self
                    .config_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
            )
            .field("public_addr", &self.config.public_addr)
            .field("internal_addr", &self.config.internal_addr)
            .field("default_backend", &self.config.default_backend)
            // Ids only: the specs themselves can carry a header with a key in it.
            .field(
                "backends",
                &self
                    .config
                    .backends
                    .iter()
                    .map(|backend| backend.id.clone())
                    .collect::<Vec<_>>(),
            )
            .field(
                "team",
                &self.config.team.as_ref().map(|team| team.bots.len()),
            )
            .field("extra_roots", &self.roots)
            .field("expiry_ticker", &self.expiry_ticker)
            .field("offline", &self.offline)
            .field("fresh", &self.fresh)
            .finish()
    }
}

impl Resolved {
    /// Resolve everything: data dir → config file → defaults → env → CLI.
    pub fn load(cli: &Cli) -> anyhow::Result<Self> {
        // The data dir comes first: it decides where the default config lives.
        let search_dir = match &cli.data_dir {
            Some(dir) => expand(dir.clone()),
            None => default_data_dir(),
        };
        std::fs::create_dir_all(&search_dir)
            .with_context(|| format!("cannot create the data dir {}", search_dir.display()))?;

        let config_path = match &cli.config {
            Some(path) => {
                let path = expand(path.clone());
                if !path.is_file() {
                    bail!("--config {} does not exist", path.display());
                }
                Some(path)
            }
            None => {
                let candidate = Paths::new(&search_dir).config();
                candidate.is_file().then_some(candidate)
            }
        };

        let mut config = match &config_path {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("cannot read {}", path.display()))?;
                parse_config(&text, path)?
            }
            None => BrainConfig::default(),
        };

        // The roster: `--team FILE` wins over an inline `[team]`.
        let team_path = match &cli.team {
            Some(path) => {
                let path = expand(path.clone());
                config.team = Some(load_team_file(&path)?);
                Some(path)
            }
            None => None,
        };

        // CLI and environment overrides (clap has already merged the two, and the
        // environment is the `env = "…"` attribute on each option).
        if let Some(addr) = &cli.addr {
            config.public_addr = addr.clone();
        }
        if let Some(port) = cli.port {
            config.public_addr = replace_port(&config.public_addr, port);
        }
        if let Some(internal) = &cli.internal_addr {
            config.internal_addr = (!internal.trim().is_empty()).then(|| internal.clone());
        }
        if let Some(origin) = &cli.public_origin {
            config.public_origin = Some(origin.trim_end_matches('/').to_string());
        }
        if let Some(backend) = &cli.backend {
            // Both the default *and* every bot in the roster: a flag that only
            // half-applies surprises people, and the CLI does the same.
            config.default_backend = backend.clone();
            if let Some(team) = &mut config.team {
                for bot in &mut team.bots {
                    bot.backend = Some(backend.clone());
                }
            }
        }

        // A config file may point the data dir somewhere else, but only when
        // neither the CLI nor the environment named one: re-resolving it from a
        // file that was itself found through the data dir would be circular.
        let data_dir = match &cli.data_dir {
            Some(_) => search_dir.clone(),
            None => expand(PathBuf::from(&config.data_dir)),
        };
        config.data_dir = data_dir.to_string_lossy().to_string();
        let paths = Paths::new(&data_dir);
        if data_dir != search_dir {
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("cannot create the data dir {}", data_dir.display()))?;
        }

        // `fresh` has to be decided before the store is opened, because opening
        // it creates the file.
        let fresh = !paths.database().exists();

        let offline = cli.offline;
        if offline {
            // `--offline` must not merely prefer the fake backend, it must
            // guarantee that no model call can leave the process: the fake is
            // the default *and* the only backend.
            config.default_backend = "fake".to_string();
            config.backends = vec![BackendSpec::fake("fake")];
            if let Some(team) = &mut config.team {
                for bot in &mut team.bots {
                    bot.backend = Some("fake".to_string());
                }
            }
        }

        config.validate().map_err(|err| anyhow::anyhow!("{err}"))?;
        if let Some(origin) = &config.public_origin {
            validate_origin(origin)?;
        }

        let roots = extra_roots(&cli.roots)?;

        info!(
            data_dir = %data_dir.display(),
            config = config_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<defaults>".into()),
            team = team_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<config>".into()),
            default_backend = %config.default_backend,
            offline,
            "configuration resolved"
        );

        Ok(Self {
            config,
            data_dir,
            paths,
            config_path,
            team_path,
            roots,
            expiry_ticker: !cli.no_ticker,
            offline,
            fresh,
        })
    }

    /// The origin the login URL and the WebSocket allow-list use.
    ///
    /// A wildcard bind address is not something a browser can dial, so it is
    /// reported as loopback: the URL has to be one the owner can actually open.
    pub fn public_origin(&self) -> String {
        if let Some(origin) = &self.config.public_origin {
            return origin.trim_end_matches('/').to_string();
        }
        format!("http://{}", displayable_addr(&self.config.public_addr))
    }

    /// The resolved configuration as JSON, safe to print.
    ///
    /// Backends are rendered through [`BackendSpec::public_view`], which names
    /// the environment variable a key comes from and never a key: `--print-config`
    /// output ends up in bug reports and shell history.
    pub fn redacted(&self) -> Value {
        let config = &self.config;
        json!({
            "data_dir": self.data_dir,
            "config_path": self.config_path,
            "team_path": self.team_path,
            "public_addr": config.public_addr,
            "internal_addr": config.internal_addr,
            "public_origin": self.public_origin(),
            "default_backend": config.default_backend,
            "offline": self.offline,
            "expiry_ticker": self.expiry_ticker,
            "backends": config.backends.iter().map(BackendSpec::public_view).collect::<Vec<_>>(),
            "team": config.team,
            "settings": config.settings,
            "extra_roots": self.roots.iter().map(|root| json!({
                "id": root.id,
                "path": root.path,
                "mode": root.mode,
            })).collect::<Vec<_>>(),
            "layout": {
                "config": self.paths.config(),
                "secrets": self.paths.secrets(),
                "owner_token": self.paths.owner_token(),
                "database": self.paths.database(),
                "transcripts": self.paths.transcripts(),
                "workspaces": self.paths.workspaces(),
                "audit": self.paths.audit(),
            },
        })
    }
}

/// The default data dir.
///
/// `./data`, as documented in `config/swarmbot.example.toml`: the prototype is
/// run from a checkout, and a directory next to where you ran it is easier to
/// find — and to delete — than one hidden under `$XDG_DATA_HOME`.
pub fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}

/// Expand a leading `~` with the user's home directory.
///
/// Shells only expand `~` when it is unquoted, and a config file cannot expand
/// it at all, so `--data-dir '~/swarmbot'` would otherwise create a directory
/// literally named `~`.
pub fn expand(path: PathBuf) -> PathBuf {
    // A path that is not valid UTF-8 cannot carry a `~` we would recognise.
    let Some(text) = path.to_str().map(str::to_string) else {
        return path;
    };
    let (tilde, rest) = match text.strip_prefix("~/") {
        Some(rest) => (true, rest.to_string()),
        None => (text == "~", String::new()),
    };
    if !tilde {
        return path;
    }
    match dirs::home_dir() {
        Some(home) => home.join(rest),
        None => path,
    }
}

/// Keep the host of `addr`, replace its port.
fn replace_port(addr: &str, port: u16) -> String {
    match addr.rsplit_once(':') {
        // `[::1]:8787` splits into `[::1]` + `8787`; a bare IPv6 literal has no
        // port and must be left alone (it will fail to bind, honestly).
        Some((host, _)) if !host.contains('[') || host.ends_with(']') => format!("{host}:{port}"),
        _ => format!("{addr}:{port}"),
    }
}

/// An address a human (or a browser) can dial: a wildcard bind becomes loopback.
fn displayable_addr(addr: &str) -> String {
    match addr.rsplit_once(':') {
        Some((host, port)) if host == "0.0.0.0" || host == "::" || host == "[::]" => {
            format!(
                "127.0.0.1:{}",
                port.trim_start_matches('[').trim_end_matches(']')
            )
        }
        _ => addr.trim_matches(|c| c == '[' || c == ']').to_string(),
    }
}

fn validate_origin(origin: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(origin)
        .with_context(|| format!("public_origin `{origin}` is not an absolute URL"))?;
    if parsed.host_str().is_none() {
        bail!("public_origin `{origin}` has no host");
    }
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("public_origin `{origin}` must be http or https");
    }
    Ok(())
}

/// `--root PATH` entries, as read-write node roots.
///
/// The path has to exist: a typo here would otherwise show up much later as a
/// tool call the path policy refuses, which reads like a permission bug.
fn extra_roots(paths: &[PathBuf]) -> anyhow::Result<Vec<NodeRoot>> {
    let mut roots = Vec::new();
    let mut used: Vec<String> = vec!["work".to_string()];
    for (index, path) in paths.iter().enumerate() {
        let path = expand(path.clone());
        let resolved = std::fs::canonicalize(&path)
            .with_context(|| format!("--root {} does not exist", path.display()))?;
        if !resolved.is_dir() {
            bail!("--root {} is not a directory", resolved.display());
        }
        // The root id is what a bot names in a tool call, so it has to be a
        // readable word and it has to be unique.
        let mut id = resolved
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .filter(|name| !name.is_empty() && name != "." && name != "/")
            .unwrap_or_else(|| format!("root{index}"));
        id = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        while used.contains(&id) {
            id = format!("{id}-{index}");
        }
        used.push(id.clone());
        roots.push(NodeRoot::read_write(
            id,
            resolved.to_string_lossy().to_string(),
        ));
    }
    Ok(roots)
}

/// The keys this brain will use: the environment variables its configured
/// backends name, plus `<data>/secrets.json` (`{"ENV_NAME": "key"}`), which wins.
///
/// Collecting the environment values here — rather than leaving them for the
/// registry to find — is what lets the caller register every one of them with the
/// log scrubber before a single line is written. A missing file is not an error: a
/// brain with no credentials still runs the offline demo. Values are never logged,
/// and a file anybody on the machine can read is worth a warning, because the
/// whole point of 0600 is that an API key belongs to one user.
pub fn load_secrets(
    paths: &Paths,
    config: &BrainConfig,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut secrets = BTreeMap::new();
    for backend in &config.backends {
        if let Some(name) = &backend.api_key_env
            && let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            secrets.insert(name.clone(), value);
        }
    }
    let from_env = secrets.len();

    let file = paths.secrets();
    if !file.exists() {
        info!(
            count = from_env,
            path = %file.display(),
            "no secrets file; keys come from the environment only"
        );
        return Ok(secrets);
    }
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("cannot read {}", file.display()))?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", file.display()))?;
    let Some(object) = value.as_object() else {
        bail!(
            "{} must be a JSON object of \"ENV_NAME\": \"key\"",
            file.display()
        );
    };
    for (name, key) in object {
        let Some(key) = key.as_str() else {
            bail!("{}: the value of `{name}` must be a string", file.display());
        };
        if key.trim().is_empty() {
            warn!(key = %name, "an empty secret is treated as absent");
            continue;
        }
        // The file wins over the environment, which is the registry's own
        // precedence: an operator who wrote a key down meant it.
        secrets.insert(name.clone(), key.to_string());
    }
    if !is_owner_only(&file) {
        warn!(
            path = %file.display(),
            "secrets.json is readable by more than its owner; run `chmod 600` on it"
        );
    }
    // Only the *names* are ever logged: a key that reaches the log file has
    // already leaked.
    info!(
        count = secrets.len(),
        from_environment = from_env,
        path = %file.display(),
        "secrets loaded"
    );
    Ok(secrets)
}

/// Write a file that only its owner may read, creating it atomically enough for
/// a single-writer daemon: write, then tighten the mode.
#[cfg(unix)]
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    // No mode bits to set: the platform's own ACLs are the only lever.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

/// Tighten an existing file to 0600, ignoring "already fine" and "not ours".
#[cfg(unix)]
pub fn ensure_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if !is_owner_only(path)
        && let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    {
        warn!(path = %path.display(), error = %err, "cannot tighten the file mode to 0600");
    }
}

#[cfg(not(unix))]
pub fn ensure_private(_path: &Path) {}

/// True when the file's mode keeps group and others out.
#[cfg(unix)]
pub fn is_owner_only(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o077 == 0)
        .unwrap_or(true)
}

#[cfg(not(unix))]
pub fn is_owner_only(_path: &Path) -> bool {
    true
}

/// Merge a partial settings object over a base.
///
/// One function serves both `PATCH /api/settings` and the config file's
/// `[settings]` table, so "only the keys you send change" means the same thing
/// in both places — and so an unknown key is rejected the same way in both.
pub fn merge_settings(base: &Settings, patch: &Value) -> Result<Settings, DomainError> {
    let Some(patch) = patch.as_object() else {
        return Err(DomainError::invalid("settings must be a JSON object"));
    };
    let mut merged =
        serde_json::to_value(base).map_err(|err| DomainError::internal(err.to_string()))?;
    let Some(object) = merged.as_object_mut() else {
        return Err(DomainError::internal(
            "settings did not serialize to an object",
        ));
    };
    for (key, value) in patch {
        if !object.contains_key(key) {
            let known: Vec<&str> = object.keys().map(String::as_str).collect();
            return Err(DomainError::invalid(format!(
                "unknown setting `{key}`; known settings are {}",
                known.join(", ")
            )));
        }
        object.insert(key.clone(), value.clone());
    }
    let settings: Settings = serde_json::from_value(merged)
        .map_err(|err| DomainError::invalid(format!("invalid settings: {err}")))?;
    validate_settings(&settings)?;
    Ok(settings)
}

/// The settings half of [`BrainConfig::validate`], for a value that never
/// travels through a whole config.
pub fn validate_settings(settings: &Settings) -> Result<(), DomainError> {
    if settings.max_hops == 0 {
        return Err(DomainError::invalid("max_hops must be at least 1"));
    }
    if settings.max_tool_rounds == 0 {
        return Err(DomainError::invalid("max_tool_rounds must be at least 1"));
    }
    if settings.max_running_tasks_per_bot == 0 {
        return Err(DomainError::invalid(
            "max_running_tasks_per_bot must be at least 1",
        ));
    }
    if settings.approval_expiry_secs == 0 {
        return Err(DomainError::invalid(
            "approval_expiry_secs must be at least 1",
        ));
    }
    if !(0.05..=0.95).contains(&settings.compact_at_fraction) {
        return Err(DomainError::invalid(
            "compact_at_fraction must be between 0.05 and 0.95",
        ));
    }
    Ok(())
}

/// Parse a possibly partial config file by merging it over the defaults.
///
/// Errors keep the file's own words: a TOML line number, or the serde complaint
/// naming the field. "invalid config" is not actionable; "line 7: unknown
/// variant" is.
fn parse_config(text: &str, origin: &Path) -> anyhow::Result<BrainConfig> {
    let overlay: toml::Value =
        toml::from_str(text).with_context(|| format!("cannot parse {}", origin.display()))?;
    let mut overlay = serde_json::to_value(overlay).with_context(|| {
        format!(
            "{} uses a value shape this build cannot read",
            origin.display()
        )
    })?;
    normalize_backend_kinds(&mut overlay);
    layer_backends(&mut overlay).with_context(|| format!("cannot read {}", origin.display()))?;
    let mut merged = serde_json::to_value(BrainConfig::default())
        .context("the built-in default config does not serialize — this is a bug in this crate")?;
    merge_json(&mut merged, overlay);
    serde_json::from_value(merged)
        .with_context(|| format!("{} does not describe a valid BrainConfig", origin.display()))
}

/// The shipped example config and the docs spell the OpenAI kind
/// `openai_compat`; `BackendKind`'s serde name is `open_ai_compat` (it accepts the
/// documented spelling as an alias). Normalizing here keeps the two spellings
/// working even if that alias ever moves.
fn normalize_backend_kinds(overlay: &mut Value) {
    let Some(backends) = overlay.get_mut("backends").and_then(Value::as_array_mut) else {
        return;
    };
    for backend in backends {
        if let Some(Value::String(kind)) = backend.get_mut("kind")
            && kind == "openai_compat"
        {
            *kind = "open_ai_compat".to_string();
        }
    }
}

/// A config's `[[backends]]` array replaces the default one wholesale, but each
/// entry may still be partial, so every entry is layered over the defaults for
/// its kind. Without this a two-line backend spec fails on fields nobody thinks
/// about (`timeout_secs`).
fn layer_backends(overlay: &mut Value) -> anyhow::Result<()> {
    let Some(backends) = overlay.get("backends").and_then(Value::as_array).cloned() else {
        return Ok(());
    };
    let mut layered = Vec::with_capacity(backends.len());
    for entry in backends {
        let mut base = serde_json::to_value(backend_base(&entry))
            .context("a backend spec does not serialize — this is a bug in this crate")?;
        merge_json(&mut base, entry);
        layered.push(base);
    }
    overlay["backends"] = Value::Array(layered);
    Ok(())
}

/// The defaults one `[[backends]]` entry inherits, keyed by its `kind`.
fn backend_base(entry: &Value) -> BackendSpec {
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("fake");
    let mut base = BackendSpec::fake(id);
    if kind != "fake" {
        // Network kinds inherit the timeouts and retries their constructors use,
        // and no model: a missing `model` should stay visibly missing rather than
        // quietly becoming "fake".
        base.timeout_secs = 180;
        base.max_retries = 3;
        base.model = None;
    }
    base
}

/// Deep-merge `overlay` into `base`: objects merge key by key, everything else
/// (including arrays) replaces wholesale.
fn merge_json(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, value) in overlay_map {
                merge_json(base_map.entry(key).or_insert(Value::Null), value);
            }
        }
        (slot, value) => *slot = value,
    }
}

/// Parse a `TeamSpec` TOML file — the shape `--team FILE` takes.
///
/// Strict on purpose, and identical to the CLI's loader: a roster that loads for
/// `swarmbot-brain --team` must also load for `swarmbot --team`.
pub fn load_team_file(path: &Path) -> anyhow::Result<TeamSpec> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the team file {}", path.display()))?;
    let team: TeamSpec =
        toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))?;
    team.validate()
        .map_err(|err| anyhow::anyhow!("{}: {err}", path.display()))?;
    Ok(team)
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::config::BackendKind;

    fn base_cli() -> Cli {
        Cli::parse_from(["swarmbot-brain"])
    }

    #[test]
    fn a_minimal_toml_fills_in_the_defaults() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
            default_backend = "fake"

            [[backends]]
            id = "fake"
            kind = "fake"
            model = "fake"

            [settings]
            max_hops = 3

            [team]
            [[team.bots]]
            name = "lead"
            label = "Lead"
            instructions = "you lead"

            [[team.bots]]
            name = "scout"
            label = "Scout"

            [[team.groups]]
            title = "Team"
            members = ["lead", "scout"]
            lead = "lead"
            "#,
        )
        .expect("write");
        let cli = Cli {
            config: Some(path),
            data_dir: Some(dir.path().join("data")),
            ..base_cli()
        };
        let resolved = Resolved::load(&cli).expect("loads");
        let backend = resolved.config.backend("fake").expect("configured");
        // A partial [settings] table changes only what it names, including a
        // nested policy map: the merge is deep, key by key.
        assert_eq!(resolved.config.settings.max_hops, 3);
        assert_eq!(
            resolved.config.settings.max_tool_rounds,
            Settings::default().max_tool_rounds
        );
        assert_eq!(
            resolved.config.settings.safety,
            Settings::default().safety,
            "an absent [settings.safety] keeps the default ceilings"
        );
        // A roster is parsed strictly, exactly as the CLI parses one: a bot
        // without a label is a config error rather than a silently invented one.
        let team = resolved.config.team.as_ref().expect("a team");
        assert_eq!(team.bots.len(), 2);
        assert_eq!(team.bots[0].instructions, "you lead");
        assert_eq!(team.bots[1].instructions, "", "instructions default");
        assert_eq!(team.groups[0].lead, "lead");
        // A backend entry nobody gave a timeout to inherits its kind's default.
        assert_eq!(backend.timeout_secs, 30);
        assert_eq!(backend.max_retries, 0);
    }

    #[test]
    fn the_shipped_example_configs_load() {
        // These are the files the README points at. If a change here breaks
        // them, the breakage is in this crate, not in the example.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let config = root.join("config/swarmbot.example.toml");
        let team = root.join("config/team.example.toml");
        let dir = tempfile::tempdir().expect("a temp dir");
        let cli = Cli {
            config: Some(config.clone()),
            data_dir: Some(dir.path().to_path_buf()),
            team: Some(team),
            ..base_cli()
        };
        assert!(config.is_file(), "{} should exist", config.display());
        let resolved = Resolved::load(&cli).expect("the example config loads");
        resolved.config.validate().expect("and is valid");
        assert_eq!(resolved.config.default_backend, "fake");
        let roster = resolved.config.team.as_ref().expect("a team");
        assert_eq!(roster.bots.len(), 4);
        assert_eq!(roster.groups[0].title, "Team");
        // The example's settings table survives the merge.
        assert_eq!(resolved.config.settings.max_hops, 8);
        assert!(resolved.config.settings.bot_messaging);
    }

    #[test]
    fn a_network_backend_inherits_the_network_defaults() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
            default_backend = "inferhub"

            [[backends]]
            id = "inferhub"
            kind = "openai_compat"
            base_url = "https://api.inferhub.dev/v1"
            api_key_env = "INFERHUB_API_KEY"
            model = "ali/qwen3.8-flash"
            [backends.extra_body]
            max_bid_per_mtok = 0.5
            "#,
        )
        .expect("write");
        let cli = Cli {
            config: Some(path),
            data_dir: Some(dir.path().to_path_buf()),
            ..base_cli()
        };
        let resolved = Resolved::load(&cli).expect("loads");
        let backend = resolved.config.backend("inferhub").expect("configured");
        // The documented spelling of the kind loads.
        assert_eq!(backend.kind, BackendKind::OpenAiCompat);
        // Knobs nobody wrote down take the network defaults, not the fake's.
        assert_eq!(backend.timeout_secs, 180);
        assert_eq!(backend.max_retries, 3);
        assert_eq!(backend.model.as_deref(), Some("ali/qwen3.8-flash"));
        // A gateway's own parameter survives verbatim.
        assert_eq!(backend.extra_body["max_bid_per_mtok"], json!(0.5));
        // The declared key name reaches the secrets map from the environment.
        // SAFETY: this variable name is unique to this test, and the process-wide
        // environment is only touched here, so no other test can observe it.
        unsafe { std::env::set_var("SWARMBOT_TEST_INFERHUB_KEY", "sk-from-the-environment") };
        let mut with_env = resolved.config.clone();
        with_env.backends[0].api_key_env = Some("SWARMBOT_TEST_INFERHUB_KEY".into());
        let secrets = load_secrets(&resolved.paths, &with_env).expect("secrets");
        assert_eq!(
            secrets["SWARMBOT_TEST_INFERHUB_KEY"],
            "sk-from-the-environment"
        );
        // The file wins over the environment.
        std::fs::write(
            resolved.paths.secrets(),
            "{\"SWARMBOT_TEST_INFERHUB_KEY\":\"sk-from-the-file\"}",
        )
        .expect("write");
        let secrets = load_secrets(&resolved.paths, &with_env).expect("secrets");
        assert_eq!(secrets["SWARMBOT_TEST_INFERHUB_KEY"], "sk-from-the-file");
        unsafe { std::env::remove_var("SWARMBOT_TEST_INFERHUB_KEY") };
    }

    #[test]
    fn cli_and_environment_beat_the_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "public_addr = \"127.0.0.1:9000\"\ndefault_backend = \"fake\"\n",
        )
        .expect("write");
        let cli = Cli {
            config: Some(path),
            data_dir: Some(dir.path().to_path_buf()),
            addr: Some("127.0.0.1:1234".into()),
            backend: Some("other".into()),
            ..base_cli()
        };
        // `--backend other` names a backend that is not configured, so the
        // override is applied and validation reports the mismatch.
        let err = Resolved::load(&cli).unwrap_err();
        assert!(err.to_string().contains("default_backend"), "{err}");

        let cli = Cli {
            addr: Some("127.0.0.1:1234".into()),
            port: Some(4321),
            config: Some(dir.path().join("config.toml")),
            data_dir: Some(dir.path().to_path_buf()),
            ..base_cli()
        };
        let resolved = Resolved::load(&cli).expect("loads");
        // `--port` replaces the port of whatever `--addr` produced.
        assert_eq!(resolved.config.public_addr, "127.0.0.1:4321");
    }

    #[test]
    fn offline_forces_the_fake_backend() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let cli = Cli {
            data_dir: Some(dir.path().to_path_buf()),
            offline: true,
            backend: Some("inferhub".into()),
            ..base_cli()
        };
        let resolved = Resolved::load(&cli).expect("loads");
        assert!(resolved.offline);
        assert_eq!(resolved.config.default_backend, "fake");
        assert_eq!(resolved.config.backends.len(), 1);
    }

    #[test]
    fn an_internal_addr_can_be_switched_off() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let cli = Cli {
            data_dir: Some(dir.path().to_path_buf()),
            internal_addr: Some(String::new()),
            ..base_cli()
        };
        let resolved = Resolved::load(&cli).expect("loads");
        assert_eq!(resolved.config.internal_addr, None);
    }

    #[test]
    fn roots_have_to_exist_and_get_readable_ids() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let project = dir.path().join("My Project");
        std::fs::create_dir_all(&project).expect("create");
        let cli = Cli {
            data_dir: Some(dir.path().to_path_buf()),
            roots: vec![project.clone(), project.clone()],
            ..base_cli()
        };
        let resolved = Resolved::load(&cli).expect("loads");
        assert_eq!(resolved.roots.len(), 2);
        assert_eq!(resolved.roots[0].id, "my-project");
        assert_ne!(resolved.roots[0].id, resolved.roots[1].id, "ids are unique");

        let cli = Cli {
            data_dir: Some(dir.path().to_path_buf()),
            roots: vec![dir.path().join("nope")],
            ..base_cli()
        };
        assert!(Resolved::load(&cli).is_err());
    }

    #[test]
    fn a_wildcard_bind_prints_a_dialable_login_origin() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let cli = Cli {
            data_dir: Some(dir.path().to_path_buf()),
            addr: Some("0.0.0.0:8787".into()),
            ..base_cli()
        };
        let resolved = Resolved::load(&cli).expect("loads");
        assert_eq!(resolved.public_origin(), "http://127.0.0.1:8787");

        let cli = Cli {
            data_dir: Some(dir.path().to_path_buf()),
            public_origin: Some("https://brain.example.com/".into()),
            ..base_cli()
        };
        let resolved = Resolved::load(&cli).expect("loads");
        assert_eq!(resolved.public_origin(), "https://brain.example.com");

        let cli = Cli {
            data_dir: Some(dir.path().to_path_buf()),
            public_origin: Some("not a url".into()),
            ..base_cli()
        };
        assert!(Resolved::load(&cli).is_err());
    }

    #[test]
    fn the_printed_config_names_key_sources_but_holds_no_keys() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(
            dir.path().join("config.toml"),
            r#"
            default_backend = "inferhub"
            [[backends]]
            id = "inferhub"
            kind = "openai_compat"
            base_url = "https://api.inferhub.dev/v1"
            api_key_env = "INFERHUB_API_KEY"
            model = "ali/qwen3.8-flash"
            "#,
        )
        .expect("write");
        std::fs::write(
            dir.path().join("secrets.json"),
            "{\"INFERHUB_API_KEY\":\"sk-super-secret\"}",
        )
        .expect("write");
        let cli = Cli {
            data_dir: Some(dir.path().to_path_buf()),
            ..base_cli()
        };
        let resolved = Resolved::load(&cli).expect("loads");
        let secrets = load_secrets(&resolved.paths, &resolved.config).expect("secrets");
        assert_eq!(secrets["INFERHUB_API_KEY"], "sk-super-secret");

        let printed = serde_json::to_string_pretty(&resolved.redacted()).expect("json");
        assert!(!printed.contains("sk-super-secret"), "{printed}");
        assert!(!printed.contains("api_key\""), "{printed}");
        assert!(
            printed.contains("INFERHUB_API_KEY"),
            "the env name is shown"
        );
        assert!(printed.contains("swarmbot.sqlite"), "the layout is shown");
    }

    #[test]
    fn secrets_must_be_an_object_of_strings() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = Paths::new(dir.path());
        let config = BrainConfig::default();
        std::fs::write(paths.secrets(), "[1,2]").expect("write");
        assert!(load_secrets(&paths, &config).is_err());
        std::fs::write(paths.secrets(), "{\"A\": 3}").expect("write");
        assert!(load_secrets(&paths, &config).is_err());
        // An empty value is treated as absent rather than as a key of "".
        std::fs::write(paths.secrets(), "{\"A\": \"  \"}").expect("write");
        let secrets = load_secrets(&paths, &config).expect("loads");
        assert!(secrets.is_empty());
    }

    #[test]
    fn private_files_are_written_owner_only() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("nested/owner-token");
        write_private(&path, "secret\n").expect("write");
        assert!(is_owner_only(&path));
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "secret\n");
    }

    #[test]
    fn settings_merge_only_touches_the_keys_it_names() {
        let base = Settings::default();
        let merged = merge_settings(&base, &json!({"max_hops": 3})).expect("merges");
        assert_eq!(merged.max_hops, 3);
        assert_eq!(merged.max_tool_rounds, base.max_tool_rounds);
        assert_eq!(merged.safety, base.safety);

        let err = merge_settings(&base, &json!({"max_hops": 0})).unwrap_err();
        assert_eq!(err.code(), "invalid_input");
        let err = merge_settings(&base, &json!({"nope": 1})).unwrap_err();
        assert!(err.to_string().contains("unknown setting"), "{err}");
        let err = merge_settings(&base, &json!([])).unwrap_err();
        assert_eq!(err.code(), "invalid_input");
        let err = merge_settings(&base, &json!({"compact_at_fraction": 2.0})).unwrap_err();
        assert!(err.to_string().contains("compact_at_fraction"), "{err}");
    }

    #[test]
    fn ports_are_replaced_without_breaking_ipv6() {
        assert_eq!(replace_port("127.0.0.1:8787", 9000), "127.0.0.1:9000");
        assert_eq!(replace_port("[::1]:8787", 9000), "[::1]:9000");
        assert_eq!(replace_port("localhost", 9000), "localhost:9000");
        assert_eq!(displayable_addr("0.0.0.0:8787"), "127.0.0.1:8787");
        assert_eq!(displayable_addr("127.0.0.1:8787"), "127.0.0.1:8787");
    }

    #[test]
    fn the_debug_rendering_holds_no_header_value() {
        // `extra_headers` is where an operator puts a gateway's own
        // `Authorization`, so it is the field a derived `Debug` would leak.
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut resolved = Resolved {
            config: BrainConfig::default(),
            data_dir: dir.path().to_path_buf(),
            paths: Paths::new(dir.path()),
            config_path: None,
            team_path: None,
            roots: Vec::new(),
            expiry_ticker: true,
            offline: false,
            fresh: true,
        };
        let mut spec = BackendSpec::openai_compat("inferhub", "https://x/v1", "KEY_ENV", "m");
        spec.extra_headers
            .insert("authorization".into(), "Bearer sk-do-not-print".into());
        resolved.config.backends.push(spec);

        let rendered = format!("{resolved:?}");
        assert!(!rendered.contains("sk-do-not-print"), "{rendered}");
        assert!(!rendered.contains("authorization"), "{rendered}");
        assert!(
            rendered.contains("inferhub"),
            "the id is useful: {rendered}"
        );
    }

    #[test]
    fn a_tilde_is_expanded_to_the_home_directory() {
        let Some(home) = dirs::home_dir() else {
            return; // no home in this environment: nothing to assert
        };
        assert_eq!(expand(PathBuf::from("~/x")), home.join("x"));
        assert_eq!(expand(PathBuf::from("/tmp/x")), PathBuf::from("/tmp/x"));
    }
}
