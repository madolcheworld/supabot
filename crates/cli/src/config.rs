//! Configuration: where a `BrainConfig` comes from and what the CLI layers on
//! top of it.
//!
//! Precedence, weakest first: the built-in defaults, `<data-dir>/config.toml`
//! (or `--config`), then the flags (`--backend`, `--team`, `--data-dir`). A
//! config file may be *partial*: it is merged key-by-key over the defaults, so
//! a two-line file is as valid as the shipped example.
//!
//! The secret rule this module exists to keep: a key value is read into the
//! secrets map and nowhere else. Nothing here ever prints one — `config show`
//! renders only the *name* of the environment variable or file entry a key
//! comes from, and whether one is set.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::Value as JsonValue;
use swarmbot_contracts::bot::normalize_name;
use swarmbot_contracts::config::{BackendKind, BackendSpec, BrainConfig, TeamSpec};
use swarmbot_contracts::node::NodeRoot;

/// Where the effective config came from — `config show` says so honestly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigSource {
    File(PathBuf),
    Defaults,
}

impl ConfigSource {
    pub fn describe(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::Defaults => "(built-in defaults)".to_string(),
        }
    }
}

/// What the command line asked for, before any file is read.
#[derive(Default)]
pub struct LoadRequest<'a> {
    /// `--config FILE`; when absent, `<data-dir>/config.toml` is used if it
    /// exists.
    pub config_path: Option<&'a Path>,
    /// `--data-dir DIR`; when absent, the config's `data_dir`, else `./data`.
    pub data_dir: Option<&'a Path>,
    /// `--team FILE`, a TOML `TeamSpec`.
    pub team_file: Option<&'a Path>,
    /// `--backend ID`: overrides `default_backend` *and* every bot's backend,
    /// because a flag that only half-applies surprises people.
    pub backend: Option<&'a str>,
    /// `--root PATH` (repeatable), extra read-write roots for the local node.
    pub roots: &'a [PathBuf],
}

/// The fully resolved inputs a swarm build needs.
///
/// `Debug` is hand-written: `secrets` may hold API keys, and a derived impl
/// would print them into any `expect_err` message or log line. Only the count
/// is ever rendered.
pub struct Loaded {
    pub config: BrainConfig,
    pub source: ConfigSource,
    pub secrets: BTreeMap<String, String>,
    /// The effective data dir (the flag, else the config's, else `./data`).
    pub data_dir: PathBuf,
    /// The `--root` paths as `NodeRoot::read_write("cli", …)`.
    pub roots: Vec<NodeRoot>,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("source", &self.source)
            .field("data_dir", &self.data_dir)
            .field("default_backend", &self.config.default_backend)
            .field("backends", &self.config.backends.len())
            .field("team", &self.config.team.as_ref().map(|t| t.bots.len()))
            .field(
                "secrets",
                &format_args!("{} key(s), values redacted", self.secrets.len()),
            )
            .field("roots", &self.roots)
            .finish()
    }
}

/// Load the config file (or the defaults), apply the CLI overrides, read the
/// secrets. Never fails silently: an explicit `--config` that does not exist
/// is an error, a missing `<data-dir>/config.toml` is normal.
pub fn load(request: &LoadRequest<'_>) -> Result<Loaded> {
    let default_data_dir = request.data_dir.unwrap_or_else(|| Path::new("data"));

    let (mut config, source) = match request.config_path {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read the config file {}", path.display()))?;
            (
                parse_config(&text, path)?,
                ConfigSource::File(path.to_path_buf()),
            )
        }
        None => {
            let candidate = default_data_dir.join("config.toml");
            if candidate.exists() {
                let text = std::fs::read_to_string(&candidate)
                    .with_context(|| format!("cannot read {}", candidate.display()))?;
                (
                    parse_config(&text, &candidate)?,
                    ConfigSource::File(candidate),
                )
            } else {
                (BrainConfig::default(), ConfigSource::Defaults)
            }
        }
    };

    if let Some(team_file) = request.team_file {
        config.team = Some(load_team_file(team_file)?);
    }
    if let Some(backend) = request.backend {
        config.default_backend = backend.to_string();
        if let Some(team) = &mut config.team {
            for bot in &mut team.bots {
                bot.backend = Some(backend.to_string());
            }
        }
    }

    // The effective data dir wins over whatever the file said, so the rest of
    // the program (and `config show`) sees one truth.
    let data_dir = if request.data_dir.is_some() {
        default_data_dir.to_path_buf()
    } else {
        PathBuf::from(&config.data_dir)
    };
    config.data_dir = data_dir.to_string_lossy().to_string();

    // A team that names no backend per bot inherits the (possibly overridden)
    // default; `validate` inside `Swarm::new` catches an unknown id with a
    // message that names the bot.
    let secrets = load_secrets(&data_dir, &config)?;
    let roots = request
        .roots
        .iter()
        .map(|path| NodeRoot::read_write("cli", path.to_string_lossy().to_string()))
        .collect();

    Ok(Loaded {
        config,
        source,
        secrets,
        data_dir,
        roots,
    })
}

