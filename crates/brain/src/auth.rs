//! Owner authentication, and the credentials around it.
//!
//! One owner, one brain (decision D0.2), so there is no user table: there is a
//! **token** in `<data>/owner-token` (0600) that authorizes REST calls, and a
//! **session cookie** minted by `POST /api/session` for a browser that cannot
//! carry an `Authorization` header — most importantly the `/ws/live` upgrade,
//! which a browser can only authenticate with a cookie.
//!
//! Three rules shape this module:
//!
//! * the login URL is printed **once**, when the token file is created, and the
//!   token itself stays in a file only its owner can read;
//! * credentials are compared in constant time and are never logged — the token
//!   is registered with [`crate::telemetry`] so a stray log line is scrubbed;
//! * a WebSocket upgrade additionally has to come from an allowed `Origin`,
//!   because a browser will happily attach the session cookie to a page from
//!   anywhere on the internet (cross-site WebSocket hijacking).
//!
//! The Ed25519 identity and the pairing-code alphabet are *not* re-implemented
//! here: `swarmbot-node-protocol` owns both, and this crate only decides where the
//! identity file lives and when it is minted. That matters because a node pins the
//! brain's public key at pairing (decision D1.3) — two implementations of "the
//! brain's key" would eventually disagree about which one a node trusted.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, COOKIE, ORIGIN};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};
use tracing::{info, warn};

use swarmbot_contracts::error::DomainError;

use crate::config::{Paths, ensure_private, write_private};

/// The brain's Ed25519 identity, owned by `swarmbot-node-protocol`: it signs
/// `node.hello` and its public half is handed out at pairing.
pub use swarmbot_node_protocol::identity::BrainIdentity as Identity;

/// The session cookie's name.
pub const SESSION_COOKIE: &str = "swarmbot_session";

/// How many live sessions are kept before the oldest is dropped.
const MAX_SESSIONS: usize = 64;

/// Bytes of entropy in a token or a session id.
const SECRET_BYTES: usize = 32;

/// A pairing code is single-use and lives ten minutes (decision D1.3).
pub const PAIRING_CODE_TTL_SECS: i64 = 10 * 60;

/// Who a request was authenticated as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Principal {
    /// The bearer token from `<data>/owner-token`.
    Owner,
    /// A session cookie minted by `POST /api/session`.
    Session,
}

impl Principal {
    /// The name used in access logs. Never carries a credential.
    pub fn label(self) -> &'static str {
        match self {
            Self::Owner => "owner-token",
            Self::Session => "session",
        }
    }
}

/// The owner token, the sessions minted from it, and the Origin allow-list.
pub struct OwnerAuth {
    token: String,
    sessions: Mutex<VecDeque<String>>,
    /// Cookie carries `Secure` when the configured origin is https: a token over
    /// plaintext HTTP would be readable by anything on the path.
    secure_cookie: bool,
    /// Exact origins allowed to upgrade a WebSocket.
    allowed_origins: Vec<String>,
}

impl OwnerAuth {
    /// Read `<data>/owner-token`, or mint it.
    ///
    /// The second return value says whether the token was minted now, which is
    /// the only moment the login URL may be printed: a URL that shows up in
    /// every log is not a one-time URL.
    pub fn load_or_create(
        paths: &Paths,
        public_origin: &str,
        public_addr: &str,
    ) -> anyhow::Result<(Arc<Self>, bool)> {
        let file = paths.owner_token();
        let (token, minted) = match std::fs::read_to_string(&file) {
            Ok(existing) => {
                let token = existing.trim().to_string();
                if token.is_empty() {
                    anyhow::bail!("{} exists but is empty", file.display());
                }
                // Somebody may have created the file by hand with loose modes.
                ensure_private(&file);
                (token, false)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let token = random_token();
                write_private(&file, &format!("{token}\n"))
                    .with_context(|| format!("cannot write {}", file.display()))?;
                (token, true)
            }
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", file.display()));
            }
        };

        let secure_cookie = public_origin.starts_with("https://");
        let mut allowed_origins = vec![public_origin.trim_end_matches('/').to_string()];
        // The listener's own origin is always allowed: a UI served by the brain
        // itself is the common case, and its port is only known after binding.
        allowed_origins.push(format!("http://{}", public_addr));
        allowed_origins.push(format!("https://{}", public_addr));

