//! TLS termination for the brain's **public** listener (D9.25).
//!
//! Until this module existed, `wss://` and `https://` were handled only on the
//! *client* side: `crates/node` rewrote a brain URL to `wss` and `tokio-tungstenite`
//! dialled it with rustls, but no brain ever listened on TLS. The README's
//! "unverified" list said exactly that, and it was true — a node could be
//! *configured* for `wss://` and there was nothing to dial.
//!
//! ## Why a custom [`Listener`], and not a second server
//!
//! axum's `axum::serve` is generic over a [`Listener`], and `TlsListener` here is
//! that trait over a `TcpListener` plus a `TlsAcceptor`. So TLS costs **no second
//! listener, no second router and no change to the route table** — the same
//! `app(state)` is served, and the node gateway, `/ws/live` and every `/api/*`
//! route keep the same middleware stack they had in plaintext. The alternative
//! (`axum-server`, or `hyper` by hand) is a second server type with its own
//! shutdown path, which is exactly the "two places to bind, two places to shut
//! down" the gateway's socket adapter already rejected for the same reason.
//!
//! ## The rules this keeps
//!
//! * **A failed handshake is one connection, not an outage.** The [`Listener`]
//!   contract says an accept error must be logged and retried; a client with a
//!   stale certificate must not take the listener down. TLS handshake failures
//!   are logged at `debug` (they are routine: scanners, health probes, a wrong
//!   `https://` on a plaintext port) and the loop continues.
//! * **The key never reaches a log line.** Files are read once at boot; only
//!   their *paths* are ever mentioned, and a parse failure names the file, never
//!   the bytes.
//! * **Half a configuration is refused at boot, not at the first connection.**
//!   `--tls-cert` without `--tls-key` is a start-up error naming both flags: the
//!   alternative is a brain that comes up, accepts `/api/health`, and fails every
//!   node dial with a handshake error the operator has to correlate.
//!
//! ## What is still not claimed
//!
//! Certificate *renewal* and hot reload are not implemented: the certificate is
//! read once, at boot, and rotating it means restarting the brain. A/C for the
//! plaintext listener is also unchanged — see `docs/node-protocol.md` §13 for the
//! paths that remain unverified (`wss://` against a real CA chain, and any
//! non-unix host).

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::Context as _;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tracing::{debug, info};

/// Where the certificate and its private key live.
///
/// Both are paths, never contents: a private key is a secret, and the operator's
/// config file is not the place to paste one (D2.3's rule for API keys applies
/// here for the same reason). PEM is the only format accepted — the one `openssl`,
/// Let's Encrypt and every ACME client emit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsPaths {
    /// PEM certificate chain, leaf first.
    pub cert: PathBuf,
    /// PEM private key (PKCS#8, PKCS#1 or SEC1 — rustls-pemfile accepts all three).
    pub key: PathBuf,
}

impl TlsPaths {
    /// Resolve "both flags or neither" into a typed decision.
    ///
    /// Returning `None` for `(None, None)` is the plaintext case; giving only one
    /// is an error that names **both** flags, because the operator who typed one
    /// is missing the other and a message naming only what they typed is useless.
    pub fn from_flags(
        cert: Option<PathBuf>,
        key: Option<PathBuf>,
    ) -> anyhow::Result<Option<TlsPaths>> {
        match (cert, key) {
            (None, None) => Ok(None),
            (Some(cert), Some(key)) => Ok(Some(TlsPaths { cert, key })),
            (Some(_), None) => anyhow::bail!(
                "`--tls-cert` was given without `--tls-key`: TLS needs both. Pass the private key \
                 with `--tls-key`, or drop `--tls-cert` to serve plaintext"
            ),
            (None, Some(_)) => anyhow::bail!(
                "`--tls-key` was given without `--tls-cert`: TLS needs both. Pass the certificate \
                 with `--tls-cert`, or drop `--tls-key` to serve plaintext"
            ),
        }
    }

    /// Read both files and build a server configuration.
    ///
    /// Every failure names the path and what was wrong with it — "invalid
    /// certificate" without the file is the message an operator cannot act on.
    pub fn load(&self) -> anyhow::Result<Arc<ServerConfig>> {
        let certs = load_certs(&self.cert)?;
        let key = load_key(&self.key)?;
        install_provider()?;
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .with_context(|| {
                format!(
                    "the certificate `{}` and key `{}` were rejected together",
                    self.cert.display(),
                    self.key.display()
                )
            })?;
        Ok(Arc::new(config))
    }
}

