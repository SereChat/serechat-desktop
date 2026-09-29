//! Main screen: sidebar of saved sessions, messages, composer, menus, and the
//! settings page.
//!
//! The screen never touches the disk or network itself: it emits [`Action`]s
//! (send, save, delete, …) that the app carries out.
//!
//! ponytail: replies render as plain text; render Markdown next.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arboard::Clipboard;
use serechat::{Completion, Error, Message, Model, Role, Session, StoredMessage, StreamEvent, Usage, new_session_id, unix_now};
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use crate::app::Action;
use crate::editor::Editor;
use crate::paint::{Painter, Rect, fade, mix};
use crate::settings::{SettingsView, Totals};
use crate::text::{Align, Style, TextLayout};
use crate::theme::{self, Scheme};
use crate::ui::{ButtonStyle, Ui, button, chevron, edit_key, id, keycap, logo};

/// Model used until the user picks one.
pub const DEFAULT_MODEL: &str = "claude-sonnet-5.5";
/// Composer grows up to this many lines, then scrolls.
const COMPOSER_MAX_LINES: usize = 8;
/// Vertical space between messages.
const MESSAGE_GAP: f32 = 20.0;
/// Inner padding of boxed messages.
const BOX_PAD: (f32, f32) = (12.0, 10.0);
/// Height of the caption row under a reply.
const META_H: f32 = 30.0;
/// Height of the "Reasoning" toggle above a reply.
const REASONING_ROW: f32 = 30.0;
/// Name of the platform's primary shortcut modifier.
const PRIMARY_KEY: &str = if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" };

/// How much the model should think before answering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reasoning {
    /// Leave it to the model.
    #[default]
    Auto,
    /// No reasoning.
    Off,
    /// Brief reasoning.
    Low,
    /// Balanced reasoning.
    Medium,
    /// Thorough reasoning.
    High,
}

impl Reasoning {
    const ALL: [Self; 5] = [Self::Auto, Self::Off, Self::Low, Self::Medium, Self::High];

    /// Value stored in the config file; also the API's effort name.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Parses a config value; unknown values mean [`Reasoning::Auto`].
    #[must_use]
    pub fn from_key(key: Option<&str>) -> Self {
        Self::ALL.into_iter().find(|r| Some(r.key()) == key).unwrap_or_default()
    }

    /// The API's `reasoning.effort`, or `None` to omit it.
    fn effort(self) -> Option<&'static str> {
        (self != Self::Auto).then(|| self.key())
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Off => "Off",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::Auto => "Model default",
            Self::Off => "Answer right away",
            Self::Low => "Think briefly",
            Self::Medium => "Balanced",
            Self::High => "Think it through",
        }
    }
}

/// What the main area shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Chat,
    Settings,
}

/// Drop-down menus in the composer toolbar.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Model,
    Reasoning,
}

/// A layout cached for `(text length, wrap width, scale)`.
type LayoutCache = Option<((usize, u32, u32), TextLayout)>;

/// Returns the layout of `text` wrapped at `width`, re-laying out only when
/// the text, width or scale changed. Takes fields separately so the rest of
/// the entry stays borrowable.
fn cached_layout<'a>(cache: &'a mut LayoutCache, text: &str, style: Style, p: &Painter, width: f32) -> &'a TextLayout {
    let key = (text.len(), width.to_bits(), p.scale.to_bits());
    if cache.as_ref().is_none_or(|(k, _)| *k != key) {
        *cache = Some((key, p.layout(text, style, Some(width))));
    }
    &cache.as_ref().expect("layout was just filled").1
}

/// A reply being streamed.
struct ActiveStream {
    id: u64,
    cancel: Arc<AtomicBool>,
    /// Model answering, for pricing the reply.
    model: String,
}

/// One message in a conversation.
struct Entry {
    id: u64,
    message: StoredMessage,
    /// The reasoning block is expanded.
    show_reasoning: bool,
    layout: LayoutCache,
    reasoning_layout: LayoutCache,
}

impl Entry {
    fn new(id: u64, message: StoredMessage) -> Self {
        Self { id, message, show_reasoning: false, layout: None, reasoning_layout: None }
    }

    /// Drawn inside a bordered box (prompts and errors) rather than bare.
    fn boxed(&self) -> bool {
        self.message.role == Role::User || self.message.failed
    }

    /// Height of the entry at `width`, filling the layout caches.
    fn measure(&mut self, p: &Painter, width: f32) -> f32 {
        let boxed = self.boxed();
        let wrap = if boxed { width - 2.0 * BOX_PAD.0 } else { width };
        let text_h = cached_layout(&mut self.layout, &self.message.content, theme::BODY, p, wrap).height();
        if boxed {
            return text_h + 2.0 * BOX_PAD.1;
        }
        let mut height = text_h.max(18.0) + META_H;
        if !self.message.reasoning.is_empty() {
            height += REASONING_ROW;
            if self.show_reasoning {
                let layout = cached_layout(&mut self.reasoning_layout, &self.message.reasoning, theme::SMALL, p, wrap - 14.0);
                height += layout.height() + 12.0;
            }
        }
        height
    }
}

/// One conversation; saved as a [`Session`] once it has messages.
struct Conversation {
    id: u64,
    session_id: String,
    title: String,
    created: u64,
    updated: u64,
    entries: Vec<Entry>,
    stream: Option<ActiveStream>,
}

impl Conversation {
    fn new(id: u64) -> Self {
        let now = unix_now();
        Self {
            id,
            session_id: new_session_id(),
            title: String::new(),
            created: now,
            updated: now,
            entries: Vec::new(),
            stream: None,
        }
    }

    /// Snapshot for saving; the empty placeholder of a pending reply is skipped.
    fn to_session(&self) -> Session {
        Session {
            id: self.session_id.clone(),
            title: self.title.clone(),
            created: self.created,
            updated: self.updated,
            messages: self.entries.iter().filter(|e| !e.message.content.is_empty()).map(|e| e.message.clone()).collect(),
        }
    }

    fn cost(&self) -> f64 {
        self.entries.iter().map(|e| e.message.cost).sum()
    }

    fn tokens(&self) -> u64 {
        self.entries.iter().map(|e| e.message.usage.input_tokens + e.message.usage.output_tokens).sum()
    }
}