        info!(
            path = %file.display(),
            minted,
            secure_cookie,
            "owner token ready"
        );
        Ok((
            Arc::new(Self {
                token,
                sessions: Mutex::new(VecDeque::new()),
                secure_cookie,
                allowed_origins,
            }),
            minted,
        ))
    }

    /// An in-memory brain (tests) has no data dir to keep a token in.
    pub fn ephemeral(token: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            token: token.into(),
            sessions: Mutex::new(VecDeque::new()),
            secure_cookie: false,
            allowed_origins: Vec::new(),
        })
    }

    /// The owner token. Only ever handed to the login URL and the log scrubber.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Authenticate a request: a bearer token, or a session cookie, or both.
    ///
    /// Either credential on its own is enough, and a *wrong* bearer does not
    /// cancel a valid cookie. That matters because the blocking client keeps the
    /// owner token after `POST /api/session` — a WebSocket upgrade cannot read a
    /// cookie jar — so every later request carries both, and refusing one
    /// because the other is present would lock the CLI and the UI out.
    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, DomainError> {
        let mut bearer_seen = false;
        if let Some(value) = header_str(headers, AUTHORIZATION)
            && let Some(token) = value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        {
            bearer_seen = true;
            let token = token.trim();
            if constant_time_eq(token.as_bytes(), self.token.as_bytes()) {
                return Ok(Principal::Owner);
            }
            if self.is_session(token) {
                // A session id also works as a bearer, which is how a WebSocket
                // client that cannot set a cookie authenticates.
                return Ok(Principal::Session);
            }
        }
        if let Some(cookie) = header_str(headers, COOKIE)
            && let Some(session) = cookie_value(&cookie, SESSION_COOKIE)
            && self.is_session(&session)
        {
            return Ok(Principal::Session);
        }
        Err(DomainError::Unauthorized {
            message: if bearer_seen {
                "that bearer token is not this brain's owner token, and no session cookie is valid"
                    .to_string()
            } else {
                format!(
                    "send `Authorization: Bearer <token>` or the `{SESSION_COOKIE}` cookie from POST /api/session"
                )
            },
        })
    }

    /// Is this origin allowed to open a WebSocket with our cookie attached?
    ///
    /// A request without an `Origin` header is not a browser (a CLI, the GPUI
    /// app, a test) and cannot be a cross-site attack, so it passes. Loopback
    /// origins pass on any port: the brain binds a port that may differ from the
    /// configured one, and a page from `127.0.0.1` is by definition the owner's
    /// own machine.
    pub fn origin_allowed(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = header_str(headers, ORIGIN) else {
            return true;
        };
        self.origin_value_allowed(&origin)
    }

    /// The same rule, for a caller that already has the origin string — the CORS
    /// layer, which sees it as a header *value*.
    pub fn origin_value_allowed(&self, origin: &str) -> bool {
        let origin = origin.trim().trim_end_matches('/');
        if origin.is_empty() || origin.eq_ignore_ascii_case("null") {
            // `Origin: null` is what a sandboxed iframe or a `file://` page
            // sends; neither is an owner's UI we can distinguish, so it is out.
            return false;
        }
        if self
            .allowed_origins
            .iter()
            .any(|allowed| allowed.trim_end_matches('/') == origin)
        {
            return true;
        }
        url::Url::parse(origin)
            .map(|url| is_loopback(url.host_str().unwrap_or_default()))
            .unwrap_or(false)
    }

    /// Mint a session id and remember it.
    pub fn create_session(&self) -> String {
        let id = random_token();
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        while sessions.len() >= MAX_SESSIONS {
            if let Some(dropped) = sessions.pop_front() {
                warn!("dropping the oldest session: too many are live");
                let _ = dropped;
            } else {
                break;
            }
        }
        sessions.push_back(id.clone());
        id
    }

    pub fn is_session(&self, value: &str) -> bool {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .iter()
            .any(|session| constant_time_eq(session.as_bytes(), value.as_bytes()))
    }

    /// Forget every session. The owner token still works.
    pub fn revoke_sessions(&self) -> usize {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let count = sessions.len();
        sessions.clear();
        count
    }

    /// The `Set-Cookie` value for a fresh session.
    ///
    /// `HttpOnly` keeps a script from reading it, `SameSite=Lax` keeps another
    /// site from riding along on a cross-site POST, and `Path=/` makes it apply
    /// to the whole API rather than to `/api/session` alone.
    pub fn cookie_header(&self, session: &str) -> String {
        let secure = if self.secure_cookie { "; Secure" } else { "" };
        format!("{SESSION_COOKIE}={session}; HttpOnly; SameSite=Lax; Path=/{secure}")
    }

    /// The one-time login URL.
    ///
    /// The token travels in the fragment, not in the query: a fragment is never
    /// sent to a server, so it cannot end up in an access log, a `Referer`
    /// header or a proxy's history.
    pub fn login_url(&self, origin: &str) -> String {
        format!("{}/#token={}", origin.trim_end_matches('/'), self.token)
    }
}

