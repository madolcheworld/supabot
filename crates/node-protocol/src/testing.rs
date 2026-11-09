//! In-memory sockets for tests — this crate's counterpart of the reference
//! package's `@openbot/node-protocol/testing`.
//!
//! Two adapters over the [`Socket`](crate::mux::Socket) trait:
//!
//! * [`LineSocket`]: newline-delimited JSON frames over any byte duplex
//!   (`tokio::io::duplex`, a TCP loopback pair, …). Rendered frames never
//!   contain raw newlines (`serde_json` escapes them), so line splitting is
//!   lossless — and [`duplex_pair`] wires two of them back to back for mux
//!   tests.
//! * [`ScriptSocket`]: a canned inbound script plus a recording of everything
//!   the peer sent, for wire-level assertions without a second peer.
//!
//! This module is a test-support module: it is always compiled (it is tiny)
//! and downstream crates' tests use it too.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use crate::mux::Socket;
use crate::wire::ProtocolError;

/// A [`Socket`] of newline-delimited frames over a byte stream.
pub struct LineSocket<T> {
    io: BufReader<T>,
}

impl<T: tokio::io::AsyncRead> LineSocket<T> {
    pub fn new(io: T) -> LineSocket<T> {
        LineSocket {
            io: BufReader::new(io),
        }
    }
}

impl<T> Socket for LineSocket<T>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    async fn send_text(&mut self, text: &str) -> Result<(), ProtocolError> {
        let io = self.io.get_mut();
        io.write_all(text.as_bytes()).await?;
        io.write_all(b"\n").await?;
        io.flush().await?;
        Ok(())
    }

    // Cancellation-safe: BufReader keeps partially read bytes buffered.
    async fn recv_text(&mut self) -> Result<Option<String>, ProtocolError> {
        let mut line = String::new();
        let n = self.io.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(
            line.trim_end_matches('\n')
                .trim_end_matches('\r')
                .to_string(),
        ))
    }
}

/// Two `LineSocket`s over `tokio::io::duplex` — an in-memory, frame-preserving
/// socket pair. No network, no ports; the pair dies with the test.
pub fn duplex_pair(
    capacity: usize,
) -> (
    LineSocket<tokio::io::DuplexStream>,
    LineSocket<tokio::io::DuplexStream>,
) {
    let (a, b) = tokio::io::duplex(capacity);
    (LineSocket::new(a), LineSocket::new(b))
}

/// A scripted socket: `recv_text` replays the inbound script, `send_text`
/// records into a log the test can inspect.
pub struct ScriptSocket {
    inbound: VecDeque<String>,
    outbound: Arc<Mutex<Vec<String>>>,
    /// When the script runs out: report end-of-stream, or pend forever
    /// (a silent peer the driver keeps serving).
    eof_when_empty: bool,
}

impl ScriptSocket {
    /// A socket whose inbound script is followed by silence (recv pends
    /// forever) — the peer stays alive and keeps answering.
    pub fn scripted(frames: impl IntoIterator<Item = impl Into<String>>) -> ScriptSocket {
        ScriptSocket {
            inbound: frames.into_iter().map(Into::into).collect(),
            outbound: Arc::new(Mutex::new(Vec::new())),
            eof_when_empty: false,
        }
    }

    /// A socket that reports EOF once the script is exhausted.
    pub fn ending(frames: impl IntoIterator<Item = impl Into<String>>) -> ScriptSocket {
        ScriptSocket {
            inbound: frames.into_iter().map(Into::into).collect(),
            outbound: Arc::new(Mutex::new(Vec::new())),
            eof_when_empty: true,
        }
    }

    /// A handle to the outbound log, for inspecting what the peer sent after
    /// the socket has been moved into it.
    pub fn sent_handle(&self) -> SentLog {
        SentLog(Arc::clone(&self.outbound))
    }
}

/// The recorded outbound frames of a [`ScriptSocket`].
#[derive(Clone, Debug, Default)]
pub struct SentLog(Arc<Mutex<Vec<String>>>);

impl SentLog {
    pub fn frames(&self) -> Vec<String> {
        // Test-support module: recover a poisoned log like everywhere else.
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Socket for ScriptSocket {
    async fn send_text(&mut self, text: &str) -> Result<(), ProtocolError> {
        self.outbound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(text.to_string());
        Ok(())
    }

    async fn recv_text(&mut self) -> Result<Option<String>, ProtocolError> {
        if let Some(frame) = self.inbound.pop_front() {
            return Ok(Some(frame));
        }
        if self.eof_when_empty {
            return Ok(None);
        }
        // A silent peer: pend until the test (or the liveness timer) ends it.
        std::future::pending::<()>().await;
        Ok(None)
    }
}
