//! Signing in to MCP servers on the web, as the MCP authorization spec
//! describes: OAuth 2.1 with PKCE, through the user's browser.
//!
//! 1. The server's `401` names its Protected Resource Metadata (RFC 9728)
//!    in `WWW-Authenticate`, or it is found at the well-known URIs.
//! 2. That names the authorization server, whose metadata is found through
//!    RFC 8414 or `OpenID` Connect discovery, its `issuer` checked.
//! 3. The client is a pre-registered one from the settings, one registered
//!    before with this issuer, or one registered now (RFC 7591, as a
//!    `native` application).
//! 4. The browser opens the authorization URL (S256 challenge, `state`,
//!    `resource` per RFC 8707); the redirect comes back to a one-off
//!    listener on `127.0.0.1` (RFC 8252); `iss` is checked (RFC 9207).
//! 5. The code is exchanged for tokens, which are refreshed later.
//!
//! Client ID Metadata Documents need an HTTPS URL hosting this app's
//! client metadata; until there is one, dynamic registration is used.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::config::{OAuthClient, Registration, Tokens};
use super::transport::agent;
use crate::sha256;

/// How long the user has to finish signing in.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Largest metadata or token response read.
const MAX_JSON: u64 = 1 << 20;

/// A parsed `WWW-Authenticate: Bearer …` challenge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Challenge {
    /// Where the Protected Resource Metadata is.
    pub resource_metadata: Option<String>,
    /// Scopes the server asks for.
    pub scope: Option<String>,
    /// `invalid_token`, `insufficient_scope`, …
    pub error: Option<String>,
}

/// Parses the parameters of a `Bearer` challenge (RFC 6750 §3). Other
/// schemes in the same header are skipped.
#[must_use]
pub fn parse_challenge(header: &str) -> Challenge {
    let mut challenge = Challenge::default();
    let mut rest = header.trim();
    let mut in_bearer = false;
    while !rest.is_empty() {
        // A token followed by `=` is a parameter; otherwise it starts a scheme.
        let end = rest.find(|c: char| c == '=' || c == ',' || c.is_whitespace()).unwrap_or(rest.len());
        let token = &rest[..end];
        let after = rest[end..].trim_start();
        if let Some(value_part) = after.strip_prefix('=') {
            let value_part = value_part.trim_start();
            let (value, remainder) = if let Some(quoted) = value_part.strip_prefix('"') {
                let mut value = String::new();
                let mut chars = quoted.char_indices();
                let mut close = quoted.len();
                while let Some((i, c)) = chars.next() {
                    match c {
                        '\\' => {
                            if let Some((_, escaped)) = chars.next() {
                                value.push(escaped);
                            }
                        }
                        '"' => {
                            close = i + 1;
                            break;
                        }
                        c => value.push(c),
                    }
                }
                (value, &quoted[close.min(quoted.len())..])
            } else {
                let end = value_part.find([',', ' ']).unwrap_or(value_part.len());
                (value_part[..end].to_owned(), &value_part[end..])
            };
            if in_bearer {
                match token.to_ascii_lowercase().as_str() {
                    "resource_metadata" => challenge.resource_metadata = Some(value),
                    "scope" => challenge.scope = Some(value),
                    "error" => challenge.error = Some(value),
                    _ => {}
                }
            }
            rest = remainder.trim_start().trim_start_matches(',').trim_start();
        } else {
            if !token.is_empty() {
                in_bearer = token.eq_ignore_ascii_case("bearer");
            }
            rest = after.trim_start_matches(',').trim_start();
            if token.is_empty() && !rest.is_empty() {
                // Stray punctuation; skip a character.
                rest = &rest[rest.chars().next().map_or(1, char::len_utf8)..];
            }
        }
    }
    challenge
}

/// What a sign-in needs to know.
pub struct Request<'a> {
    /// The MCP server's URL.
    pub url: &'a str,
    /// Pre-registered client settings.
    pub client: &'a OAuthClient,
    /// The server's latest challenge.
    pub challenge: &'a Challenge,
    /// Tokens from an earlier sign-in, whose scopes are kept.
    pub previous: Option<&'a Tokens>,
    /// Clients registered before, by issuer.
    pub registered: &'a BTreeMap<String, Registration>,
}

/// What a sign-in produced: the tokens, and a client newly registered with
/// an issuer (to be remembered).
pub struct Outcome {
    /// The new tokens.
    pub tokens: Tokens,
    /// `(issuer, client)` when a client was registered.
    pub registered: Option<(String, Registration)>,
}