/// The team for commands that need one: `--team` (already applied by
/// [`load`]), else the config's `[team]`, else the demo team on the effective
/// default backend.
pub fn team_or_demo(config: &mut BrainConfig) {
    if config.team.is_none() {
        let backend = config.default_backend.clone();
        config.team = Some(BrainConfig::demo_team(&backend));
    }
}

/// Upgrade a shared `fake` backend to **one fake per scripted bot**, before the
/// swarm is composed — the `demo`'s own idiom ([`crate::demo::demo_config`]:
/// `fake-lead`, `fake-scout`, …), applied to a `run`/`chat` that carries
/// `--script`.
///
/// Why this exists: a `fake` backend is a single FIFO queue. When every bot
/// resolves to the same one (what `--backend fake` means), the turns a script
/// pushes for different bots sit in one queue — and under a multi-threaded
/// runtime a bot woken mid-delegation can pop a turn that was scripted for
/// another bot. A single real `run` happens to be deterministic, but the
/// milestone requires a *scripted* run to be deterministic, and on a busy
/// machine it was not (observed: 7 wrong-turn failures in 25 multi-threaded
/// test runs before this split existed). One queue per bot removes the window
/// entirely: each scripted queue then has exactly one consumer.
///
/// The rules, all of them honesty-preserving:
///
/// * Only bots the script names take part, and only when **two or more** of
///   them would share one backend id. A script naming a single bot has one
///   queue and one consumer — no race — so nothing is split and the backend
///   list stays exactly as configured.
/// * Only backends of kind `fake` are ever split. A bot on a real backend
///   (`open_ai_compat`, `claude_cli`, …) keeps it no matter what the script
///   says: synthesising a fake would silently replace a real model, and
///   [`crate::script::load`] refuses such a script with its own wording.
/// * The synthesised spec is a clone of the shared one with a new id, so
///   backend options (an echo fake stays an echo fake) survive the split.
/// * Bots the script does not name stay on the shared backend: no scripted
///   turn is ever queued there, so what they pop is the same empty-queue
///   placeholder whoever pops it.
///
/// This runs in the config-resolution path (after `--backend`/`--team`/
/// [`team_or_demo`], before [`crate::embedded::Embedded::build`]) because the
/// backend set is built from the config — a running swarm's registry is never
/// mutated.
///
/// It never fails and never reports: an unreadable or invalid script is left
/// to [`crate::script::load`], which runs after the roster exists and owns the
/// canonical refusals (unknown bot, non-fake backend, parse errors). This step
/// must not fork their wording, so on any doubt it simply changes nothing.
pub fn per_bot_fakes_for_script(config: &mut BrainConfig, script: &Path) {
    let Ok(text) = std::fs::read_to_string(script) else {
        return;
    };
    let Ok(turns) = crate::script::parse(&text) else {
        return;
    };
    let handles: Vec<String> = turns.iter().map(|turn| normalize_name(&turn.bot)).collect();
    split_shared_fakes(config, &handles);
}

