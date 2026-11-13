//! `swarmbot-node` — **the hands**. A daemon that pairs with a brain, dials out
//! to it, and executes `fs.*` / `shell.exec` calls under *its own* policy.
//!
//! The brain decides first; the node decides finally (decision D1.4). Nothing
//! in this crate can be talked into widening that: the [`PathPolicy`] comes
//! from the node's own roots, the ceilings from its own
//! [`PolicyMap`](swarmbot_contracts::PolicyMap), and the effective ceiling is
//! `min(brain, node)` — an argument order a malicious brain cannot exploit,
//! because the brain's half is never an input here.
//!
//! # Module map
//!
//! * [`cli`] — the command line: `pair`, `run`, `status`.
//! * [`config`] — [`config::NodeConfig`] and [`config::NodePaths`]: the data
//!   dir, roots, ceilings, heartbeat, shell on/off; `<data>/node.toml` with the
//!   CLI winning.
//! * [`identity`] — [`identity::Identity`]: the loaded keypair plus the brain
//!   key **pinned** at pairing, with a hand-written `Debug` that cannot leak
//!   key material; and [`identity::pair`], the `POST /node/pair` round-trip.
//! * [`handlers`] — [`handlers::NodeRuntime`] (policy + executor + audit) and
//!   [`handlers::NodeHandler`], the [`RpcHandler`](swarmbot_node_protocol::RpcHandler)
//!   that gates on `node.hello`, routes methods, maps payloads to
//!   [`NodeCall`](swarmbot_tools::NodeCall), translates errors and asks the
//!   brain for approvals.
//! * [`shell`] — the streaming `shell.exec` runner: real chunks, real pids,
//!   `killpg` on interrupt/timeout/disconnect.
//! * [`dial`] — the control socket: signed upgrade, exponential-backoff redial,
//!   the 4401 stop, heartbeats.
//! * [`stream`] — the data-stream dialer: `fs.download`'s bytes, sent with
//!   backpressure and terminated by `{sha256, size}`.
//! * [`state`] — the small live-state file `run` maintains and `status` reads.
//!
//! # What is *not* here
//!
//! No pty, no tunnels, no browser, no desktop, no upload. Every method the
//! reference protocol names but this build does not implement answers with
//! [`swarmbot_node_protocol::rpc::unimplemented`] — a typed
//! `PolicyRefused { retry: false }`, never a silent success and never a fake.
//!
//! **`fs.download` is implemented** and it is the one method whose bytes do not
//! ride the control socket: [`stream`] dials the brain's `/node/stream/:id` with
//! the one-time token the RPC carried, sends the file in bounded chunks, and
//! answers only after the brain has acknowledged the last one. A node behind NAT
//! still needs no inbound port: this crate only ever dials out.
//!
//! # Honesty note on `shell.exec`
//!
//! [`swarmbot_tools::LocalExecutor`] answers a `ShellExec` with one
//! [`ToolResult`](swarmbot_contracts::ToolResult): it buffers, then reports
//! `exit=<code>` plus stdout/stderr sections. A socket needs *chunks* with a
//! real pid, so the call takes the streaming shape instead — and that shape is
//! **the same implementation**, called rather than copied:
//! [`swarmbot_tools::handlers::shell::exec_shell_streaming`]. [`shell`] here is
//! only the adapter, and what this crate contributes is its own half of the
//! decision: the node's [`PolicyEngine`](swarmbot_tools::PolicyEngine), its
//! `PathPolicy`, its spill directory and audit line, plus the mapping from
//! [`swarmbot_tools::ShellEvent`]s onto the wire's `ShellChunk` JSON.
//!
//! The deliberate difference between the two shapes is the **run loop**, not the
//! spawn: buffered waits for one exit and formats a `ToolResult`, streaming feeds
//! an emitter and can be cut short by its transport. Both loops live side by side
//! in `crates/tools/src/handlers/shell.rs`, and both spawn through the same
//! `build_shell_command` — so the process-group, scrubbed-environment and
//! `killpg` invariants are one body of code, and a change to one shape cannot
//! silently drift from the other across a crate boundary.

pub mod cli;
pub mod config;
pub mod dial;
pub mod handlers;
pub mod identity;
pub mod shell;
pub mod state;
pub mod stream;

use std::path::PathBuf;

use thiserror::Error;

pub use config::{NodeConfig, NodePaths};
pub use dial::{
    Backoff, DialPlan, Next, NodeSocket, ServeOptions, SessionEnd, StopReason, decide, serve,
    session,
};
pub use handlers::{BrainLink, NodeHandler, NodeRuntime, PeerLink};
pub use identity::{Identity, PairOutcome};
pub use shell::{ProcGuard, ProcRegistry, ShellFailure, ShellOptions, ShellRun, run_streaming};
pub use state::{NodeState, RunStatus, StateWriter};