/// Signs in through the browser, which this opens. Blocks until the user
/// finishes, gives up, or `cancel` is raised.
///
/// # Errors
/// A message for the user: discovery, registration, the user's refusal, a
/// mismatched issuer or state, or a failed token exchange.
pub fn sign_in(request: &Request<'_>, cancel: &AtomicBool) -> Result<Outcome, String> {
    let (resource, scopes_supported, issuer) = discover_resource(request.url, request.challenge)?;
    let metadata = discover_issuer(&issuer)?;
    // From here on, the issuer is the one the validated metadata names (it
    // may differ by a trailing slash): the `iss` of the answer must match it.
    let issuer = metadata.get("issuer").and_then(Value::as_str).unwrap_or(&issuer).to_owned();
    let field = |key: &str| metadata.get(key).and_then(Value::as_str).map(str::to_owned);
    let authorization_endpoint = field("authorization_endpoint").ok_or("The authorization server names no authorization endpoint.")?;
    let token_endpoint = field("token_endpoint").ok_or("The authorization server names no token endpoint.")?;
    for endpoint in [&authorization_endpoint, &token_endpoint] {
        if !secure(endpoint) {
            return Err(format!("The authorization server's endpoint {endpoint} is not HTTPS."));
        }
    }
    let methods = metadata.get("code_challenge_methods_supported").and_then(Value::as_array);
    if methods.is_some_and(|m| !m.iter().any(|v| v == "S256")) {
        return Err("The authorization server does not support PKCE with S256, which MCP requires.".into());
    }
    let strings = |key: &str| metadata.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>());

    // The redirect listener comes first: a registration names its port.
    let configured_port = request.client.callback_port;
    let stored = request.registered.get(&issuer).filter(|_| request.client.client_id.is_none());
    let stored_port = stored.and_then(|r| port_of(&r.redirect_uri));
    let listener = configured_port
        .or(stored_port)
        .and_then(|port| TcpListener::bind(("127.0.0.1", port)).ok())
        .map_or_else(|| TcpListener::bind(("127.0.0.1", 0)), Ok)
        .map_err(|e| format!("The sign-in redirect could not be received: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let mut registered = None;
    let client = if let Some(client_id) = &request.client.client_id {
        let auth_methods = strings("token_endpoint_auth_methods_supported").unwrap_or_default();
        let auth_method = match &request.client.client_secret {
            None => "none",
            Some(_) if auth_methods.iter().any(|m| m == "client_secret_post") && !auth_methods.iter().any(|m| m == "client_secret_basic") => "client_secret_post",
            Some(_) => "client_secret_basic",
        };
        Registration { client_id: client_id.clone(), client_secret: request.client.client_secret.clone(), auth_method: auth_method.into(), redirect_uri: redirect_uri.clone() }
    } else if let Some(client) = stored.filter(|r| r.redirect_uri == redirect_uri) {
        client.clone()
    } else {
        let endpoint = field("registration_endpoint").ok_or(
            "This server's sign-in needs a registered OAuth client. Register one with its provider, then add \"oauth\": { \"clientId\": \"…\" } to the server in mcp.json.",
        )?;
        let client = register(&endpoint, &redirect_uri, request.client.scope.as_deref())?;
        registered = Some((issuer.clone(), client.clone()));
        client
    };

    // Scopes: the challenge's, else the resource's; earlier grants kept.
    let mut scopes: Vec<String> = Vec::new();
    let wanted = request.client.scope.clone().or_else(|| request.challenge.scope.clone()).or_else(|| scopes_supported.map(|s| s.join(" ")));
    for scope in wanted.iter().chain(request.previous.and_then(|t| t.scope.as_ref())).flat_map(|s| s.split_whitespace()) {
        if !scopes.iter().any(|s| s == scope) {
            scopes.push(scope.to_owned());
        }
    }
    if !scopes.is_empty() && strings("scopes_supported").is_some_and(|s| s.iter().any(|x| x == "offline_access")) && !scopes.iter().any(|s| s == "offline_access") {
        scopes.push("offline_access".into());
    }
    let scope = (!scopes.is_empty()).then(|| scopes.join(" "));

    let verifier = random_string(64);
    let state = random_string(32);
    let challenge = serechat::base64(&sha256::digest(verifier.as_bytes()), true);
    let mut url = format!(
        "{authorization_endpoint}{}response_type=code&client_id={}&redirect_uri={}&code_challenge={challenge}&code_challenge_method=S256&state={state}&resource={}",
        if authorization_endpoint.contains('?') { '&' } else { '?' },
        encode(&client.client_id),
        encode(&redirect_uri),
        encode(&resource),
    );
    if let Some(scope) = &scope {
        url.push_str("&scope=");
        url.push_str(&encode(scope));
    }
    url.push_str(provider_params(&issuer));
    open_browser(&url).map_err(|e| format!("The browser could not be opened: {e}"))?;

    let params = receive_redirect(&listener, &state, cancel)?;
    // RFC 9207: a present `iss` must be the issuer; an advertised one must be present.
    let iss_supported = metadata.get("authorization_response_iss_parameter_supported").and_then(Value::as_bool) == Some(true);
    match params.get("iss") {
        Some(iss) if *iss != issuer => return Err("The sign-in answer came from a different authorization server; it was ignored.".into()),
        None if iss_supported => return Err("The sign-in answer did not say which authorization server sent it; it was ignored.".into()),
        _ => {}
    }
    if let Some(error) = params.get("error") {
        let detail = params.get("error_description").map_or(String::new(), |d| format!(": {d}"));
        return Err(if error == "access_denied" { "Sign-in was declined.".into() } else { format!("Sign-in failed ({error}){detail}") });
    }
    let code = params.get("code").ok_or("The sign-in answer had no authorization code.")?;
    let form = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code.clone()),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
        ("resource", resource.clone()),
    ];
    let reply = token_request(&token_endpoint, &client, form).map_err(|e| e.message())?;
    let tokens = Tokens {
        url: request.url.to_owned(),
        issuer,
        token_endpoint,
        resource,
        client,
        access_token: String::new(),
        refresh_token: None,
        expires_at: None,
        scope,
    };
    Ok(Outcome { tokens: apply(tokens, &reply)?, registered })
}

