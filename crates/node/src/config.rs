//! What this node is: its data dir, the directories it will act inside, and
//! the ceilings it will not let a brain raise.
//!
//! Two invariants live here, and both are about who owns the machine:
//!
//! * **CLI wins over the file.** `<data>/node.toml` is what the owner wrote
//!   down once; `--root`, `--ceiling`, `--no-shell` are what they mean *this
//!   run*. A flag that lost to a stale file would silently widen or narrow
//!   what a daemon does, and neither direction is safe to guess.
//! * **A ceiling nobody set is `ask`.** [`default_ceilings`] follows
//!   [`swarmbot_contracts::PolicyMap::default`]: read-only work inside the
//!   roots runs, everything else asks. Nothing here defaults to `always`,
//!   because an auto-allow nobody granted is a bypass.
//!
//! The file holds no secrets (the keypair lives in `identity.json`, mode
//! 0600), so it is written world-readable like any other config.
//!
//! # The default roots
//!
//! A node that never had a root configured still gets two: its workspace
//! (read-write) and its **spill directory** (read-only). The second one exists
//! because a `shell.exec` exit chunk names `<data>/spill/<inv>-stdout.log` when
//! output went over the cap — and a path the bot cannot open is a broken
//! promise, not a policy. It is read-only on purpose: reading your own output
//! back is not permission to use that directory as scratch space, and a write
//! there would bypass neither the ceiling nor the audit line but would make the
//! spill dir a second, unaudited workspace.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use swarmbot_contracts::{Capabilities, NodeRoot, PolicyCeiling, PolicyMap, RootMode, Surface};
use swarmbot_tools::{PathPolicy, PolicyEngine};

use crate::NodeError;

/// The capability names this build can honestly declare. `desktop`, `browser`
/// and `pty` are never declared: a node that advertises what it cannot do
/// teaches the brain to hand it work that will fail (or, worse, to believe a
/// fake success).
pub const CAP_FS: &str = "fs@1";
pub const CAP_SHELL: &str = "shell@1";

/// Default heartbeat period (§3.4 of the node protocol doc: 15 s).
pub const DEFAULT_HEARTBEAT_SECS: u64 = 15;

/// The name of the config file inside the data dir.
pub const CONFIG_FILE: &str = "node.toml";

/// The node's data directory layout. Everything the daemon persists lives
/// under one root so `--data-dir` moves the whole node — including its
/// identity, which is why the default is the XDG data dir the tools crate's
/// deny list already protects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodePaths {
    pub data_dir: PathBuf,
}

impl NodePaths {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    /// The default data dir: `$XDG_DATA_HOME/swarmbot` (usually
    /// `~/.local/share/swarmbot`), which is exactly where
    /// [`PathPolicy::default_deny_list`] expects a node's `identity.json` to
    /// be, so a bot can never read the key that signs its own node's upgrades.
    /// Falls back to `./data` when the platform has no data dir.
    pub fn default_dir() -> PathBuf {
        dirs::data_dir()
            .map(|dir| dir.join("swarmbot"))
            .unwrap_or_else(|| PathBuf::from("data"))
    }

    pub fn identity(&self) -> PathBuf {
        self.data_dir.join("identity.json")
    }

    pub fn config(&self) -> PathBuf {
        self.data_dir.join(CONFIG_FILE)
    }

    /// The default workspace root: where relative paths and shell commands
    /// land when the owner named no root of their own.
    pub fn workspace(&self) -> PathBuf {
        self.data_dir.join("workspace")
    }

    pub fn audit(&self) -> PathBuf {
        self.data_dir.join("audit")
    }

    /// Output beyond the per-stream cap is spilled here so a model can page it
    /// instead of losing it. It is a **read-only root** by default: a bot may
    /// read a spilled file back, never write here (see [`NodeConfig::default`]
    /// and [`NodeConfig::new_in`]).
    pub fn spill(&self) -> PathBuf {
        self.data_dir.join("spill")
    }

    /// The live-state file `run` maintains and `status` reads.
    pub fn state(&self) -> PathBuf {
        self.data_dir.join("node-state.json")
    }