/// Everything a worker thread needs to stream one reply.
pub struct SendJob {
    /// Conversation the reply belongs to.
    pub conversation: u64,
    /// Identifies this stream so late events from a stopped one are dropped.
    pub stream: u64,
    /// Model identifier.
    pub model: String,
    /// Reasoning effort, or `None` for the model default.
    pub reasoning: Option<&'static str>,
    /// Conversation history including the new prompt.
    pub input: Vec<Message>,
    /// Raised to abort the stream.
    pub cancel: Arc<AtomicBool>,
}

/// One row of a drop-down menu.
struct MenuItem {
    label: String,
    detail: String,
    selected: bool,
}

/// State of the chat screen.
pub struct Chat {
    conversations: Vec<Conversation>,
    current: u64,
    next_id: u64,
    page: Page,
    settings: SettingsView,
    composer: Editor,
    /// Composer layout and text origin from the last frame, for keyboard
    /// navigation and mouse hit-testing.
    composer_layout: Option<(TextLayout, (f32, f32))>,
    composer_scroll: f32,
    selecting: bool,
    models: Vec<Model>,
    model: String,
    reasoning: Reasoning,
    menu: Option<Menu>,
    /// Where the open menu was drawn last frame; blocks hover beneath it.
    menu_rect: Option<Rect>,
    menu_scroll: f32,
    scroll: f32,
    scroll_target: f32,
    stick_to_bottom: bool,
    sidebar_scroll: f32,
    /// Conversation whose delete button was clicked once, awaiting confirmation.
    confirm_delete: Option<u64>,
    /// Entry whose text was just copied, and when.
    copied: Option<(u64, f32)>,
}

