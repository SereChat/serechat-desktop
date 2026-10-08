//! OAuth 2.1 tokens for the API: trading an authorization code for them,
//! keeping them fresh, and revoking them on sign-out.
//!
//! SereChat Desktop is a pre-registered public client (no registration, no
//! secret). The app runs the browser part (authorization code with PKCE and
//! a loopback redirect, RFC 8252); this module does the rest.
//!
//! Access tokens last an hour and refresh tokens rotate on every use, so
//! every clone of a signed-in [`Client`] shares one [`Auth`], whose lock lets
//! only one refresh run at a time.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::client::{Client, read_json};
use crate::error::{Error, Result};

/// SereChat Desktop's OAuth client id.
pub const CLIENT_ID: &str = "serechat-desktop";
/// What the app asks to be allowed: language models, media generation, the
/// file library its media inputs and outputs go through, and reading the
/// account's balances.
pub const SCOPES: &str = "chat media files account";
/// The resource tokens are issued for: the REST API.
pub const RESOURCE: &str = "https://serechat.com/v1";
/// How long before it expires an access token is refreshed.
const MARGIN: Duration = Duration::from_secs(60);
/// The longest access token lifetime believed, so a hostile `expires_in`
/// cannot overflow the clock.
const MAX_LIFETIME: u64 = 24 * 60 * 60;

/// A grant's tokens. Not `Debug`, so they never end up in logs.
#[derive(Clone)]
pub struct Tokens {
    /// Bearer token for API requests.
    pub access_token: String,
    /// Traded for new tokens; it changes on every refresh.
    pub refresh_token: String,
    /// When the access token stops working.
    pub expires_at: Instant,
}

impl Tokens {
    /// Tokens known only by a stored refresh token: the first request
    /// refreshes them.
    #[must_use]
    pub fn stored(refresh_token: String) -> Self {
        Self { access_token: String::new(), refresh_token, expires_at: Instant::now() }
    }
}

/// News about a grant that the app must act on.
pub enum GrantEvent {
    /// The tokens were refreshed: store this new refresh token, the old one
    /// no longer works.
    Rotated(String),
    /// The grant was revoked or has expired: sign in again.
    Expired,
    /// The grant lacks a scope the app needs: sign in again to allow it.
    MissingScope,
}

/// The token endpoint's answer.
#[derive(Deserialize)]
struct Reply {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

/// The tokens every clone of a signed-in [`Client`] shares.
pub(crate) struct Auth {
    /// `None` once the grant has ended. Held for the length of a refresh.
    tokens: Mutex<Option<Tokens>>,
    /// Reads the stored refresh token, which another copy of the app may
    /// have rotated.
    load: fn() -> Option<String>,
    notify: Box<dyn Fn(GrantEvent) + Send + Sync>,
}

impl Auth {
    fn new(tokens: Tokens, load: fn() -> Option<String>, notify: impl Fn(GrantEvent) + Send + Sync + 'static) -> Self {
        Self { tokens: Mutex::new(Some(tokens)), load, notify: Box::new(notify) }
    }

    fn lock(&self) -> MutexGuard<'_, Option<Tokens>> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// An access token good for at least another minute, refreshed through
    /// `refresh` first if need be.
    pub(crate) fn access_token(&self, refresh: &dyn Fn(&str) -> Result<Tokens>) -> Result<String> {
        let mut tokens = self.lock();
        match &*tokens {
            Some(t) if t.expires_at > Instant::now() + MARGIN => Ok(t.access_token.clone()),
            _ => self.refresh(&mut tokens, refresh),
        }
    }

    /// A new access token after `refused` was refused, unless another
    /// request has replaced it already.
    pub(crate) fn replace(&self, refused: &str, refresh: &dyn Fn(&str) -> Result<Tokens>) -> Result<String> {
        let mut tokens = self.lock();
        match &*tokens {
            Some(t) if t.access_token != refused => Ok(t.access_token.clone()),
            _ => self.refresh(&mut tokens, refresh),
        }
    }

