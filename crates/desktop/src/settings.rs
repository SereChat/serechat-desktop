//! Settings page: appearance and the browser, skills, MCP servers, usage, and the account
//! with updates.

use std::collections::HashSet;

use arboard::Clipboard;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use crate::app::{Action, McpAction};
use crate::browser::{self, Installed};
use crate::chat::{ReasoningView, format_cost, group_digits};
use crate::mcp::{ServerView, Status as McpStatus};
use crate::paint::{Painter, Rect, fade, mix};
use crate::skills::Catalog;
use crate::text::{Align, Style};
use crate::theme::{self, Palette, Scheme};
use crate::ui::{ButtonStyle, FieldStyle, TextField, Ui, button, edit_key, id, keycap, switch, text_field};
use crate::update::Status as UpdateStatus;

/// Widest the settings column gets.
const CONTENT_WIDTH: f32 = 680.0;
/// Space between sections.
const SECTION_GAP: f32 = 40.0;
/// Height of a row inside a settings group.
const ROW_H: f32 = 68.0;
/// Height of a tool's row in an opened MCP server card.
const TOOL_ROW: f32 = 24.0;

/// Usage summed over every saved session.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    /// USD spent.
    pub cost: f64,
    /// Tokens billed.
    pub tokens: u64,
    /// Sessions with at least one message.
    pub sessions: usize,
}

/// The settings tabs, in header order.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    General,
    Skills,
    Mcp,
    Usage,
    Account,
}

impl Tab {
    const ALL: [Self; 5] = [Self::General, Self::Skills, Self::Mcp, Self::Usage, Self::Account];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Skills => "Skills",
            Self::Mcp => "MCP",
            Self::Usage => "Usage",
            Self::Account => "Account",
        }
    }
}

/// The add-server form's fields, in tab order.
const NAME: usize = 0;
const TARGET: usize = 1;
const EXTRA: usize = 2;

/// Open tab and scroll state of the settings page, and what it lists.
#[derive(Default)]
pub struct SettingsView {
    tab: Tab,
    scroll: f32,
    /// Content height measured last frame, for clamping the scroll.
    content_h: f32,
    /// The skills found; `None` while looking.
    skills: Option<Catalog>,
    /// MCP servers and their state.
    mcp: Vec<ServerView>,
    /// The latest MCP message (an import's result, a config error): text
    /// and whether it is an error.
    mcp_note: Option<(String, bool)>,
    /// MCP servers whose tool list is open.
    open_servers: HashSet<String>,
    /// MCP server whose Remove button was clicked once.
    confirm_remove: Option<String>,
    /// The add-server form, while open: its fields and error.
    form: Option<Form>,
    /// What the updater is doing.
    update: Option<UpdateStatus>,
    /// Whether updates install themselves.
    auto_update: bool,
    /// The browsers the agent can drive; `None` while looking.
    browsers: Option<Vec<Installed>>,
    /// Key of the browser picked; `None` for Auto.
    browser: Option<String>,
    /// The focused field's caret, for the input method's window.
    caret: Option<Rect>,
}

/// The add-server form.
#[derive(Default)]
struct Form {
    fields: [TextField; 3],
    focus: Option<usize>,
    error: Option<String>,
}

impl SettingsView {
    /// Lists the skills the open chat can use (`None` while looking).
    pub fn set_skills(&mut self, report: Option<Catalog>) {
        self.skills = report;
    }

    /// Shows the MCP servers' current state.
    pub fn set_mcp(&mut self, servers: Vec<ServerView>) {
        self.open_servers.retain(|name| servers.iter().any(|s| &s.name == name));
        self.mcp = servers;
    }

    /// Shows a message on the MCP tab (`error` in the danger colour).
    pub fn set_mcp_note(&mut self, note: Option<(String, bool)>) {
        self.mcp_note = note;
    }

    /// Shows what the updater is doing.
    pub fn set_update(&mut self, status: UpdateStatus, auto: bool) {
        self.update = Some(status);
        self.auto_update = auto;
    }

    /// Shows the browsers found, and the one picked (`None`: Auto).
    pub fn set_browsers(&mut self, found: Vec<Installed>, choice: Option<String>) {
        self.browsers = Some(found);
        self.browser = choice;
    }

    /// Where the focused field's caret is.
    #[must_use]
    pub fn caret(&self) -> Option<Rect> {
        self.caret.filter(|_| self.form.as_ref().is_some_and(|f| f.focus.is_some()))
    }

    /// Text committed by an input method, for the focused field.
    pub fn insert(&mut self, text: &str) {
        if let Some(form) = &mut self.form
            && let Some(focus) = form.focus
        {
            form.fields[focus].editor.insert(text);
        }
    }