/// Re-exported so an embedder (or the brain's integration tests) builds peers
/// and payloads against exactly the protocol versions this daemon was written
/// with. The short aliases keep call sites readable.
pub use swarmbot_contracts;
pub use swarmbot_contracts as contracts;
pub use swarmbot_node_protocol;
pub use swarmbot_node_protocol as node_protocol;
pub use swarmbot_tools;
pub use swarmbot_tools as tools;

/// The crate's version, which is also what `node.hello` and the pairing request
/// report. One definition so the two cannot drift.
pub const NODE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Every way this daemon can fail, as a value.
///
/// None of the variants can carry key material: the private key only ever
/// exists inside [`identity::Identity`]'s private field (redacted `Debug`) and
/// inside `<data>/identity.json` at mode 0600. A pairing failure carries the
/// *brain's* reason verbatim, because that is the only text that explains a
/// rejected code — and a code is not a credential once it has been used.
#[derive(Debug, Error)]
pub enum NodeError {
    #[error("configuration: {0}")]
    Config(String),

    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("identity: {0}")]
    Identity(String),

    #[error("not paired: {} does not exist — run `swarmbot-node pair <brain-url> <code>` first", path.display())]
    NotPaired { path: PathBuf },

    /// The brain answered `POST /node/pair` with a refusal. `reason` is the
    /// brain's own message, printed verbatim so an expired, used or mistyped
    /// code says *which* it was.
    #[error("the brain refused to pair (HTTP {status}): {reason}")]
    PairingRejected { status: u16, reason: String },

    #[error("cannot reach the brain at {url}: {reason}")]
    Unreachable { url: String, reason: String },

    #[error("authentication: {0}")]
    Auth(String),

    #[error("protocol: {0}")]
    Protocol(String),

    #[error("json: {0}")]
    Json(String),
}

impl NodeError {
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn identity(message: impl Into<String>) -> Self {
        Self::Identity(message.into())
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::Auth(message.into())
    }
}

impl From<swarmbot_node_protocol::IdentityError> for NodeError {
    fn from(error: swarmbot_node_protocol::IdentityError) -> Self {
        // `IdentityError`'s own Display never contains key bytes (it names
        // lengths and encodings), so forwarding the string is safe.
        Self::Identity(error.to_string())
    }
}

impl From<swarmbot_node_protocol::ProtocolError> for NodeError {
    fn from(error: swarmbot_node_protocol::ProtocolError) -> Self {
        Self::Protocol(error.to_string())
    }
}

impl From<serde_json::Error> for NodeError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

impl From<swarmbot_node_protocol::AuthError> for NodeError {
    fn from(error: swarmbot_node_protocol::AuthError) -> Self {
        Self::Auth(error.to_string())
    }
}

/// Initialise `tracing` for the daemon: `RUST_LOG`/`SWARMBOT_NODE_LOG` decide
/// the level, and the default keeps connection and policy decisions visible
/// without printing payload bodies.
///
/// There is no secret scrubber here because there are no secrets to scrub —
/// this crate holds one Ed25519 key, it lives in a private field, and the
/// redaction test in [`identity`] proves it never reaches a `Debug` impl.
pub fn init_tracing(verbose: bool) {
    use tracing_subscriber::EnvFilter;
    let default_level = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_env("SWARMBOT_NODE_LOG")
        .or_else(|_| EnvFilter::try_from_env("RUST_LOG"))
        .unwrap_or_else(|_| EnvFilter::new(default_level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_render_as_readable_sentences() {
        let error = NodeError::PairingRejected {
            status: 410,
            reason: "pairing code expired".into(),
        };
        assert_eq!(
            error.to_string(),
            "the brain refused to pair (HTTP 410): pairing code expired"
        );
        let error = NodeError::NotPaired {
            path: PathBuf::from("/data/identity.json"),
        };
        assert!(error.to_string().contains("swarmbot-node pair"), "{error}");
    }

    #[test]
    fn no_error_variant_prints_key_material() {
        // The signing key is 32 bytes; its base64url and hex spellings are the
        // two shapes a leak could take. Neither may appear in any error.
        let key = swarmbot_node_protocol::ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let secret_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            key.to_bytes(),
        );
        let secret_hex = hex::encode(key.to_bytes());
        let errors = [
            NodeError::config("x"),
            NodeError::identity("x"),
            NodeError::auth("x"),
            NodeError::NotPaired {
                path: PathBuf::from("/x"),
            },
            NodeError::PairingRejected {
                status: 400,
                reason: "x".into(),
            },
            NodeError::Unreachable {
                url: "ws://x".into(),
                reason: "x".into(),
            },
            NodeError::Protocol("x".into()),
            NodeError::Json("x".into()),
            NodeError::from(swarmbot_node_protocol::IdentityError::InvalidOrigin),
            NodeError::from(swarmbot_node_protocol::IdentityError::KeyLength { len: 32 }),
        ];
        for error in errors {
            for rendered in [error.to_string(), format!("{error:?}")] {
                assert!(!rendered.contains(&secret_b64), "{rendered}");
                assert!(!rendered.contains(&secret_hex), "{rendered}");
            }
        }
    }
}