    /// Create the directories a node needs before it can run. A failure here
    /// is an error, not a warning: a node that cannot create its workspace
    /// would answer every call with a path denial the owner cannot diagnose.
    pub fn ensure_dirs(&self, workspace: &Path) -> Result<(), NodeError> {
        for dir in [&self.data_dir, workspace, &self.audit(), &self.spill()] {
            std::fs::create_dir_all(dir).map_err(|source| NodeError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        Ok(())
    }
}

/// The on-disk shape of `<data>/node.toml`. Every field is optional so an
/// empty file is a valid config, and so a file written by an older build still
/// loads.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    label: Option<String>,
    /// `ws://`, `wss://`, `http://` or `https://` — the control socket is
    /// derived from it, and `http(s)` is what pairing posts to.
    #[serde(default)]
    brain_url: Option<String>,
    #[serde(default)]
    heartbeat_secs: Option<u64>,
    /// `false` withdraws the `shell@1` capability entirely.
    #[serde(default)]
    shell: Option<bool>,
    #[serde(default)]
    auto_approve_read_only: Option<bool>,
    #[serde(default)]
    roots: Vec<RootFile>,
    /// Extra deny entries on top of [`PathPolicy::default_deny_list`].
    #[serde(default)]
    deny: Vec<PathBuf>,
    #[serde(default)]
    ceilings: Option<PolicyMap>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct RootFile {
    /// Optional: a root without an id gets one derived from its directory name.
    #[serde(default)]
    id: Option<String>,
    path: PathBuf,
    #[serde(default)]
    mode: Option<RootMode>,
}

/// The resolved configuration of one node.
///
/// `Debug` is derived because nothing in here is secret — the keypair lives in
/// [`crate::identity::Identity`], which redacts by hand.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeConfig {
    pub data_dir: PathBuf,
    /// How the owner refers to this machine; defaults to the hostname.
    pub label: String,
    /// Where the brain is, as typed (`ws://`, `wss://`, `http://` or `https://`).
    pub brain_url: Option<String>,
    pub roots: Vec<NodeRoot>,
    /// Extra deny entries on top of the tools crate's default list.
    pub deny: Vec<PathBuf>,
    /// The node's **own** ceilings. A brain can ask; it can never raise these.
    pub ceilings: PolicyMap,
    /// Read-only surfaces skip the approval round-trip when the ceiling says
    /// `ask`. Off by default in the file, on by default in practice: reading
    /// inside a root the owner granted is what the grant was for.
    pub auto_approve_read_only: bool,
    pub shell_enabled: bool,
    pub heartbeat_secs: u64,
}

impl Default for NodeConfig {
    fn default() -> Self {
        let data_dir = NodePaths::default_dir();
        Self {
            label: hostname(),
            brain_url: None,
            roots: default_roots(&data_dir),
            deny: Vec::new(),
            ceilings: default_ceilings(),
            auto_approve_read_only: true,
            shell_enabled: true,
            heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
            data_dir,
        }
    }
}

/// The ceilings a node starts with: `ask` everywhere, `always` for reading.
///
/// Deliberately *not* `uniform(Always)`: a node whose owner never thought
/// about policy must still ask before it writes or shells out.
pub fn default_ceilings() -> PolicyMap {
    let mut map = PolicyMap::default();
    map.set(Surface::ReadFile, PolicyCeiling::Always);
    map
}

/// The roots a node starts with when the owner configured none.
///
/// Two of them, in this order:
///
/// 1. `<data>/workspace`, **read-write** — and first, because
///    [`NodeConfig::workspace`] and the hello roster both take the first
///    read-write root as "where this node works";
/// 2. `<data>/spill`, **read-only** — the directory a spilled `shell.exec`
///    result names, so the bot can page its own output back with `fs.read`.
///
/// The spill root is read-only because the two capabilities are different: being
/// able to re-read truncated output is a repair, while writing into that
/// directory would make it a second workspace that the owner never granted.
/// A write there is refused with `PathDenied { reason: "read-only root" }`.
///
/// An explicit `--root`/`--read-only-root` replaces these (the CLI is what the
/// owner means *this run*); an owner who names roots themselves and still wants
/// spilled output readable adds `--read-only-root <data>/spill`.
fn default_roots(data_dir: &Path) -> Vec<NodeRoot> {
    let paths = NodePaths::new(data_dir.to_path_buf());
    vec![
        NodeRoot::read_write("workspace", paths.workspace().display().to_string()),
        NodeRoot::read_only("spill", paths.spill().display().to_string()),
    ]
}

impl NodeConfig {
    /// The defaults rooted at one data dir, without reading a file. `load` is
    /// what the daemon uses; this is what a test (or an embedder that builds a
    /// node in memory) uses when it wants the default workspace root to point
    /// at a `TempDir`.
    pub fn new_in(data_dir: impl Into<PathBuf>) -> NodeConfig {
        let data_dir = expand_tilde(data_dir.into());
        NodeConfig {
            label: hostname(),
            brain_url: None,
            roots: default_roots(&data_dir),
            deny: Vec::new(),
            ceilings: default_ceilings(),
            auto_approve_read_only: true,
            shell_enabled: true,
            heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
            data_dir,
        }
    }