    /// Keyboard input while a field has focus. Returns whether it was used.
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState, cb: &mut Option<Clipboard>, actions: &mut Vec<Action>) -> bool {
        let Some(form) = &mut self.form else { return false };
        let Some(focus) = form.focus else { return false };
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.form = None,
            Key::Named(NamedKey::Tab) => form.focus = Some(if mods.shift_key() { (focus + 2) % 3 } else { (focus + 1) % 3 }),
            Key::Named(NamedKey::Enter) if focus == EXTRA && !mods.control_key() && !mods.super_key() => form.fields[EXTRA].editor.insert("\n"),
            Key::Named(NamedKey::Enter) => self.submit(actions),
            _ => return edit_key(&mut form.fields[focus].editor, event, mods, cb),
        }
        true
    }

    /// Adds the server the form describes, or says what is wrong.
    fn submit(&mut self, actions: &mut Vec<Action>) {
        let Some(form) = &mut self.form else { return };
        let [name, target, extra] = &form.fields;
        match crate::mcp::config::from_form(name.editor.text(), target.editor.text(), extra.editor.text()) {
            Ok(server) if self.mcp.iter().any(|s| s.name == server.name) => {
                form.error = Some(format!("There is already a server named {}.", server.name));
                form.focus = Some(NAME);
            }
            Ok(server) => {
                actions.push(Action::Mcp(McpAction::Add(Box::new(server))));
                self.form = None;
            }
            Err(e) => form.error = Some(e),
        }
    }

    /// Draws the page into `area` (the whole main area, header included).
    #[allow(clippy::too_many_arguments, reason = "one argument per setting shown; a struct would only rename them")]
    pub fn draw(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        area: Rect,
        scheme: Scheme,
        reasoning: ReasoningView,
        totals: Totals,
        actions: &mut Vec<Action>,
    ) {
        let t = p.theme;
        let bar = Rect::new(area.x, area.y, area.w, theme::HEADER_HEIGHT);
        p.rect(Rect::new(bar.x, bar.bottom() - 1.0, bar.w, 1.0), t.border, 0.0);
        let title = p.layout("Settings", theme::LABEL, None);
        p.text(&title, bar.x + 16.0, bar.y + (bar.h - title.height()) * 0.5, t.text);
        let close = p.layout("to close", theme::TINY, None);
        let close_x = bar.right() - 16.0 - close.width();
        p.text(&close, close_x, bar.y + (bar.h - close.height()) * 0.5, t.text_faint);
        keycap(p, "Esc", close_x - 6.0, bar.y + (bar.h - 20.0) * 0.5);
        if let Some(tab) = tabs(p, ui, bar.x + 16.0 + title.width() + 20.0, bar, self.tab) {
            self.tab = tab;
            self.scroll = 0.0;
        }

        let body = Rect::new(area.x, bar.bottom(), area.w, area.h - bar.h);
        if ui.hovered(body) {
            self.scroll += ui.scroll;
        }
        self.scroll = self.scroll.clamp(0.0, (self.content_h - body.h).max(0.0));
        let width = CONTENT_WIDTH.min(body.w - 64.0);
        let x = body.x + ((body.w - width) * 0.5).round();

        // Content scrolled under the header must not react to the mouse.
        ui.blocker = Some(bar);
        let clip = p.push_clip(body);
        let top = body.y + 32.0 - self.scroll.round();
        let mut y = top;
        // Theme cards and usage stats share a three-column grid.
        let card_w = ((width - 24.0) / 3.0).floor();
        self.caret = None;

        match self.tab {
            Tab::General => {
                y = section(p, x, y, "Colour scheme", "It applies instantly.");
                let preview_h = (card_w * 0.58).round();
                let card_h = preview_h + 52.0;
                for (i, choice) in Scheme::ALL.into_iter().enumerate() {
                    let card = Rect::new(x + i as f32 * (card_w + 12.0), y, card_w, card_h);
                    if theme_card(p, ui, card, preview_h, choice, choice == scheme) && choice != scheme {
                        actions.push(Action::SetTheme(choice));
                    }
                }
                y += card_h + SECTION_GAP;

                y = section(p, x, y, "Chat", "How replies from thinking models look.");
                let row = group(p, x, y, width);
                setting_row(p, row, "Model reasoning", "Show what the model considered before answering.", 264.0);
                let labels = ReasoningView::ALL.map(ReasoningView::label);
                let selected = ReasoningView::ALL.iter().position(|v| *v == reasoning).unwrap_or(0);
                if let Some(choice) = segmented(p, ui, control(row, 264.0), &labels, selected) {
                    actions.push(Action::SetReasoningView(ReasoningView::ALL[choice]));
                }
                y += ROW_H + SECTION_GAP;
                y = self.draw_browsers(p, ui, x, y, width, actions);
            }
            Tab::Skills => y = self.draw_skills(p, x, y, width),
            Tab::Mcp => y = self.draw_mcp(p, ui, x, y, width, actions),
            Tab::Usage => {
                y = section(p, x, y, "Usage", "Totals across every session saved on this device.");
                let stats = [
                    ("Total spent", format_cost(totals.cost)),
                    ("Tokens", group_digits(totals.tokens)),
                    ("Sessions", group_digits(totals.sessions as u64)),
                ];
                for (i, (label, value)) in stats.iter().enumerate() {
                    let stat = Rect::new(x + i as f32 * (card_w + 12.0), y, card_w, 84.0);
                    p.bordered(stat, t.surface, theme::RADIUS, 1.0, t.border);
                    p.label(label, theme::CAPTION, stat.x + 16.0, stat.y + 16.0, t.text_faint);
                    p.label(value, Style::semibold(24.0), stat.x + 16.0, stat.y + 38.0, t.text);
                }
                y += 84.0;
            }
            Tab::Account => {
                y = self.draw_updates(p, ui, x, y, width, actions);
                y += SECTION_GAP;

                y = section(p, x, y, "Data", "Your sessions never leave this device except to reach the model.");
                let row = group(p, x, y, width);
                setting_row(p, row, "Saved sessions", "Stored as JSON files in ~/.serechat/sessions", 116.0);
                if button(p, ui, control(row, 116.0), "Open folder", ButtonStyle::Secondary, true) {
                    actions.push(Action::OpenDataDir);
                }
                y += ROW_H + SECTION_GAP;

                y = section(p, x, y, "Account", "Manage how this device is signed in to SereChat.");
                let row = group(p, x, y, width);
                setting_row(p, row, "SereChat account", "Signed in on this device. Signing out keeps your sessions.", 96.0);
                if button(p, ui, control(row, 96.0), "Sign out", ButtonStyle::Danger, true) {
                    actions.push(Action::SignOut);
                }
                y += ROW_H + 32.0;

                let footer = p.layout(concat!("SereChat Desktop ", env!("CARGO_PKG_VERSION")), theme::TINY, None);
                p.text_aligned(&footer, x, y, Align::Center, width, t.text_faint);
                y += footer.height();
            }
        }

        p.set_clip(clip);
        ui.blocker = None;
        self.content_h = y - top + 64.0;
    }
}