impl Chat {
    /// The chat screen with the saved `sessions` (newest first) in the sidebar
    /// and a fresh conversation open.
    #[must_use]
    pub fn new(model: Option<String>, reasoning: Reasoning, sessions: Vec<Session>) -> Self {
        let mut chat = Self {
            conversations: Vec::new(),
            current: 0,
            next_id: 1,
            page: Page::Chat,
            settings: SettingsView::default(),
            composer: Editor::default(),
            composer_layout: None,
            composer_scroll: 0.0,
            selecting: false,
            models: Vec::new(),
            model: model.unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
            reasoning,
            menu: None,
            menu_rect: None,
            menu_scroll: 0.0,
            scroll: 0.0,
            scroll_target: 0.0,
            stick_to_bottom: true,
            sidebar_scroll: 0.0,
            confirm_delete: None,
            copied: None,
        };
        for session in sessions.into_iter().filter(|s| !s.messages.is_empty()) {
            let id = chat.next_id();
            let entries = session.messages.into_iter().map(|m| Entry::new(chat.next_id(), m)).collect();
            chat.conversations.push(Conversation {
                id,
                session_id: session.id,
                title: session.title,
                created: session.created,
                updated: session.updated,
                entries,
                stream: None,
            });
        }
        chat.new_conversation();
        chat
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn current(&mut self) -> &mut Conversation {
        let current = self.current;
        self.conversations
            .iter_mut()
            .find(|c| c.id == current)
            .expect("the current conversation always exists")
    }

    fn select(&mut self, id: u64) {
        self.current = id;
        self.page = Page::Chat;
        self.menu = None;
        self.scroll = 0.0;
        self.scroll_target = 0.0;
        self.stick_to_bottom = true;
    }

    /// Opens an empty conversation, reusing an existing empty one.
    fn new_conversation(&mut self) {
        if let Some(empty) = self.conversations.iter().find(|c| c.entries.is_empty()).map(|c| c.id) {
            self.select(empty);
            return;
        }
        let id = self.next_id();
        self.conversations.insert(0, Conversation::new(id));
        self.select(id);
    }

    /// Removes a conversation and its saved session.
    fn delete_conversation(&mut self, id: u64, actions: &mut Vec<Action>) {
        let Some(index) = self.conversations.iter().position(|c| c.id == id) else {
            return;
        };
        let conversation = self.conversations.remove(index);
        if let Some(stream) = conversation.stream {
            stream.cancel.store(true, Ordering::Relaxed);
        }
        actions.push(Action::DeleteSession(conversation.session_id));
        if self.current == id {
            self.new_conversation();
        }
    }

    /// Stores the model list, keeping the selection valid.
    pub fn set_models(&mut self, models: Vec<Model>) {
        if !models.iter().any(|m| m.id == self.model)
            && let Some(fallback) = models.iter().find(|m| m.id == DEFAULT_MODEL).or(models.first())
        {
            self.model.clone_from(&fallback.id);
        }
        self.models = models;
    }

    /// Cancels every running stream (used on sign-out).
    pub fn cancel_all(&mut self) {
        for conversation in &mut self.conversations {
            if let Some(stream) = conversation.stream.take() {
                stream.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Usage summed over every conversation with messages.
    fn totals(&self) -> Totals {
        let saved = self.conversations.iter().filter(|c| !c.entries.is_empty());
        saved.fold(Totals::default(), |t, c| Totals { cost: t.cost + c.cost(), tokens: t.tokens + c.tokens(), sessions: t.sessions + 1 })
    }

    /// Takes the composer text and starts a reply, if sending is possible.
    fn send(&mut self, actions: &mut Vec<Action>) {
        if self.composer.text().trim().is_empty() || self.current().stream.is_some() {
            return;
        }
        let text = self.composer.take().trim().to_owned();
        let (user_id, reply_id, stream_id) = (self.next_id(), self.next_id(), self.next_id());
        let model = self.model.clone();
        let reasoning = self.reasoning.effort();
        let cancel = Arc::new(AtomicBool::new(false));

        let conversation = self.current();
        if conversation.title.is_empty() {
            conversation.title = text.lines().next().unwrap_or_default().chars().take(80).collect();
        }
        conversation.updated = unix_now();
        conversation.entries.push(Entry::new(user_id, StoredMessage::new(Role::User, text)));
        let input = conversation
            .entries
            .iter()
            .filter(|e| !e.message.failed && !e.message.content.is_empty())
            .map(|e| Message { role: e.message.role, content: e.message.content.clone() })
            .collect();
        conversation.entries.push(Entry::new(reply_id, StoredMessage::new(Role::Assistant, String::new())));
        conversation.stream = Some(ActiveStream { id: stream_id, cancel: Arc::clone(&cancel), model: model.clone() });
        let conversation_id = conversation.id;
        actions.push(Action::SaveSession(conversation.to_session()));

        // Most recently used conversation moves to the top of the sidebar.
        if let Some(index) = self.conversations.iter().position(|c| c.id == conversation_id) {
            let moved = self.conversations.remove(index);
            self.conversations.insert(0, moved);
        }
        self.stick_to_bottom = true;
        self.composer_scroll = 0.0;
        actions.push(Action::Send(SendJob { conversation: conversation_id, stream: stream_id, model, reasoning, input, cancel }));
    }

    fn stream_target(conversations: &mut [Conversation], conversation: u64, stream: u64) -> Option<&mut Conversation> {
        conversations
            .iter_mut()
            .find(|c| c.id == conversation && c.stream.as_ref().is_some_and(|s| s.id == stream))
    }

    /// Applies one streamed update.
    pub fn stream_event(&mut self, conversation: u64, stream: u64, event: StreamEvent) {
        let Some(conversation) = Self::stream_target(&mut self.conversations, conversation, stream) else {
            return;
        };
        let (Some(stream), Some(entry)) = (&conversation.stream, conversation.entries.last_mut()) else {
            return;
        };
        let message = &mut entry.message;
        match event {
            StreamEvent::Text(delta) => {
                // Models often open with blank lines; don't render them.
                let delta = if message.content.is_empty() { delta.trim_start() } else { &delta };
                message.content.push_str(delta);
            }
            StreamEvent::Reasoning(delta) => message.reasoning.push_str(&delta),
            StreamEvent::Completed(Completion { usage, reasoning }) => {
                if message.reasoning.is_empty() {
                    message.reasoning = reasoning;
                }
                message.cost = self.models.iter().find(|m| m.id == stream.model).map_or(0.0, |m| m.cost(usage));
                message.model = Some(stream.model.clone());
                message.usage = usage;
            }
        }
    }

    /// Finishes a stream and queues a save. Returns `true` if the server
    /// rejected our token.
    pub fn stream_end(&mut self, conversation: u64, stream: u64, result: Result<bool, Error>, actions: &mut Vec<Action>) -> bool {
        let unauthorized = result.as_ref().is_err_and(Error::is_unauthorized);
        let next = self.next_id();
        let Some(conversation) = Self::stream_target(&mut self.conversations, conversation, stream) else {
            return false;
        };
        // A stream the user stopped is already detached, so anything short of
        // a completed, non-empty reply here is a failure worth showing.
        conversation.stream = None;
        conversation.updated = unix_now();
        let reply = conversation.entries.last_mut().filter(|e| e.message.role == Role::Assistant && !e.message.failed);
        let empty = reply.as_ref().is_none_or(|e| e.message.content.is_empty());
        let error = match result {
            Err(error) => Some(error.to_string()),
            Ok(true) if empty => Some("The model returned an empty response.".to_owned()),
            Ok(true) => None,
            Ok(false) => Some("The connection closed before the reply finished.".to_owned()),
        };
        if let Some(error) = error {
            match reply {
                Some(entry) if entry.message.content.is_empty() => {
                    entry.message.content = error;
                    entry.message.failed = true;
                    entry.layout = None;
                }
                _ => {
                    let mut message = StoredMessage::new(Role::Assistant, error);
                    message.failed = true;
                    conversation.entries.push(Entry::new(next, message));
                }
            }
        }
        actions.push(Action::SaveSession(conversation.to_session()));
        unauthorized
    }

    /// Stops the current conversation's stream, keeping any partial reply.
    fn stop(&mut self, actions: &mut Vec<Action>) {
        let conversation = self.current();
        if let Some(stream) = conversation.stream.take() {
            stream.cancel.store(true, Ordering::Relaxed);
            if conversation.entries.last().is_some_and(|e| e.message.role == Role::Assistant && e.message.content.is_empty()) {
                conversation.entries.pop();
            }
            actions.push(Action::SaveSession(conversation.to_session()));
        }
    }

    /// Whether any conversation is streaming (drives redraws for animations).
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        self.conversations.iter().any(|c| c.stream.is_some())
    }

    fn toggle_settings(&mut self) {
        self.menu = None;
        self.page = if self.page == Page::Settings { Page::Chat } else { Page::Settings };
    }

    /// Keyboard input.
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState, cb: &mut Option<Clipboard>, actions: &mut Vec<Action>) {
        let primary = if cfg!(target_os = "macos") { mods.super_key() } else { mods.control_key() };
        match &event.logical_key {
            Key::Named(NamedKey::Escape) if self.menu.is_some() => self.menu = None,
            Key::Named(NamedKey::Escape) if self.page == Page::Settings => self.page = Page::Chat,
            Key::Character(c) if primary && c == "," => self.toggle_settings(),
            Key::Character(c) if primary && c.eq_ignore_ascii_case("n") => self.new_conversation(),
            _ if self.page == Page::Settings => {}
            Key::Named(NamedKey::Enter) if mods.shift_key() => self.composer.insert("\n"),
            Key::Named(NamedKey::Enter) => self.send(actions),
            Key::Named(NamedKey::Escape) => self.stop(actions),
            Key::Named(key @ (NamedKey::ArrowUp | NamedKey::ArrowDown)) => {
                if let Some((layout, _)) = &self.composer_layout {
                    let (x, y) = layout.caret(self.composer.cursor());
                    let line = layout.line_height();
                    let target = if *key == NamedKey::ArrowUp { y - line * 0.5 } else { y + line * 1.5 };
                    let byte = if target < 0.0 {
                        0
                    } else if target > layout.height() {
                        self.composer.text().len()
                    } else {
                        layout.hit(x, target)
                    };
                    self.composer.set_cursor(byte, mods.shift_key());
                }
            }
            _ => {
                edit_key(&mut self.composer, event, mods, cb);
            }
        }
    }

    /// Draws the screen.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, scheme: Scheme, actions: &mut Vec<Action>) {
        let sidebar = Rect::new(0.0, 0.0, theme::SIDEBAR_WIDTH, view.h);
        let main = Rect::new(sidebar.w, 0.0, view.w - sidebar.w, view.h);

        // An open menu is drawn last, on top. Its rect from the previous
        // frame keeps the widgets beneath it from reacting to the mouse.
        ui.blocker = if self.menu.is_some() { self.menu_rect } else { None };
        self.draw_sidebar(p, ui, sidebar, actions);
        if self.page == Page::Settings {
            ui.blocker = None;
            let totals = self.totals();
            self.settings.draw(p, ui, main, scheme, totals, actions);
            return;
        }
        self.draw_header(p, main);
        let (composer_top, toolbar) = self.draw_composer(p, ui, main, actions);
        let messages = Rect::new(main.x, theme::HEADER_HEIGHT, main.w, composer_top - theme::HEADER_HEIGHT - 12.0);
        self.draw_messages(p, ui, messages, actions);
        ui.blocker = None;
        self.draw_open_menu(p, ui, toolbar, actions);
    }

    fn draw_sidebar(&mut self, p: &mut Painter, ui: &mut Ui, area: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        p.rect(area, t.panel, 0.0);
        p.rect(Rect::new(area.right() - 1.0, 0.0, 1.0, area.h), t.border, 0.0);

        // macOS draws the traffic lights over our (transparent) title bar.
        let top = if cfg!(target_os = "macos") { 36.0 } else { 8.0 };
        let brand = p.layout("SereChat", Style::semibold(14.5), None);
        p.text(&brand, 18.0, top + 18.0 - brand.height() * 0.5, t.text);

        let on_chat = self.page == Page::Chat;
        let fresh = self.conversations.iter().any(|c| c.id == self.current && c.entries.is_empty());
        let new_chat = Rect::new(8.0, top + 42.0, area.w - 16.0, 30.0);
        if list_row(p, ui, new_chat, id("new-chat"), "+  New chat", Some(&format!("{PRIMARY_KEY}+N")), on_chat && fresh) {
            self.new_conversation();
        }

        let list_top = new_chat.bottom() + 18.0;
        p.label("Sessions", theme::CAPTION, 18.0, list_top, t.text_faint);
        let list = Rect::new(0.0, list_top + 24.0, area.w, area.h - list_top - 24.0 - 52.0);
        let item_h = 30.0;
        let saved = self.conversations.iter().filter(|c| !c.entries.is_empty());
        let content_h = saved.clone().count() as f32 * (item_h + 1.0);
        if ui.hovered(list) {
            self.sidebar_scroll += ui.scroll;
        }
        self.sidebar_scroll = self.sidebar_scroll.clamp(0.0, (content_h - list.h).max(0.0));
        if content_h == 0.0 {
            p.label("No sessions yet", theme::SMALL, 18.0, list.y + 6.0, t.text_faint);
        }

        let clip = p.push_clip(list);
        let now = unix_now();
        let (mut open, mut confirm, mut delete, mut confirm_hovered) = (None, None, None, false);
        let mut y = list.y - self.sidebar_scroll;
        for conversation in saved {
            let item = Rect::new(8.0, y, area.w - 16.0, item_h);
            y += item_h + 1.0;
            if item.bottom() < list.y || item.y > list.bottom() {
                continue;
            }
            // Rows scrolled under the list edges must not react.
            let hovered = ui.hovered(item) && list.contains(ui.mouse);
            let hover = ui.anim(id(("conversation", conversation.id)), f32::from(u8::from(hovered)));
            let selected = on_chat && conversation.id == self.current;
            p.rect(item, if selected { t.active } else { fade(t.hover, hover) }, theme::RADIUS_SM);

            // Right side: age, or the delete control while hovered.
            let confirming = self.confirm_delete == Some(conversation.id);
            confirm_hovered |= confirming && hovered;
            let mut on_control = false;
            let right_w = if confirming {
                let del = Rect::new(item.right() - 60.0, item.y + 4.0, 56.0, item_h - 8.0);
                let over = hovered && del.contains(ui.mouse);
                p.rect(del, fade(t.danger, if over { 0.24 } else { 0.14 }), theme::RADIUS_SM);
                p.label_centered("Delete", theme::CAPTION, del, t.danger);
                on_control = over;
                if over && ui.clicked(del) {
                    delete = Some(conversation.id);
                }
                64.0
            } else if hovered {
                let x = Rect::new(item.right() - 26.0, item.y + 5.0, 20.0, 20.0);
                let over = x.contains(ui.mouse);
                if over {
                    p.rect(x, t.active, theme::RADIUS_SM);
                }
                p.label_centered("×", Style::regular(15.0), x, if over { t.text } else { t.text_faint });
                on_control = over;
                if over && ui.clicked(x) {
                    confirm = Some(conversation.id);
                }
                30.0
            } else if conversation.stream.is_some() {
                let pulse = 0.5 + 0.5 * (ui.time * 4.0).sin();
                p.rect(Rect::new(item.right() - 16.0, item.y + item_h * 0.5 - 3.0, 6.0, 6.0), fade(t.accent, 0.4 + 0.6 * pulse), 3.0);
                24.0
            } else {
                let age = p.layout(&ago(now, conversation.updated), theme::TINY, None);
                p.text(&age, item.right() - 10.0 - age.width(), item.y + (item_h - age.height()) * 0.5, t.text_faint);
                age.width() + 18.0
            };

            let mut title = p.layout(&conversation.title, theme::SMALL, None);
            title.truncate(p.fonts, item.w - 10.0 - right_w);
            let color = if selected { t.text } else { mix(t.text_muted, t.text, hover) };
            p.text(&title, item.x + 10.0, item.y + (item_h - title.height()) * 0.5, color);
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if !on_control && ui.clicked(item) {
                    open = Some(conversation.id);
                }
            }
        }
        p.set_clip(clip);

        // Leaving the row cancels a pending delete.
        if !confirm_hovered {
            self.confirm_delete = None;
        }
        if confirm.is_some() {
            self.confirm_delete = confirm;
        }
        if let Some(id) = delete {
            self.confirm_delete = None;
            self.delete_conversation(id, actions);
        }
        if let Some(id) = open {
            self.select(id);
        }

        p.rect(Rect::new(0.0, area.h - 47.0, area.w - 1.0, 1.0), t.border, 0.0);
        let settings = Rect::new(8.0, area.h - 39.0, area.w - 16.0, 30.0);
        if list_row(p, ui, settings, id("settings"), "Settings", Some(&format!("{PRIMARY_KEY}+,")), !on_chat) {
            self.toggle_settings();
        }
    }

