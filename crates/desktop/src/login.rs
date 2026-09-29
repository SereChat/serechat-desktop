//! First-run sign-in screen driving the device-code flow:
//! request -> approve in browser -> type the 6-digit code -> exchange.

use arboard::Clipboard;
use serechat::{AccessToken, Error};
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use crate::app::Action;
use crate::editor::Editor;
use crate::paint::{Painter, Rect, mix};
use crate::text::{Align, Style};
use crate::theme;
use crate::ui::{ButtonStyle, Ui, button, edit_key, logo};

/// Number of digits in a SereChat authorization code.
const CODE_LEN: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Nothing started yet.
    Idle,
    /// Waiting for `request_authorization`.
    Requesting,
    /// Browser opened; waiting for the user to type the code.
    AwaitingCode,
    /// Waiting for `exchange_code`.
    Verifying,
}

/// State of the sign-in screen.
pub struct Login {
    phase: Phase,
    code: Editor,
    request_id: Option<String>,
    error: Option<String>,
    /// Informational note, e.g. "link copied".
    notice: Option<String>,
}

impl Login {
    /// A fresh screen, optionally explaining why the user landed here.
    #[must_use]
    pub fn new(error: Option<String>) -> Self {
        Self {
            phase: Phase::Idle,
            code: Editor::restricted(CODE_LEN, |c| c.is_ascii_digit()),
            request_id: None,
            error,
            notice: None,
        }
    }