/// Authorization parameters a provider needs beyond the standard ones.
/// Google issues refresh tokens only for `access_type=offline` (it ignores
/// the `offline_access` scope), and only on a consent screen.
fn provider_params(issuer: &str) -> &'static str {
    if issuer.trim_end_matches('/') == "https://accounts.google.com" { "&access_type=offline&prompt=consent" } else { "" }
}

/// Opens the authorization page in the user's browser.
#[cfg(not(test))]
fn open_browser(url: &str) -> std::io::Result<()> {
    crate::platform::open_url(url)
}

/// Tests play the browser: the URL goes to them instead.
#[cfg(test)]
#[allow(clippy::unnecessary_wraps, reason = "matches the real opener's signature")]
fn open_browser(url: &str) -> std::io::Result<()> {
    tests::OPENED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(url.to_owned());
    Ok(())
}

/// Why a refresh failed.
#[derive(Debug)]
pub enum RefreshError {
    /// The grant is gone: the user must sign in again.
    Rejected(String),
    /// The authorization server could not be reached; try later.
    Unreachable(String),
}

impl RefreshError {
    fn message(&self) -> String {
        match self {
            Self::Rejected(m) | Self::Unreachable(m) => m.clone(),
        }
    }
}

/// Uses `tokens`' refresh token for a new access token.
///
/// # Errors
/// See [`RefreshError`]; no refresh token counts as rejected.
pub fn refresh(tokens: &Tokens) -> Result<Tokens, RefreshError> {
    let Some(refresh_token) = &tokens.refresh_token else { return Err(RefreshError::Rejected("No refresh token.".into())) };
    let mut form = vec![("grant_type", "refresh_token".to_owned()), ("refresh_token", refresh_token.clone())];
    if !tokens.resource.is_empty() {
        form.push(("resource", tokens.resource.clone()));
    }
    let reply = token_request(&tokens.token_endpoint, &tokens.client, form)?;
    apply(tokens.clone(), &reply).map_err(RefreshError::Rejected)
}

/// `tokens` updated from a token endpoint's reply.
fn apply(mut tokens: Tokens, reply: &Value) -> Result<Tokens, String> {
    let access = reply.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or("The token endpoint sent no access token.")?;
    if reply.get("token_type").and_then(Value::as_str).is_some_and(|t| !t.eq_ignore_ascii_case("bearer")) {
        return Err("The token endpoint sent a token of a type MCP does not use.".into());
    }
    access.clone_into(&mut tokens.access_token);
    // A server that rotates refresh tokens sends a new one; otherwise keep ours.
    if let Some(refresh) = reply.get("refresh_token").and_then(Value::as_str) {
        tokens.refresh_token = Some(refresh.to_owned());
    }
    tokens.expires_at = reply.get("expires_in").and_then(Value::as_u64).map(|secs| serechat::unix_now() + secs);
    if let Some(scope) = reply.get("scope").and_then(Value::as_str) {
        tokens.scope = Some(scope.to_owned());
    }
    Ok(tokens)
}

/// POSTs a token request, authenticating the client as it registered.
fn token_request(endpoint: &str, client: &Registration, mut form: Vec<(&str, String)>) -> Result<Value, RefreshError> {
    let mut request = agent().post(endpoint).header("Accept", "application/json");
    match (client.auth_method.as_str(), &client.client_secret) {
        ("client_secret_basic", Some(secret)) => {
            let pair = format!("{}:{}", encode(&client.client_id), encode(secret));
            request = request.header("Authorization", format!("Basic {}", serechat::base64(pair.as_bytes(), false)));
        }
        (_, Some(secret)) => {
            form.push(("client_id", client.client_id.clone()));
            form.push(("client_secret", secret.clone()));
        }
        (_, None) => form.push(("client_id", client.client_id.clone())),
    }
    let response = request.send_form(form.iter().map(|(k, v)| (*k, v.as_str()))).map_err(|e| RefreshError::Unreachable(format!("The authorization server could not be reached: {e}")))?;
    let status = response.status().as_u16();
    let mut text = String::new();
    let _ = response.into_body().into_with_config().limit(MAX_JSON).reader().read_to_string(&mut text);
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if (200..300).contains(&status) && value.is_object() {
        return Ok(value);
    }
    let error = value.get("error").and_then(Value::as_str).unwrap_or_default();
    let detail = value.get("error_description").and_then(Value::as_str).map_or(String::new(), |d| format!(": {d}"));
    let message = format!("The authorization server refused the token request (HTTP {status} {error}){detail}");
    if status >= 500 || status == 429 { Err(RefreshError::Unreachable(message)) } else { Err(RefreshError::Rejected(message)) }
}