impl SettingsView {
    /// The browser picker: Auto, then each browser found. Returns where it ends.
    fn draw_browsers(&mut self, p: &mut Painter, ui: &mut Ui, x: f32, y: f32, width: f32, actions: &mut Vec<Action>) -> f32 {
        let t = p.theme;
        let y = section(p, x, y, "Browser", "The agent browses in a window of its own, with a fresh profile, never yours.");
        let Some(found) = &self.browsers else {
            let row = group(p, x, y, width);
            setting_row(p, row, "Looking for browsers…", "Checking where Chrome-family browsers are installed.", 0.0);
            return y + ROW_H;
        };
        // (key, title, description)
        let auto = match found.first() {
            Some(first) => format!("Uses {}, the first found of Chrome, Brave, Helium, Edge and Chromium.", first.name),
            None => "No supported browser found. Install Chrome, Brave, Helium, Edge or Chromium.".to_owned(),
        };
        let mut rows: Vec<(Option<String>, String, String)> = vec![(None, "Auto".to_owned(), auto)];
        rows.extend(found.iter().map(|b| (Some(b.key.to_owned()), b.name.to_owned(), b.path.display().to_string())));
        if let Some(key) = &self.browser
            && !found.iter().any(|b| b.key == key)
        {
            rows.push((Some(key.clone()), browser::name(key).unwrap_or(key).to_owned(), "Not found on this computer, so Auto is used.".to_owned()));
        }
        let list = Rect::new(x, y, width, ROW_H * rows.len() as f32);
        p.bordered(list, t.surface, theme::RADIUS, 1.0, t.border);
        for (i, (key, title, text)) in rows.into_iter().enumerate() {
            let row = Rect::new(x, y + i as f32 * ROW_H, width, ROW_H);
            if i > 0 {
                p.rect(Rect::new(row.x + 1.0, row.y, row.w - 2.0, 1.0), t.border, 0.0);
            }
            let selected = key == self.browser;
            let hovered = !selected && ui.hovered(row);
            let hover = ui.anim(id(("browser-row", i)), f32::from(u8::from(hovered)));
            p.rect(Rect::new(row.x + 4.0, row.y + 4.0, row.w - 8.0, row.h - 8.0), fade(t.hover, hover), theme::RADIUS_SM);
            radio(p, Rect::new(row.x + 16.0, row.y + (row.h - 14.0) * 0.5, 14.0, 14.0), selected);
            p.label(&title, theme::LABEL, row.x + 42.0, row.y + 15.0, t.text);
            let mut text = p.layout(&text, theme::SMALL, None);
            text.truncate(p.fonts, row.w - 58.0);
            p.text(&text, row.x + 42.0, row.y + 36.0, t.text_muted);
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(row) {
                    self.browser.clone_from(&key);
                    actions.push(Action::SetBrowser(key));
                }
            }
        }
        list.bottom()
    }

    /// The skills the agent can load in the open chat, and the problems
    /// found reading them. Returns where the section ends.
    fn draw_skills(&self, p: &mut Painter, x: f32, y: f32, width: f32) -> f32 {
        let t = p.theme;
        let mut y = section(p, x, y, "Skills", "Yours from ~/.agents/skills, plus a project's own from its .agents/skills.");
        let Some(report) = &self.skills else {
            let row = group(p, x, y, width);
            setting_row(p, row, "Looking for skills…", "Reading .agents/skills folders.", 0.0);
            return y + ROW_H;
        };
        // (title, description, badge)
        let mut rows: Vec<(String, String, &str)> =
            report.skills.iter().map(|s| (s.name.clone(), s.description.replace('\n', " "), s.scope.label())).collect();
        if report.skills.is_empty() {
            rows.push(("No skills found".to_owned(), "Add one as .agents/skills/<name>/SKILL.md, in a project or your home folder.".to_owned(), ""));
        }
        let list = Rect::new(x, y, width, ROW_H * rows.len() as f32);
        p.bordered(list, t.surface, theme::RADIUS, 1.0, t.border);
        for (i, (title, text, badge)) in rows.iter().enumerate() {
            let row = Rect::new(x, y, width, ROW_H);
            if i > 0 {
                p.rect(Rect::new(row.x + 1.0, row.y, row.w - 2.0, 1.0), t.border, 0.0);
            }
            let badge = p.layout(badge, theme::TINY, None);
            p.text(&badge, row.right() - 16.0 - badge.width(), row.y + 17.0, t.text_faint);
            let mut title = p.layout(title, theme::LABEL, None);
            title.truncate(p.fonts, row.w - 48.0 - badge.width());
            p.text(&title, row.x + 16.0, row.y + 15.0, t.text);
            let mut text = p.layout(text, theme::SMALL, None);
            text.truncate(p.fonts, row.w - 32.0);
            p.text(&text, row.x + 16.0, row.y + 36.0, t.text_muted);
            y += ROW_H;
        }
        for warning in &report.warnings {
            let text = p.layout(warning, theme::SMALL, Some(width));
            p.text(&text, x, y + 8.0, t.danger);
            y += text.height() + 8.0;
        }
        y
    }

    /// The MCP tab: the servers, adding and importing them. Returns where it ends.
    fn draw_mcp(&mut self, p: &mut Painter, ui: &mut Ui, x: f32, y: f32, width: f32, actions: &mut Vec<Action>) -> f32 {
        let t = p.theme;
        let mut y = section(p, x, y, "MCP servers", "Tools from Model Context Protocol servers, offered in every chat. Read-only tools run at once; the rest ask first.");
        let add = Rect::new(x, y, 112.0, 30.0);
        if button(p, ui, add, "Add server", ButtonStyle::Primary, self.form.is_none()) {
            self.form = Some(Form { focus: Some(NAME), ..Form::default() });
        }
        let import = Rect::new(add.right() + 8.0, y, 176.0, 30.0);
        if button(p, ui, import, "Import from clipboard", ButtonStyle::Secondary, true) {
            actions.push(Action::Mcp(McpAction::Import));
        }
        let file = Rect::new(import.right() + 8.0, y, 120.0, 30.0);
        if button(p, ui, file, "Edit mcp.json", ButtonStyle::Ghost, true) {
            actions.push(Action::Mcp(McpAction::OpenFile));
        }
        y += 30.0 + 14.0;
        if let Some((note, error)) = &self.mcp_note {
            let text = p.layout(note, theme::SMALL, Some(width));
            p.text(&text, x, y, if *error { t.danger } else { t.text_muted });
            y += text.height() + 14.0;
        }
        if self.form.is_some() {
            y = self.draw_form(p, ui, x, y, width, actions) + 14.0;
        }
        if self.mcp.is_empty() && self.form.is_none() {
            let card = Rect::new(x, y, width, 76.0);
            p.bordered(card, t.surface, theme::RADIUS, 1.0, t.border);
            p.label("No servers yet", theme::LABEL, card.x + 16.0, card.y + 16.0, t.text);
            let mut hint = p.layout("Add one, or copy a server's settings (its \"mcpServers\" JSON) from its README and import them.", theme::SMALL, None);
            hint.truncate(p.fonts, card.w - 32.0);
            p.text(&hint, card.x + 16.0, card.y + 40.0, t.text_muted);
            return y + card.h;
        }
        let servers = self.mcp.clone();
        let mut confirm_hovered = false;
        for server in &servers {
            let (bottom, hovered) = self.draw_server(p, ui, server, Rect::new(x, y, width, 0.0), actions);
            confirm_hovered |= hovered && self.confirm_remove.as_deref() == Some(&server.name);
            y = bottom + 10.0;
        }
        // Moving off the card cancels a pending removal.
        if !confirm_hovered {
            self.confirm_remove = None;
        }
        y
    }

    /// The add-server form. Returns where it ends.
    fn draw_form(&mut self, p: &mut Painter, ui: &mut Ui, x: f32, y: f32, width: f32, actions: &mut Vec<Action>) -> f32 {
        let t = p.theme;
        let Some(form) = &mut self.form else { return y };
        let web = form.fields[TARGET].editor.text().trim().starts_with("http");
        let label_w = 150.0;
        let field_x = x + 16.0 + label_w;
        let field_w = width - 32.0 - label_w;
        let rows: [(&str, &str, &str, f32); 3] = [
            ("Name", "Its tools are named after it", "github", 32.0),
            ("Command or URL", "What starts it, or where it is", "npx -y @modelcontextprotocol/server-memory   or   https://…/mcp", 32.0),
            if web {
                ("Headers", "Optional, one per line", "Authorization: Bearer ${GITHUB_TOKEN}", 76.0)
            } else {
                ("Environment", "Optional, one per line", "API_KEY=…", 76.0)
            },
        ];
        let card_h = 16.0 + rows.iter().map(|r| r.3 + 12.0).sum::<f32>() + 30.0 + 16.0 + if form.error.is_some() { 24.0 } else { 0.0 };
        let card = Rect::new(x, y, width, card_h);
        p.bordered(card, t.surface, theme::RADIUS, 1.0, t.border_strong);
        let mut row_y = y + 16.0;
        let mut pressed_field = None;
        for (index, (label, hint, placeholder, h)) in rows.iter().enumerate() {
            p.label(label, theme::LABEL, x + 16.0, row_y + 2.0, t.text);
            p.label(hint, theme::TINY, x + 16.0, row_y + 20.0, t.text_faint);
            let rect = Rect::new(field_x, row_y, field_w, *h);
            let style = FieldStyle { placeholder, multiline: index == EXTRA, mono: index != NAME };
            let (pressed, caret) = text_field(p, ui, rect, &mut form.fields[index], form.focus == Some(index), &style);
            if pressed {
                pressed_field = Some(index);
            }
            if caret.is_some() {
                self.caret = caret;
            }
            row_y += h + 12.0;
        }
        if let Some(error) = &form.error {
            p.label(error, theme::SMALL, field_x, row_y, t.danger);
            row_y += 24.0;
        }
        let add = Rect::new(field_x, row_y, 72.0, 30.0);
        let cancel = Rect::new(add.right() + 8.0, row_y, 76.0, 30.0);
        let hint = p.layout("Enter to add · Tab for the next field", theme::TINY, None);
        p.text(&hint, cancel.right() + 16.0, row_y + (30.0 - hint.height()) * 0.5, t.text_faint);
        let submit = button(p, ui, add, "Add", ButtonStyle::Primary, true);
        let cancelled = button(p, ui, cancel, "Cancel", ButtonStyle::Ghost, true);
        if ui.pressed {
            form.focus = pressed_field.or(if ui.hovered(card) { form.focus } else { None });
        }
        if cancelled {
            self.form = None;
        } else if submit {
            self.submit(actions);
        }
        y + card_h
    }

    /// One server's card, starting at `area`'s top. Returns its bottom and
    /// whether the mouse is over it.
    fn draw_server(&mut self, p: &mut Painter, ui: &mut Ui, server: &ServerView, area: Rect, actions: &mut Vec<Action>) -> (f32, bool) {
        let t = p.theme;
        let (x, width) = (area.x, area.w);
        let open = self.open_servers.contains(&server.name);
        let (status, error) = match &server.status {
            McpStatus::Off => ("Off".to_owned(), false),
            McpStatus::Connecting => ("Connecting…".to_owned(), false),
            McpStatus::Ready if server.tools.is_empty() => ("Connected · no tools".to_owned(), false),
            McpStatus::Ready => {
                let count = server.tools.len();
                let shown = if open { "hide" } else { "show" };
                // Some servers need it for every call, others only for more.
                let sign_in = if server.takes_sign_in && !server.signed_in { " · sign-in available" } else { "" };
                (format!("{count} tool{}{sign_in} · {shown}", if count == 1 { "" } else { "s" }), false)
            }
            McpStatus::NeedsSignIn(None) => ("Sign in to use this server.".to_owned(), false),
            McpStatus::NeedsSignIn(Some(e)) | McpStatus::Failed(e) => (e.clone(), true),
            McpStatus::SigningIn => ("Finish signing in in your browser…".to_owned(), false),
        };
        // Controls, right to left: the switch, Remove, then the state's own button.
        let header_y = area.y + 14.0;
        let mut right = x + width - 16.0;
        let toggle = Rect::new(right - 34.0, header_y + 1.0, 34.0, 20.0);
        right = toggle.x - 12.0;
        let confirming = self.confirm_remove.as_deref() == Some(&server.name);
        let remove = Rect::new(right - if confirming { 84.0 } else { 76.0 }, header_y - 4.0, if confirming { 84.0 } else { 76.0 }, 28.0);
        right = remove.x - 8.0;
        let state_button = match (&server.status, server.web) {
            (McpStatus::SigningIn, _) => Some(("Cancel", ButtonStyle::Secondary, McpAction::CancelSignIn(server.name.clone()))),
            (McpStatus::NeedsSignIn(_), true) => Some(("Sign in", ButtonStyle::Primary, McpAction::SignIn(server.name.clone()))),
            (McpStatus::Ready, true) if server.takes_sign_in && !server.signed_in => Some(("Sign in", ButtonStyle::Primary, McpAction::SignIn(server.name.clone()))),
            (McpStatus::Failed(_), _) => Some(("Retry", ButtonStyle::Secondary, McpAction::Reconnect(server.name.clone()))),
            (_, true) if server.signed_in && server.enabled => Some(("Sign out", ButtonStyle::Ghost, McpAction::SignOut(server.name.clone()))),
            _ => None,
        };
        let state_rect = state_button.as_ref().map(|(label, ..)| {
            let w = p.layout(label, theme::LABEL, None).width() + 28.0;
            Rect::new(right - w, header_y - 4.0, w, 28.0)
        });
        if let Some(rect) = state_rect {
            right = rect.x - 8.0;
        }

        // Text: name and version, the command or URL, the status.
        let text_w = right - x - 32.0;
        let status_layout = p.layout(&status, theme::SMALL, error.then_some(width - 48.0));
        let mut height = 14.0 + 20.0 + 4.0 + 18.0 + 6.0 + status_layout.height() + 14.0;
        let tool_rows = if open { server.tools.len() + server.warnings.len() } else { 0 };
        if open {
            height += 8.0 + tool_rows as f32 * TOOL_ROW + 8.0;
        }
        let card = Rect::new(x, area.y, width, height);
        let hovered = ui.hovered(card);
        p.bordered(card, t.surface, theme::RADIUS, 1.0, if hovered { t.border_strong } else { t.border });
        let dot = match &server.status {
            McpStatus::Ready => t.added,
            McpStatus::Failed(_) => t.danger,
            McpStatus::NeedsSignIn(_) | McpStatus::SigningIn => t.accent,
            McpStatus::Connecting => fade(t.text_muted, 0.4 + 0.6 * ((ui.time * 4.0).sin() * 0.5 + 0.5)),
            McpStatus::Off => t.border_strong,
        };
        if server.status == McpStatus::Connecting || server.status == McpStatus::SigningIn {
            ui.animating = true;
        }
        p.rect(Rect::new(x + 16.0, header_y + 6.0, 8.0, 8.0), dot, 4.0);
        let mut name = p.layout(&server.name, theme::LABEL, None);
        name.truncate(p.fonts, text_w - 80.0);
        p.text(&name, x + 32.0, header_y + (20.0 - name.height()) * 0.5, t.text);
        if !server.version.is_empty() && server.status == McpStatus::Ready {
            let version = p.layout(&format!("MCP {}", server.version), theme::TINY, None);
            p.text(&version, x + 40.0 + name.width(), header_y + (20.0 - version.height()) * 0.5, t.text_faint);
        }
        let mut target = p.layout(&server.target, Style::mono(12.0), None);
        target.truncate(p.fonts, width - 48.0);
        p.text(&target, x + 32.0, header_y + 24.0, t.text_muted);
        let status_y = header_y + 24.0 + 18.0 + 6.0;
        p.text(&status_layout, x + 32.0, status_y, if error { t.danger } else { t.text_faint });

        // The tools, when opened.
        if open {
            let list_y = status_y + status_layout.height() + 14.0;
            p.rect(Rect::new(x + 1.0, list_y, width - 2.0, 1.0), t.border, 0.0);
            let mut row_y = list_y + 8.0;
            for (tool, description) in &server.tools {
                let mut name = p.layout(tool, Style::mono(12.0), None);
                name.truncate(p.fonts, width * 0.4);
                p.text(&name, x + 32.0, row_y + (TOOL_ROW - name.height()) * 0.5, t.text);
                let mut text = p.layout(description, theme::SMALL, None);
                text.truncate(p.fonts, (width - 64.0 - name.width() - 16.0).max(0.0));
                p.text(&text, x + 48.0 + name.width(), row_y + (TOOL_ROW - text.height()) * 0.5, t.text_faint);
                row_y += TOOL_ROW;
            }
            for warning in &server.warnings {
                let mut text = p.layout(warning, theme::SMALL, None);
                text.truncate(p.fonts, width - 64.0);
                p.text(&text, x + 32.0, row_y + (TOOL_ROW - text.height()) * 0.5, t.danger);
                row_y += TOOL_ROW;
            }
        }

        // Controls.
        let mut on_control = false;
        if switch(p, ui, toggle, server.enabled, id(("mcp-switch", &server.name))) {
            actions.push(Action::Mcp(McpAction::Enable(server.name.clone(), !server.enabled)));
        }
        on_control |= ui.hovered(toggle);
        let (label, style) = if confirming { ("Confirm", ButtonStyle::Danger) } else { ("Remove", ButtonStyle::Ghost) };
        if button(p, ui, remove, label, style, true) {
            if confirming {
                self.confirm_remove = None;
                actions.push(Action::Mcp(McpAction::Remove(server.name.clone())));
            } else {
                self.confirm_remove = Some(server.name.clone());
            }
        }
        on_control |= ui.hovered(remove);
        if let (Some((label, style, action)), Some(rect)) = (state_button, state_rect) {
            if button(p, ui, rect, label, style, true) {
                actions.push(Action::Mcp(action));
            }
            on_control |= ui.hovered(rect);
        }
        // The card itself opens and closes the tool list.
        let expandable = !server.tools.is_empty() || !server.warnings.is_empty();
        if hovered && !on_control && expandable {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(card) && !self.open_servers.remove(&server.name) {
                self.open_servers.insert(server.name.clone());
            }
        }
        (card.bottom(), hovered)
    }

    /// The updates section. Returns where it ends.
    fn draw_updates(&self, p: &mut Painter, ui: &mut Ui, x: f32, y: f32, width: f32, actions: &mut Vec<Action>) -> f32 {
        let mut y = section(p, x, y, "Updates", "New versions come from the project's GitHub releases.");
        let status = self.update.clone().unwrap_or(UpdateStatus::Idle);
        let text = match &status {
            UpdateStatus::Disabled(why) => why.clone(),
            UpdateStatus::Idle => "Checks for updates shortly after starting.".to_owned(),
            UpdateStatus::Checking => "Checking for updates…".to_owned(),
            UpdateStatus::UpToDate => "You have the latest version.".to_owned(),
            UpdateStatus::Available { version, note: None, .. } => format!("Version {version} is available."),
            UpdateStatus::Available { version, note: Some(note), .. } => format!("Version {version} is available. {note}"),
            UpdateStatus::Installing(version) => format!("Downloading version {version}…"),
            UpdateStatus::Ready(version) => format!("Version {version} is installed. Restart to use it."),
            UpdateStatus::Failed(e) => e.clone(),
        };
        let (label, style, enabled, action) = match &status {
            UpdateStatus::Disabled(_) => ("Check now", ButtonStyle::Secondary, false, None),
            UpdateStatus::Checking => ("Checking…", ButtonStyle::Secondary, false, None),
            UpdateStatus::Installing(_) => ("Installing…", ButtonStyle::Secondary, false, None),
            UpdateStatus::Available { note: None, .. } => ("Install", ButtonStyle::Primary, true, Some(Action::InstallUpdate)),
            UpdateStatus::Available { url, .. } => ("Download", ButtonStyle::Secondary, true, Some(Action::OpenLink(url.clone()))),
            UpdateStatus::Ready(_) => ("Restart", ButtonStyle::Primary, true, Some(Action::RestartToUpdate)),
            UpdateStatus::Idle | UpdateStatus::UpToDate | UpdateStatus::Failed(_) => ("Check now", ButtonStyle::Secondary, true, Some(Action::CheckForUpdates)),
        };
        let row = group(p, x, y, width);
        setting_row(p, row, concat!("SereChat Desktop ", env!("CARGO_PKG_VERSION")), &text, 116.0);
        if button(p, ui, control(row, 116.0), label, style, enabled)
            && let Some(action) = action
        {
            actions.push(action);
        }
        y += ROW_H + 10.0;
        let row = group(p, x, y, width);
        setting_row(p, row, "Install updates automatically", "Download new versions in the background; they start with the next launch.", 44.0);
        let toggle = Rect::new(row.right() - 16.0 - 34.0, row.y + (row.h - 20.0) * 0.5, 34.0, 20.0);
        if switch(p, ui, toggle, self.auto_update, id("auto-update")) {
            actions.push(Action::SetAutoUpdate(!self.auto_update));
        }
        y += ROW_H;
        y
    }
}