    /// Load `<data_dir>/node.toml` over the defaults. A missing file is normal
    /// (a first run); a malformed one is an error, because silently falling
    /// back to defaults would hide a typo that changed what this node allows.
    pub fn load(data_dir: impl Into<PathBuf>) -> Result<NodeConfig, NodeError> {
        let data_dir = expand_tilde(data_dir.into());
        let paths = NodePaths::new(data_dir.clone());
        let mut config = NodeConfig::new_in(data_dir);

        let path = paths.config();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(config),
            Err(source) => return Err(NodeError::Io { path, source }),
        };
        let file: ConfigFile = toml::from_str(&text)
            .map_err(|error| NodeError::Config(format!("{}: {error}", path.display())))?;

        if let Some(label) = file.label {
            config.label = label;
        }
        if let Some(url) = file.brain_url {
            config.brain_url = Some(url);
        }
        if let Some(secs) = file.heartbeat_secs {
            config.heartbeat_secs = secs.max(1);
        }
        if let Some(shell) = file.shell {
            config.shell_enabled = shell;
        }
        if let Some(auto) = file.auto_approve_read_only {
            config.auto_approve_read_only = auto;
        }
        if let Some(ceilings) = file.ceilings {
            config.ceilings = ceilings;
        }
        config.deny = file.deny;
        if !file.roots.is_empty() {
            config.roots = roots_from(file.roots.into_iter().map(|root| {
                (
                    root.id,
                    expand_tilde(root.path),
                    root.mode.unwrap_or(RootMode::ReadWrite),
                )
            }));
        }
        Ok(config)
    }

    /// Write `<data_dir>/node.toml`. Used by `pair` (so the brain URL survives)
    /// and available to an owner who would rather edit a file than repeat six
    /// flags.
    pub fn save(&self) -> Result<(), NodeError> {
        let paths = self.paths();
        std::fs::create_dir_all(&paths.data_dir).map_err(|source| NodeError::Io {
            path: paths.data_dir.clone(),
            source,
        })?;
        let file = ConfigFile {
            label: Some(self.label.clone()),
            brain_url: self.brain_url.clone(),
            heartbeat_secs: Some(self.heartbeat_secs),
            shell: Some(self.shell_enabled),
            auto_approve_read_only: Some(self.auto_approve_read_only),
            roots: self
                .roots
                .iter()
                .map(|root| RootFile {
                    id: Some(root.id.clone()),
                    path: PathBuf::from(&root.path),
                    mode: Some(root.mode),
                })
                .collect(),
            deny: self.deny.clone(),
            ceilings: Some(self.ceilings.clone()),
        };
        let text = toml::to_string_pretty(&file)
            .map_err(|error| NodeError::Config(format!("cannot render node.toml: {error}")))?;
        let path = paths.config();
        std::fs::write(&path, text).map_err(|source| NodeError::Io { path, source })?;
        Ok(())
    }