    /// Title bar of the chat area: session title and what it has cost.
    fn draw_header(&mut self, p: &mut Painter, main: Rect) {
        let t = p.theme;
        let bar = Rect::new(main.x, 0.0, main.w, theme::HEADER_HEIGHT);
        p.rect(Rect::new(bar.x, bar.bottom() - 1.0, bar.w, 1.0), t.border, 0.0);
        let conversation = self.current();
        let (tokens, cost) = (conversation.tokens(), conversation.cost());
        let title = if conversation.title.is_empty() { "New chat".to_owned() } else { conversation.title.clone() };

        let mut right_w = 0.0;
        if tokens > 0 {
            let spent = p.layout(&format!("{} tokens  ·  {}", group_digits(tokens), format_cost(cost)), theme::SMALL, None);
            right_w = spent.width() + 24.0;
            p.text(&spent, bar.right() - 16.0 - spent.width(), bar.y + (bar.h - spent.height()) * 0.5, t.text_faint);
        }
        let mut layout = p.layout(&title, theme::LABEL, None);
        layout.truncate(p.fonts, bar.w - 32.0 - right_w);
        p.text(&layout, bar.x + 16.0, bar.y + (bar.h - layout.height()) * 0.5, t.text);
    }

    /// Draws the open menu (if any) above its toolbar button and applies a choice.
    fn draw_open_menu(&mut self, p: &mut Painter, ui: &mut Ui, toolbar: [Rect; 2], actions: &mut Vec<Action>) {
        let Some(menu) = self.menu else {
            self.menu_rect = None;
            return;
        };
        let (anchor, width, header, items) = match menu {
            Menu::Model => {
                let items = self
                    .models
                    .iter()
                    .map(|m| MenuItem {
                        label: if m.name.is_empty() { m.id.clone() } else { m.name.clone() },
                        detail: price(m),
                        selected: m.id == self.model,
                    })
                    .collect::<Vec<_>>();
                (toolbar[0], 360.0, ("Model", "Input / output per 1M tokens"), items)
            }
            Menu::Reasoning => {
                let items = Reasoning::ALL
                    .iter()
                    .map(|r| MenuItem { label: r.label().to_owned(), detail: r.detail().to_owned(), selected: *r == self.reasoning })
                    .collect();
                (toolbar[1], 280.0, ("Reasoning effort", ""), items)
            }
        };
        let chosen = self.draw_menu(p, ui, anchor, width, header, &items);
        if let Some(index) = chosen {
            match menu {
                Menu::Model => {
                    if let Some(model) = self.models.get(index) {
                        self.model.clone_from(&model.id);
                        actions.push(Action::SelectModel(model.id.clone()));
                    }
                }
                Menu::Reasoning => {
                    self.reasoning = Reasoning::ALL[index];
                    actions.push(Action::SetReasoning(self.reasoning));
                }
            }
            self.menu = None;
            ui.released = false;
        }
        let inside = self.menu_rect.is_some_and(|r| r.contains(ui.press_pos)) || anchor.contains(ui.press_pos);
        if ui.released && !inside {
            self.menu = None;
        }
    }

