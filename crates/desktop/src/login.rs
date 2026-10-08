//! First-run sign-in screen. Signing in is OAuth 2.1 with PKCE for native
//! apps (RFC 8252): the system browser shows SereChat's consent page and
//! sends its answer to a one-off listener on `127.0.0.1`, and the code it
//! carries is traded for tokens.

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use accesskit::Role;
use serechat::{BASE_URL, CLIENT_ID, Client, RESOURCE, SCOPES, Tokens};
use winit::event::KeyEvent;
use winit::keyboard::{Key, NamedKey};

use crate::a11y::Node;
use crate::app::Action;
use crate::mcp::oauth::{encode, random_string, receive_redirect};
use crate::paint::{Painter, Rect};
use crate::sha256;
use crate::text::{Align, Style};
use crate::theme;
use crate::ui::{ButtonStyle, Ui, button, logo};

/// A sign-in waiting for the browser.
pub struct Flow {
    /// SereChat's consent page, to open in the browser.
    pub url: String,
    listener: TcpListener,
    redirect_uri: String,
    verifier: String,
    state: String,
}

impl Flow {
    /// Listens on a free loopback port and builds the consent page's URL,
    /// with a fresh PKCE verifier and `state`.
    ///
    /// # Errors
    /// No loopback port could be opened.
    pub fn new() -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let redirect_uri = format!("http://127.0.0.1:{}/callback", listener.local_addr()?.port());
        let (verifier, state) = (random_string(64), random_string(32));
        let challenge = serechat::base64(&sha256::digest(verifier.as_bytes()), true);
        let url = format!(
            "{BASE_URL}/oauth/authorize?response_type=code&client_id={CLIENT_ID}&redirect_uri={}&code_challenge={challenge}&code_challenge_method=S256&state={state}&scope={}&resource={}",
            encode(&redirect_uri),
            encode(SCOPES),
            encode(RESOURCE),
        );
        Ok(Self { url, listener, redirect_uri, verifier, state })
    }

    /// Waits for the browser's answer, checks it came from SereChat, and
    /// trades its code for tokens. Blocks until then (ten minutes at most)
    /// or until `cancel` is raised.
    ///
    /// # Errors
    /// A message for the user.
    pub fn finish(self, client: &Client, cancel: &AtomicBool) -> Result<Tokens, String> {
        let params = receive_redirect(&self.listener, &self.state, cancel)?;
        // RFC 9207: only SereChat's own answer counts.
        if params.get("iss").map(String::as_str) != Some(BASE_URL) {
            return Err("The sign-in answer did not come from SereChat, so it was ignored. Please try again.".into());
        }
        if let Some(error) = params.get("error") {
            return Err(match (error.as_str(), params.get("error_description")) {
                ("access_denied", _) => "Sign-in was declined in the browser.".into(),
                (_, Some(detail)) => format!("Sign-in failed: {detail}"),
                (error, None) => format!("Sign-in failed ({error})."),
            });
        }
        let code = params.get("code").ok_or("The sign-in answer had no authorization code.")?;
        client.exchange_code(code, &self.redirect_uri, &self.verifier).map_err(|e| e.to_string())
    }
}

/// State of the sign-in screen.
pub struct Login {
    /// The consent page, while a sign-in waits for the browser.
    waiting: Option<String>,
    /// Raised to give up on the sign-in that is waiting.
    cancel: Arc<AtomicBool>,
    /// Numbers sign-ins, so the answer to one given up on is ignored.
    flow: u64,
    error: Option<String>,
    /// Informational note, e.g. "link copied".
    notice: Option<String>,
}

impl Login {
    /// A fresh screen, optionally explaining why the user landed here.
    #[must_use]
    pub fn new(error: Option<String>) -> Self {
        Self { waiting: None, cancel: Arc::default(), flow: 0, error, notice: None }
    }

    /// A sign-in started with consent page `url`. Returns its number and
    /// the flag that gives up on it.
    pub fn waiting(&mut self, url: String) -> (u64, Arc<AtomicBool>) {
        self.stop();
        self.cancel = Arc::default();
        self.flow += 1;
        self.waiting = Some(url);
        (self.flow, Arc::clone(&self.cancel))
    }

    /// Handles the end of sign-in `flow`. Returns the tokens on success.
    pub fn finished(&mut self, flow: u64, result: Result<Tokens, String>) -> Option<Tokens> {
        if flow != self.flow || self.waiting.take().is_none() {
            return None;
        }
        match result {
            Ok(tokens) => Some(tokens),
            Err(e) => {
                self.error = Some(e);
                None
            }
        }
    }

    /// Shows why a sign-in could not start or finish.
    pub fn fail(&mut self, message: String) {
        self.stop();
        self.error = Some(message);
    }

    /// Records that the browser could not be opened and the link was copied instead.
    pub fn browser_failed(&mut self) {
        self.notice = Some("Couldn't open your browser. The sign-in link was copied; paste it into a browser.".into());
    }