/// The split itself, over the normalized bot handles a script names. Split out
/// from the file read so the rewrite is testable without a filesystem.
fn split_shared_fakes(config: &mut BrainConfig, handles: &[String]) {
    let Some(team) = &config.team else {
        return;
    };

    // Resolve every scripted handle to (index into team.bots, backend id).
    // Handles the roster does not know are skipped — `script::load` refuses
    // them later, naming the roster. Backends that are not a *configured*
    // `fake` are skipped: a real model must never be replaced by a synthesised
    // fake, and an id the config does not carry is a boot error with its own
    // wording (the registry's built-in `fake` invariant only covers a brain
    // that boots, which this config has not decided yet).
    let mut scripted: Vec<(usize, String)> = Vec::new();
    for handle in handles {
        let Some(index) = team
            .bots
            .iter()
            .position(|bot| normalize_name(&bot.name) == *handle)
        else {
            continue;
        };
        if scripted.iter().any(|(seen, _)| *seen == index) {
            // The same bot scripted twice is still one consumer of one queue.
            continue;
        }
        let backend = team.bots[index]
            .backend
            .clone()
            .unwrap_or_else(|| config.default_backend.clone());
        if config
            .backend(&backend)
            .is_some_and(|spec| spec.kind == BackendKind::Fake)
        {
            scripted.push((index, backend));
        }
    }

    // A backend only has a race to remove when two or more *distinct* scripted
    // bots would share its queue.
    let mut consumers: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, backend) in &scripted {
        *consumers.entry(backend).or_default() += 1;
    }

    // Plan the new ids first (immutable pass), then apply (mutable pass): the
    // borrow of `team` and the writes to `config.backends` cannot overlap.
    let mut plan: Vec<(usize, String, String)> = Vec::new();
    let mut used: BTreeSet<String> = config.backends.iter().map(|spec| spec.id.clone()).collect();
    // The registry's always-present built-in keeps its id, whatever the
    // config lists.
    used.insert(swarmbot_llm::FAKE_BACKEND_ID.to_string());
    for (index, backend) in &scripted {
        if consumers.get(backend.as_str()).copied().unwrap_or(0) < 2 {
            continue;
        }
        let name = normalize_name(&team.bots[*index].name);
        let mut new_id = crate::demo::per_bot_fake_id(&name);
        // A pathological team ("scout researcher" and "scout-researcher" both
        // normalize to the same handle) or a config that already uses the id
        // gets a numbered variant instead of a duplicate-id boot failure.
        for suffix in 2usize.. {
            if used.insert(new_id.clone()) {
                break;
            }
            new_id = format!("{}-{suffix}", crate::demo::per_bot_fake_id(&name));
        }
        plan.push((*index, new_id, backend.clone()));
    }

    for (index, new_id, backend) in plan {
        // `config.backend(&backend)` was Some when the plan was made and
        // nothing removed it since; a miss would mean the config changed
        // underneath this function, and skipping is then the only honest move.
        let Some(mut spec) = config.backend(&backend).cloned() else {
            continue;
        };
        spec.id = new_id.clone();
        config.backends.push(spec);
        if let Some(team) = &mut config.team {
            team.bots[index].backend = Some(new_id);
        }
    }
}

/// Parse a `TeamSpec` TOML file. Parse errors keep the line number the TOML
/// parser reports — "line 7, column 3" is actionable, "invalid team" is not.
pub fn load_team_file(path: &Path) -> Result<TeamSpec> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the team file {}", path.display()))?;
    let team: TeamSpec = toml::from_str(&text)
        .with_context(|| format!("cannot parse the team file {}", path.display()))?;
    team.validate()
        .map_err(|err| anyhow!("{}: {err}", path.display()))?;
    Ok(team)
}

/// Parse a possibly partial `BrainConfig` by merging it over the defaults.
fn parse_config(text: &str, origin: &Path) -> Result<BrainConfig> {
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
        .context("the built-in default config does not serialize — this is a bug")?;
    merge_json(&mut merged, overlay);
    serde_json::from_value(merged)
        .with_context(|| format!("{} does not describe a valid BrainConfig", origin.display()))
}

/// The shipped example config and the docs spell the OpenAI kind
/// `openai_compat`; the serde variant is `open_ai_compat`. Accept the spelling
/// people will actually type rather than failing on a copy of our own example.
fn normalize_backend_kinds(overlay: &mut JsonValue) {
    let Some(backends) = overlay
        .get_mut("backends")
        .and_then(JsonValue::as_array_mut)
    else {
        return;
    };
    for backend in backends {
        if let Some(JsonValue::String(kind)) = backend.get_mut("kind")
            && kind == "openai_compat"
        {
            *kind = "open_ai_compat".to_string();
        }
    }
}

/// A config's `[[backends]]` array replaces the default one wholesale (appending
/// to a list the defaults filled is never what a config file means) — but each
/// entry may still be partial, so every entry is layered over the defaults for
/// its kind. Without this, a two-line backend spec fails on fields nobody
/// thinks about (`timeout_secs`).
fn layer_backends(overlay: &mut JsonValue) -> Result<()> {
    let Some(backends) = overlay
        .get("backends")
        .and_then(JsonValue::as_array)
        .cloned()
    else {
        return Ok(());
    };
    let mut layered = Vec::with_capacity(backends.len());
    for entry in backends {
        let mut base = serde_json::to_value(backend_base(&entry))?;
        merge_json(&mut base, entry);
        layered.push(base);
    }
    overlay["backends"] = JsonValue::Array(layered);
    Ok(())
}

/// The defaults one `[[backends]]` entry inherits, keyed by its `kind`.
fn backend_base(entry: &JsonValue) -> BackendSpec {
    let id = entry
        .get("id")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();
    let kind = entry
        .get("kind")
        .and_then(JsonValue::as_str)
        .unwrap_or("fake");
    let mut base = BackendSpec::fake(id);
    if kind != "fake" {
        // Network kinds inherit the timeouts/retries their constructors use,
        // and no model: a missing `model` should stay visibly missing.
        base.timeout_secs = 180;
        base.max_retries = 3;
        base.model = None;
    }
    base
}