/// Make sure rustls has a process-wide crypto provider, and fail with a *typed*
/// error if it does not.
///
/// This is not optional bookkeeping. `rustls` can pick a provider automatically
/// only when exactly one of its `aws-lc-rs`/`ring` features is on, and in this
/// workspace **both are**: `reqwest`'s and `tokio-tungstenite`'s dependency
/// graphs each turn one of them on, and Cargo features are additive. So the
/// automatic path is unavailable here, and `ServerConfig::builder()` *panics*
/// with "Could not automatically determine the process-level CryptoProvider"
/// rather than returning an error — a panic at boot from a TLS *configuration*
/// path, which is exactly the shape AGENTS.md says a library must not have.
///
/// `install_default` returns `Err` when a provider is already installed, and that
/// is the normal case on the second and later calls (the first `load` wins), so
/// the error is ignored on purpose: "somebody already chose" is the outcome we
/// want. The provider named here is `aws_lc_rs` because that is the one rustls
/// exposes in this build — its `ring` module is compiled out (only one provider's
/// `rustls` feature is enabled, and this workspace's lock resolved to
/// `aws-lc-rs`), so naming `ring` would fail to compile.
fn install_provider() -> anyhow::Result<()> {
    // The implementation lives in `node-protocol`, because the **node** needs the
    // same thing for its `--brain-ca` connector and two copies would drift — and
    // they did: the brain's copy was written first, and the node's absence of one
    // surfaced only under the full gate, because a standalone test run had already
    // installed a provider earlier in the same process (D9.25).
    swarmbot_node_protocol::ensure_crypto_provider().map_err(|reason| anyhow::anyhow!("{reason}"))
}

fn load_certs(
    path: &Path,
) -> anyhow::Result<Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot open the TLS certificate `{}`", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("`{}` is not a PEM certificate chain", path.display()))?;
    if certs.is_empty() {
        anyhow::bail!(
            "`{}` holds no PEM certificate: expected at least one `-----BEGIN CERTIFICATE-----` \
             block, leaf first",
            path.display()
        );
    }
    Ok(certs)
}

fn load_key(
    path: &Path,
) -> anyhow::Result<tokio_rustls::rustls::pki_types::PrivateKeyDer<'static>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot open the TLS private key `{}`", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    // `rustls-pemfile` accepts PKCS#8, PKCS#1 and SEC1, which is what makes
    // "whatever openssl produced" work without asking the operator to convert.
    let key = rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("`{}` is not a PEM private key", path.display()))?;
    key.ok_or_else(|| {
        anyhow::anyhow!(
            "`{}` holds no PEM private key: expected a `-----BEGIN PRIVATE KEY-----` (or RSA/EC) \
             block",
            path.display()
        )
    })
}

/// A [`Listener`](axum::serve::Listener) that terminates TLS.
///
/// One `TcpListener`, one `TlsAcceptor`, and no other state: the accepted stream
/// is a `TlsStream<TcpStream>`, which is `AsyncRead + AsyncWrite + Unpin`, so
/// axum's `serve` needs nothing else.
pub struct TlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
}

impl TlsListener {
    pub fn new(listener: TcpListener, config: Arc<ServerConfig>) -> Self {
        Self {
            listener,
            acceptor: TlsAcceptor::from(config),
        }
    }

    /// The bound address, for the startup log and for a test that let the OS pick
    /// the port.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

/// The accepted connection type. A newtype so the `Listener` impl's associated
/// type is nameable, and so a future change (a different transport, a mux) has a
/// place to live without changing the trait impl's shape.
pub struct TlsStream(tokio_rustls::server::TlsStream<tokio::net::TcpStream>);

impl AsyncRead for TlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream;
    type Addr = SocketAddr;

    /// Accept, then handshake. **A failed handshake is retried, not returned**:
    /// the trait's contract says an accept error must be logged and retried, and
    /// a `TlsAcceptor::accept` error is the TLS equivalent — a probing client, a
    /// certificate mismatch on the far side, a port scanned by something that
    /// speaks HTTP over a TLS port. Letting any of those end the listener would
    /// turn a stray connection into an outage.
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, addr)) => match self.acceptor.accept(stream).await {
                    Ok(tls) => {
                        debug!(peer = %addr, "a TLS connection was accepted");
                        return (TlsStream(tls), addr);
                    }
                    // Deliberately `debug`, not `warn`: these are routine on a
                    // public listener, and a warning per probe is how a real
                    // warning gets ignored. The address is enough to correlate.
                    Err(error) => {
                        debug!(peer = %addr, error = %error, "a TLS handshake failed");
                    }
                },
                Err(error) => {
                    // A persistent `accept` error (fd exhaustion) is retried with
                    // a small pause rather than a spin: the trait has no way to
                    // signal "give up", and returning would end the listener.
                    tracing::warn!(error = %error, "accepting a connection failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        TlsListener::local_addr(self)
    }
}

