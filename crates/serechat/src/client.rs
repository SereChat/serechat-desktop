//! Blocking HTTP client for the SereChat API.
//!
//! Every call blocks the calling thread; the desktop app runs them on worker
//! threads so the render loop never waits on the network.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use ureq::http::Response;

use crate::auth::{Auth, Tokens};
use crate::error::{Error, Result};

/// Production API origin.
pub const BASE_URL: &str = "https://serechat.com";

/// A cheaply clonable handle to the API. Clones share one connection pool
/// and, once signed in, one set of tokens.
#[derive(Clone)]
pub struct Client {
    pub(crate) agent: ureq::Agent,
    base: String,
    pub(crate) auth: Option<Arc<Auth>>,
}

impl std::fmt::Debug for Client {
    // Hand-written so the tokens never end up in logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base)
            .field("authenticated", &self.auth.is_some())
            .finish_non_exhaustive()
    }
}

/// A chat model offered by the API.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Model {
    /// Identifier sent in requests, e.g. `claude-sonnet-5.5`.
    pub id: String,
    /// Display name, e.g. `Claude Sonnet 5.5`.
    #[serde(default)]
    pub name: String,
    /// USD per million prompt tokens; `0` when the server omits it.
    #[serde(default)]
    pub input_cost_per_million: f64,
    /// USD per million generated tokens; `0` when the server omits it.
    #[serde(default)]
    pub output_cost_per_million: f64,
    /// USD per million prompt tokens read from the cache; the input price when omitted.
    #[serde(default)]
    pub cache_read_cost_per_million: Option<f64>,
    /// USD per million prompt tokens written to the cache; the input price when omitted.
    #[serde(default)]
    pub cache_write_cost_per_million: Option<f64>,
    /// Accepted input kinds, e.g. `text`, `image`, `audio`.
    #[serde(default)]
    pub input_types: Vec<String>,
    /// Prompt plus output tokens the model accepts; `0` when the server omits it.
    #[serde(default)]
    pub context_window: u64,
    /// Reasoning efforts the model accepts (`none`, `minimal`, `low`,
    /// `medium`, `high`, `xhigh`, `max`); empty when it cannot reason.
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
}

impl Model {
    /// Whether the model can read images. Unknown (no list) counts as yes so
    /// an older server never blocks attachments.
    #[must_use]
    pub fn accepts_images(&self) -> bool {
        self.input_types.is_empty() || self.input_types.iter().any(|t| t == "image")
    }

    /// Cost in USD of a response with the given token counts, pricing cached
    /// prompt tokens the way the server bills them.
    #[must_use]
    pub fn cost(&self, usage: crate::Usage) -> f64 {
        let cached = usage.cached_tokens;
        let written = usage.cache_write_tokens;
        let uncached = usage.input_tokens.saturating_sub(cached + written);
        (uncached as f64 * self.input_cost_per_million
            + cached as f64 * self.cache_read_cost_per_million.unwrap_or(self.input_cost_per_million)
            + written as f64 * self.cache_write_cost_per_million.unwrap_or(self.input_cost_per_million)
            + usage.output_tokens as f64 * self.output_cost_per_million)
            / 1_000_000.0
    }
}

impl Client {
    /// Creates a signed-out client against [`BASE_URL`]; see [`Client::signed_in`].
    #[must_use]
    pub fn new() -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            // Streams may legitimately idle while a model thinks; no body
            // timeout, cancellation is handled by the caller instead.
            .user_agent(concat!("serechat-desktop/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { agent, base: BASE_URL.to_owned(), auth: None }
    }

    /// Lists the available chat models.
    ///
    /// # Errors
    /// Network failure or a non-success response.
    pub fn models(&self) -> Result<Vec<Model>> {
        #[derive(Deserialize)]
        struct Body {
            data: Vec<Model>,
        }
        let response = self.agent.get(format!("{}/v1/models", self.base)).call()?;
        let body: Body = read_json(response)?;
        Ok(body.data)
    }

    /// Downloads a web page or text resource (no credentials are sent),
    /// reading at most `limit` bytes. Returns the content type and body.
    ///
    /// # Errors
    /// Network failure or a non-success status. Invalid UTF-8 is replaced.
    pub fn fetch_text(&self, url: &str, limit: u64) -> Result<(String, String)> {
        let mut response = check_status(self.agent.get(url).call()?)?;
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        // Truncate rather than fail: a long page is still useful.
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut std::io::Read::take(response.body_mut().with_config().reader(), limit), &mut body)?;
        Ok((content_type, String::from_utf8_lossy(&body).into_owned()))
    }