/// Registers this app as a public native client (RFC 7591).
fn register(endpoint: &str, redirect_uri: &str, scope: Option<&str>) -> Result<Registration, String> {
    if !secure(endpoint) {
        return Err(format!("The registration endpoint {endpoint} is not HTTPS."));
    }
    let mut body = json!({
        "client_name": "SereChat Desktop",
        "client_uri": "https://serechat.com",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    });
    if let Some(scope) = scope {
        body["scope"] = scope.into();
    }
    let response = agent()
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body.to_string())
        .map_err(|e| format!("The authorization server could not be reached: {e}"))?;
    let status = response.status().as_u16();
    let mut text = String::new();
    let _ = response.into_body().into_with_config().limit(MAX_JSON).reader().read_to_string(&mut text);
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let Some(client_id) = value.get("client_id").and_then(Value::as_str).filter(|_| (200..300).contains(&status)) else {
        let detail = value.get("error_description").or_else(|| value.get("error")).and_then(Value::as_str).unwrap_or("no reason given");
        return Err(format!("This app could not register with the server's sign-in (HTTP {status}: {detail})."));
    };
    let secret = value.get("client_secret").and_then(Value::as_str).map(str::to_owned);
    let method = value.get("token_endpoint_auth_method").and_then(Value::as_str).unwrap_or(if secret.is_some() { "client_secret_basic" } else { "none" });
    Ok(Registration { client_id: client_id.to_owned(), client_secret: secret, auth_method: method.to_owned(), redirect_uri: redirect_uri.to_owned() })
}

/// Finds the server's Protected Resource Metadata: the `resource` to ask
/// for, its scopes, and the authorization server. A server without the
/// metadata (an older revision) is its own authorization server.
fn discover_resource(server: &str, challenge: &Challenge) -> Result<(String, Option<Vec<String>>, String), String> {
    let (origin, path) = split_url(server).ok_or("The server URL is not valid.")?;
    let mut candidates = Vec::new();
    if let Some(url) = &challenge.resource_metadata {
        candidates.push(url.clone());
    } else {
        if !path.is_empty() && path != "/" {
            candidates.push(format!("{origin}/.well-known/oauth-protected-resource{}", path.trim_end_matches('/')));
        }
        candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));
    }
    for url in candidates {
        let Ok(metadata) = fetch_json(&url) else { continue };
        let resource = metadata.get("resource").and_then(Value::as_str).map_or_else(|| canonical(server), str::to_owned);
        if !resource_allowed(server, &resource) {
            return Err(format!("The server's metadata is for {resource}, not {server}; refusing to sign in."));
        }
        let issuer = metadata
            .get("authorization_servers")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().find_map(Value::as_str))
            .ok_or("The server's metadata names no authorization server.")?;
        let scopes = metadata.get("scopes_supported").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect());
        return Ok((resource, scopes, issuer.to_owned()));
    }
    Ok((canonical(server), None, origin))
}

/// Whether `server` publishes Protected Resource Metadata naming an
/// authorization server: it takes sign-ins, even if it lets some requests
/// (listing tools, say) through without one.
#[must_use]
pub fn protected(server: &str) -> bool {
    let Some((origin, path)) = split_url(server) else { return false };
    let mut candidates = vec![format!("{origin}/.well-known/oauth-protected-resource")];
    if !path.is_empty() && path != "/" {
        candidates.insert(0, format!("{origin}/.well-known/oauth-protected-resource{}", path.trim_end_matches('/')));
    }
    candidates.iter().any(|url| fetch_json(url).is_ok_and(|m| m.get("authorization_servers").and_then(Value::as_array).is_some_and(|a| !a.is_empty())))
}

/// Fetches the authorization server's metadata from the URLs the spec
/// lists, in order, and checks its `issuer`.
fn discover_issuer(issuer: &str) -> Result<Value, String> {
    if !secure(issuer) {
        return Err(format!("The authorization server {issuer} is not HTTPS."));
    }
    let (origin, path) = split_url(issuer).ok_or("The authorization server's address is not valid.")?;
    let path = path.trim_end_matches('/');
    let candidates = if path.is_empty() {
        vec![format!("{origin}/.well-known/oauth-authorization-server"), format!("{origin}/.well-known/openid-configuration")]
    } else {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            format!("{origin}/.well-known/openid-configuration{path}"),
            format!("{origin}{path}/.well-known/openid-configuration"),
        ]
    };
    for url in candidates {
        let Ok(metadata) = fetch_json(&url) else { continue };
        let found = metadata.get("issuer").and_then(Value::as_str).unwrap_or_default();
        // ponytail: a trailing slash is forgiven; RFC 8414 asks for an exact match,
        // but several servers list their issuer one way and serve it the other.
        if found.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(format!("The authorization server's metadata names {found} as its issuer, not {issuer}; refusing to sign in."));
        }
        return Ok(metadata);
    }
    Err(format!("The authorization server {issuer} publishes no metadata, so this app cannot sign in to it."))
}

/// GETs a JSON object.
fn fetch_json(url: &str) -> Result<Value, String> {
    let response = agent().get(url).header("Accept", "application/json").call().map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }
    let mut text = String::new();
    response.into_body().into_with_config().limit(MAX_JSON).reader().read_to_string(&mut text).map_err(|e| e.to_string())?;
    serde_json::from_str::<Value>(&text).ok().filter(Value::is_object).ok_or_else(|| "not a JSON object".to_owned())
}