    pub fn paths(&self) -> NodePaths {
        NodePaths::new(self.data_dir.clone())
    }

    /// The filesystem boundary of this node: its roots plus the credential-shaped
    /// default deny list (and any extra the owner named).
    pub fn path_policy(&self) -> PathPolicy {
        let mut deny = PathPolicy::default_deny_list();
        deny.extend(self.deny.iter().cloned());
        PathPolicy::new(self.roots.clone(), deny)
    }

    /// The ceiling engine.
    ///
    /// The node's own map goes in **both** slots: it never learns the brain's
    /// ceilings, and it does not need to — [`PolicyEngine::ceiling`] is
    /// `min(brain, node)`, so feeding it the node's map twice yields exactly
    /// the node's map, and the brain applies its own `min` before forwarding.
    /// A request from the brain therefore cannot raise anything here.
    pub fn policy_engine(&self) -> PolicyEngine {
        PolicyEngine::new(
            self.ceilings.clone(),
            self.ceilings.clone(),
            self.auto_approve_read_only,
        )
    }

    /// What this node honestly declares: always `fs@1`, and `shell@1` only
    /// while shell is enabled.
    pub fn capabilities(&self) -> Capabilities {
        let mut names = vec![CAP_FS];
        if self.shell_enabled {
            names.push(CAP_SHELL);
        }
        Capabilities::new(names)
    }

    /// Where relative paths and shell commands land: the first read-write root,
    /// else the first root, else `<data>/workspace`.
    pub fn workspace(&self) -> PathBuf {
        self.roots
            .iter()
            .find(|root| matches!(root.mode, RootMode::ReadWrite))
            .or_else(|| self.roots.first())
            .map(|root| PathBuf::from(&root.path))
            .unwrap_or_else(|| self.paths().workspace())
    }

    /// The strictest ceiling across every surface — the honest aggregate to
    /// report in `node.hello`, whose `policy.ceiling` is a single value.
    ///
    /// Reporting the *loosest* would tell the brain it may skip approvals this
    /// node will still demand; reporting the strictest can only cost an extra
    /// question, which is the safe direction.
    pub fn aggregate_ceiling(&self) -> PolicyCeiling {
        self.ceilings
            .surfaces
            .values()
            .fold(self.ceilings.default, |acc, ceiling| acc.min(*ceiling))
    }
}

/// Build roots from `(id, path, mode)` triples, deriving and de-duplicating
/// ids. Two roots with the same id would make the roster and the hello object
/// ambiguous, so a collision gets the index appended rather than being dropped.
fn roots_from(roots: impl Iterator<Item = (Option<String>, PathBuf, RootMode)>) -> Vec<NodeRoot> {
    let mut out: Vec<NodeRoot> = Vec::new();
    for (index, (id, path, mode)) in roots.enumerate() {
        let derived = id.unwrap_or_else(|| derived_root_id(&path, index));
        let id = if out.iter().any(|root| root.id == derived) {
            format!("{derived}-{index}")
        } else {
            derived
        };
        out.push(NodeRoot {
            id,
            path: path.display().to_string(),
            mode,
        });
    }
    out
}

/// A root id from its directory name (`/srv/work` → `work`), falling back to
/// `root<n>` for paths with no usable name (a bare `/`, a non-UTF-8 tail).
fn derived_root_id(path: &Path, index: usize) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("root{index}"))
}

/// Expand a leading `~` the same way the brain's config does: a config file
/// cannot expand it, and a shell only expands it unquoted.
pub fn expand_tilde(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str().map(str::to_string) else {
        return path;
    };
    let rest = match text.strip_prefix("~/") {
        Some(rest) => rest.to_string(),
        None if text == "~" => String::new(),
        None => return path,
    };
    match dirs::home_dir() {
        Some(home) => home.join(rest),
        None => path,
    }
}