    fn refresh(&self, tokens: &mut Option<Tokens>, refresh: &dyn Fn(&str) -> Result<Tokens>) -> Result<String> {
        let Some(current) = tokens.as_ref() else { return Err(signed_out()) };
        let mut result = refresh(&current.refresh_token);
        // Another copy of the app may have rotated the stored token since.
        if result.as_ref().is_err_and(|e| e.code() == Some("invalid_grant"))
            && let Some(stored) = (self.load)().filter(|s| *s != current.refresh_token)
        {
            result = refresh(&stored);
        }
        match result {
            Ok(fresh) => {
                // Told under the lock, so the app stores rotations in order.
                (self.notify)(GrantEvent::Rotated(fresh.refresh_token.clone()));
                let access = fresh.access_token.clone();
                *tokens = Some(fresh);
                Ok(access)
            }
            // Refused rather than unreachable: the grant is gone.
            Err(e @ Error::Api { status: 400..=428 | 430..=499, .. }) => {
                self.end(tokens, GrantEvent::Expired);
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Ends the grant, telling the app the first time only.
    fn end(&self, tokens: &mut Option<Tokens>, why: GrantEvent) {
        if tokens.take().is_some() {
            (self.notify)(why);
        }
    }

    /// The server refused even a fresh token.
    pub(crate) fn expire(&self) {
        self.end(&mut self.lock(), GrantEvent::Expired);
    }

    /// The server wants a scope the grant lacks.
    pub(crate) fn missing_scope(&self) {
        (self.notify)(GrantEvent::MissingScope);
    }
}

/// What requests get once the grant has ended.
fn signed_out() -> Error {
    Error::Api { status: 401, code: Some("signed_out".into()), message: "You are signed out. Sign in again to continue.".into() }
}

impl Client {
    /// A copy of this client that sends `tokens` and keeps them fresh.
    /// `load` reads the stored refresh token (another copy of the app may
    /// have rotated it); `notify` hears of refreshes and of the grant's end.
    #[must_use]
    pub fn signed_in(&self, tokens: Tokens, load: fn() -> Option<String>, notify: impl Fn(GrantEvent) + Send + Sync + 'static) -> Self {
        let mut client = self.clone();
        client.auth = Some(Arc::new(Auth::new(tokens, load, notify)));
        client
    }

    /// Trades the authorization code the browser brought back for tokens.
    ///
    /// # Errors
    /// Network failure, or the token endpoint's refusal (`invalid_grant`:
    /// the code expired or was used).
    pub fn exchange_code(&self, code: &str, redirect_uri: &str, verifier: &str) -> Result<Tokens> {
        self.token_request(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
            ("resource", RESOURCE),
        ])
    }

    /// Trades a refresh token for new tokens.
    pub(crate) fn refresh(&self, refresh_token: &str) -> Result<Tokens> {
        self.token_request(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("client_id", CLIENT_ID)])
    }

    fn token_request(&self, form: &[(&str, &str)]) -> Result<Tokens> {
        let reply: Reply = read_json(self.agent.post(self.url("/oauth/token")).send_form(form.iter().copied())?)?;
        // Tokens go into headers and the keychain helpers' input: token characters only.
        let valid = |t: &str| !t.is_empty() && t.len() <= 512 && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b));
        if !valid(&reply.access_token) || !valid(&reply.refresh_token) {
            return Err(Error::Response { code: None, message: "SereChat sent malformed tokens.".into() });
        }
        let lifetime = Duration::from_secs(reply.expires_in.min(MAX_LIFETIME));
        Ok(Tokens { access_token: reply.access_token, refresh_token: reply.refresh_token, expires_at: Instant::now() + lifetime })
    }