    /// Handles the result of step 1. Returns the request id whose approval
    /// page should now be opened.
    pub fn requested(&mut self, result: Result<String, Error>) -> Option<String> {
        match result {
            // The id ends up in a URL handed to the OS; accept only UUID-ish text.
            Ok(id) if !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') => {
                self.phase = Phase::AwaitingCode;
                self.request_id = Some(id.clone());
                Some(id)
            }
            Ok(_) => {
                self.fail("The server returned an invalid sign-in request.".into());
                None
            }
            Err(e) => {
                self.fail(e.to_string());
                None
            }
        }
    }

    /// Handles the result of step 2. Returns the token on success.
    pub fn exchanged(&mut self, result: Result<AccessToken, Error>) -> Option<AccessToken> {
        let error = match result {
            Ok(token) => return Some(token),
            Err(e) => e,
        };
        self.phase = Phase::AwaitingCode;
        self.code.take();
        self.error = Some(match error.code() {
            Some("invalid_code") => "That code is incorrect. Check your browser and try again.".into(),
            Some("authorization_pending") => "Approve the request in your browser first, then enter the code.".into(),
            Some("request_not_found" | "request_invalidated") => {
                self.phase = Phase::Idle;
                self.request_id = None;
                "This sign-in request has expired. Please start again.".into()
            }
            _ => error.to_string(),
        });
        None
    }

    /// Records that the browser could not be opened and the link was copied instead.
    pub fn browser_failed(&mut self) {
        self.notice = Some("Couldn't open your browser. The sign-in link was copied; paste it into a browser.".into());
    }

    fn fail(&mut self, message: String) {
        self.phase = Phase::Idle;
        self.request_id = None;
        self.error = Some(message);
    }

    fn start(&mut self, actions: &mut Vec<Action>) {
        self.phase = Phase::Requesting;
        self.error = None;
        self.notice = None;
        self.code.take();
        actions.push(Action::StartLogin);
    }

    fn verify(&mut self, actions: &mut Vec<Action>) {
        if self.phase != Phase::AwaitingCode || self.code.text().len() != CODE_LEN {
            return;
        }
        if let Some(request_id) = self.request_id.clone() {
            self.phase = Phase::Verifying;
            self.error = None;
            actions.push(Action::VerifyCode { request_id, code: self.code.text().to_owned() });
        }
    }

    /// Keyboard input.
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState, cb: &mut Option<Clipboard>, actions: &mut Vec<Action>) {
        match (&event.logical_key, self.phase) {
            (Key::Named(NamedKey::Enter), Phase::Idle) => self.start(actions),
            (Key::Named(NamedKey::Enter), _) => self.verify(actions),
            (_, Phase::AwaitingCode) => {
                edit_key(&mut self.code, event, mods, cb);
                // Submit as soon as the last digit lands.
                if self.code.text().len() == CODE_LEN {
                    self.verify(actions);
                }
            }
            _ => {}
        }
    }

    /// Draws the screen.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        let awaiting = matches!(self.phase, Phase::AwaitingCode | Phase::Verifying);
        let width = 400.0;
        let inner = width - 64.0;
        let subtitle = if awaiting {
            "Approve SereChat Desktop in your browser, then enter the 6-digit code shown there."
        } else {
            "Sign in with your SereChat account to start chatting."
        };
        let subtitle = p.layout(subtitle, Style { line_height: 1.5, ..theme::SMALL }, Some(inner));
        let message = self.error.as_deref().map(|e| (e, t.danger)).or(self.notice.as_deref().map(|n| (n, t.text_muted)));
        let message = message.map(|(text, color)| (p.layout(text, theme::SMALL, Some(inner)), color));

        let mut height = 32.0 + 40.0 + 20.0 + 30.0 + 6.0 + subtitle.height() + 24.0 + 36.0 + 32.0;
        if awaiting {
            height += 52.0 + 16.0 + 36.0;
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
        p.text_aligned(&subtitle, x, y, Align::Center, inner, t.text_muted);
        y += subtitle.height() + 24.0;

        if awaiting {
            self.draw_code(p, ui, Rect::new(x, y, inner, 52.0));
            y += 52.0 + 16.0;
        }

        let (label, enabled) = match self.phase {
            Phase::Idle => ("Continue in browser  →", true),
            Phase::Requesting => ("Opening browser…", false),
            Phase::AwaitingCode => ("Verify code", self.code.text().len() == CODE_LEN),
            Phase::Verifying => ("Verifying…", false),
        };
        if button(p, ui, Rect::new(x, y, inner, 36.0), label, ButtonStyle::Primary, enabled) {
            if self.phase == Phase::Idle {
                self.start(actions);
            } else {
                self.verify(actions);
            }
        }
        y += 36.0;

        if awaiting {
            y += 8.0;
            let half = (inner - 8.0) * 0.5;
            if button(p, ui, Rect::new(x, y, half, 28.0), "Reopen browser", ButtonStyle::Ghost, true)
                && let Some(request_id) = self.request_id.clone()
            {
                actions.push(Action::OpenAuthPage(request_id));
            }
            if button(p, ui, Rect::new(x + half + 8.0, y, half, 28.0), "Start over", ButtonStyle::Ghost, true) {
                self.start(actions);
            }
            y += 28.0;
        }

        if let Some((layout, color)) = &message {
            y += 16.0;
            p.text_aligned(layout, x, y, Align::Center, inner, *color);
        }
    }

    /// The six digit boxes.
    fn draw_code(&mut self, p: &mut Painter, ui: &mut Ui, area: Rect) {
        let t = p.theme;
        let gap = 8.0;
        let size = ((area.w - gap * (CODE_LEN as f32 - 1.0)) / CODE_LEN as f32).min(48.0);
        let start = area.x + (area.w - (size * CODE_LEN as f32 + gap * (CODE_LEN as f32 - 1.0))) * 0.5;
        let digits = self.code.text().as_bytes();
        let active = digits.len().min(CODE_LEN - 1);
        let editable = self.phase == Phase::AwaitingCode;
        for i in 0..CODE_LEN {
            let cell = Rect::new(start + i as f32 * (size + gap), area.y, size, area.h);
            let focus = ui.anim(crate::ui::id(("code", i)), f32::from(u8::from(editable && i == active)));
            p.bordered(cell, t.bg, theme::RADIUS, 1.0, mix(t.border_strong, t.accent, focus));
            if let Some(&digit) = digits.get(i) {
                let s = [digit];
                let text = std::str::from_utf8(&s).unwrap_or_default();
                p.label_centered(text, Style::semibold(22.0), cell, t.text);
            } else if editable && i == active && ui.caret_visible() {
                p.rect(Rect::new(cell.x + cell.w * 0.5 - 0.75, cell.y + 15.0, 1.5, cell.h - 30.0), t.accent, 0.5);
            }
            if editable && ui.hovered(cell) {
                ui.cursor = CursorIcon::Text;
            }
        }
    }
}