/// The tab strip in the header, starting at `x`; the open tab is underlined
/// in the accent. Returns a newly clicked tab.
fn tabs(p: &mut Painter, ui: &mut Ui, mut x: f32, bar: Rect, open: Tab) -> Option<Tab> {
    let t = p.theme;
    let mut clicked = None;
    for tab in Tab::ALL {
        let label = p.layout(tab.label(), theme::LABEL, None);
        let cell = Rect::new(x, bar.y, label.width() + 24.0, bar.h);
        let hovered = tab != open && ui.hovered(cell);
        let hover = ui.anim(id(("settings-tab", tab.label())), f32::from(u8::from(hovered)));
        p.rect(Rect::new(cell.x, cell.y + 8.0, cell.w, cell.h - 16.0), fade(t.hover, hover), theme::RADIUS_SM);
        let color = if tab == open { t.text } else { mix(t.text_muted, t.text, hover) };
        p.text(&label, cell.x + 12.0, cell.y + (cell.h - label.height()) * 0.5, color);
        if tab == open {
            p.rect(Rect::new(cell.x + 8.0, cell.bottom() - 2.0, cell.w - 16.0, 2.0), t.accent, 1.0);
        }
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(cell) {
                clicked = Some(tab);
            }
        }
        x += cell.w;
    }
    clicked
}