    /// Gives up on the sign-in that is waiting.
    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.waiting = None;
    }

    fn start(&mut self, actions: &mut Vec<Action>) {
        self.error = None;
        self.notice = None;
        actions.push(Action::StartLogin);
    }

    /// Keyboard input.
    pub fn key(&mut self, event: &KeyEvent, actions: &mut Vec<Action>) {
        if event.logical_key == Key::Named(NamedKey::Enter) && self.waiting.is_none() {
            self.start(actions);
        }
    }

    /// Draws the screen.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        let width = 400.0;
        let inner = width - 64.0;
        let subtitle_text = if self.waiting.is_some() {
            "Approve SereChat Desktop in your browser. You'll continue here once you do."
        } else {
            "Sign in with your SereChat account to start chatting."
        };
        let subtitle = p.layout(subtitle_text, Style { line_height: 1.5, ..theme::SMALL }, Some(inner));
        let message = self.error.as_deref().map(|e| (e, t.danger)).or(self.notice.as_deref().map(|n| (n, t.text_muted)));
        let message = message.map(|(text, color)| (p.layout(text, theme::SMALL, Some(inner)), color));

        let mut height = 32.0 + 40.0 + 20.0 + 30.0 + 6.0 + subtitle.height() + 24.0 + 36.0 + 32.0;
        if self.waiting.is_some() {
            height += 8.0 + 28.0;
        }
        if let Some((layout, _)) = &message {
            height += layout.height() + 16.0;
        }

        let card = Rect::new(((view.w - width) * 0.5).round(), ((view.h - height) * 0.5).round(), width, height);
        p.shadow(Rect::new(card.x, card.y + 8.0, card.w, card.h), t.shadow, theme::RADIUS, 24.0);
        p.bordered(card, t.panel, theme::RADIUS, 1.0, t.border_strong);

        let x = card.x + 32.0;
        let mut y = card.y + 32.0;
        logo(p, Rect::new(card.x + (card.w - 40.0) * 0.5, y, 40.0, 40.0));
        y += 40.0 + 20.0;
        let title = p.layout("Sign in to SereChat", theme::TITLE, None);
        p.text_aligned(&title, x, y, Align::Center, inner, t.text);
        y += 30.0 + 6.0;
        ui.describe(|| Node::new(Role::Heading, Rect::new(x, y - 36.0, inner, 30.0), "Sign in to SereChat"));
        p.text_aligned(&subtitle, x, y, Align::Center, inner, t.text_muted);
        ui.describe(|| Node::new(Role::Label, Rect::new(x, y, inner, subtitle.height()), subtitle_text));
        y += subtitle.height() + 24.0;

        let (label, enabled) = if self.waiting.is_some() { ("Waiting for your browser…", false) } else { ("Continue in browser  →", true) };
        if button(p, ui, Rect::new(x, y, inner, 36.0), label, ButtonStyle::Primary, enabled) {
            self.start(actions);
        }
        y += 36.0;

        if let Some(url) = self.waiting.clone() {
            y += 8.0;
            let half = (inner - 8.0) * 0.5;
            if button(p, ui, Rect::new(x, y, half, 28.0), "Reopen browser", ButtonStyle::Ghost, true) {
                actions.push(Action::OpenAuthPage(url));
            }
            if button(p, ui, Rect::new(x + half + 8.0, y, half, 28.0), "Cancel", ButtonStyle::Ghost, true) {
                self.stop();
            }
            y += 28.0;
        }

        if let Some((layout, color)) = &message {
            y += 16.0;
            p.text_aligned(layout, x, y, Align::Center, inner, *color);
            if let Some(text) = self.error.as_deref().or(self.notice.as_deref()) {
                ui.describe(|| Node::new(Role::Alert, Rect::new(x, y, inner, layout.height()), text));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Plays the browser: comes back to `flow`'s redirect with `query` and its state.
    fn answer(flow: &Flow, query: &str) -> std::thread::JoinHandle<()> {
        let target = format!("/callback?{query}&state={}", flow.state);
        let port = flow.listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.write_all(format!("GET {target} HTTP/1.1\r\n\r\n").as_bytes()).unwrap();
            let _ = stream.read_to_string(&mut String::new());
        })
    }

    #[test]
    fn consent_url_and_answers() {
        let flow = Flow::new().unwrap();
        let challenge = serechat::base64(&sha256::digest(flow.verifier.as_bytes()), true);
        assert!(flow.url.starts_with("https://serechat.com/oauth/authorize?response_type=code&client_id=serechat-desktop&redirect_uri=http%3A%2F%2F127.0.0.1%3A"));
        assert!(flow.url.contains("%2Fcallback&") && flow.url.contains(&format!("&code_challenge={challenge}&code_challenge_method=S256&state={}&", flow.state)));
        assert!(flow.url.ends_with("&scope=chat%20media%20files&resource=https%3A%2F%2Fserechat.com%2Fv1"));

        // An answer naming another issuer is refused before any code is used.
        let browser = answer(&flow, "code=c&iss=https%3A%2F%2Fevil.example");
        let Err(error) = flow.finish(&Client::new(), &AtomicBool::new(false)) else { panic!("a foreign answer was accepted") };
        browser.join().unwrap();
        assert!(error.contains("did not come from SereChat"), "{error}");

        let flow = Flow::new().unwrap();
        let browser = answer(&flow, "error=access_denied&iss=https%3A%2F%2Fserechat.com");
        let Err(error) = flow.finish(&Client::new(), &AtomicBool::new(false)) else { panic!("a refusal was accepted") };
        assert_eq!(error, "Sign-in was declined in the browser.");
        browser.join().unwrap();
    }

    #[test]
    fn only_the_latest_sign_in_counts() {
        let mut login = Login::new(None);
        let (first, cancel_first) = login.waiting("https://serechat.com/a".into());
        let (second, _) = login.waiting("https://serechat.com/b".into());
        assert!(cancel_first.load(Ordering::Relaxed), "starting again gives up on the first");
        assert!(login.finished(first, Err("Sign-in was cancelled.".into())).is_none());
        assert!(login.error.is_none() && login.waiting.is_some(), "the first one's end is ignored");
        assert!(login.finished(second, Err("Sign-in was declined in the browser.".into())).is_none());
        assert_eq!(login.error.as_deref(), Some("Sign-in was declined in the browser."));
        assert!(login.finished(second, Err("again".into())).is_none() && login.error.as_deref() != Some("again"), "each ends once");
    }
}
