//! The blocking HTTP layer: a thin, explicit wrapper over `ureq`.
//!
//! Deliberately small — every [`crate::BrainClient`] method names its own URL
//! and calls one of `get`/`post`/`patch`/`delete` here, so a reader can follow
//! a request end to end without macro magic. Two choices matter:
//!
//! * `http_status_as_error(false)`: ureq's default turns 4xx/5xx into an
//!   error that *discards the body*, and the brain's error envelope
//!   (`{"error":{"code","message"}}`) lives in that body. So every status
//!   arrives here as a normal response and [`crate::ClientError::from_response`]
//!   decides what it means.
//! * timeouts are applied per request (not per agent) so `with_timeout` and
//!   the longer `probe` budget never need to rebuild the agent — which would
//!   throw away the session cookie jar `login()` filled.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::ClientError;

/// Default budget for one request: local REST calls are milliseconds; this
/// only catches a hung brain.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// `GET /api/backends/probe` makes the brain dial out to real LLM backends,
/// which can legitimately take much longer than a local call.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// Shared authentication state: `login()` may run long after the client was
/// built, and `live()` (a different module) needs the same token.
pub(crate) type SharedToken = Arc<Mutex<Option<String>>>;

/// Lock without ever panicking on a poisoned mutex: a stub test thread that
/// died mid-write must not take the whole client down.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct Http {
    agent: ureq::Agent,
    token: SharedToken,
    timeout: Duration,
}

impl Http {
    pub(crate) fn new(token: SharedToken) -> Self {
        let config = ureq::Agent::config_builder()
            // Non-2xx statuses come back as responses so the error body (the
            // documented envelope) can be read; see the module docs.
            .http_status_as_error(false)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            token,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub(crate) fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) fn set_token(&self, token: &str) {
        *lock(&self.token) = Some(token.to_string());
    }

    pub(crate) fn token(&self) -> Option<String> {
        lock(&self.token).clone()
    }

    pub(crate) fn get<T: DeserializeOwned>(&self, url: &str) -> Result<T, ClientError> {
        self.get_with_timeout(url, self.timeout)
    }

    /// GET with an explicit budget (used by `probe_backends`).
    pub(crate) fn get_with_timeout<T: DeserializeOwned>(
        &self,
        url: &str,
        timeout: Duration,
    ) -> Result<T, ClientError> {
        let request = self.timed(self.authed(self.agent.get(url)), timeout).call();
        self.finish(url, request)
    }

    pub(crate) fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let request = self
            .timed(self.authed(self.agent.post(url)), self.timeout)
            .send_json(body);
        self.finish(url, request)
    }

    /// POST without a request body (interrupt, cancel, pairing codes, …).
    pub(crate) fn post_empty<T: DeserializeOwned>(&self, url: &str) -> Result<T, ClientError> {
        let request = self
            .timed(self.authed(self.agent.post(url)), self.timeout)
            .send_empty();
        self.finish(url, request)
    }

    pub(crate) fn patch<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let request = self
            .timed(self.authed(self.agent.patch(url)), self.timeout)
            .send_json(body);
        self.finish(url, request)
    }

    /// DELETE, which answers 204 with no body on success.
    pub(crate) fn delete(&self, url: &str) -> Result<(), ClientError> {
        let response = self
            .timed(self.authed(self.agent.delete(url)), self.timeout)
            .call()
            .map_err(|err| Self::transport(url, err))?;
        let status = response.status().as_u16();
        if status >= 400 {
            return Err(ClientError::from_response(
                status,
                &Self::body_text(response, url)?,
            ));
        }
        Ok(())
    }

    /// Attach the bearer token when one is set. Without a token the header is
    /// omitted entirely: a brain with auth disabled must still work, and an
    /// empty `Bearer ` would only confuse a strict server.
    fn authed<S>(&self, request: ureq::RequestBuilder<S>) -> ureq::RequestBuilder<S> {
        match self.token() {
            Some(token) => request.header("authorization", format!("Bearer {token}")),
            None => request,
        }
    }

    /// Per-request timeout: keeps one agent (and its cookie jar) for the
    /// client's whole life while still letting `with_timeout` and `probe`
    /// pick their own budget.
    fn timed<S>(
        &self,
        request: ureq::RequestBuilder<S>,
        timeout: Duration,
    ) -> ureq::RequestBuilder<S> {
        request.config().timeout_global(Some(timeout)).build()
    }

    fn finish<T: DeserializeOwned>(
        &self,
        url: &str,
        response: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<T, ClientError> {
        let response = response.map_err(|err| Self::transport(url, err))?;
        let status = response.status().as_u16();
        let text = Self::body_text(response, url)?;
        if status >= 400 {
            return Err(ClientError::from_response(status, &text));
        }
        serde_json::from_str(&text).map_err(|err| ClientError::Decode {
            reason: format!("cannot decode the answer of `{url}` as JSON: {err}"),
        })
    }

    /// Read the whole body as text. ureq caps this at 10 MiB, far above
    /// anything the documented endpoints return.
    fn body_text(
        response: ureq::http::Response<ureq::Body>,
        url: &str,
    ) -> Result<String, ClientError> {
        let mut body = response.into_body();
        body.read_to_string()
            .map_err(|err| Self::transport(url, err))
    }

    fn transport(url: &str, err: ureq::Error) -> ClientError {
        tracing::debug!(%url, error = %err, "brain request failed in transport");
        ClientError::Transport {
            url: url.to_string(),
            reason: err.to_string(),
        }
    }
}

impl Clone for Http {
    /// Cloning shares the agent (connection pool *and* cookie jar) and the
    /// token, so a clone is another handle to the same session, not a new one.
    fn clone(&self) -> Self {
        Self {
            agent: self.agent.clone(),
            token: Arc::clone(&self.token),
            timeout: self.timeout,
        }
    }
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http")
            .field("timeout", &self.timeout)
            .field("has_token", &lock(&self.token).is_some())
            .finish_non_exhaustive()
    }
}