/// Deep-merge `overlay` into `base`: objects merge key by key, everything
/// else (including arrays — `[[backends]]`, team lists) replaces wholesale,
/// because appending to a list the defaults already filled is never what a
/// config file means.
fn merge_json(base: &mut JsonValue, overlay: JsonValue) {
    match (base, overlay) {
        (JsonValue::Object(base_map), JsonValue::Object(overlay_map)) => {
            for (key, value) in overlay_map {
                merge_json(base_map.entry(key).or_insert(JsonValue::Null), value);
            }
        }
        (slot, value) => *slot = value,
    }
}

/// Secrets from the process environment (only the variables the configured
/// backends name) and `<data-dir>/secrets.json` when present. The file wins
/// over the environment, matching the registry's own "secrets store first"
/// precedence.
///
/// The returned map is handed straight to `compose`; no code path renders its
/// values.
pub fn load_secrets(data_dir: &Path, config: &BrainConfig) -> Result<BTreeMap<String, String>> {
    let mut secrets = BTreeMap::new();
    for backend in &config.backends {
        if let Some(name) = &backend.api_key_env
            && let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            secrets.insert(name.clone(), value);
        }
    }
    let path = data_dir.join("secrets.json");
    if path.exists() {
        warn_if_not_private(&path);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let file: BTreeMap<String, String> = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a JSON object of string keys", path.display()))?;
        secrets.extend(file);
    }
    Ok(secrets)
}

/// A secrets file anybody on the box can read is a leaked key waiting to
/// happen; say so (without, of course, printing any of it).
#[cfg(unix)]
fn warn_if_not_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                "{} is mode {mode:o}; run `chmod 600 {}` so only you can read it",
                path.display(),
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_not_private(_path: &Path) {}