    /// POSTs `body` as JSON and returns the raw response after checking the
    /// status. Used by streaming calls that consume the body incrementally.
    pub(crate) fn post(&self, path: &str, body: &Value, auth: bool) -> Result<Response<ureq::Body>> {
        let body = serde_json::to_vec(body)?;
        self.call(auth, |token| bearer(self.agent.post(self.url(path)), token).content_type("application/json").send(&body[..]))
    }

    /// POSTs a raw `body` of `content_type` with the token and returns the
    /// response after checking the status.
    pub(crate) fn post_bytes(&self, path: &str, content_type: &str, body: &[u8]) -> Result<Response<ureq::Body>> {
        self.call(true, |token| bearer(self.agent.post(self.url(path)), token).content_type(content_type).send(body))
    }

    /// DELETE with a body (`DELETE /api/user/files` takes its ids as JSON).
    pub(crate) fn delete_bytes(&self, path: &str, content_type: &str, body: &[u8]) -> Result<Response<ureq::Body>> {
        self.call(true, |token| bearer(self.agent.delete(self.url(path)).force_send_body(), token).content_type(content_type).send(body))
    }

    pub(crate) fn post_json<T: serde::de::DeserializeOwned>(&self, path: &str, body: &Value, auth: bool) -> Result<T> {
        read_json(self.post(path, body, auth)?)
    }

    /// GETs an absolute `url` and returns the response after checking the
    /// status. The token is only ever sent to the API's own origin.
    pub(crate) fn get(&self, url: &str, auth: bool) -> Result<Response<ureq::Body>> {
        self.call(auth && self.owns(url), |token| bearer(self.agent.get(url), token).call())
    }

    /// Sends a request through `send`, which gets the access token when
    /// `auth` and the client is signed in, and checks the status.
    fn call(&self, auth: bool, send: impl Fn(Option<&str>) -> Result<Response<ureq::Body>, ureq::Error>) -> Result<Response<ureq::Body>> {
        self.call_with(auth, send, &|token| self.refresh(token))
    }

    /// [`Client::call`], refreshing tokens through `refresh`. A refused
    /// token is refreshed and the request sent once more; a token refused
    /// even then, or one missing a scope, is reported to the app.
    pub(crate) fn call_with(
        &self,
        auth: bool,
        send: impl Fn(Option<&str>) -> Result<Response<ureq::Body>, ureq::Error>,
        refresh: &dyn Fn(&str) -> Result<Tokens>,
    ) -> Result<Response<ureq::Body>> {
        let Some(grant) = self.auth.as_deref().filter(|_| auth) else { return check_status(send(None)?) };
        let token = grant.access_token(refresh)?;
        let mut response = send(Some(&token))?;
        if response.status() == 401 {
            // Revoked or expired early: refresh once and try again.
            let token = grant.replace(&token, refresh)?;
            response = send(Some(&token))?;
            if response.status() == 401 {
                grant.expire();
            }
        }
        let result = check_status(response);
        if result.as_ref().is_err_and(|e| e.code() == Some("insufficient_scope")) {
            grant.missing_scope();
        }
        result
    }