/// Splits `url` into its origin (`https://host:port`) and path (without
/// query or fragment).
fn split_url(url: &str) -> Option<(String, String)> {
    let scheme_end = url.find("://")? + 3;
    let rest = &url[scheme_end..];
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if host_end == 0 {
        return None;
    }
    let origin = format!("{}{}", url[..scheme_end].to_ascii_lowercase(), rest[..host_end].to_ascii_lowercase());
    let path = rest[host_end..].split(['?', '#']).next().unwrap_or_default().to_owned();
    Some((origin, path))
}

/// The canonical form of a server URL (RFC 8707): lower-case scheme and
/// host, no fragment, no trailing slash.
fn canonical(url: &str) -> String {
    let Some((origin, path)) = split_url(url) else { return url.to_owned() };
    let query = url.split_once('?').map(|(_, q)| q.split('#').next().unwrap_or_default()).filter(|q| !q.is_empty());
    let mut out = format!("{origin}{}", path.trim_end_matches('/'));
    if let Some(query) = query {
        out.push('?');
        out.push_str(query);
    }
    out
}

/// Whether metadata for `resource` may be used for `server`: the same
/// origin, and the server's path under the resource's.
fn resource_allowed(server: &str, resource: &str) -> bool {
    match (split_url(server), split_url(resource)) {
        (Some((a, server_path)), Some((b, resource_path))) => {
            let (server_path, resource_path) = (server_path.trim_end_matches('/'), resource_path.trim_end_matches('/'));
            a == b && (server_path == resource_path || server_path.starts_with(&format!("{resource_path}/")) || resource_path.is_empty())
        }
        _ => false,
    }
}

/// HTTPS, or plain HTTP to this machine (local development servers).
fn secure(url: &str) -> bool {
    url.starts_with("https://") || ["http://localhost", "http://127.0.0.1", "http://[::1]"].iter().any(|p| url.starts_with(p))
}

/// The port of a loopback redirect URI.
fn port_of(uri: &str) -> Option<u16> {
    uri.strip_prefix("http://127.0.0.1:")?.split('/').next()?.parse().ok()
}

/// Percent-encodes everything but RFC 3986's unreserved characters.
#[must_use]
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Decodes a query component (`+` is a space).
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The query parameters of a request target like `/callback?a=1&b=2`.
fn query_params(target: &str) -> BTreeMap<String, String> {
    let query = target.split_once('?').map_or("", |(_, q)| q).split('#').next().unwrap_or_default();
    query.split('&').filter(|p| !p.is_empty()).map(|pair| pair.split_once('=').map_or((decode(pair), String::new()), |(k, v)| (decode(k), decode(v)))).collect()
}

/// Waits for the browser to come back to `/callback` with the right
/// `state`, answering it with a page saying how it went.
fn receive_redirect(listener: &TcpListener, state: &str, cancel: &AtomicBool) -> Result<BTreeMap<String, String>, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + SIGN_IN_TIMEOUT;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("Sign-in was cancelled.".into());
        }
        if Instant::now() > deadline {
            return Err("Sign-in timed out.".into());
        }
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(format!("The sign-in redirect could not be received: {e}")),
        };
        let Some(target) = read_target(&mut stream) else { continue };
        if !target.starts_with("/callback") {
            respond(&mut stream, "404 Not Found", "Not found.");
            continue;
        }
        let params = query_params(&target);
        // An answer for another attempt (an old tab): keep waiting for ours.
        if params.get("state").map(String::as_str) != Some(state) {
            respond(&mut stream, "400 Bad Request", "This sign-in link is out of date. Start again from SereChat.");
            continue;
        }
        let ok = params.contains_key("code") && !params.contains_key("error");
        let message = if ok { "You're signed in. You can close this tab and go back to SereChat." } else { "Sign-in did not complete. You can close this tab and go back to SereChat." };
        respond(&mut stream, "200 OK", message);
        return Ok(params);
    }
}

/// Reads an HTTP request's target from the browser (`GET <target> HTTP/1.1`).
fn read_target(stream: &mut TcpStream) -> Option<String> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 << 10 {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf);
    let mut parts = text.lines().next()?.split_whitespace();
    (parts.next()? == "GET").then(|| parts.next().map(str::to_owned))?
}