    /// Draws a menu opening upwards from `anchor`. Returns the clicked row.
    fn draw_menu(&mut self, p: &mut Painter, ui: &mut Ui, anchor: Rect, width: f32, header: (&str, &str), items: &[MenuItem]) -> Option<usize> {
        let t = p.theme;
        let (row_h, header_h, pad) = (30.0, 30.0, 4.0);
        let content_h = items.len().max(1) as f32 * row_h;
        // Scrolls when the window is too short for every row.
        let height = (header_h + content_h + 2.0 * pad).min((anchor.y - 16.0).max(header_h + row_h * 3.0));
        let area = Rect::new(anchor.x, anchor.y - 6.0 - height, width, height);
        self.menu_rect = Some(area);

        p.shadow(Rect::new(area.x, area.y + 6.0, area.w, area.h), t.shadow, theme::RADIUS, 18.0);
        p.bordered(area, t.surface, theme::RADIUS, 1.0, t.border_strong);
        p.label(header.0, theme::CAPTION, area.x + 12.0, area.y + 9.0, t.text_faint);
        let right = p.layout(header.1, theme::CAPTION, None);
        p.text(&right, area.right() - 12.0 - right.width(), area.y + 9.0, t.text_faint);
        p.rect(Rect::new(area.x, area.y + header_h, area.w, 1.0), t.border, 0.0);

        let list = Rect::new(area.x, area.y + header_h + pad, area.w, area.h - header_h - 2.0 * pad);
        if items.is_empty() {
            p.label("Loading…", theme::SMALL, list.x + 12.0, list.y + 7.0, t.text_muted);
            return None;
        }
        if ui.hovered(area) {
            self.menu_scroll += ui.scroll;
            ui.scroll = 0.0;
        }
        self.menu_scroll = self.menu_scroll.clamp(0.0, (content_h - list.h).max(0.0));

        let clip = p.push_clip(list);
        let mut chosen = None;
        for (i, item) in items.iter().enumerate() {
            let row = Rect::new(area.x + 4.0, list.y + i as f32 * row_h - self.menu_scroll, area.w - 8.0, row_h);
            if row.bottom() < list.y || row.y > list.bottom() {
                continue;
            }
            let hovered = ui.hovered(row) && list.contains(ui.mouse);
            let hover = ui.anim(id(("menu", header.0, i)), f32::from(u8::from(hovered)));
            p.rect(row, fade(t.hover, hover), theme::RADIUS_SM);

            let centre = |layout: &TextLayout| row.y + (row.h - layout.height()) * 0.5;
            if item.selected {
                p.label_centered("✓", theme::SMALL, Rect::new(row.x + 4.0, row.y, 16.0, row.h), t.accent);
            }
            let detail = p.layout(&item.detail, theme::SMALL, None);
            p.text(&detail, row.right() - 10.0 - detail.width(), centre(&detail), t.text_faint);
            let mut label = p.layout(&item.label, theme::SMALL, None);
            label.truncate(p.fonts, row.w - 50.0 - detail.width());
            p.text(&label, row.x + 24.0, centre(&label), t.text);

            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(row) {
                    chosen = Some(i);
                }
            }
        }
        p.set_clip(clip);
        chosen
    }

    /// Column holding messages and the composer inside `main`.
    fn column(main: Rect) -> (f32, f32) {
        let width = theme::COLUMN_WIDTH.min(main.w - 64.0);
        (main.x + ((main.w - width) * 0.5).round(), width)
    }

    /// Draws the composer. Returns its top edge and the toolbar's model and
    /// reasoning buttons (menu anchors).
    fn draw_composer(&mut self, p: &mut Painter, ui: &mut Ui, main: Rect, actions: &mut Vec<Action>) -> (f32, [Rect; 2]) {
        let t = p.theme;
        let (x, width) = Self::column(main);
        let pad = 12.0;
        let toolbar_h = 40.0;
        let text_w = width - 2.0 * pad;
        let layout = p.layout(self.composer.text(), theme::BODY, Some(text_w));
        let line_h = layout.line_height();
        let visible_h = layout.line_count().min(COMPOSER_MAX_LINES) as f32 * line_h;
        let card_h = pad + visible_h + toolbar_h;
        let card = Rect::new(x, main.bottom() - 20.0 - card_h, width, card_h);
        let text_area = Rect::new(card.x + pad, card.y + pad, text_w, visible_h);

        // Keep the caret inside the visible part of a tall draft.
        let (caret_x, caret_y) = layout.caret(self.composer.cursor());
        self.composer_scroll = self
            .composer_scroll
            .clamp(caret_y + line_h - visible_h, caret_y)
            .clamp(0.0, (layout.height() - visible_h).max(0.0));
        let origin = (text_area.x, text_area.y - self.composer_scroll);

        // Mouse: click to place the caret, drag to select.
        let hit = |ui: &Ui| layout.hit(ui.mouse.0 - origin.0, ui.mouse.1 - origin.1);
        if ui.hovered(Rect::new(card.x, card.y, card.w, card.h - toolbar_h + 4.0)) {
            ui.cursor = CursorIcon::Text;
            if ui.pressed {
                self.composer.set_cursor(hit(ui), ui.mods.shift_key());
                self.selecting = true;
                ui.last_edit = ui.time;
            }
        }
        if self.selecting {
            if ui.down {
                self.composer.set_cursor(hit(ui), true);
            } else {
                self.selecting = false;
            }
        }

        let focus = ui.anim(id("composer-focus"), f32::from(u8::from(ui.focused)));
        p.shadow(Rect::new(card.x, card.y + 4.0, card.w, card.h), t.shadow, theme::RADIUS, 14.0);
        p.bordered(card, t.surface, theme::RADIUS, 1.0, mix(t.border_strong, t.border_focus, focus));

        let clip = p.push_clip(Rect::new(text_area.x - 2.0, text_area.y, text_area.w + 4.0, text_area.h));
        let selection = self.composer.selection();
        if !selection.is_empty() {
            for (start, end, y) in layout.line_spans() {
                let (from, to) = (selection.start.max(start), selection.end.min(end));
                if from > to || (from == to && selection.end <= end) {
                    continue;
                }
                let x0 = layout.caret(from).0;
                // A selected line break shows as a small tail.
                let x1 = if selection.end > end { layout.caret(to).0 + 6.0 } else { layout.caret(to).0 };
                p.rect(Rect::new(origin.0 + x0, origin.1 + y, x1 - x0, line_h), t.selection, 2.0);
            }
        }
        if self.composer.text().is_empty() {
            p.label("Message SereChat…", theme::BODY, origin.0, origin.1, t.text_faint);
        } else {
            p.text(&layout, origin.0, origin.1, t.text);
        }
        if ui.caret_visible() && selection.is_empty() {
            p.rect(Rect::new(origin.0 + caret_x - 1.0, origin.1 + caret_y + 3.0, 2.0, line_h - 6.0), t.accent, 0.0);
        }
        p.set_clip(clip);

        // Toolbar: model and reasoning menus on the left, send/stop on the right.
        let item_y = card.bottom() - 8.0 - 26.0;
        let model_name = model_name(&self.models, &self.model).to_owned();
        let model = self.toolbar_button(p, ui, card.x + 6.0, item_y, &model_name, Menu::Model);
        let reasoning = format!("Reasoning: {}", self.reasoning.label());
        let reasoning = self.toolbar_button(p, ui, model.right() + 4.0, item_y, &reasoning, Menu::Reasoning);

        let streaming = self.current().stream.is_some();
        let has_text = !self.composer.text().trim().is_empty();
        let send = Rect::new(card.right() - 8.0 - 26.0, item_y, 26.0, 26.0);
        let hovered = ui.hovered(send) && (streaming || has_text);
        let hover = ui.anim(id("send"), f32::from(u8::from(hovered)));
        if streaming {
            p.rect(send, mix(t.hover, t.active, hover), theme::RADIUS_SM);
            p.rect(Rect::new(send.x + 9.0, send.y + 9.0, 8.0, 8.0), t.text, 1.5);
        } else if has_text {
            p.rect(send, fade(t.accent, 1.0 - 0.14 * hover), theme::RADIUS_SM);
            p.label_centered("↑", Style::semibold(15.0), send, t.on_accent);
        } else {
            p.rect(send, t.hover, theme::RADIUS_SM);
            p.label_centered("↑", Style::semibold(15.0), send, t.text_faint);
        }
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(send) {
                if streaming {
                    self.stop(actions);
                } else {
                    self.send(actions);
                }
            }
        }

        self.composer_layout = Some((layout, origin));
        (card.y, [model, reasoning])
    }

    /// A ghost button with a chevron that toggles `menu`. Returns its rect.
    fn toolbar_button(&mut self, p: &mut Painter, ui: &mut Ui, x: f32, y: f32, label: &str, menu: Menu) -> Rect {
        let t = p.theme;
        let text = p.layout(label, theme::SMALL, None);
        let rect = Rect::new(x, y, text.width() + 34.0, 26.0);
        let open = self.menu == Some(menu);
        let hovered = ui.hovered(rect);
        let hover = ui.anim(id(("toolbar", menu == Menu::Model)), f32::from(u8::from(hovered || open)));
        p.rect(rect, fade(t.hover, hover), theme::RADIUS_SM);
        let color = mix(t.text_muted, t.text, hover);
        p.text(&text, rect.x + 8.0, y + (26.0 - text.height()) * 0.5, color);
        chevron(p, rect.right() - 17.0, y + 11.0, true, color);
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(rect) {
                self.menu = if open { None } else { Some(menu) };
                self.menu_scroll = 0.0;
            }
        }
        rect
    }

    fn draw_messages(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        let (x, width) = Self::column(Rect::new(view.x, 0.0, view.w, 0.0));
        let dt = ui.dt;
        let copied = self.copied.filter(|(_, at)| ui.time - at < 1.5);
        let current = self.current;
        let models = &self.models;
        let conversation = self
            .conversations
            .iter_mut()
            .find(|c| c.id == current)
            .expect("the current conversation always exists");

        if conversation.entries.is_empty() {
            draw_empty(p, view);
            return;
        }
        let streaming = conversation.stream.is_some();

        // Measure everything (layouts are cached) to know the scroll range.
        let content_h = 24.0 + conversation.entries.iter_mut().map(|e| e.measure(p, width) + MESSAGE_GAP).sum::<f32>();
        let max_scroll = (content_h - view.h).max(0.0);

        if ui.hovered(view) && ui.scroll != 0.0 {
            self.scroll_target = (self.scroll_target + ui.scroll).clamp(0.0, max_scroll);
            self.stick_to_bottom = self.scroll_target >= max_scroll - 1.0;
        }
        if self.stick_to_bottom {
            self.scroll_target = max_scroll;
        }
        self.scroll_target = self.scroll_target.min(max_scroll);
        self.scroll += (self.scroll_target - self.scroll) * (1.0 - (-dt * 18.0).exp());
        if (self.scroll_target - self.scroll).abs() < 0.5 {
            self.scroll = self.scroll_target;
        } else {
            ui.animating = true;
        }

        // Widgets scrolled under the header must not react.
        let in_view = view.contains(ui.mouse);
        let clip = p.push_clip(view);
        let mut y = view.y + 24.0 - self.scroll.round();
        let last = conversation.entries.len() - 1;
        let mut copy = None;
        for (index, entry) in conversation.entries.iter_mut().enumerate() {
            let height = entry.measure(p, width);
            let area = Rect::new(x, y, width, height);
            y += height + MESSAGE_GAP;
            if area.bottom() < view.y || area.y > view.bottom() {
                continue;
            }
            if entry.boxed() {
                let (fill, border, color) = if entry.message.failed {
                    (fade(t.danger, 0.08), fade(t.danger, 0.4), t.danger)
                } else {
                    (t.surface, t.border, t.text)
                };
                p.bordered(area, fill, theme::RADIUS, 1.0, border);
                if let Some((_, layout)) = &entry.layout {
                    p.text(layout, area.x + BOX_PAD.0, area.y + BOX_PAD.1, color);
                }
                continue;
            }

            let live = streaming && index == last;
            if live && entry.message.content.is_empty() {
                // "Thinking" dots.
                for dot in 0..3 {
                    let phase = (ui.time * 5.0 - dot as f32 * 0.7).sin() * 0.5 + 0.5;
                    let dot_rect = Rect::new(x + dot as f32 * 11.0, area.y + 9.0 - phase * 3.0, 6.0, 6.0);
                    p.rect(dot_rect, fade(t.text_muted, 0.3 + 0.7 * phase), 3.0);
                }
                ui.animating = true;
                continue;
            }

            let mut text_y = area.y;
            if !entry.message.reasoning.is_empty() {
                let label = p.layout("Reasoning", theme::SMALL, None);
                let toggle = Rect::new(x - 6.0, text_y, label.width() + 34.0, 26.0);
                let hovered = in_view && ui.hovered(toggle);
                let hover = ui.anim(id(("reasoning", entry.id)), f32::from(u8::from(hovered)));
                p.rect(toggle, fade(t.hover, hover), theme::RADIUS_SM);
                let color = mix(t.text_muted, t.text, hover);
                let (cx, cy) = if entry.show_reasoning { (toggle.x + 8.0, toggle.y + 11.0) } else { (toggle.x + 10.0, toggle.y + 9.5) };
                chevron(p, cx, cy, entry.show_reasoning, color);
                p.text(&label, toggle.x + 24.0, toggle.y + (26.0 - label.height()) * 0.5, color);
                if hovered {
                    ui.cursor = CursorIcon::Pointer;
                    if ui.clicked(toggle) {
                        entry.show_reasoning = !entry.show_reasoning;
                        ui.animating = true;
                    }
                }
                text_y += REASONING_ROW;
                if let Some((_, reasoning)) = entry.reasoning_layout.as_ref().filter(|_| entry.show_reasoning) {
                    p.rect(Rect::new(x, text_y, 2.0, reasoning.height()), t.border_strong, 1.0);
                    p.text(reasoning, x + 14.0, text_y, t.text_muted);
                    text_y += reasoning.height() + 12.0;
                }
            }
            let Some((_, layout)) = &entry.layout else { continue };
            p.text(layout, x, text_y, t.text);
            if live {
                continue;
            }

            // Caption and hover actions under a finished reply.
            let meta_y = text_y + layout.height() + 6.0;
            if let Some(model) = &entry.message.model {
                let text = usage_caption(model_name(models, model), entry.message.usage, entry.message.cost);
                let caption = p.layout(&text, theme::TINY, None);
                p.text(&caption, x, meta_y + (24.0 - caption.height()) * 0.5, t.text_faint);
            }
            let is_copied = copied.is_some_and(|(id, _)| id == entry.id);
            if (in_view && ui.hovered(area)) || is_copied {
                let label = if is_copied { "✓ Copied" } else { "Copy" };
                if button(p, ui, Rect::new(area.right() - 72.0, meta_y, 72.0, 24.0), label, ButtonStyle::Ghost, true) {
                    copy = Some((entry.id, entry.message.content.clone()));
                }
            }
        }
        p.set_clip(clip);

        if let Some((id, text)) = copy {
            self.copied = Some((id, ui.time));
            actions.push(Action::Copy(text));
        }
        if copied.is_some() {
            ui.animating = true;
        }

        // Scrollbar.
        if max_scroll > 0.0 {
            let thumb_h = (view.h * view.h / content_h).max(32.0);
            let thumb_y = view.y + (view.h - thumb_h) * (self.scroll / max_scroll);
            let track = Rect::new(view.right() - 12.0, view.y, 12.0, view.h);
            let hover = ui.anim(id("scrollbar"), f32::from(u8::from(ui.hovered(track))));
            p.rect(Rect::new(view.right() - 9.0, thumb_y, 6.0, thumb_h), fade(t.text, 0.1 + 0.1 * hover), 3.0);
        }
    }
}