    /// Forgets the tokens and revokes the grant on the server. Blocks on the
    /// network; best effort, as a grant nobody refreshes runs out anyway.
    pub fn sign_out(&self) {
        let Some(tokens) = self.auth.as_deref().and_then(|auth| auth.lock().take()) else { return };
        let form = [("token", tokens.refresh_token.as_str()), ("client_id", CLIENT_ID)];
        if let Err(e) = self.agent.post(self.url("/oauth/revoke")).send_form(form) {
            eprintln!("serechat: could not revoke the sign-in: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refresh token "another copy of the app" stored.
    static STORED: Mutex<Option<String>> = Mutex::new(None);

    fn fresh(n: u32) -> Tokens {
        Tokens { access_token: format!("a{n}"), refresh_token: format!("r{n}"), expires_at: Instant::now() + Duration::from_secs(3600) }
    }

    fn refused() -> Error {
        Error::Api { status: 400, code: Some("invalid_grant".into()), message: String::new() }
    }

    /// An `Auth` (or client) whose news lands in the returned log.
    fn listener() -> (Arc<Mutex<Vec<String>>>, impl Fn(GrantEvent) + Send + Sync + 'static) {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&heard);
        let notify = move |event| {
            log.lock().unwrap().push(match event {
                GrantEvent::Rotated(token) => token,
                GrantEvent::Expired => "expired".into(),
                GrantEvent::MissingScope => "scope".into(),
            });
        };
        (heard, notify)
    }

    #[test]
    fn refreshes_one_at_a_time_and_ends_once() {
        let (heard, notify) = listener();
        let auth = Auth::new(Tokens::stored("r0".into()), || STORED.lock().unwrap().clone(), notify);
        // A stored token is refreshed before the first request, then reused.
        assert_eq!(auth.access_token(&|t| if t == "r0" { Ok(fresh(1)) } else { Err(refused()) }).unwrap(), "a1");
        assert_eq!(auth.access_token(&|_| panic!("the token is still fresh")).unwrap(), "a1");
        // A refused token is replaced once, however many requests saw it refused.
        assert_eq!(auth.replace("a1", &|t| if t == "r1" { Ok(fresh(2)) } else { Err(refused()) }).unwrap(), "a2");
        assert_eq!(auth.replace("a1", &|_| panic!("already replaced")).unwrap(), "a2");
        // Network trouble keeps the grant for the next request.
        assert!(auth.replace("a2", &|_| Err(Error::Io(std::io::ErrorKind::ConnectionReset.into()))).is_err());
        // Another copy of the app rotated the token: the stored one is tried.
        *STORED.lock().unwrap() = Some("r9".into());
        assert_eq!(auth.replace("a2", &|t| if t == "r9" { Ok(fresh(10)) } else { Err(refused()) }).unwrap(), "a10");
        // A refused grant ends, and the app hears of it once.
        *STORED.lock().unwrap() = Some("r10".into());
        assert!(auth.replace("a10", &|_| Err(refused())).is_err());
        assert_eq!(auth.access_token(&|_| panic!("the grant has ended")).unwrap_err().code(), Some("signed_out"));
        assert_eq!(*heard.lock().unwrap(), ["r1", "r2", "r10", "expired"]);
    }

    #[test]
    fn requests_retry_once_with_a_fresh_token() {
        let (heard, notify) = listener();
        let client = Client::new().signed_in(fresh(1), || None, notify);
        let reply = |status: u16, body: &str| ureq::http::Response::builder().status(status).body(ureq::Body::builder().data(body.to_owned())).map_err(ureq::Error::from);
        // A refused token is refreshed and the request sent again with the new one.
        let tokens_sent = Mutex::new(Vec::new());
        let send = |token: Option<&str>| {
            tokens_sent.lock().unwrap().push(token.map(str::to_owned));
            reply(if token == Some("a1") { 401 } else { 200 }, "{}")
        };
        assert!(client.call_with(true, send, &|t| if t == "r1" { Ok(fresh(2)) } else { Err(refused()) }).is_ok());
        assert_eq!(*tokens_sent.lock().unwrap(), [Some("a1".to_owned()), Some("a2".to_owned())]);
        // A missing scope is reported.
        let scope = r#"{"error":{"message":"Ask again.","code":"insufficient_scope"}}"#;
        assert_eq!(client.call_with(true, |_| reply(403, scope), &|_| panic!("no refresh")).unwrap_err().code(), Some("insufficient_scope"));
        // Refused even after a refresh: the grant has ended.
        assert!(client.call_with(true, |_| reply(401, "{}"), &|_| Ok(fresh(3))).is_err());
        // Requests without auth carry no token.
        assert!(client.call_with(false, |token| reply(if token.is_none() { 200 } else { 400 }, ""), &|_| panic!("no refresh")).is_ok());
        assert_eq!(*heard.lock().unwrap(), ["r2", "scope", "r3", "expired"]);
    }
}