/// The line the brain logs once TLS is on, so an operator can see *that* the
/// listener is encrypted without reading the config back.
pub fn log_enabled(paths: &TlsPaths, addr: SocketAddr) {
    info!(
        listener = %addr,
        cert = %paths.cert.display(),
        "the public listener is up over TLS: https:// and wss://"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temp dir")
    }

    /// A self-signed certificate and key, made by the `openssl` binary.
    ///
    /// A test certificate has to be *real* — a hand-written PEM would prove the
    /// parser and nothing about the handshake. `openssl` is on every machine this
    /// project is developed on, and if it is missing the test says so rather than
    /// quietly passing.
    fn self_signed(dir: &Path) -> TlsPaths {
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let output = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-keyout",
                key.to_str().expect("utf-8 path"),
                "-out",
                cert.to_str().expect("utf-8 path"),
                "-days",
                "1",
                "-nodes",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost,IP:127.0.0.1",
            ])
            .output()
            .expect("openssl runs");
        assert!(
            output.status.success(),
            "openssl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        TlsPaths { cert, key }
    }

    #[test]
    fn both_flags_or_neither_is_the_only_way_in() {
        let dir = temp();
        let paths = self_signed(dir.path());
        assert_eq!(
            TlsPaths::from_flags(None, None).expect("plaintext is fine"),
            None,
            "no flags is plainly no TLS"
        );
        let both = TlsPaths::from_flags(Some(paths.cert.clone()), Some(paths.key.clone()))
            .expect("both flags");
        assert_eq!(both.as_ref(), Some(&paths));

        // Half a configuration names *both* flags: the operator typed one and is
        // missing the other, so a message about only what they typed is useless.
        let only_cert = TlsPaths::from_flags(Some(paths.cert.clone()), None)
            .expect_err("cert without key is refused");
        let text = only_cert.to_string();
        assert!(text.contains("--tls-cert"), "{text}");
        assert!(text.contains("--tls-key"), "{text}");
        assert!(text.contains("both"), "{text}");

        let only_key = TlsPaths::from_flags(None, Some(paths.key.clone()))
            .expect_err("key without cert is refused");
        let text = only_key.to_string();
        assert!(text.contains("--tls-cert"), "{text}");
        assert!(text.contains("--tls-key"), "{text}");
    }

    #[test]
    fn a_real_certificate_and_key_load_into_a_server_config() {
        let dir = temp();
        let paths = self_signed(dir.path());
        let config = paths.load().expect("a self-signed pair loads");
        // The only observable fact about a `ServerConfig` from outside is that it
        // exists; the handshake itself is proven in the integration test, where a
        // client that trusts this certificate completes one.
        assert!(Arc::strong_count(&config) >= 1);
    }

    #[test]
    fn a_missing_certificate_names_the_file_and_the_flag() {
        let dir = temp();
        let paths = TlsPaths {
            cert: dir.path().join("nope.pem"),
            key: dir.path().join("nope.key"),
        };
        let error = paths.load().expect_err("a missing file is refused");
        let text = format!("{error:#}");
        assert!(
            text.contains("nope.pem"),
            "the message names the file: {text}"
        );
        assert!(text.contains("TLS certificate"), "and what it was: {text}");
    }

    #[test]
    fn a_file_that_is_not_a_certificate_says_so_and_never_echoes_it() {
        let dir = temp();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        // A file with the *shape* of content but no PEM block.
        std::fs::File::create(&cert)
            .and_then(|mut file| file.write_all(b"this is not a certificate"))
            .expect("write the bad certificate");
        std::fs::File::create(&key)
            .and_then(|mut file| file.write_all(b"nor is this a key"))
            .expect("write the bad key");
        let paths = TlsPaths { cert, key };
        let error = paths.load().expect_err("garbage is refused");
        let text = format!("{error:#}");
        assert!(
            text.contains("no PEM certificate"),
            "the message says what was expected: {text}"
        );
        // The refusal names the file, never its contents: a key file's bytes must
        // not reach a log line or an error message (D2.3's rule, applied here).
        assert!(
            !text.contains("this is not a certificate"),
            "the content is not echoed: {text}"
        );
    }

    #[test]
    fn a_key_file_that_holds_nothing_is_a_typed_refusal() {
        let dir = temp();
        let paths = self_signed(dir.path());
        // Overwrite the key with an empty file: the certificate still parses, so
        // this isolates the key half.
        std::fs::write(&paths.key, b"").expect("truncate the key");
        let error = paths.load().expect_err("an empty key is refused");
        let text = format!("{error:#}");
        assert!(
            text.contains("no PEM private key"),
            "the message says what was expected: {text}"
        );
        assert!(text.contains("key.pem"), "{text}");
    }

    #[test]
    fn the_listener_reports_the_address_it_was_bound_to() {
        // The OS chose the port, so the address has to be read back — that is what
        // the startup log prints and what a test dials.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let dir = temp();
        let paths = self_signed(dir.path());
        let config = paths.load().expect("a config");
        runtime.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let bound = listener.local_addr().expect("addr");
            let tls = TlsListener::new(listener, config);
            assert_eq!(
                tls.local_addr().expect("addr"),
                bound,
                "the TLS listener answers with the socket's own address"
            );
        });
    }
}