/// The welcome state of an empty conversation.
fn draw_empty(p: &mut Painter, view: Rect) {
    let t = p.theme;
    let cx = view.x + view.w * 0.5;
    let top = view.y + (view.h * 0.5 - 150.0).max(24.0);
    logo(p, Rect::new(cx - 18.0, top, 36.0, 36.0));
    let title = p.layout("What can I help with?", theme::TITLE, None);
    p.text_aligned(&title, view.x, top + 56.0, Align::Center, view.w, t.text);
    let subtitle = p.layout("Ask anything. Every session is saved on this device.", theme::SMALL, None);
    p.text_aligned(&subtitle, view.x, top + 92.0, Align::Center, view.w, t.text_muted);

    // Keyboard shortcuts, the way Zed's welcome page lists them.
    let new_chat = format!("{PRIMARY_KEY}+N");
    let settings = format!("{PRIMARY_KEY}+,");
    let shortcuts = [
        ("New chat", new_chat.as_str()),
        ("Send", "Enter"),
        ("New line", "Shift+Enter"),
        ("Stop reply", "Esc"),
        ("Settings", settings.as_str()),
    ];
    let width = 260.0;
    let mut y = top + 136.0;
    for (label, keys) in shortcuts {
        let text = p.layout(label, theme::SMALL, None);
        p.text(&text, cx - width * 0.5, y + (26.0 - text.height()) * 0.5, t.text_muted);
        keycap(p, keys, cx + width * 0.5, y + 3.0);
        y += 30.0;
    }
}