    /// `path` on the API's origin.
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Whether `url` is on the API's own origin, where the token may go.
    pub(crate) fn owns(&self, url: &str) -> bool {
        url.strip_prefix(&self.base).is_some_and(|rest| rest.starts_with('/'))
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// `request` with `token` as its bearer credential, if there is one.
fn bearer<B>(request: ureq::RequestBuilder<B>, token: Option<&str>) -> ureq::RequestBuilder<B> {
    match token {
        Some(token) => request.header("Authorization", format!("Bearer {token}")),
        None => request,
    }
}

/// Decodes a JSON body after checking the status code.
pub(crate) fn read_json<T: serde::de::DeserializeOwned>(response: Response<ureq::Body>) -> Result<T> {
    let mut response = check_status(response)?;
    let text = response.body_mut().read_to_string()?;
    Ok(serde_json::from_str(&text)?)
}

/// Turns non-2xx responses into [`Error::Api`], extracting the server's
/// message from the OpenAI-style `{"error": {...}}` envelope when present.
fn check_status(mut response: Response<ureq::Body>) -> Result<Response<ureq::Body>> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let text = response.body_mut().read_to_string().unwrap_or_default();
    let (code, message) = parse_error_body(&text);
    Err(Error::Api {
        status: status.as_u16(),
        code,
        message: message.unwrap_or_else(|| status.canonical_reason().unwrap_or("request failed").to_owned()),
    })
}

/// Extracts `(code, message)` from an error body. Accepts
/// `{"error": {"code", "message"}}`, flat `{"error": "code", "message"}`,
/// OAuth's `{"error": "code", "error_description"}`, and the web routes'
/// `{"error": "Message.", "code"}` and `{"detail": "Message."}`.
pub(crate) fn parse_error_body(text: &str) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return (None, None);
    };
    let error = value.get("error").unwrap_or(&value);
    let field = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    // A bare string is a code (`not_found`) or a sentence for the user.
    let (code, sentence) = match error.as_str() {
        Some(s) if s.contains(' ') => (None, Some(s.to_owned())),
        bare => (bare.map(str::to_owned), None),
    };
    let code = field(error, "code").or_else(|| field(&value, "code")).or(code);
    let message = field(error, "message")
        .or_else(|| field(&value, "message"))
        .or(sentence)
        .or_else(|| field(&value, "error_description"))
        .or_else(|| field(&value, "detail"));
    (code, message)
}

#[cfg(test)]
mod tests {
    use super::{Model, parse_error_body};
    use crate::Usage;

    #[test]
    fn model_pricing() {
        let json = r#"{"id":"m","name":"M","input_cost_per_million":2,"output_cost_per_million":10}"#;
        let model: Model = serde_json::from_str(json).unwrap();
        let cost = model.cost(Usage::new(1_000, 500));
        assert!((cost - 0.007).abs() < 1e-12);
        let bare: Model = serde_json::from_str(r#"{"id":"m"}"#).unwrap();
        assert!(bare.cost(Usage::new(5, 5)).abs() < f64::EPSILON);
        assert_eq!(bare.context_window, 0);

        // 1M input: 600k cached at 0.2, 100k written at 2.5, 300k plain at 2.
        let json = r#"{"id":"m","input_cost_per_million":2,"output_cost_per_million":10,
            "cache_read_cost_per_million":0.2,"cache_write_cost_per_million":2.5,"context_window":1000000}"#;
        let cached: Model = serde_json::from_str(json).unwrap();
        let usage = Usage { input_tokens: 1_000_000, output_tokens: 0, cached_tokens: 600_000, cache_write_tokens: 100_000 };
        assert!((cached.cost(usage) - (0.6 + 0.12 + 0.25)).abs() < 1e-9);
        assert_eq!(cached.context_window, 1_000_000);
    }

    #[test]
    fn error_bodies() {
        let nested = r#"{"error":{"message":"Wrong code","type":"x","code":"invalid_code"}}"#;
        assert_eq!(parse_error_body(nested), (Some("invalid_code".into()), Some("Wrong code".into())));
        let flat = r#"{"error":"authorization_pending","message":"Not yet"}"#;
        assert_eq!(parse_error_body(flat), (Some("authorization_pending".into()), Some("Not yet".into())));
        assert_eq!(parse_error_body("<html>"), (None, None));
        let sentence = r#"{"error":"This generation has already started."}"#;
        assert_eq!(parse_error_body(sentence), (None, Some("This generation has already started.".into())));
        let detail = r#"{"detail":"File too large. Maximum size is 25MB."}"#;
        assert_eq!(parse_error_body(detail), (None, Some("File too large. Maximum size is 25MB.".into())));
        let oauth = r#"{"error":"invalid_grant","error_description":"Refresh token is invalid"}"#;
        assert_eq!(parse_error_body(oauth), (Some("invalid_grant".into()), Some("Refresh token is invalid".into())));
        let scope = r#"{"error":"This token was not granted the 'files' scope.","code":"insufficient_scope","scope":"files"}"#;
        assert_eq!(parse_error_body(scope), (Some("insufficient_scope".into()), Some("This token was not granted the 'files' scope.".into())));
    }
}