impl fmt::Debug for OwnerAuth {
    /// Manual, because the derived one would print the owner token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnerAuth")
            .field("token", &"[redacted]")
            .field(
                "sessions",
                &self.sessions.lock().map(|s| s.len()).unwrap_or(0),
            )
            .field("secure_cookie", &self.secure_cookie)
            .field("allowed_origins", &self.allowed_origins)
            .finish()
    }
}

/// The origin every node signature is bound to, normalized the way the protocol
/// normalizes it (`ws`→`http`, no path, no query, no trailing slash).
///
/// Computed once at boot and used for three things that must not drift: the
/// identity file, the `brainOrigin` a node pins when it pairs, and the string an
/// upgrade signature is verified against.
pub fn brain_origin(public_origin: &str) -> anyhow::Result<String> {
    swarmbot_node_protocol::auth::brain_origin_from_url(public_origin).map_err(|error| {
        anyhow::anyhow!("`{public_origin}` cannot be the origin a node pins: {error}")
    })
}

/// Load the brain's identity from `<data>/brain-identity.json`, or mint it.
///
/// A file that exists but cannot be read is an error, not a reason to generate a
/// new key: every node that paired with this brain pinned the old public key, and
/// rotating it silently would leave them unable to verify `node.hello` — which
/// looks exactly like a compromised brain.
///
/// The stored `origin` wins over the configured one for the same reason. A
/// mismatch is worth a warning, because it means the brain moved and its nodes
/// have not been told.
pub fn load_or_create_identity(paths: &Paths, origin: &str) -> anyhow::Result<Identity> {
    let file = paths.brain_identity();
    if file.exists() {
        let identity = Identity::load(&file).with_context(|| {
            format!(
                "{} exists but cannot be read as a brain identity; refusing to rotate the key                  a paired node pinned",
                file.display()
            )
        })?;
        ensure_private(&file);
        if identity.origin != origin {
            warn!(
                stored = %identity.origin,
                configured = %origin,
                "the brain identity was created for a different origin; nodes verify signatures \
                 against the origin they pinned, so the stored one is kept"
            );
        }
        return Ok(identity);
    }
    let identity = Identity::generate(origin)
        .map_err(|err| anyhow::anyhow!("cannot create a brain identity: {err}"))?;
    identity
        .save(&file)
        .map_err(|err| anyhow::anyhow!("cannot write {}: {err}", file.display()))?;
    info!(
        path = %file.display(),
        fingerprint = %identity_fingerprint(&identity),
        "the brain identity was created"
    );
    Ok(identity)
}

/// A short, safe-to-log name for an identity: the first 16 hex characters of
/// `sha256(public key)`.
///
/// The public key is not secret, but a log full of 43-character base64url keys is
/// unreadable, and a fingerprint is enough to tell two brains apart.
pub fn identity_fingerprint(identity: &Identity) -> String {
    let digest = Sha256::digest(identity.public_key_base64url().as_bytes());
    hex::encode(&digest[..8])
}

