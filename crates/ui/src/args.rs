//! Command-line arguments for `swarmbot-ui`.
//!
//! Kept in the library (not `main.rs`) so `--selftest` and the integration
//! tests can build the same [`Args`] the binary parses.

use clap::Parser;

use crate::proto::SecretToken;

/// SwarmBot desktop UI (GPUI): watch bots cooperate and delegate.
#[derive(Parser, Debug, Clone)]
#[command(name = "swarmbot-ui", version, about)]
pub struct Args {
    /// The brain to talk to (REST + `/ws/live`).
    #[arg(long, default_value = "http://127.0.0.1:8787", env = "SWARMBOT_BRAIN")]
    pub brain: String,

    /// A one-time login token — the highest-precedence credential source.
    ///
    /// `SWARMBOT_TOKEN` is deliberately *not* folded into this field by clap:
    /// the resolver in `lib.rs` reads it as its own source, just below the
    /// flag and above `<data-dir>/owner-token`, so the precedence is testable
    /// and the log can name the source that actually won. When no source
    /// yields a token, the app shows a login prompt in the window instead of
    /// failing. The value is never logged — [`SecretToken`]'s hand-written
    /// `Debug` redacts it.
    #[arg(long)]
    pub token: Option<SecretToken>,

    /// Start `swarmbot-brain` as a child, wait for `/api/health`, and kill it on
    /// exit. Uses `std::process` — no tokio in this process (D7.2).
    #[arg(long)]
    pub spawn_brain: bool,

    /// Spawn the brain with the scripted `fake` backend and the demo team, so
    /// the app is usable with no model credentials and no network. Implies
    /// `--spawn-brain`.
    #[arg(long)]
    pub offline: bool,

    /// Data directory for a spawned brain (defaults to a fresh temp dir that is
    /// removed on exit).
    #[arg(long)]
    pub data_dir: Option<String>,

    /// Port for a spawned brain to listen on (defaults to the `--brain` port).
    #[arg(long)]
    pub brain_addr: Option<String>,

    /// Open a window, drive a few fabricated live events through the reducer,
    /// render, write a report and exit 0. The machine-checkable proof that a
    /// window opens and paints on this machine.
    #[arg(long)]
    pub selftest: bool,

    /// Where to write the selftest report (stdout only when omitted).
    #[arg(long)]
    pub report: Option<String>,
}

impl Args {
    /// `--offline` implies `--spawn-brain`.
    pub fn should_spawn(&self) -> bool {
        self.spawn_brain || self.offline
    }
}
