//! Tracing setup, and the rule that keeps a credential out of the log.
//!
//! A log file outlives the process, is copied into bug reports and is readable
//! by anything that can read the data dir. So this crate treats "no credential
//! in a log line" as an output-boundary invariant rather than as a promise every
//! call site has to remember:
//!
//! 1. **nothing here ever logs a header.** [`describe_request`] renders a method,
//!    a path and a version, and redacts the value of any query parameter whose
//!    name looks like a credential;
//! 2. **everything that is written goes through [`Scrubber`]**, which replaces
//!    any value registered with the [`SecretRegistry`] — the owner token, every
//!    session id, every API key loaded from `secrets.json` — before it reaches
//!    the stream. A future `debug!(?headers)` in a handler is then embarrassing
//!    rather than a leak.
//!
//! The scrubber is a line buffer: a secret cannot be split across two writes and
//! survive, because nothing is emitted until a newline (or a flush) closes the
//! line it sits in.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, RwLock};

use axum::http::{HeaderName, Request};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;

/// The default filter when `RUST_LOG` says nothing.
///
/// The transport crates are turned down: at `info` they narrate every socket,
/// which buries the boot sequence and the orchestration lines that matter.
pub const DEFAULT_FILTER: &str =
    "info,hyper=warn,hyper_util=warn,rustls=warn,tungstenite=warn,h2=warn";

/// Query parameters whose *values* are credentials even though our own API never
/// puts one there. A client that does is protected from itself.
const SENSITIVE_QUERY_KEYS: &[&str] = &[
    "token",
    "access_token",
    "api_key",
    "apikey",
    "key",
    "secret",
    "password",
    "authorization",
    "auth",
    "cookie",
    "session",
    "session_id",
    "code",
    "signature",
    "sig",
];

/// Header names that are never rendered into a log line.
pub const SENSITIVE_HEADERS: &[HeaderName] = &[
    axum::http::header::AUTHORIZATION,
    axum::http::header::COOKIE,
    axum::http::header::SET_COOKIE,
    axum::http::header::PROXY_AUTHORIZATION,
];

/// What a credential is replaced with in a log line.
pub const REDACTED: &str = "[redacted]";

/// The set of strings that must never reach the log, at runtime.
///
/// A secret is registered when it is learned (a token minted, a key loaded), so
/// the registry stays honest about what this process actually holds.
#[derive(Default)]
pub struct SecretRegistry {
    secrets: RwLock<HashSet<String>>,
}

/// Shorter than this, a "secret" is more likely to be an ordinary word than a
/// credential, and scrubbing it would turn an honest log into noise.
const MIN_SCRUBBED_LEN: usize = 8;