/// A 32-byte random token, base64url without padding.
pub fn random_token() -> String {
    let mut bytes = [0u8; SECRET_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Compare without short-circuiting on the first difference.
///
/// Length is compared first: it is not a secret here (both a token and a session
/// id are 43 base64url characters), and skipping the byte loop for a mismatched
/// length is what keeps this cheap enough to run on every request.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn header_str(headers: &HeaderMap, name: axum::http::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Find one cookie's value in a `Cookie:` header.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

fn is_loopback(host: &str) -> bool {
    let host = host.trim_matches(|c| c == '[' || c == ']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The file a `--check` run proves it can open, without composing a swarm.
pub fn check_store(paths: &Paths) -> anyhow::Result<()> {
    let path = paths.database();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let store = swarmbot_store::SqliteStore::open(&path)
        .map_err(|err| anyhow::anyhow!("cannot open {}: {err}", path.display()))?;
    // `SqliteStore` is a concrete type here, so the trait that declares the
    // method has to be in scope (a `dyn Persistence` gets it from the vtable).
    use swarmbot_contracts::repo::BotRepo as _;
    let bots = store
        .list_bots()
        .map_err(|err| anyhow::anyhow!("cannot read the roster: {err}"))?;
    info!(path = %path.display(), bots = bots.len(), "store opened");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("a header name"),
                HeaderValue::from_str(value).expect("a header value"),
            );
        }
        map
    }

    fn auth() -> Arc<OwnerAuth> {
        OwnerAuth::ephemeral("the-owner-token")
    }

    #[test]
    fn the_owner_token_authenticates_and_nothing_else_does() {
        let auth = auth();
        let ok = headers(&[("authorization", "Bearer the-owner-token")]);
        assert_eq!(auth.authenticate(&ok).expect("owner"), Principal::Owner);

        for bad in [
            headers(&[]),
            headers(&[("authorization", "Bearer nope")]),
            headers(&[("authorization", "Basic dXNlcjpwYXNz")]),
            headers(&[("cookie", "swarmbot_session=nope")]),
            headers(&[("authorization", "the-owner-token")]),
        ] {
            let err = auth.authenticate(&bad).unwrap_err();
            assert_eq!(err.code(), "unauthorized", "{err}");
        }
    }

    #[test]
    fn a_session_cookie_authenticates_after_login() {
        let auth = auth();
        let session = auth.create_session();
        assert!(auth.is_session(&session));
        let with_cookie = headers(&[("cookie", &format!("other=1; {SESSION_COOKIE}={session}"))]);
        assert_eq!(
            auth.authenticate(&with_cookie).expect("session"),
            Principal::Session
        );
        // A session id also works as a bearer, which is how a WebSocket client
        // that cannot set a cookie can still authenticate.
        let as_bearer = headers(&[("authorization", &format!("Bearer {session}"))]);
        assert_eq!(
            auth.authenticate(&as_bearer).expect("session"),
            Principal::Session
        );
        assert_eq!(auth.revoke_sessions(), 1);
        assert!(!auth.is_session(&session));
        assert!(auth.authenticate(&with_cookie).is_err());
        // Revoking sessions never invalidates the owner token.
        assert!(
            auth.authenticate(&headers(&[("authorization", "Bearer the-owner-token")]))
                .is_ok()
        );
    }

    #[test]
    fn both_credentials_at_once_still_authorize() {
        // This is what the blocking client sends after `login()`: the session
        // cookie from the jar *and* the owner token as a bearer, because a
        // WebSocket upgrade cannot read the jar.
        let auth = auth();
        let session = auth.create_session();
        let both = headers(&[
            ("authorization", "Bearer the-owner-token"),
            ("cookie", &format!("{SESSION_COOKIE}={session}")),
        ]);
        assert_eq!(auth.authenticate(&both).expect("owner"), Principal::Owner);

        // A stale bearer must not cancel a live cookie, or a client that rotated
        // its token would be locked out of a session it already paid for.
        let stale = headers(&[
            ("authorization", "Bearer an-old-token"),
            ("cookie", &format!("{SESSION_COOKIE}={session}")),
        ]);
        assert_eq!(
            auth.authenticate(&stale).expect("session"),
            Principal::Session
        );
        let err = auth
            .authenticate(&headers(&[("authorization", "Bearer an-old-token")]))
            .unwrap_err();
        assert_eq!(err.code(), "unauthorized");
        assert!(err.to_string().contains("bearer token"), "{err}");
    }

    #[test]
    fn sessions_are_capped() {
        let auth = auth();
        let first = auth.create_session();
        for _ in 0..MAX_SESSIONS {
            auth.create_session();
        }
        assert!(!auth.is_session(&first), "the oldest session is dropped");
    }

    #[test]
    fn the_cookie_is_httponly_lax_and_scoped_to_the_api() {
        let auth = auth();
        let cookie = auth.cookie_header("abc");
        assert!(cookie.starts_with("swarmbot_session=abc;"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Path=/"));
        assert!(!cookie.contains("Secure"), "http origin: no Secure flag");

        let secure = OwnerAuth {
            secure_cookie: true,
            ..OwnerAuth::ephemeral_for_test()
        };
        assert!(secure.cookie_header("abc").contains("Secure"));
    }

    #[test]
    fn the_login_url_keeps_the_token_out_of_the_query() {
        let auth = auth();
        let url = auth.login_url("http://127.0.0.1:8787/");
        assert_eq!(url, "http://127.0.0.1:8787/#token=the-owner-token");
        let (before, fragment) = url.split_once('#').expect("a fragment");
        assert!(!before.contains("the-owner-token"), "{url}");
        assert!(fragment.starts_with("token="));
    }

    #[test]
    fn origins_are_allowed_by_list_or_by_being_loopback() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = Paths::new(dir.path());
        let (auth, minted) =
            OwnerAuth::load_or_create(&paths, "https://brain.example.com", "127.0.0.1:8787")
                .expect("loads");
        assert!(minted, "a fresh data dir mints a token");
        let (_, again) =
            OwnerAuth::load_or_create(&paths, "https://brain.example.com", "127.0.0.1:8787")
                .expect("loads");
        assert!(!again, "and only mints it once");
        assert_eq!(
            std::fs::read_to_string(paths.owner_token())
                .expect("read")
                .trim(),
            auth.token()
        );

        for allowed in [
            "https://brain.example.com",
            "http://127.0.0.1:8787",
            "http://127.0.0.1:1", // any loopback port
            "http://localhost:3000",
            "http://[::1]:3000",
        ] {
            assert!(
                auth.origin_allowed(&headers(&[("origin", allowed)])),
                "{allowed} should be allowed"
            );
        }
        for refused in [
            "http://evil.example",
            "https://127.0.0.1.evil.example",
            "null",
        ] {
            assert!(
                !auth.origin_allowed(&headers(&[("origin", refused)])),
                "{refused} should be refused"
            );
        }
        // No Origin header at all: a native client, not a browser.
        assert!(auth.origin_allowed(&headers(&[])));
        // An https origin means the cookie is marked Secure.
        assert!(auth.cookie_header("x").contains("Secure"));
    }

    #[test]
    fn the_brain_identity_survives_a_reload_and_never_prints_its_key() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = Paths::new(dir.path());
        let origin = "http://127.0.0.1:8787";
        let first = load_or_create_identity(&paths, origin).expect("created");
        let public = first.public_key_base64url();
        assert_eq!(public.len(), 43, "32 bytes, base64url without padding");
        assert_eq!(first.origin, origin);
        assert!(crate::config::is_owner_only(&paths.brain_identity()));

        // A reload is the same key: a node pins it at pairing.
        let second = load_or_create_identity(&paths, origin).expect("reloaded");
        assert_eq!(second.public_key_base64url(), public, "the key is stable");
        assert_eq!(identity_fingerprint(&second), identity_fingerprint(&first));
        assert_eq!(identity_fingerprint(&first).len(), 16);

        // A different configured origin does not silently rewrite the identity.
        let third = load_or_create_identity(&paths, "https://brain.example.com").expect("reloaded");
        assert_eq!(third.origin, origin, "the stored origin wins");

        let debug = format!("{first:?}");
        assert!(!debug.contains(&public), "{debug}");
        let stored = std::fs::read_to_string(paths.brain_identity()).expect("read");
        assert!(stored.contains("secretKey"), "the file holds the key");
    }

    #[test]
    fn a_corrupt_identity_file_is_an_error_not_a_rotation() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = Paths::new(dir.path());
        std::fs::write(paths.brain_identity(), "{not json").expect("write");
        let err = load_or_create_identity(&paths, "http://127.0.0.1:1").unwrap_err();
        assert!(err.to_string().contains("refusing to rotate"), "{err:#}");
    }

    #[test]
    fn the_owner_auth_debug_hides_the_token() {
        let auth = auth();
        let debug = format!("{auth:?}");
        assert!(!debug.contains("the-owner-token"), "{debug}");
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn constant_time_comparison_is_a_comparison() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn cookies_are_parsed_by_name() {
        assert_eq!(
            cookie_value("a=1; swarmbot_session=xyz; b=2", "swarmbot_session").as_deref(),
            Some("xyz")
        );
        assert_eq!(cookie_value("a=1", "swarmbot_session"), None);
        assert_eq!(
            cookie_value("swarmbot_session=", "swarmbot_session").as_deref(),
            Some("")
        );
        assert_eq!(cookie_value("", "swarmbot_session"), None);
    }

    impl OwnerAuth {
        /// A handle the tests can flip flags on; the fields are private so a
        /// route cannot accidentally depend on one.
        fn ephemeral_for_test() -> Self {
            Self {
                token: "t".into(),
                sessions: Mutex::new(VecDeque::new()),
                secure_cookie: false,
                allowed_origins: Vec::new(),
            }
        }
    }
}