/// Section heading with a description; returns where its content starts.
fn section(p: &mut Painter, x: f32, y: f32, title: &str, description: &str) -> f32 {
    let t = p.theme;
    p.label(title, Style::semibold(15.0), x, y, t.text);
    p.label(description, theme::SMALL, x, y + 24.0, t.text_muted);
    y + 58.0
}

/// A bordered group holding one row; returns the row's rect.
fn group(p: &mut Painter, x: f32, y: f32, width: f32) -> Rect {
    let t = p.theme;
    let rect = Rect::new(x, y, width, ROW_H);
    p.bordered(rect, t.surface, theme::RADIUS, 1.0, t.border);
    rect
}

/// Title and description on the left of a settings row, leaving `reserved`
/// pixels on the right for its control.
fn setting_row(p: &mut Painter, row: Rect, title: &str, description: &str, reserved: f32) {
    let t = p.theme;
    p.label(title, theme::LABEL, row.x + 16.0, row.y + 15.0, t.text);
    let mut text = p.layout(description, theme::SMALL, None);
    text.truncate(p.fonts, row.w - 48.0 - reserved);
    p.text(&text, row.x + 16.0, row.y + 36.0, t.text_muted);
}

/// A control of `width` right-aligned in `row`.
fn control(row: Rect, width: f32) -> Rect {
    Rect::new(row.right() - 16.0 - width, row.y + (row.h - 30.0) * 0.5, width, 30.0)
}