/// Parse one `--ceiling` value: `surface=never|ask|always`, or a bare level
/// that becomes the default for every surface not named explicitly.
pub fn parse_ceiling(value: &str) -> Result<CeilingFlag, NodeError> {
    let bad = || {
        NodeError::Config(format!(
            "`{value}` is not a ceiling; expected `surface=never|ask|always` \
             (surfaces: shell, read_file, write_file, edit_file, computer) or a bare level"
        ))
    };
    match value.split_once('=') {
        Some((surface, level)) => {
            let surface = parse_surface(surface).ok_or_else(bad)?;
            let ceiling = parse_ceiling_level(level).ok_or_else(bad)?;
            Ok(CeilingFlag::Surface { surface, ceiling })
        }
        None => {
            let ceiling = parse_ceiling_level(value).ok_or_else(bad)?;
            Ok(CeilingFlag::Default(ceiling))
        }
    }
}

/// What one `--ceiling` flag asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CeilingFlag {
    /// A bare level: the default for every surface.
    Default(PolicyCeiling),
    /// `surface=level`.
    Surface {
        surface: Surface,
        ceiling: PolicyCeiling,
    },
}

pub fn parse_surface(value: &str) -> Option<Surface> {
    match value.trim() {
        "shell" => Some(Surface::Shell),
        "read_file" | "read-file" | "read" => Some(Surface::ReadFile),
        "write_file" | "write-file" | "write" => Some(Surface::WriteFile),
        "edit_file" | "edit-file" | "edit" => Some(Surface::EditFile),
        "computer" => Some(Surface::Computer),
        _ => None,
    }
}

/// The wire/log spelling of a ceiling. `PolicyCeiling` has no `Display` (it is
/// data, not a message), and a status line that printed `Ask` would not match
/// the `ask` an owner has to type.
pub fn ceiling_label(ceiling: PolicyCeiling) -> &'static str {
    match ceiling {
        PolicyCeiling::Never => "never",
        PolicyCeiling::Ask => "ask",
        PolicyCeiling::Always => "always",
    }
}

pub fn parse_ceiling_level(value: &str) -> Option<PolicyCeiling> {
    match value.trim().to_ascii_lowercase().as_str() {
        "never" => Some(PolicyCeiling::Never),
        "ask" => Some(PolicyCeiling::Ask),
        "always" => Some(PolicyCeiling::Always),
        _ => None,
    }
}