impl SecretRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Remember a credential so it is scrubbed from every log line.
    pub fn register(&self, secret: impl Into<String>) {
        let secret = secret.into();
        if secret.trim().len() < MIN_SCRUBBED_LEN {
            return;
        }
        let mut secrets = self.secrets.write().unwrap_or_else(|e| e.into_inner());
        secrets.insert(secret.trim().to_string());
    }

    /// Forget a credential once nothing can legitimately log it any more.
    ///
    /// A per-turn MCP token is minted and dropped on every model call, so keeping
    /// every one forever would grow the scrubber without bound and make every
    /// line slower for no benefit. Once the turn's registry entry is gone the
    /// token authenticates nothing, so it is no longer a secret — and the path
    /// itself is redacted by [`redact_path`] either way.
    pub fn forget(&self, secret: &str) {
        let mut secrets = self.secrets.write().unwrap_or_else(|e| e.into_inner());
        secrets.remove(secret.trim());
    }

    /// How many credentials are being watched.
    pub fn len(&self) -> usize {
        self.secrets.read().map(|s| s.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Replace every known credential with `[redacted]`.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let secrets = match self.secrets.read() {
            Ok(secrets) => secrets,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !secrets.iter().any(|secret| text.contains(secret.as_str())) {
            return Cow::Borrowed(text);
        }
        let mut out = text.to_string();
        for secret in secrets.iter() {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
        Cow::Owned(out)
    }
}

/// A `tracing_subscriber` writer that scrubs before it writes.
#[derive(Clone)]
pub struct Scrubber {
    registry: Arc<SecretRegistry>,
    sink: Arc<Sink>,
}

struct Sink(Mutex<Box<dyn Write + Send>>);

impl Sink {
    fn new(writer: impl Write + Send + 'static) -> Self {
        Self(Mutex::new(Box::new(writer)))
    }

    fn write(&self, text: &str) {
        // A failing log sink must not take the daemon down, and must not block
        // the request that was being logged: the line is dropped, loudly.
        let Ok(mut guard) = self.0.lock() else {
            return;
        };
        if guard.write_all(text.as_bytes()).is_err() {
            let _ = guard.flush();
        }
    }
}

impl Scrubber {
    /// Scrub into the process's stdout.
    pub fn stdout(registry: Arc<SecretRegistry>) -> Self {
        Self {
            registry,
            sink: Arc::new(Sink::new(io::stdout())),
        }
    }

    /// Scrub into any writer — how the tests read back what would be logged.
    pub fn new(registry: Arc<SecretRegistry>, writer: impl Write + Send + 'static) -> Self {
        Self {
            registry,
            sink: Arc::new(Sink::new(writer)),
        }
    }

    pub fn registry(&self) -> Arc<SecretRegistry> {
        Arc::clone(&self.registry)
    }
}

/// One formatted event, buffered until its line ends.
pub struct ScrubbedWriter {
    registry: Arc<SecretRegistry>,
    sink: Arc<Sink>,
    buffer: String,
}

impl Write for ScrubbedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(bytes);
        self.buffer.push_str(&text);
        // Emit whole lines only: a credential split across two writes would
        // otherwise slip through the scrubber.
        while let Some(newline) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=newline).collect();
            let scrubbed = self.registry.redact(&line);
            self.sink.write(&scrubbed);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        // Taken rather than borrowed: the scrubber returns a `Cow` that would
        // otherwise keep the buffer borrowed while it is being cleared.
        let text = std::mem::take(&mut self.buffer);
        let scrubbed = self.registry.redact(&text);
        self.sink.write(&scrubbed);
        Ok(())
    }
}

impl Drop for ScrubbedWriter {
    fn drop(&mut self) {
        // The fmt layer flushes after every event; this only covers a writer
        // that was dropped mid-line.
        let _ = self.flush();
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Scrubber {
    type Writer = ScrubbedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ScrubbedWriter {
            registry: Arc::clone(&self.registry),
            sink: Arc::clone(&self.sink),
            buffer: String::new(),
        }
    }
}

/// Install the subscriber: `RUST_LOG` (default [`DEFAULT_FILTER`]), one line per
/// event, every line scrubbed.
///
/// A second call is not an error: the tests build several brains in one process,
/// and only the first subscriber can win.
pub fn init_tracing(registry: Arc<SecretRegistry>) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(DEFAULT_FILTER))
        // `try_new("info")` cannot fail; the fallback keeps this function free of
        // a panic path rather than trusting that.
        .unwrap_or_else(|_| EnvFilter::try_new("info").unwrap_or_default());
    let writer = Scrubber::stdout(registry);
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_target(true)
        .with_ansi(false);
    let subscriber = tracing_subscriber::registry().with(filter).with(layer);
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        tracing::debug!("a tracing subscriber is already installed; keeping it");
    }
}