/// Equal-width options in one outlined control, the selected one filled.
/// Returns a newly clicked option.
fn segmented(p: &mut Painter, ui: &mut Ui, rect: Rect, labels: &[&str], selected: usize) -> Option<usize> {
    let t = p.theme;
    p.bordered(rect, t.surface, theme::RADIUS_SM, 1.0, t.border_strong);
    let w = rect.w / labels.len() as f32;
    let mut clicked = None;
    for (i, label) in labels.iter().enumerate() {
        let cell = Rect::new((rect.x + i as f32 * w).round(), rect.y, w.round(), rect.h);
        let inner = Rect::new(cell.x + 3.0, cell.y + 3.0, cell.w - 6.0, cell.h - 6.0);
        let hovered = ui.hovered(cell) && i != selected;
        let hover = ui.anim(id(("segment", label, rect.y.to_bits())), f32::from(u8::from(hovered)));
        if i == selected {
            p.rect(inner, t.active, theme::RADIUS_SM - 1.0);
        } else {
            p.rect(inner, fade(t.hover, hover), theme::RADIUS_SM - 1.0);
        }
        let color = if i == selected { t.text } else { mix(t.text_muted, t.text, hover) };
        p.label_centered(label, theme::LABEL, cell, color);
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(cell) {
                clicked = Some(i);
            }
        }
    }
    clicked
}

