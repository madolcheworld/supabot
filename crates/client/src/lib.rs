//! `swarmbot-client`: the **blocking** client every SwarmBot frontend shares.
//!
//! Blocking on purpose: the GPUI desktop app must not drag a tokio runtime
//! into its process (it runs these calls on a background thread), and the CLI
//! is simpler without one. HTTP goes through `ureq`, the live event socket
//! through `tungstenite` — no async runtime anywhere in this crate.
//!
//! The wire types *are* [`swarmbot_contracts`] types ([`swarmbot_contracts::Bot`],
//! [`swarmbot_contracts::Entry`], …); this crate adds no parallel DTOs, only
//! the small envelopes composite endpoints return.
//!
//! ```no_run
//! use swarmbot_client::{BrainClient, SessionAction};
//!
//! # fn run() -> Result<(), swarmbot_client::ClientError> {
//! let client = BrainClient::new("http://127.0.0.1:8787")?.with_token("sekrit");
//! let health = client.health()?;
//! assert!(health.ok);
//!
//! // Live updates: reconnecting (with backoff) is the caller's job; resume
//! // with the newest cursor so nothing is silently missed.
//! let mut live = client.live(None, None)?;
//! while let Some(event) = live.next_event()? {
//!     println!("{event:?} (cursor {:?})", live.cursor());
//! }
//! # Ok(()) }
//! ```
//!
//! Modules:
//!
//! * [`client`] — [`BrainClient`], one method per brain endpoint, plus the
//!   composite response types;
//! * [`error`] — [`ClientError`] and its mapping into the contracts'
//!   `DomainError`;
//! * [`live`] — [`LiveSubscription`], the `/ws/live` event socket;
//! * `http` (crate-internal) — the thin ureq wrapper;
//! * `testing` (only with `cfg(test)` or the `test-support` feature) — a
//!   stub brain on loopback for tests.

pub mod client;
pub mod error;
mod http;
pub mod live;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;

pub use client::{
    BackendModels, BackendProbe, BackendView, BrainClient, Health, ModelOption, NewBotRequest,
    NewRoutineRequest, PairingCode, SentMessage, SessionAction, TaskDetail,
};
pub use error::ClientError;
pub use live::LiveSubscription;