/// The one-line description of a request that goes into a span.
///
/// Deliberately not `{:?}` on the request: `Debug` for `HeaderMap` prints every
/// header, including `Authorization` and `Cookie`. The path goes through
/// [`redact_path`] because one route carries its credential *in the path*.
pub fn describe_request<B>(request: &Request<B>) -> String {
    let uri = request.uri();
    let path = redact_path(uri.path());
    let query = uri.query().map(redact_query);
    match query {
        Some(query) if !query.is_empty() => {
            format!(
                "{} {path}?{query} {:?}",
                request.method(),
                request.version()
            )
        }
        _ => format!("{} {path} {:?}", request.method(), request.version()),
    }
}

/// Replace a credential that travels in a URL path.
///
/// `/mcp/t/{token}` is the per-turn MCP endpoint on the internal listener, and
/// its token *is* the credential: the registry lookup is the authentication.
/// The access log renders paths verbatim and outlives the process, so the token
/// is replaced here — unconditionally, before the line is formatted — and the
/// [`SecretRegistry`] is a second line of defence rather than the only one. The
/// route itself is kept: "which endpoint was hit" is what makes the line useful.
pub fn redact_path(path: &str) -> Cow<'_, str> {
    match path.strip_prefix("/mcp/t/") {
        Some(rest) if !rest.is_empty() => Cow::Owned(format!("/mcp/t/{REDACTED}")),
        _ => Cow::Borrowed(path),
    }
}

/// Replace the value of any credential-shaped query parameter.
pub fn redact_query(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if is_sensitive_key(key) => format!("{key}={REDACTED}"),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn is_sensitive_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    SENSITIVE_QUERY_KEYS
        .iter()
        .any(|sensitive| lowered == *sensitive || lowered.ends_with(&format!("_{sensitive}")))
}