/// A selectable card previewing `scheme`. Returns whether it was clicked.
fn theme_card(p: &mut Painter, ui: &mut Ui, card: Rect, preview_h: f32, scheme: Scheme, selected: bool) -> bool {
    let t = p.theme;
    let hovered = ui.hovered(card);
    let hover = ui.anim(id(("theme-card", scheme.key())), f32::from(u8::from(hovered)));
    let border = if selected { t.accent } else { mix(t.border, t.border_strong, hover) };
    p.bordered(card, t.surface, theme::RADIUS + 2.0, if selected { 2.0 } else { 1.0 }, border);
    preview(p, Rect::new(card.x + 8.0, card.y + 8.0, card.w - 16.0, preview_h), scheme.palette());

    // Radio button and name.
    let label_y = card.y + preview_h + 16.0;
    radio(p, Rect::new(card.x + 12.0, label_y + 7.0, 14.0, 14.0), selected);
    let name = p.layout(scheme.label(), theme::LABEL, None);
    p.text(&name, card.x + 34.0, label_y + (28.0 - name.height()) * 0.5, t.text);

    if hovered {
        ui.cursor = CursorIcon::Pointer;
    }
    ui.clicked(card)
}

/// A 14px radio button, filled in the accent when `selected`.
fn radio(p: &mut Painter, rect: Rect, selected: bool) {
    let t = p.theme;
    p.bordered(rect, [0.0; 4], 7.0, 1.5, if selected { t.accent } else { t.border_strong });
    if selected {
        p.rect(Rect::new(rect.x + 4.0, rect.y + 4.0, 6.0, 6.0), t.accent, 3.0);
    }
}