/// Answers the browser with a small page.
fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>SereChat</title>\
         <style>body{{font:16px system-ui,sans-serif;display:grid;place-items:center;height:90vh;margin:0;background:#161618;color:#ededef}}\
         @media (prefers-color-scheme:light){{body{{background:#fff;color:#18181b}}}}</style></head><body><p>{message}</p></body></html>"
    );
    let reply = format!("HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}", body.len());
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.flush();
}

/// A random string of `len` characters from the PKCE alphabet.
///
/// The randomness comes from std's `RandomState`, whose `SipHash` keys are
/// seeded from the operating system's secure generator; hashing a counter
/// under those keys gives unpredictable output without another dependency.
fn random_string(len: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut out = String::with_capacity(len);
    let mut counter = 0u64;
    while out.len() < len {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(counter);
        hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        counter += 1;
        // 64 bits give ten characters without bias worth noticing (66^10 < 2^64).
        let mut bits = hasher.finish();
        for _ in 0..10 {
            if out.len() == len {
                break;
            }
            out.push(char::from(ALPHABET[(bits % ALPHABET.len() as u64) as usize]));
            bits /= ALPHABET.len() as u64;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges() {
        let header = r#"Bearer resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", scope="files:read files:write""#;
        let c = parse_challenge(header);
        assert_eq!(c.resource_metadata.as_deref(), Some("https://mcp.example.com/.well-known/oauth-protected-resource"));
        assert_eq!(c.scope.as_deref(), Some("files:read files:write"));
        let c = parse_challenge(r#"Basic realm="x", Bearer error="insufficient_scope", scope=admin, error_description="needs \"admin\"""#);
        assert_eq!((c.error.as_deref(), c.scope.as_deref()), (Some("insufficient_scope"), Some("admin")));
        assert_eq!(parse_challenge(r#"Basic realm="x""#), Challenge::default());
        assert_eq!(parse_challenge(r#"Bearer scope="unterminated"#).scope.as_deref(), Some("unterminated"));
        for junk in ["", "=", ",,,", "Bearer =", "Bearer a=\"", "é=ü"] {
            let _ = parse_challenge(junk);
        }
    }

    #[test]
    fn urls() {
        assert_eq!(split_url("HTTPS://Mcp.Example.com:8443/Path/mcp?x=1#f"), Some(("https://mcp.example.com:8443".into(), "/Path/mcp".into())));
        assert_eq!(split_url("https:///x"), None);
        assert_eq!(canonical("https://Mcp.Example.com/mcp/"), "https://mcp.example.com/mcp");
        assert_eq!(canonical("https://a.dev"), "https://a.dev");
        assert!(resource_allowed("https://a.dev/mcp", "https://a.dev"));
        assert!(resource_allowed("https://a.dev/mcp/", "https://a.dev/mcp"));
        assert!(resource_allowed("https://a.dev/team/mcp", "https://a.dev/team"));
        assert!(!resource_allowed("https://a.dev/mcp", "https://b.dev/mcp"));
        assert!(!resource_allowed("https://a.dev/mcpx", "https://a.dev/mcp"));
        assert!(secure("https://x") && secure("http://localhost:3000") && !secure("http://evil.dev"));
        assert_eq!(port_of("http://127.0.0.1:53682/callback"), Some(53682));
        assert_eq!(provider_params("https://accounts.google.com"), "&access_type=offline&prompt=consent");
        assert_eq!(provider_params("https://mcp.linear.app"), "");
        assert_eq!(encode("a b/é"), "a%20b%2F%C3%A9");
        assert_eq!(decode("a+b%2F%C3%A9%zz%"), "a b/é%zz%");
        let params = query_params("/callback?code=abc%3D&state=s&iss=https%3A%2F%2Fauth.dev");
        assert_eq!((params["code"].as_str(), params["iss"].as_str()), ("abc=", "https://auth.dev"));
    }

    #[test]
    fn verifiers_are_random_and_valid() {
        let (a, b) = (random_string(64), random_string(64));
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c)));
    }

    use super::super::fake::{self, Reply};

    /// Authorization URLs the code under test "opened in the browser".
    pub(super) static OPENED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    /// Discovery against real servers, read-only (no registration):
    /// `cargo test -p serechat-desktop live_discovery -- --ignored --nocapture`.
    #[test]
    #[ignore = "reaches public MCP servers"]
    fn live_discovery() {
        use super::super::config::{ServerConfig, Transport};
        use super::super::transport::{Connection, Error, OnNotify};
        for url in ["https://mcp.linear.app/mcp", "https://mcp.notion.com/mcp", "https://mcp.sentry.dev/mcp"] {
            let config = ServerConfig::new("x".into(), Transport::Http { url: url.into(), headers: Vec::new(), sse: false });
            let notify: OnNotify = std::sync::Arc::new(|_| {});
            let challenge = match Connection::open(&config, None, &notify, &AtomicBool::new(false)) {
                Err(Error::Auth { challenge, .. }) => parse_challenge(&challenge),
                Err(e) => panic!("{url}: {e}"),
                Ok(_) => panic!("{url} answered without a token"),
            };
            let (resource, scopes, issuer) = discover_resource(url, &challenge).unwrap();
            let metadata = discover_issuer(&issuer).unwrap();
            println!("{url}: challenge {challenge:?}\n  resource {resource}, scopes {scopes:?}, issuer {issuer}");
            println!("  registration: {}, S256: {}", metadata.get("registration_endpoint").is_some(), metadata["code_challenge_methods_supported"]);
        }
    }

    /// Google Calendar's MCP server, unsigned and read-only: it speaks the
    /// stateless protocol, lists tools to anyone, and wants a sign-in to run
    /// them. `cargo test -p serechat-desktop live_google -- --ignored`.
    #[test]
    #[ignore = "reaches Google"]
    fn live_google_calendar() {
        use super::super::config::{ServerConfig, Transport};
        use super::super::transport::{Connection, Error, MODERN, OnNotify, list_tools};
        let url = "https://calendarmcp.googleapis.com/mcp/v1";
        let config = ServerConfig::new("x".into(), Transport::Http { url: url.into(), headers: Vec::new(), sse: false });
        let notify: OnNotify = std::sync::Arc::new(|_| {});
        let cancel = AtomicBool::new(false);
        let (connection, hello) = Connection::open(&config, None, &notify, &cancel).unwrap();
        assert_eq!(hello.version, MODERN);
        assert!(list_tools(&connection, &cancel).unwrap().iter().any(|t| t["name"] == "list_events"));
        assert!(protected(url), "it offers a sign-in although it let us in");
        let call = connection.request("tools/call", json!({ "name": "list_calendars", "arguments": {} }), &[], std::time::Duration::from_secs(30), &cancel);
        let Err(Error::Auth { status: 401, challenge }) = call else { panic!("an unsigned call must be refused: {call:?}") };
        let (resource, scopes, issuer) = discover_resource(url, &parse_challenge(&challenge)).unwrap();
        assert!(resource_allowed(url, &resource) && scopes.is_some_and(|s| !s.is_empty()));
        let metadata = discover_issuer(&issuer).unwrap();
        assert_eq!(metadata["issuer"], "https://accounts.google.com");
        assert_eq!(provider_params(metadata["issuer"].as_str().unwrap()), "&access_type=offline&prompt=consent");
    }

    #[test]
    fn signing_in_end_to_end() {
        let verifier: std::sync::Arc<std::sync::Mutex<Option<String>>> = std::sync::Arc::default();
        let seen = std::sync::Arc::clone(&verifier);
        let base = std::sync::Arc::new(std::sync::OnceLock::<String>::new());
        let known = std::sync::Arc::clone(&base);
        let url = fake::serve(move |r| -> Reply {
            let base = known.get().cloned().unwrap_or_default();
            let ok = |v: Value| (200, vec![], v.to_string());
            match r.path.as_str() {
                // Like Google, the resource names its issuer with a trailing slash the metadata lacks.
                "/.well-known/oauth-protected-resource" => ok(json!({ "resource": base, "authorization_servers": [format!("{base}/")] })),
                "/.well-known/oauth-authorization-server" => ok(json!({
                    "issuer": base, "authorization_endpoint": format!("{base}/authorize"), "token_endpoint": format!("{base}/token"),
                    "registration_endpoint": format!("{base}/register"), "authorization_response_iss_parameter_supported": true,
                })),
                "/register" => ok(json!({ "client_id": "c" })),
                "/token" => {
                    let form = query_params(&format!("?{}", r.body));
                    *seen.lock().unwrap() = form.get("code_verifier").cloned();
                    assert_eq!(form.get("code").map(String::as_str), Some("the-code"));
                    ok(json!({ "access_token": "token", "token_type": "Bearer" }))
                }
                _ => (404, vec![], String::new()),
            }
        });
        base.set(url.clone()).unwrap();
        // The "browser": finds the authorization URL and comes back with a code.
        let issuer = url.clone();
        let browser = std::thread::spawn(move || {
            let opened = loop {
                if let Some(url) = OPENED.lock().unwrap().iter().find(|u| u.starts_with(&issuer)).cloned() {
                    break url;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            let params = query_params(&opened);
            let target = format!("/callback?code=the-code&state={}&iss={}", encode(&params["state"]), encode(&issuer));
            let port = port_of(&params["redirect_uri"]).unwrap();
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.write_all(format!("GET {target} HTTP/1.1\r\n\r\n").as_bytes()).unwrap();
            let mut page = String::new();
            let _ = stream.read_to_string(&mut page);
            params
        });
        let challenge = Challenge::default();
        let request = Request { url: &format!("{url}/mcp"), client: &OAuthClient::default(), challenge: &challenge, previous: None, registered: &BTreeMap::new() };
        let outcome = sign_in(&request, &AtomicBool::new(false)).unwrap();
        let params = browser.join().unwrap();
        assert_eq!(outcome.tokens.access_token, "token");
        assert_eq!(outcome.registered.as_ref().map(|(issuer, client)| (issuer.as_str(), client.client_id.as_str())), Some((url.as_str(), "c")));
        assert_eq!(params["code_challenge_method"], "S256");
        // The resource the server's metadata names (here its origin), per RFC 9728.
        assert_eq!(params["resource"], url);
        // PKCE: the verifier sent to the token endpoint hashes to the challenge.
        let verifier = verifier.lock().unwrap().clone().unwrap();
        assert_eq!(serechat::base64(&sha256::digest(verifier.as_bytes()), true), params["code_challenge"]);
    }

    /// A resource server and authorization server in one, at `base`.
    fn authorization_server() -> String {
        let base = std::sync::Arc::new(std::sync::OnceLock::<String>::new());
        let known = std::sync::Arc::clone(&base);
        let url = fake::serve(move |r| -> Reply {
            let base = known.get().cloned().unwrap_or_default();
            let ok = |v: Value| (200, vec![], v.to_string());
            match (r.method.as_str(), r.path.as_str()) {
                // The path-specific metadata URI is tried first.
                ("GET", "/.well-known/oauth-protected-resource/team/mcp") => ok(json!({ "resource": format!("{base}/team/mcp"), "authorization_servers": [format!("{base}/auth")], "scopes_supported": ["read", "write"] })),
                ("GET", "/.well-known/oauth-authorization-server/auth") => ok(json!({
                    "issuer": format!("{base}/auth"),
                    "authorization_endpoint": format!("{base}/auth/authorize"),
                    "token_endpoint": format!("{base}/auth/token"),
                    "registration_endpoint": format!("{base}/auth/register"),
                    "code_challenge_methods_supported": ["S256"],
                    "scopes_supported": ["read", "write", "offline_access"],
                })),
                ("POST", "/auth/register") => {
                    let body = r.json();
                    assert_eq!(body["application_type"], "native");
                    assert_eq!(body["token_endpoint_auth_method"], "none");
                    assert!(body["redirect_uris"][0].as_str().is_some_and(|u| u.starts_with("http://127.0.0.1:")));
                    ok(json!({ "client_id": "client-1" }))
                }
                ("POST", "/auth/token") => {
                    let form = query_params(&format!("?{}", r.body));
                    assert_eq!(form.get("client_id").map(String::as_str), Some("client-1"));
                    assert_eq!(form.get("resource").map(String::as_str), Some(format!("{base}/team/mcp").as_str()));
                    match form.get("grant_type").map(String::as_str) {
                        Some("refresh_token") if form.get("refresh_token").map(String::as_str) == Some("r1") => {
                            ok(json!({ "access_token": "t2", "token_type": "Bearer", "expires_in": 60 }))
                        }
                        Some("refresh_token") => (400, vec![], json!({ "error": "invalid_grant" }).to_string()),
                        _ => ok(json!({ "access_token": "t1", "token_type": "Bearer", "refresh_token": "r1", "expires_in": 3600 })),
                    }
                }
                _ => (404, vec![], String::new()),
            }
        });
        base.set(url.clone()).unwrap();
        url
    }

    #[test]
    fn discovery_registration_and_tokens() {
        let base = authorization_server();
        let server = format!("{base}/team/mcp");
        let (resource, scopes, issuer) = discover_resource(&server, &Challenge::default()).unwrap();
        assert_eq!((resource.as_str(), issuer.as_str()), (server.as_str(), format!("{base}/auth").as_str()));
        assert_eq!(scopes, Some(vec!["read".to_owned(), "write".to_owned()]));
        let metadata = discover_issuer(&issuer).unwrap();
        assert!(metadata["token_endpoint"].as_str().unwrap().ends_with("/auth/token"));
        assert!(discover_issuer(&format!("{base}/elsewhere")).is_err(), "no metadata, no sign-in");

        let client = register(&format!("{base}/auth/register"), "http://127.0.0.1:9/callback", None).unwrap();
        assert_eq!((client.client_id.as_str(), client.auth_method.as_str()), ("client-1", "none"));
        let form = vec![("grant_type", "authorization_code".to_owned()), ("code", "c".to_owned()), ("resource", resource.clone())];
        let reply = token_request(&format!("{base}/auth/token"), &client, form).map_err(|e| e.message()).unwrap();
        let tokens = apply(Tokens { token_endpoint: format!("{base}/auth/token"), resource, client, ..Tokens::default() }, &reply).unwrap();
        assert_eq!((tokens.access_token.as_str(), tokens.refresh_token.as_deref()), ("t1", Some("r1")));
        let refreshed = refresh(&tokens).unwrap();
        assert_eq!((refreshed.access_token.as_str(), refreshed.refresh_token.as_deref()), ("t2", Some("r1")));
        let stale = Tokens { refresh_token: Some("gone".into()), ..tokens };
        assert!(matches!(refresh(&stale), Err(RefreshError::Rejected(_))), "a refused grant means signing in again");
    }

    #[test]
    fn metadata_for_another_resource_is_refused() {
        let other = fake::serve(|_| -> Reply { (200, vec![], json!({ "resource": "https://evil.example/mcp", "authorization_servers": ["https://evil.example"] }).to_string()) });
        assert!(discover_resource(&format!("{other}/mcp"), &Challenge::default()).unwrap_err().contains("refusing"));
    }

    #[test]
    fn the_redirect_is_matched_by_state() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = std::thread::spawn(move || {
            let mut pages = Vec::new();
            for target in ["/favicon.ico", "/callback?code=x&state=old", "/callback?code=abc&state=s1&iss=https%3A%2F%2Fauth"] {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
                stream.write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes()).unwrap();
                let mut page = String::new();
                let _ = stream.read_to_string(&mut page);
                pages.push(page);
            }
            pages
        });
        let params = receive_redirect(&listener, "s1", &AtomicBool::new(false)).unwrap();
        assert_eq!((params["code"].as_str(), params["iss"].as_str()), ("abc", "https://auth"));
        let pages = browser.join().unwrap();
        assert!(pages[0].starts_with("HTTP/1.1 404") && pages[1].contains("out of date") && pages[2].contains("signed in"));
        let quiet = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        assert!(receive_redirect(&quiet, "s", &AtomicBool::new(true)).unwrap_err().contains("cancelled"));
    }

    #[test]
    fn token_replies() {
        let tokens = Tokens { refresh_token: Some("old".into()), ..Tokens::default() };
        let updated = apply(tokens.clone(), &json!({ "access_token": "a", "token_type": "bearer", "expires_in": 60 })).unwrap();
        assert_eq!((updated.access_token.as_str(), updated.refresh_token.as_deref()), ("a", Some("old")), "no new refresh token keeps the old one");
        assert!(updated.expires_at.is_some_and(|t| t > serechat::unix_now()));
        assert!(apply(tokens.clone(), &json!({ "access_token": "a", "token_type": "mac" })).is_err());
        assert!(apply(tokens, &json!({})).is_err());
    }
}