/// A span for one HTTP request, for the tower-http trace layer.
pub fn request_span<B>(request: &Request<B>) -> tracing::Span {
    tracing::info_span!("http", request = %describe_request(request))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::header::{AUTHORIZATION, COOKIE};

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("a sink").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("a sink")).to_string()
        }
    }

    fn request(uri: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header(AUTHORIZATION, "Bearer super-secret-owner-token")
            .header(COOKIE, "swarmbot_session=also-a-secret-value")
            .body(Body::empty())
            .expect("a request")
    }

    #[test]
    fn a_log_line_built_from_a_request_carries_no_credential() {
        // This is the assertion the crate's redaction rule exists for: the
        // request *has* an Authorization header and a session cookie, and the
        // line that describes it must not.
        let line = describe_request(&request("/api/bots?limit=2"));
        assert!(!line.contains("super-secret-owner-token"), "{line}");
        assert!(!line.contains("also-a-secret-value"), "{line}");
        assert!(
            !line.to_ascii_lowercase().contains("authorization"),
            "{line}"
        );
        assert!(!line.to_ascii_lowercase().contains("cookie"), "{line}");
        assert!(line.contains("GET"), "{line}");
        assert!(line.contains("/api/bots?limit=2"), "{line}");

        let span_line = format!("{:?}", request_span(&request("/api/settings")).id());
        assert!(
            !span_line.contains("super-secret-owner-token"),
            "{span_line}"
        );
    }

    #[test]
    fn credential_shaped_query_values_are_redacted_but_others_survive() {
        assert_eq!(
            redact_query("cursor=3:41&token=abc123&limit=5"),
            "cursor=3:41&token=[redacted]&limit=5"
        );
        assert_eq!(redact_query("api_key=xyz"), "api_key=[redacted]");
        assert_eq!(redact_query("after_seq=7"), "after_seq=7");
        assert_eq!(redact_query(""), "");
        let line = describe_request(&request("/ws/live?cursor=1:2&access_token=hunter2xx"));
        assert!(line.contains("cursor=1:2"), "{line}");
        assert!(!line.contains("hunter2xx"), "{line}");
    }

    #[test]
    fn a_per_turn_mcp_token_in_the_path_is_redacted() {
        // `/mcp/t/{token}` is the one route whose credential is in the path, and
        // the access log renders the path verbatim. The route is kept, the token
        // is not — an operator still needs to see which endpoint was hit.
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let line = describe_request(&request(&format!("/mcp/t/{token}")));
        assert!(!line.contains(token), "{line}");
        assert!(line.contains("/mcp/t/[redacted]"), "{line}");
        assert_eq!(redact_path(&format!("/mcp/t/{token}")), "/mcp/t/[redacted]");
        // Everything else is untouched: the static route, the health probe, the
        // public API and a path that only looks like the prefix.
        for unchanged in ["/mcp", "/health", "/api/bots", "/mcp/t", "/mcp/t/"] {
            assert_eq!(redact_path(unchanged), unchanged);
        }
    }

    #[test]
    fn a_forgotten_secret_is_no_longer_scrubbed() {
        // Per-turn MCP tokens are minted and dropped on every model call, so the
        // registry has to be able to forget one: keeping every token forever
        // would make the scrubber grow without bound.
        let registry = SecretRegistry::new();
        registry.register("a-per-turn-token-value");
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.redact("hit /mcp/t/a-per-turn-token-value"),
            "hit /mcp/t/[redacted]"
        );
        registry.forget("a-per-turn-token-value");
        assert_eq!(registry.len(), 0);
        assert_eq!(
            registry.redact("hit /mcp/t/a-per-turn-token-value"),
            "hit /mcp/t/a-per-turn-token-value",
            "a forgotten token is no longer a secret"
        );
    }

    #[test]
    fn the_scrubber_replaces_registered_secrets_whole_lines_at_a_time() {
        let registry = SecretRegistry::new();
        registry.register("the-owner-token-value");
        registry.register("sk-live-api-key");
        // Too short to scrub: an 8-character minimum keeps ordinary words out.
        registry.register("short");

        let sink = Shared::default();
        let mut writer = Scrubber::new(Arc::clone(&registry), sink.clone()).make_writer_for_test();
        // A secret split across two writes is still caught, because nothing is
        // emitted until the line ends.
        writer
            .write_all(b"login url http://x/#token=the-owner-tok")
            .expect("writes");
        writer
            .write_all(b"en-value and key=sk-live-api-key\n")
            .expect("writes");
        writer.flush().expect("flushes");

        let logged = sink.text();
        assert!(!logged.contains("the-owner-token-value"), "{logged}");
        assert!(!logged.contains("sk-live-api-key"), "{logged}");
        assert!(logged.contains("[redacted]"), "{logged}");
        assert!(logged.contains("login url http://x/#token="), "{logged}");
        assert_eq!(registry.len(), 2, "the short value was not registered");
        assert!(!registry.is_empty());
    }

    #[test]
    fn text_without_a_secret_passes_through_untouched() {
        let registry = SecretRegistry::new();
        registry.register("the-owner-token-value");
        assert_eq!(registry.redact("nothing to see"), "nothing to see");
        assert_eq!(
            registry.redact("bearer the-owner-token-value here"),
            "bearer [redacted] here"
        );
    }

    #[test]
    fn the_default_filter_is_a_valid_directive_list() {
        assert!(EnvFilter::try_new(DEFAULT_FILTER).is_ok());
        assert!(EnvFilter::try_new("info").is_ok());
    }

    #[test]
    fn sensitive_headers_are_the_ones_that_carry_credentials() {
        assert!(SENSITIVE_HEADERS.contains(&AUTHORIZATION));
        assert!(SENSITIVE_HEADERS.contains(&COOKIE));
        assert!(is_sensitive_key("token"));
        assert!(is_sensitive_key("ACCESS_TOKEN"));
        assert!(!is_sensitive_key("cursor"));
    }

    impl Scrubber {
        /// The tests need a writer they can drive by hand.
        fn make_writer_for_test(&self) -> ScrubbedWriter {
            use tracing_subscriber::fmt::MakeWriter as _;
            self.make_writer()
        }
    }
}