/// A miniature of the app drawn in `c`'s colours.
fn preview(p: &mut Painter, r: Rect, c: &Palette) {
    p.bordered(r, c.bg, theme::RADIUS_SM, 1.0, c.border_strong);
    let inner = Rect::new(r.x + 1.0, r.y + 1.0, r.w - 2.0, r.h - 2.0);

    // Sidebar: rounded on the left only, with a hairline on its right.
    let side = Rect::new(inner.x, inner.y, (inner.w * 0.3).round(), inner.h);
    p.rect(side, c.panel, theme::RADIUS_SM - 1.0);
    p.rect(Rect::new(side.right() - 4.0, side.y, 4.0, side.h), c.panel, 0.0);
    p.rect(Rect::new(side.right(), side.y, 1.0, side.h), c.border, 0.0);
    p.rect(Rect::new(side.x + 4.0, side.y + 17.0, side.w - 8.0, 10.0), c.active, 2.0);
    for (i, w) in [0.5, 0.7, 0.55, 0.62, 0.45].into_iter().enumerate() {
        let alpha = if i == 1 { 0.9 } else { 0.45 };
        p.rect(Rect::new(side.x + 7.0, side.y + 8.0 + i as f32 * 10.0, (side.w - 14.0) * w, 3.0), fade(c.text_muted, alpha), 1.5);
    }

    // Chat: a prompt box, reply lines and the composer with its send button.
    let main = Rect::new(side.right() + 1.0, inner.y, inner.right() - side.right() - 1.0, inner.h);
    let pad = (main.w * 0.1).round();
    let col = Rect::new(main.x + pad, main.y, main.w - 2.0 * pad, main.h);
    p.bordered(Rect::new(col.x, col.y + 10.0, col.w, 12.0), c.surface, 2.0, 1.0, c.border);
    p.rect(Rect::new(col.x + 5.0, col.y + 14.5, col.w * 0.4, 3.0), fade(c.text, 0.7), 1.5);
    for (i, w) in [0.92, 0.84, 0.6].into_iter().enumerate() {
        p.rect(Rect::new(col.x, col.y + 30.0 + i as f32 * 8.0, col.w * w, 3.0), fade(c.text, 0.55), 1.5);
    }
    let composer = Rect::new(col.x, main.bottom() - 22.0, col.w, 16.0);
    p.bordered(composer, c.surface, 3.0, 1.0, c.border_strong);
    p.rect(Rect::new(composer.right() - 13.0, composer.y + 3.0, 10.0, 10.0), c.accent, 2.0);
}