/// This machine's hostname, or `unknown` when the OS will not say. Not a
/// secret: it goes into the pairing request and into `node.hello`.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a 256-byte array on the stack and `gethostname` writes
    // at most `len` bytes into it; nothing else aliases the pointer.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc != 0 {
        return "unknown".to_string();
    }
    let len = buf.iter().position(|byte| *byte == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..len]).trim().to_string();
    if name.is_empty() {
        "unknown".to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, text: &str) {
        std::fs::create_dir_all(dir).expect("data dir");
        std::fs::write(dir.join(CONFIG_FILE), text).expect("write node.toml");
    }

    #[test]
    fn a_missing_config_file_is_the_default_node() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = NodeConfig::load(tmp.path()).expect("loads");
        assert_eq!(config.data_dir, tmp.path());
        // Two defaults since the spill dir became a root: the workspace the
        // node works in, and the spill directory it may only read. This used to
        // assert one; the count changed, the *behaviour* it protects (the first
        // root is a read-write workspace) did not.
        assert_eq!(
            config.roots.len(),
            2,
            "the default workspace and read-only spill roots: {:?}",
            config.roots
        );
        assert_eq!(config.roots[0].mode, RootMode::ReadWrite);
        assert!(
            config.roots[0].path.ends_with("workspace"),
            "{:?}",
            config.roots
        );
        assert!(config.shell_enabled);
        assert_eq!(config.heartbeat_secs, DEFAULT_HEARTBEAT_SECS);
        assert!(!config.label.is_empty());
    }

    #[test]
    fn the_default_roots_are_the_workspace_then_a_read_only_spill_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = NodeConfig::new_in(tmp.path());
        let paths = config.paths();

        assert_eq!(config.roots.len(), 2, "{:?}", config.roots);
        // Order matters: `workspace()` and the hello roster both read the first
        // read-write root, and the spill root must never win that race.
        assert_eq!(config.roots[0].id, "workspace");
        assert_eq!(config.roots[0].mode, RootMode::ReadWrite);
        assert_eq!(PathBuf::from(&config.roots[0].path), paths.workspace());
        assert_eq!(config.roots[1].id, "spill");
        assert_eq!(
            config.roots[1].mode,
            RootMode::ReadOnly,
            "the spill dir is a read-back surface, not a second workspace"
        );
        assert_eq!(PathBuf::from(&config.roots[1].path), paths.spill());
        assert_eq!(config.workspace(), paths.workspace());

        // `Default` builds the same pair from the platform data dir.
        let default = NodeConfig::default();
        assert_eq!(default.roots.len(), 2, "{:?}", default.roots);
        assert_eq!(default.roots[0].mode, RootMode::ReadWrite);
        assert_eq!(default.roots[1].mode, RootMode::ReadOnly);
        assert!(default.roots[1].path.ends_with("spill"));
    }

    #[test]
    fn a_spilled_file_can_be_read_back_but_the_spill_directory_is_not_writable() {
        // The bug this pins: the exit chunk names `<data>/spill/<inv>-stdout.log`,
        // and with no root covering that directory the bot was handed a path it
        // could not open. Reading it must work; writing there must not, or the
        // spill dir becomes an ungranted workspace.
        use swarmbot_contracts::ToolError;

        let tmp = tempfile::tempdir().expect("tempdir");
        let config = NodeConfig::load(tmp.path()).expect("loads");
        let policy = config.path_policy();
        let workspace = config.workspace();
        let spill = config.paths().spill();
        std::fs::create_dir_all(&spill).expect("spill dir");
        let spilled = spill.join("inv-1-stdout.log");
        std::fs::write(&spilled, b"the overflow").expect("write the spill file");

        let read = policy
            .resolve(&spilled, &workspace, false)
            .expect("a spilled result must be readable back through fs.read");
        assert!(
            read.starts_with(spill.canonicalize().expect("canon")),
            "{}",
            read.display()
        );

        let denied = policy
            .resolve(&spill.join("mine.txt"), &workspace, true)
            .expect_err("the spill root is read-only");
        match denied {
            ToolError::PathDenied { reason, .. } => assert_eq!(reason, "read-only root"),
            other => panic!("expected PathDenied, got {other:?}"),
        }
        // …and the read-only root does not weaken the workspace: a write there
        // still resolves.
        policy
            .resolve(&workspace.join("notes.txt"), &workspace, true)
            .expect("the workspace is still read-write");
    }

    #[test]
    fn defaults_ask_for_everything_but_reading() {
        let map = default_ceilings();
        assert_eq!(map.default, PolicyCeiling::Ask);
        assert_eq!(map.get(Surface::ReadFile), PolicyCeiling::Always);
        assert_eq!(map.get(Surface::Shell), PolicyCeiling::Ask);
        assert_eq!(map.get(Surface::WriteFile), PolicyCeiling::Ask);
        // Nothing defaults to a bypass.
        for surface in [
            Surface::Shell,
            Surface::WriteFile,
            Surface::EditFile,
            Surface::Computer,
        ] {
            assert_ne!(map.get(surface), PolicyCeiling::Always);
        }
    }

    #[test]
    fn the_file_sets_roots_ceilings_and_shell() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("work");
        let read_only = tmp.path().join("ref");
        std::fs::create_dir_all(&root).expect("work");
        std::fs::create_dir_all(&read_only).expect("ref");
        write(
            tmp.path(),
            &format!(
                r#"label = "bench"
brain_url = "wss://brain.example"
heartbeat_secs = 5
shell = false

[[roots]]
path = "{}"

[[roots]]
id = "reference"
path = "{}"
mode = "read_only"

[ceilings]
default = "always"
[ceilings.surfaces]
shell = "never"
"#,
                root.display(),
                read_only.display()
            ),
        );
        let config = NodeConfig::load(tmp.path()).expect("loads");
        assert_eq!(config.label, "bench");
        assert_eq!(config.brain_url.as_deref(), Some("wss://brain.example"));
        assert_eq!(config.heartbeat_secs, 5);
        assert!(!config.shell_enabled);
        assert_eq!(config.roots.len(), 2);
        assert_eq!(
            config.roots[0].id, "work",
            "derived from the directory name"
        );
        assert_eq!(config.roots[0].mode, RootMode::ReadWrite);
        assert_eq!(config.roots[1].id, "reference");
        assert_eq!(config.roots[1].mode, RootMode::ReadOnly);
        assert_eq!(config.ceilings.default, PolicyCeiling::Always);
        assert_eq!(config.ceilings.get(Surface::Shell), PolicyCeiling::Never);
        // Shell off withdraws the capability; fs stays.
        let caps = config.capabilities();
        assert!(caps.has("fs"));
        assert!(!caps.has("shell"), "--no-shell must withdraw shell@1");
    }

    #[test]
    fn a_malformed_config_is_an_error_not_a_silent_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(tmp.path(), "shell = maybe\n");
        let error = NodeConfig::load(tmp.path()).expect_err("must not load");
        assert!(matches!(error, NodeError::Config(_)), "{error:?}");
        assert!(error.to_string().contains(CONFIG_FILE), "{error}");
    }

    #[test]
    fn an_unknown_config_key_is_rejected() {
        // A typo in a safety-relevant file must not be ignored: `shel = false`
        // would otherwise leave shell enabled and look configured.
        let tmp = tempfile::tempdir().expect("tempdir");
        write(tmp.path(), "shel = false\n");
        assert!(matches!(
            NodeConfig::load(tmp.path()),
            Err(NodeError::Config(_))
        ));
    }

    #[test]
    fn config_round_trips_through_toml() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = NodeConfig::load(tmp.path()).expect("loads");
        config.label = "round trip".into();
        config.brain_url = Some("ws://127.0.0.1:1".into());
        config.shell_enabled = false;
        config.heartbeat_secs = 7;
        config.ceilings.set(Surface::Shell, PolicyCeiling::Never);
        config
            .roots
            .push(NodeRoot::read_only("ref", tmp.path().display().to_string()));
        config.save().expect("saves");
        let back = NodeConfig::load(tmp.path()).expect("loads");
        assert_eq!(back, config);
    }

    #[test]
    fn ceilings_parse_from_flags() {
        assert_eq!(
            parse_ceiling("shell=never").expect("parses"),
            CeilingFlag::Surface {
                surface: Surface::Shell,
                ceiling: PolicyCeiling::Never
            }
        );
        assert_eq!(
            parse_ceiling("read-file=always").expect("parses"),
            CeilingFlag::Surface {
                surface: Surface::ReadFile,
                ceiling: PolicyCeiling::Always
            }
        );
        assert_eq!(
            parse_ceiling("ask").expect("parses"),
            CeilingFlag::Default(PolicyCeiling::Ask)
        );
        assert!(parse_ceiling("shell=sometimes").is_err());
        assert!(parse_ceiling("nonsense=never").is_err());
        assert!(parse_ceiling("never=never").is_err());
        // The error names the accepted shapes.
        let error = parse_ceiling("nope").expect_err("refused");
        assert!(
            error.to_string().contains("surface=never|ask|always"),
            "{error}"
        );
    }

    #[test]
    fn duplicate_root_ids_are_kept_distinct() {
        let roots = roots_from(
            [
                (None, PathBuf::from("/srv/work"), RootMode::ReadWrite),
                (None, PathBuf::from("/mnt/work"), RootMode::ReadOnly),
                (
                    Some("work".to_string()),
                    PathBuf::from("/opt/w"),
                    RootMode::ReadWrite,
                ),
            ]
            .into_iter(),
        );
        let ids: Vec<&str> = roots.iter().map(|root| root.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), 3, "root ids must be unique: {ids:?}");
        assert_eq!(roots[2].id, "work-2");
    }

    #[test]
    fn a_root_with_no_usable_name_gets_an_index_id() {
        let roots = roots_from([(None, PathBuf::from("/"), RootMode::ReadOnly)].into_iter());
        assert_eq!(roots[0].id, "root0");
    }

    #[test]
    fn the_aggregate_ceiling_is_the_strictest_one() {
        let mut config = NodeConfig {
            ceilings: PolicyMap::uniform(PolicyCeiling::Always),
            ..NodeConfig::default()
        };
        assert_eq!(config.aggregate_ceiling(), PolicyCeiling::Always);
        config.ceilings.set(Surface::Shell, PolicyCeiling::Never);
        assert_eq!(
            config.aggregate_ceiling(),
            PolicyCeiling::Never,
            "one `never` makes the whole node report `never`"
        );
        config.ceilings.set(Surface::Shell, PolicyCeiling::Ask);
        assert_eq!(config.aggregate_ceiling(), PolicyCeiling::Ask);
    }

    #[test]
    fn the_engine_never_lifts_a_node_ceiling() {
        let config = NodeConfig {
            ceilings: PolicyMap::uniform(PolicyCeiling::Never),
            ..NodeConfig::default()
        };
        let engine = config.policy_engine();
        for surface in [
            Surface::Shell,
            Surface::ReadFile,
            Surface::WriteFile,
            Surface::EditFile,
        ] {
            assert_eq!(engine.ceiling(surface), PolicyCeiling::Never);
        }
    }

    #[test]
    fn the_workspace_prefers_a_read_write_root() {
        let mut config = NodeConfig {
            roots: vec![
                NodeRoot::read_only("ref", "/srv/ref"),
                NodeRoot::read_write("work", "/srv/work"),
            ],
            ..NodeConfig::default()
        };
        assert_eq!(config.workspace(), PathBuf::from("/srv/work"));
        config.roots = vec![NodeRoot::read_only("ref", "/srv/ref")];
        assert_eq!(config.workspace(), PathBuf::from("/srv/ref"));
        config.roots = Vec::new();
        assert_eq!(config.workspace(), config.paths().workspace());
    }

    #[test]
    fn the_deny_list_covers_the_nodes_own_identity() {
        // The invariant that keeps a bot from reading the key that signs this
        // node's upgrades: the default data dir is where the deny list points.
        let config = NodeConfig::default();
        let policy = config.path_policy();
        let identity = config.paths().identity();
        assert!(
            policy.is_denied(&identity),
            "{} must be deny-listed",
            identity.display()
        );
        // Roots still resolve inside themselves.
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = NodeConfig::load(tmp.path()).expect("loads");
        config.roots = vec![NodeRoot::read_write(
            "tmp",
            tmp.path().display().to_string(),
        )];
        let policy = config.path_policy();
        let resolved = policy
            .resolve(Path::new("a.txt"), tmp.path(), true)
            .expect("inside the root");
        assert!(resolved.starts_with(tmp.path().canonicalize().expect("canon")));
    }

    #[test]
    fn tilde_expansion_matches_the_brain() {
        let home = dirs::home_dir().expect("HOME is set in tests");
        assert_eq!(expand_tilde(PathBuf::from("~/x")), home.join("x"));
        assert_eq!(expand_tilde(PathBuf::from("~")), home);
        assert_eq!(expand_tilde(PathBuf::from("/x/~y")), PathBuf::from("/x/~y"));
        assert_eq!(expand_tilde(PathBuf::from("rel")), PathBuf::from("rel"));
    }

    #[test]
    fn hostname_is_never_empty() {
        assert!(!hostname().is_empty());
    }

    #[test]
    fn capabilities_never_claim_what_this_build_lacks() {
        let mut config = NodeConfig::default();
        assert_eq!(config.capabilities().as_slice(), &["fs@1", "shell@1"]);
        config.shell_enabled = false;
        assert_eq!(config.capabilities().as_slice(), &["fs@1"]);
        for absent in ["desktop", "browser", "pty", "screen", "computer", "tunnel"] {
            assert!(
                !config.capabilities().has(absent),
                "{absent} must never be declared"
            );
        }
    }
}
