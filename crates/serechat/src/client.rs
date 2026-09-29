//! Blocking HTTP client for the SereChat API.
//!
//! Every call blocks the calling thread; the desktop app runs them on worker
//! threads so the render loop never waits on the network.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use ureq::http::Response;

use crate::error::{Error, Result};

/// Production API origin.
pub const BASE_URL: &str = "https://serechat.com";

/// A cheaply clonable handle to the API. Clones share one connection pool.
#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: Option<String>,
}

impl std::fmt::Debug for Client {
    // Hand-written so the bearer token never ends up in logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base)
            .field("authenticated", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

/// Result of a successful device-code exchange.
#[derive(Debug, Clone, Deserialize)]
pub struct AccessToken {
    /// Bearer token for inference requests.
    pub access_token: String,
    /// Lifetime in seconds (currently one year).
    pub expires_in: u64,
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
}

impl Model {
    /// Cost in USD of a response with the given token counts.
    #[must_use]
    pub fn cost(&self, usage: crate::Usage) -> f64 {
        (usage.input_tokens as f64 * self.input_cost_per_million + usage.output_tokens as f64 * self.output_cost_per_million)
            / 1_000_000.0
    }
}

impl Client {
    /// Creates a client against [`BASE_URL`], optionally authenticated.
    #[must_use]
    pub fn new(token: Option<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            // Streams may legitimately idle while a model thinks; no body
            // timeout, cancellation is handled by the caller instead.
            .user_agent(concat!("serechat-desktop/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { agent, base: BASE_URL.to_owned(), token }
    }

    /// Returns a copy of this client using `token` for authentication.
    #[must_use]
    pub fn with_token(&self, token: String) -> Self {
        Self { token: Some(token), ..self.clone() }
    }

    /// The browser URL where the user approves a device-code request.
    #[must_use]
    pub fn authorize_url(&self, request_id: &str) -> String {
        format!("{}/authorize-app?request_id={request_id}", self.base)
    }

    /// Step 1 of the device-code flow: registers an authorization request and
    /// returns its `request_id` (valid for 10 minutes).
    ///
    /// # Errors
    /// Network failure or a non-success response.
    pub fn request_authorization(&self, app_name: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Body {
            request_id: String,
        }
        let body: Body = self.post_json("/api/auth/app/request", &json!({ "app_name": app_name }), false)?;
        Ok(body.request_id)
    }

    /// Step 2 of the device-code flow: trades the 6-digit code the user saw in
    /// the browser for a bearer token.
    ///
    /// # Errors
    /// Network failure or an API error; notable codes are `invalid_code`,
    /// `authorization_pending`, `request_not_found` and `request_invalidated`.
    pub fn exchange_code(&self, request_id: &str, code: &str) -> Result<AccessToken> {
        self.post_json("/api/auth/app/exchange", &json!({ "request_id": request_id, "code": code }), false)
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

    /// POSTs `body` as JSON and returns the raw response after checking the
    /// status. Used by streaming calls that consume the body incrementally.
    pub(crate) fn post(&self, path: &str, body: &Value, auth: bool) -> Result<Response<ureq::Body>> {
        let mut request = self
            .agent
            .post(format!("{}{path}", self.base))
            .content_type("application/json");
        if auth {
            let token = self.token.as_deref().unwrap_or_default();
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = request.send(serde_json::to_vec(body)?)?;
        check_status(response)
    }

    fn post_json<T: serde::de::DeserializeOwned>(&self, path: &str, body: &Value, auth: bool) -> Result<T> {
        read_json(self.post(path, body, auth)?)
    }
}

/// Decodes a JSON body after checking the status code.
fn read_json<T: serde::de::DeserializeOwned>(response: Response<ureq::Body>) -> Result<T> {
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

/// Extracts `(code, message)` from an error body. Accepts both
/// `{"error": {"code", "message"}}` and flat `{"error": "code", "message"}`.
pub(crate) fn parse_error_body(text: &str) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return (None, None);
    };
    let error = value.get("error").unwrap_or(&value);
    let field = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    let code = field(error, "code").or_else(|| error.as_str().map(str::to_owned));
    let message = field(error, "message").or_else(|| field(&value, "message"));
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
        let cost = model.cost(Usage { input_tokens: 1_000, output_tokens: 500 });
        assert!((cost - 0.007).abs() < 1e-12);
        let bare: Model = serde_json::from_str(r#"{"id":"m"}"#).unwrap();
        assert!(bare.cost(Usage { input_tokens: 5, output_tokens: 5 }).abs() < f64::EPSILON);
    }

    #[test]
    fn error_bodies() {
        let nested = r#"{"error":{"message":"Wrong code","type":"x","code":"invalid_code"}}"#;
        assert_eq!(parse_error_body(nested), (Some("invalid_code".into()), Some("Wrong code".into())));
        let flat = r#"{"error":"authorization_pending","message":"Not yet"}"#;
        assert_eq!(parse_error_body(flat), (Some("authorization_pending".into()), Some("Not yet".into())));
        assert_eq!(parse_error_body("<html>"), (None, None));
    }
}