/// The rendered `config show` output. Deliberately a hand-built string, not a
/// serialization of the config: the point is what a human checks before a run,
/// and the invariant that no key value can appear — the only secret-adjacent
/// facts rendered are variable *names* and whether they are set.
pub fn show(loaded: &Loaded) -> String {
    let config = &loaded.config;
    let mut out = String::new();
    out.push_str(&format!("config source: {}\n", loaded.source.describe()));
    out.push_str(&format!("data_dir: {}\n", loaded.data_dir.display()));
    out.push_str(&format!("public_addr: {}\n", config.public_addr));
    out.push_str(&format!(
        "internal_addr: {}\n",
        config.internal_addr.as_deref().unwrap_or("(disabled)")
    ));
    out.push_str(&format!("default_backend: {}\n", config.default_backend));
    out.push_str("backends:\n");
    if config.backends.is_empty() {
        out.push_str("  (none — only the built-in `fake` will resolve)\n");
    }
    for backend in &config.backends {
        let view = backend.public_view();
        let kind = view["kind"].as_str().unwrap_or("?");
        let model = view["model"].as_str().unwrap_or("-");
        let mut line = format!("  {:<12} kind={kind:<14} model={model}", backend.id);
        if let Some(url) = view["base_url"].as_str() {
            line.push_str(&format!("  base_url={url}"));
        }
        if let Some(env) = view["api_key_env"].as_str() {
            let state = if loaded.secrets.contains_key(env) {
                "set"
            } else {
                "MISSING"
            };
            // The name of the variable is public information; its value is not
            // in this map unless it resolved, and only its presence is shown.
            line.push_str(&format!("  api_key_env={env} ({state})"));
        }
        out.push_str(&line);
        out.push('\n');
    }
    match &config.team {
        Some(team) => {
            let bots: Vec<String> = team
                .bots
                .iter()
                .map(|bot| {
                    format!(
                        "@{} ({})",
                        bot.name,
                        bot.backend.as_deref().unwrap_or(&config.default_backend)
                    )
                })
                .collect();
            out.push_str(&format!(
                "team: {} bots: {}\n",
                team.bots.len(),
                bots.join(", ")
            ));
            for group in &team.groups {
                out.push_str(&format!(
                    "  group \"{}\": members [{}], lead @{}\n",
                    group.title,
                    group.members.join(", "),
                    group.lead
                ));
            }
        }
        None => out.push_str("team: (none — commands will fall back to the demo team)\n"),
    }
    let settings = &config.settings;
    out.push_str(&format!(
        "settings: bot_messaging={} max_hops={} max_tool_rounds={} tasks_per_bot={} \
         approval_expiry={}s auto_approve_read_only={}\n",
        settings.bot_messaging,
        settings.max_hops,
        settings.max_tool_rounds,
        settings.max_running_tasks_per_bot,
        settings.approval_expiry_secs,
        settings.auto_approve_read_only,
    ));
    let secrets_path = loaded.data_dir.join("secrets.json");
    if secrets_path.exists() {
        // Key *names* only. Printing the values would defeat the 0600.
        let names: Vec<&String> = loaded.secrets.keys().collect();
        out.push_str(&format!(
            "secrets: {} ({} key(s): {})\n",
            secrets_path.display(),
            names.len(),
            names
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        out.push_str(&format!(
            "secrets: {} not present; keys come from the environment\n",
            secrets_path.display()
        ));
    }
    if !loaded.roots.is_empty() {
        out.push_str("extra roots (--root):\n");
        for root in &loaded.roots {
            out.push_str(&format!("  {} ({:?})\n", root.path, root.mode));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmp::TempDir;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).expect("write");
        path
    }

    #[test]
    fn defaults_apply_when_there_is_no_file() {
        let dir = TempDir::new("config").expect("temp");
        let loaded = load(&LoadRequest {
            data_dir: Some(dir.path()),
            ..Default::default()
        })
        .expect("loads");
        assert_eq!(loaded.source, ConfigSource::Defaults);
        assert_eq!(loaded.config.default_backend, "fake");
        assert_eq!(loaded.data_dir, dir.path());
        assert!(loaded.roots.is_empty());
    }

    #[test]
    fn a_partial_toml_file_merges_over_the_defaults() {
        let dir = TempDir::new("config").expect("temp");
        write(
            dir.path(),
            "config.toml",
            "default_backend = \"inferhub\"\n\n[[backends]]\nid = \"fake\"\nkind = \"fake\"\n",
        );
        let loaded = load(&LoadRequest {
            data_dir: Some(dir.path()),
            ..Default::default()
        })
        .expect("loads");
        assert!(matches!(loaded.source, ConfigSource::File(_)));
        assert_eq!(loaded.config.default_backend, "inferhub");
        // The file's backends array replaced the default one wholesale.
        assert_eq!(loaded.config.backends.len(), 1);
        // Untouched keys kept their defaults.
        assert_eq!(
            loaded.config.public_addr,
            BrainConfig::default().public_addr
        );
        assert_eq!(loaded.config.settings.max_hops, 8);
    }

    #[test]
    fn an_explicit_config_that_is_missing_or_broken_says_so() {
        let dir = TempDir::new("config").expect("temp");
        let missing = dir.path().join("nope.toml");
        let err = load(&LoadRequest {
            config_path: Some(&missing),
            data_dir: Some(dir.path()),
            ..Default::default()
        })
        .expect_err("missing file");
        assert!(err.to_string().contains("cannot read"), "{err}");

        let broken = write(dir.path(), "broken.toml", "default_backend = \n");
        let err = load(&LoadRequest {
            config_path: Some(&broken),
            data_dir: Some(dir.path()),
            ..Default::default()
        })
        .expect_err("bad toml");
        // The TOML parser's own line number survives into the message chain
        // (which is what `swarmbot` prints with {err:#}).
        assert!(format!("{err:#}").contains("line"), "{err:#}");
    }

    #[test]
    fn the_backend_flag_overrides_the_team() {
        let dir = TempDir::new("config").expect("temp");
        let team = write(
            dir.path(),
            "team.toml",
            "[[bots]]\nname = \"lead\"\nlabel = \"Lead\"\nbackend = \"other\"\n\n\
             [[bots]]\nname = \"scout\"\nlabel = \"Scout\"\n\n\
             [[groups]]\ntitle = \"Team\"\nmembers = [\"lead\", \"scout\"]\nlead = \"lead\"\n",
        );
        let loaded = load(&LoadRequest {
            data_dir: Some(dir.path()),
            team_file: Some(&team),
            backend: Some("fake-x"),
            ..Default::default()
        })
        .expect("loads");
        assert_eq!(loaded.config.default_backend, "fake-x");
        let team = loaded.config.team.as_ref().expect("a team");
        assert!(
            team.bots
                .iter()
                .all(|b| b.backend.as_deref() == Some("fake-x"))
        );
    }

    #[test]
    fn a_bad_team_file_reports_the_toml_line() {
        let dir = TempDir::new("config").expect("temp");
        let team = write(dir.path(), "team.toml", "[[bots]]\nname = \n");
        let err = load_team_file(&team).expect_err("broken team");
        assert!(format!("{err:#}").contains("line"), "{err:#}");
    }

    #[test]
    fn a_team_naming_an_unknown_bot_is_refused() {
        let dir = TempDir::new("config").expect("temp");
        let team = write(
            dir.path(),
            "team.toml",
            "[[groups]]\ntitle = \"Team\"\nmembers = [\"ghost\"]\nlead = \"ghost\"\n",
        );
        let err = load_team_file(&team).expect_err("unknown member");
        assert!(err.to_string().contains("unknown bot"), "{err:#}");
    }

    #[test]
    fn roots_become_read_write_cli_node_roots() {
        let dir = TempDir::new("config").expect("temp");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("create");
        let roots = vec![project.clone()];
        let loaded = load(&LoadRequest {
            data_dir: Some(dir.path()),
            roots: &roots,
            ..Default::default()
        })
        .expect("loads");
        assert_eq!(loaded.roots.len(), 1);
        assert_eq!(loaded.roots[0].id, "cli");
        assert_eq!(
            loaded.roots[0].mode,
            swarmbot_contracts::node::RootMode::ReadWrite
        );
        assert_eq!(loaded.roots[0].path, project.to_string_lossy());
    }

    #[test]
    fn secrets_come_from_the_file_and_never_reach_the_shown_config() {
        let dir = TempDir::new("config").expect("temp");
        write(
            dir.path(),
            "config.toml",
            "default_backend = \"inferhub\"\n\n\
             [[backends]]\nid = \"inferhub\"\nkind = \"openai_compat\"\n\
             base_url = \"https://api.example.invalid/v1\"\napi_key_env = \"INFERHUB_API_KEY\"\n\
             model = \"m\"\n",
        );
        write(
            dir.path(),
            "secrets.json",
            "{\"INFERHUB_API_KEY\": \"sk-super-secret-value\"}",
        );
        let loaded = load(&LoadRequest {
            data_dir: Some(dir.path()),
            ..Default::default()
        })
        .expect("loads");
        assert_eq!(
            loaded.secrets.get("INFERHUB_API_KEY").map(String::as_str),
            Some("sk-super-secret-value")
        );

        let shown = show(&loaded);
        assert!(
            !shown.contains("sk-super-secret-value"),
            "a key value must never be rendered:\n{shown}"
        );
        assert!(shown.contains("INFERHUB_API_KEY (set)"), "{shown}");
        assert!(shown.contains("secrets.json"), "{shown}");
    }

    #[test]
    fn the_debug_impl_redacts_secret_values() {
        let mut secrets = BTreeMap::new();
        secrets.insert(
            "INFERHUB_API_KEY".to_string(),
            "sk-secret-value".to_string(),
        );
        let loaded = Loaded {
            config: BrainConfig::default(),
            source: ConfigSource::Defaults,
            secrets,
            data_dir: PathBuf::from("data"),
            roots: Vec::new(),
        };
        let debug = format!("{loaded:?}");
        assert!(!debug.contains("sk-secret-value"), "{debug}");
        assert!(debug.contains("1 key(s), values redacted"), "{debug}");
    }

    #[test]
    fn a_missing_env_key_is_shown_as_missing_not_printed() {
        let loaded = Loaded {
            config: BrainConfig {
                default_backend: "inferhub".into(),
                backends: vec![swarmbot_contracts::config::BackendSpec::openai_compat(
                    "inferhub",
                    "https://api.example.invalid/v1",
                    "SWARMBOT_CLI_TEST_UNSET_KEY",
                    "m",
                )],
                ..BrainConfig::default()
            },
            source: ConfigSource::Defaults,
            secrets: BTreeMap::new(),
            data_dir: PathBuf::from("data"),
            roots: Vec::new(),
        };
        let shown = show(&loaded);
        assert!(
            shown.contains("SWARMBOT_CLI_TEST_UNSET_KEY (MISSING)"),
            "{shown}"
        );
    }

    #[test]
    fn team_or_demo_falls_back_to_the_demo_team_on_the_default_backend() {
        let mut config = BrainConfig {
            default_backend: "fake-z".into(),
            backends: vec![swarmbot_contracts::config::BackendSpec::fake("fake-z")],
            ..BrainConfig::default()
        };
        assert!(config.team.is_none());
        team_or_demo(&mut config);
        let team = config.team.as_ref().expect("a team now");
        assert_eq!(team.bots.len(), 4);
        assert!(
            team.bots
                .iter()
                .all(|b| b.backend.as_deref() == Some("fake-z"))
        );
    }

    // ---- the `--script` per-bot fake split ----

    /// The demo team on the shared `fake`, resolved through the split with
    /// `text` as the script — exactly what `cmd_run` does for
    /// `--backend fake --script FILE`.
    fn split_with_script(dir: &Path, text: &str) -> BrainConfig {
        let path = write(dir, "script.toml", text);
        let mut config = BrainConfig {
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        per_bot_fakes_for_script(&mut config, &path);
        config
    }

    fn backend_of(config: &BrainConfig, bot: &str) -> Option<String> {
        config
            .team
            .as_ref()
            .expect("a team")
            .bots
            .iter()
            .find(|candidate| candidate.name == bot)
            .and_then(|candidate| candidate.backend.clone())
    }

    fn backend_ids(config: &BrainConfig) -> Vec<&str> {
        config
            .backends
            .iter()
            .map(|spec| spec.id.as_str())
            .collect()
    }

    #[test]
    fn a_script_naming_two_bots_splits_the_shared_fake_per_bot() {
        let dir = TempDir::new("config-split").expect("temp");
        // Handles are normalized like the roster does ("@Scout" → scout), and
        // the same bot scripted twice is still one consumer.
        let config = split_with_script(
            dir.path(),
            "[[turns]]\nbot = \"lead\"\ntext = \"a\"\n\n\
             [[turns]]\nbot = \"@Scout\"\ntext = \"b\"\n\n\
             [[turns]]\nbot = \"lead\"\ntext = \"c\"\n",
        );
        assert_eq!(
            backend_ids(&config),
            vec!["fake", "fake-lead", "fake-scout"],
            "one synthesised fake per scripted bot, the demo's naming"
        );
        assert_eq!(backend_of(&config, "lead").as_deref(), Some("fake-lead"));
        assert_eq!(backend_of(&config, "scout").as_deref(), Some("fake-scout"));
        assert_eq!(
            backend_of(&config, "dev").as_deref(),
            Some("fake"),
            "a bot the script does not name stays on the shared fake"
        );
        assert_eq!(backend_of(&config, "sentinel").as_deref(), Some("fake"));
        assert!(
            config
                .backends
                .iter()
                .all(|spec| spec.kind == BackendKind::Fake),
            "the split only ever synthesises fakes"
        );
        config.validate().expect("the split config still validates");
    }

    #[test]
    fn a_script_naming_one_bot_synthesises_nothing() {
        let dir = TempDir::new("config-split-one").expect("temp");
        // One bot, one queue, no race: the change must stay minimal, so the
        // whole config is asserted byte-for-byte unchanged.
        let before = BrainConfig {
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        let mut config = before.clone();
        let path = write(
            dir.path(),
            "script.toml",
            "[[turns]]\nbot = \"lead\"\ntext = \"a\"\n\n[[turns]]\nbot = \"lead\"\ntext = \"b\"\n",
        );
        per_bot_fakes_for_script(&mut config, &path);
        assert_eq!(config, before, "the backend list is unchanged");
    }

    #[test]
    fn two_scripted_bots_on_two_fakes_share_nothing_so_nothing_splits() {
        let dir = TempDir::new("config-split-apart").expect("temp");
        // lead on `fake-a`, scout on `fake-b`: two queues, two consumers — no
        // race to remove, no synthesised backend.
        let mut config = BrainConfig {
            default_backend: "fake-a".into(),
            backends: vec![BackendSpec::fake("fake-a"), BackendSpec::fake("fake-b")],
            team: Some(BrainConfig::demo_team("fake-a")),
            ..BrainConfig::default()
        };
        if let Some(team) = &mut config.team {
            team.bots[1].backend = Some("fake-b".into());
        }
        let before = config.clone();
        let path = write(
            dir.path(),
            "script.toml",
            "[[turns]]\nbot = \"lead\"\ntext = \"a\"\n\n[[turns]]\nbot = \"scout\"\ntext = \"b\"\n",
        );
        per_bot_fakes_for_script(&mut config, &path);
        assert_eq!(config, before, "separate queues need no split");
    }

    #[test]
    fn a_broken_or_missing_script_changes_nothing() {
        let dir = TempDir::new("config-split-broken").expect("temp");
        // An invalid script is `script::load`'s refusal (with the canonical
        // wording, after the roster exists) — the split must not fork it, so
        // it silently does nothing and the boot proceeds to the real error.
        let before = BrainConfig {
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        for path in [
            write(dir.path(), "broken.toml", "[[turns]]\nbot = \n"),
            write(
                dir.path(),
                "typo.toml",
                "[[turns]]\nbot = \"lead\"\n[[turns.tool_call]]\nname = \"shell\"\n",
            ),
            dir.path().join("missing.toml"),
        ] {
            let mut config = before.clone();
            per_bot_fakes_for_script(&mut config, &path);
            assert_eq!(config, before, "{} changes nothing", path.display());
        }
    }

    #[test]
    fn a_bot_naming_no_backend_splits_via_the_default() {
        let dir = TempDir::new("config-split-default").expect("temp");
        // Team files often leave `backend` unset; the default resolves it, and
        // the split must see the backend the boot will resolve.
        let mut config = BrainConfig {
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        if let Some(team) = &mut config.team {
            for bot in &mut team.bots {
                bot.backend = None;
            }
        }
        let path = write(
            dir.path(),
            "script.toml",
            "[[turns]]\nbot = \"lead\"\ntext = \"a\"\n\n[[turns]]\nbot = \"scout\"\ntext = \"b\"\n",
        );
        per_bot_fakes_for_script(&mut config, &path);
        assert_eq!(backend_of(&config, "lead").as_deref(), Some("fake-lead"));
        assert_eq!(backend_of(&config, "scout").as_deref(), Some("fake-scout"));
        assert_eq!(
            backend_of(&config, "dev"),
            None,
            "an unscripted bot keeps inheriting the default"
        );
        assert_eq!(config.default_backend, "fake");
        config.validate().expect("valid");
    }

    #[test]
    fn a_split_fake_keeps_its_options() {
        let dir = TempDir::new("config-split-echo").expect("temp");
        // Two scripted bots on a shared *echo* fake: each gets its own clone,
        // so turns beyond the script still echo instead of falling back to the
        // placeholder — the split changes queue ownership, not behaviour.
        let mut echo = BackendSpec::fake("mirror");
        echo.options
            .insert("echo".to_string(), serde_json::Value::Bool(true));
        let mut config = BrainConfig {
            default_backend: "mirror".into(),
            backends: vec![echo],
            team: Some(BrainConfig::demo_team("mirror")),
            ..BrainConfig::default()
        };
        let path = write(
            dir.path(),
            "script.toml",
            "[[turns]]\nbot = \"lead\"\ntext = \"a\"\n\n[[turns]]\nbot = \"scout\"\ntext = \"b\"\n",
        );
        per_bot_fakes_for_script(&mut config, &path);
        let lead = backend_of(&config, "lead").expect("a per-bot fake");
        let spec = config.backend(&lead).expect("configured");
        assert_eq!(
            spec.options.get("echo").and_then(|value| value.as_bool()),
            Some(true),
            "an echo fake stays an echo fake per bot"
        );
        config.validate().expect("valid");
    }

    #[test]
    fn a_synthesised_id_never_collides_with_a_configured_one() {
        let dir = TempDir::new("config-split-collision").expect("temp");
        // A config that already uses `fake-lead` for something else: the
        // synthesised id gets a numbered variant instead of a duplicate-id
        // boot failure (the registry refuses duplicate ids).
        let mut config = BrainConfig {
            backends: vec![BackendSpec::fake("fake"), BackendSpec::fake("fake-lead")],
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        let path = write(
            dir.path(),
            "script.toml",
            "[[turns]]\nbot = \"lead\"\ntext = \"a\"\n\n[[turns]]\nbot = \"scout\"\ntext = \"b\"\n",
        );
        per_bot_fakes_for_script(&mut config, &path);
        assert_eq!(
            backend_ids(&config),
            vec!["fake", "fake-lead", "fake-lead-2", "fake-scout"]
        );
        assert_eq!(
            backend_of(&config, "lead").as_deref(),
            Some("fake-lead-2"),
            "the lead got the free id, not the taken one"
        );
        assert_eq!(backend_of(&config, "scout").as_deref(), Some("fake-scout"));
        config.validate().expect("valid");
    }

    #[test]
    fn a_real_backend_is_never_split_even_when_shared_by_the_whole_script() {
        let dir = TempDir::new("config-split-real").expect("temp");
        // The shape that splits a fake — two scripted bots, one shared
        // backend — must leave a real model alone: `script::load` refuses the
        // script, and synthesising a fake would silently replace the model.
        let mut config = BrainConfig {
            default_backend: "inferhub".into(),
            backends: vec![BackendSpec::openai_compat(
                "inferhub",
                "https://api.example.invalid/v1",
                "SWARMBOT_CLI_TEST_UNSET_KEY",
                "m",
            )],
            team: Some(BrainConfig::demo_team("inferhub")),
            ..BrainConfig::default()
        };
        let before = config.clone();
        let path = write(
            dir.path(),
            "script.toml",
            "[[turns]]\nbot = \"lead\"\ntext = \"a\"\n\n[[turns]]\nbot = \"scout\"\ntext = \"b\"\n",
        );
        per_bot_fakes_for_script(&mut config, &path);
        assert_eq!(config, before, "a real backend is never touched");
    }
}