/// Draws a left-aligned list row with an optional right-hand hint and
/// returns whether it was clicked.
fn list_row(p: &mut Painter, ui: &mut Ui, rect: Rect, key: u64, label: &str, hint: Option<&str>, selected: bool) -> bool {
    let t = p.theme;
    let hovered = ui.hovered(rect);
    let hover = ui.anim(key, f32::from(u8::from(hovered)));
    p.rect(rect, if selected { t.active } else { fade(t.hover, hover) }, theme::RADIUS_SM);
    let mut reserved = 24.0;
    if let Some(hint) = hint {
        let hint = p.layout(hint, theme::TINY, None);
        reserved = hint.width() + 28.0;
        p.text(&hint, rect.right() - 10.0 - hint.width(), rect.y + (rect.h - hint.height()) * 0.5, t.text_faint);
    }
    let mut layout = p.layout(label, theme::SMALL, None);
    layout.truncate(p.fonts, rect.w - 10.0 - reserved);
    let color = if selected { t.text } else { mix(t.text_muted, t.text, hover) };
    p.text(&layout, rect.x + 10.0, rect.y + (rect.h - layout.height()) * 0.5, color);
    if hovered {
        ui.cursor = CursorIcon::Pointer;
    }
    ui.clicked(rect)
}

/// Display name of a model id.
fn model_name<'a>(models: &'a [Model], id: &'a str) -> &'a str {
    models.iter().find(|m| m.id == id && !m.name.is_empty()).map_or(id, |m| m.name.as_str())
}

/// A model's prices, e.g. `$2 / $10`; empty when the server sent none.
fn price(model: &Model) -> String {
    if model.input_cost_per_million > 0.0 || model.output_cost_per_million > 0.0 {
        format!("${} / ${}", model.input_cost_per_million, model.output_cost_per_million)
    } else {
        String::new()
    }
}

/// Caption for a finished reply, e.g. `Claude Sonnet 5.5 · 1,204 tokens · $0.0031`.
fn usage_caption(model: &str, usage: Usage, cost: f64) -> String {
    let tokens = usage.input_tokens + usage.output_tokens;
    match (tokens, cost > 0.0) {
        (0, _) => model.to_owned(),
        (_, true) => format!("{model}  ·  {} tokens  ·  {}", group_digits(tokens), format_cost(cost)),
        (_, false) => format!("{model}  ·  {} tokens", group_digits(tokens)),
    }
}

/// Compact age of a timestamp: `now`, `5m`, `3h`, `2d`, `6w`, `1y`.
fn ago(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..60 => "now".to_owned(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        86_400..604_800 => format!("{}d", secs / 86_400),
        604_800..31_536_000 => format!("{}w", secs / 604_800),
        _ => format!("{}y", secs / 31_536_000),
    }
}

/// `1234567` -> `1,234,567`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A USD amount with enough precision to be meaningful for tiny requests.
pub fn format_cost(usd: f64) -> String {
    if usd <= 0.0 {
        "$0.00".to_owned()
    } else if usd < 0.0001 {
        "<$0.0001".to_owned()
    } else if usd < 0.01 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captions() {
        assert_eq!(group_digits(7), "7");
        assert_eq!(group_digits(1_234), "1,234");
        assert_eq!(group_digits(1_234_567), "1,234,567");
        assert_eq!(format_cost(0.0), "$0.00");
        assert_eq!(format_cost(0.000_01), "<$0.0001");
        assert_eq!(format_cost(0.003_14), "$0.0031");
        assert_eq!(format_cost(1.5), "$1.50");
        let usage = Usage { input_tokens: 1_000, output_tokens: 204 };
        assert_eq!(usage_caption("M", usage, 0.0031), "M  ·  1,204 tokens  ·  $0.0031");
        assert_eq!(usage_caption("M", usage, 0.0), "M  ·  1,204 tokens");
        assert_eq!(usage_caption("M", Usage::default(), 1.0), "M");
    }

    #[test]
    fn ages() {
        assert_eq!(ago(100, 100), "now");
        assert_eq!(ago(100, 200), "now", "clock skew must not underflow");
        assert_eq!(ago(3_600 + 59, 0), "1h");
        assert_eq!(ago(86_400 * 3, 0), "3d");
        assert_eq!(ago(604_800 * 5, 0), "5w");
        assert_eq!(ago(31_536_000 * 2, 0), "2y");
    }

    #[test]
    fn reasoning_keys() {
        for r in Reasoning::ALL {
            assert_eq!(Reasoning::from_key(Some(r.key())), r);
        }
        assert_eq!(Reasoning::Auto.effort(), None);
        assert_eq!(Reasoning::Off.effort(), Some("none"));
        assert_eq!(Reasoning::from_key(Some("bogus")), Reasoning::Auto);
    }

    #[test]
    fn sessions_round_trip_through_the_screen() {
        let mut reply = StoredMessage::new(Role::Assistant, "hello".into());
        reply.cost = 0.5;
        reply.usage = Usage { input_tokens: 3, output_tokens: 4 };
        let session = Session {
            id: "abc".into(),
            title: "Hi".into(),
            created: 1,
            updated: 2,
            messages: vec![StoredMessage::new(Role::User, "hi".into()), reply],
        };
        let empty = Session { id: "empty".into(), ..Session::default() };
        let chat = Chat::new(None, Reasoning::Auto, vec![session.clone(), empty]);
        // The saved session plus a fresh, unsaved conversation; empty files are skipped.
        assert_eq!(chat.conversations.len(), 2);
        let totals = chat.totals();
        assert_eq!((totals.sessions, totals.tokens), (1, 7));
        let restored = chat.conversations.iter().find(|c| c.session_id == "abc").map(Conversation::to_session);
        assert_eq!(restored, Some(session));
    }
}
