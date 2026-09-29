//! Spotlight: a keyboard-first search over everything (commands, projects,
//! sessions, models and themes), plus full-text search of every saved
//! message on a worker thread.

use arboard::Clipboard;
use serechat::{Model, Project, SearchHit};
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use crate::app::Action;
use crate::editor::Editor;
use crate::paint::{Painter, Rect, hexa};
use crate::text::Style;
use crate::theme::{self, Scheme};
use crate::ui::{Ui, edit_key, keycap};

/// Seconds of typing pause before searching message contents.
const DEBOUNCE: f32 = 0.18;
/// Height of a result row.
const ROW_H: f32 = 40.0;
/// Most results per group.
const PER_GROUP: usize = 6;

/// What the user chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Start a new chat.
    NewChat,
    /// Open settings.
    Settings,
    /// Switch colour scheme.
    Theme(Scheme),
    /// Open a folder as a project.
    OpenFolder,
    /// Attach files.
    Attach,
    /// Switch project (`None`: no project).
    Project(Option<String>),
    /// Open a session by id.
    Session(String),
    /// Use a model.
    Model(String),
}

/// A session as Spotlight sees it.
pub struct SessionRef<'a> {
    /// Session id.
    pub id: &'a str,
    /// Title.
    pub title: &'a str,
    /// Its project, if any.
    pub project: Option<&'a str>,
    /// Last activity.
    pub updated: u64,
}

/// Everything Spotlight searches, borrowed from the chat screen.
pub struct Context<'a> {
    /// Saved sessions.
    pub sessions: Vec<SessionRef<'a>>,
    /// Project folders.
    pub projects: &'a [Project],
    /// Available models.
    pub models: &'a [Model],
    /// Selected model id.
    pub model: &'a str,
    /// Active colour scheme.
    pub scheme: Scheme,
}

/// What Spotlight wants after an event.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Keep it open.
    Stay,
    /// Close it.
    Close,
    /// Close it and act on this.
    Pick(Pick),
}

/// One result row.
struct Row {
    group: &'static str,
    title: String,
    detail: String,
    pick: Pick,
    score: i32,
}

/// Spotlight's state while open.
#[derive(Default)]
pub struct Spotlight {
    input: Editor,
    selected: usize,
    scroll: f32,
    /// Query the rows were built for, to reset the selection when it changes.
    rows_for: String,
    /// Query and time of the last edit, for debouncing content search.
    edited: Option<f32>,
    /// Generation of the latest content search requested.
    generation: u64,
    /// Query that generation searched for.
    searched: String,
    /// Content hits for `searched`.
    hits: Vec<SearchHit>,
    /// Rows from the last frame, for keyboard picks.
    picks: Vec<Pick>,
}

/// Fuzzy score of `query` against `text`: `None` when not every query
/// character appears in order. Rewards prefixes, word starts and runs.
fn fuzzy(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let text_lower = text.to_lowercase();
    if let Some(at) = text_lower.find(query) {
        let word_start = at == 0 || !text_lower[..at].ends_with(char::is_alphanumeric);
        return Some(1000 - at as i32 + if word_start { 200 } else { 0 });
    }
    let mut score = 0;
    let mut previous: Option<usize> = None;
    let chars: Vec<char> = text_lower.chars().collect();
    let mut i = 0;
    for q in query.chars() {
        let found = chars[i..].iter().position(|c| *c == q)? + i;
        score += if previous == Some(found.wrapping_sub(1)) { 15 } else { 1 };
        if found == 0 || !chars[found - 1].is_alphanumeric() {
            score += 10;
        }
        previous = Some(found);
        i = found + 1;
    }
    Some(score - chars.len() as i32 / 8)
}

impl Spotlight {
    /// Text committed by an input method.
    pub fn insert(&mut self, text: &str) {
        self.input.insert(text);
        self.edited = None;
    }

    /// Stores content-search results if they answer the latest request.
    pub fn set_hits(&mut self, generation: u64, hits: Vec<SearchHit>) {
        if generation == self.generation {
            self.hits = hits;
        }
    }

    fn rows(&self, cx: &Context<'_>) -> Vec<Row> {
        let query = self.input.text().trim().to_lowercase();
        let mut rows = Vec::new();
        let mut group = |name: &'static str, mut items: Vec<Row>| {
            items.sort_by_key(|r| std::cmp::Reverse(r.score));
            items.truncate(if query.is_empty() && name == "Sessions" { 8 } else { PER_GROUP });
            rows.extend(items.into_iter().map(|r| Row { group: name, ..r }));
        };
        // Titles match fuzzily; details only as plain substrings, or long
        // descriptions would match almost any short query.
        let row = |title: &str, detail: String, pick: Pick, extra: i32| {
            let in_detail = || (!query.is_empty() && detail.to_lowercase().contains(&query)).then_some(100);
            fuzzy(&query, title).or_else(in_detail).map(|score| Row { group: "", title: title.to_owned(), detail, pick, score: score + extra })
        };

        let commands = [
            ("New chat", "Start a fresh conversation", Pick::NewChat),
            ("Open folder…", "Open a project folder for the agent", Pick::OpenFolder),
            ("Attach files…", "Add images, PDFs or text files", Pick::Attach),
            ("Settings", "Appearance, usage, data and account", Pick::Settings),
        ];
        group("Commands", commands.into_iter().filter_map(|(t, d, p)| row(t, d.to_owned(), p, 0)).collect());
        if !query.is_empty() {
            let themes = Scheme::ALL.into_iter().filter(|s| *s != cx.scheme);
            group("Commands", themes.filter_map(|s| row(&format!("Theme: {}", s.label()), "Switch colour scheme".into(), Pick::Theme(s), -5)).collect());
        }
        let mut projects: Vec<Row> = cx.projects.iter().filter_map(|p| row(&p.name, crate::chat::display_path(&p.path), Pick::Project(Some(p.path.clone())), 0)).collect();
        if !query.is_empty() {
            projects.extend(row("No project", "Chat without tools".into(), Pick::Project(None), -10));
        }
        group("Projects", projects);
        let now = serechat::unix_now();
        group(
            "Sessions",
            cx.sessions
                .iter()
                .filter_map(|s| {
                    let project = s.project.and_then(|p| cx.projects.iter().find(|x| x.path == p)).map_or(String::new(), |p| format!("{}  ·  ", p.name));
                    // Recent sessions first when nothing is typed.
                    let recency = -((now.saturating_sub(s.updated) / 3600).min(10_000) as i32);
                    row(s.title, format!("{project}{}", ago(now, s.updated)), Pick::Session(s.id.to_owned()), if query.is_empty() { recency } else { 0 })
                })
                .collect(),
        );
        if !query.is_empty() {
            group(
                "Models",
                cx.models
                    .iter()
                    .filter_map(|m| {
                        let name = if m.name.is_empty() { &m.id } else { &m.name };
                        let current = if m.id == cx.model { "Current model" } else { "Switch to this model" };
                        row(name, current.into(), Pick::Model(m.id.clone()), -20)
                    })
                    .collect(),
            );
        }
        if self.searched.trim().to_lowercase() == query && !query.is_empty() {
            // Sessions already listed by title are not repeated.
            let listed: Vec<Pick> = rows.iter().map(|r| r.pick.clone()).collect();
            let hits = self.hits.iter().filter(|h| !listed.contains(&Pick::Session(h.session.clone())));
            rows.extend(hits.take(PER_GROUP).map(|h| Row { group: "Messages", title: h.snippet.clone(), detail: h.title.clone(), pick: Pick::Session(h.session.clone()), score: 0 }));
        }
        rows
    }

    /// Keyboard input.
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState, cb: &mut Option<Clipboard>) -> Outcome {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => return Outcome::Close,
            Key::Character(c) if c.eq_ignore_ascii_case("k") && (mods.control_key() || mods.super_key()) => return Outcome::Close,
            Key::Named(NamedKey::Enter) => return self.picks.get(self.selected).cloned().map_or(Outcome::Close, Outcome::Pick),
            Key::Named(NamedKey::ArrowDown) => self.selected = (self.selected + 1).min(self.picks.len().saturating_sub(1)),
            Key::Named(NamedKey::ArrowUp) => self.selected = self.selected.saturating_sub(1),
            Key::Named(NamedKey::Tab) => {
                self.selected = if mods.shift_key() { self.selected.saturating_sub(1) } else { (self.selected + 1) % self.picks.len().max(1) };
            }
            _ => {
                if edit_key(&mut self.input, event, mods, cb) {
                    self.edited = None;
                }
            }
        }
        Outcome::Stay
    }

    /// Draws the modal over `view`.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, cx: &Context<'_>, actions: &mut Vec<Action>) -> Outcome {
        let t = p.theme;
        // Debounced content search.
        let query = self.input.text().trim().to_owned();
        if query != self.rows_for {
            self.rows_for.clone_from(&query);
            self.selected = 0;
            self.scroll = 0.0;
        }
        let edited = *self.edited.get_or_insert(ui.time);
        if query.chars().count() >= 2 && query != self.searched {
            if ui.time - edited >= DEBOUNCE {
                self.generation += 1;
                self.searched.clone_from(&query);
                self.hits.clear();
                actions.push(Action::Search { query: query.clone(), generation: self.generation });
            } else {
                ui.animating = true;
            }
        }

        let rows = self.rows(cx);
        self.picks = rows.iter().map(|r| r.pick.clone()).collect();
        self.selected = self.selected.min(rows.len().saturating_sub(1));

        // Dim everything beneath.
        p.rect(view, hexa(0x000000, 0.4), 0.0);
        let width = 620.0f32.min(view.w - 48.0);
        let list_h = (rows.len().max(1) as f32 * ROW_H + group_count(&rows) as f32 * 26.0).min(view.h * 0.6);
        let height = 56.0 + list_h + 38.0;
        let modal = Rect::new(view.x + ((view.w - width) * 0.5).round(), view.y + (view.h * 0.14).round(), width, height);
        p.shadow(Rect::new(modal.x, modal.y + 12.0, modal.w, modal.h), hexa(0x000000, 0.45), theme::RADIUS * 2.0, 40.0);
        p.bordered(modal, t.surface, theme::RADIUS * 1.5, 1.0, t.border_strong);

        // Search field.
        let field = Rect::new(modal.x, modal.y, modal.w, 56.0);
        magnifier(p, field.x + 20.0, field.y + 20.0, t.text_faint);
        let style = Style::regular(16.0);
        let text = p.layout(self.input.text(), style, None);
        let origin = (field.x + 48.0, field.y + (field.h - text.height()) * 0.5);
        let selection = self.input.selection();
        if !selection.is_empty() {
            let (a, b) = (text.caret(selection.start).0, text.caret(selection.end).0);
            p.rect(Rect::new(origin.0 + a, origin.1, b - a, text.height()), t.selection, 2.0);
        }
        if self.input.text().is_empty() {
            p.label("Search sessions, projects, models and commands…", style, origin.0, origin.1, t.text_faint);
        } else {
            p.text(&text, origin.0, origin.1, t.text);
        }
        if ui.caret_visible() {
            let caret = text.caret(self.input.cursor()).0;
            p.rect(Rect::new(origin.0 + caret - 1.0, origin.1 + 3.0, 2.0, text.height() - 6.0), t.accent, 0.0);
        }
        p.rect(Rect::new(modal.x, field.bottom(), modal.w, 1.0), t.border, 0.0);

        // Results, grouped.
        let list = Rect::new(modal.x, field.bottom() + 1.0, modal.w, list_h);
        let content_h = rows.len() as f32 * ROW_H + group_count(&rows) as f32 * 26.0;
        if ui.hovered(list) {
            self.scroll = (self.scroll + ui.scroll).clamp(0.0, (content_h - list.h).max(0.0));
        }
        // Keep the keyboard selection in view.
        let selected_y = row_y(&rows, self.selected);
        self.scroll = self.scroll.clamp((selected_y + ROW_H - list.h).max(0.0), selected_y.max(0.0)).clamp(0.0, (content_h - list.h).max(0.0));
        let clip = p.push_clip(list);
        let mut picked = None;
        if rows.is_empty() {
            let searching = query.chars().count() >= 2 && query != self.searched;
            let empty = if searching { "Searching…" } else { "No results" };
            p.label(empty, theme::SMALL, list.x + 20.0, list.y + 12.0, t.text_faint);
        }
        let mut y = list.y - self.scroll;
        let mut last_group = "";
        for (index, row) in rows.iter().enumerate() {
            if row.group != last_group {
                last_group = row.group;
                p.label(row.group, theme::CAPTION, list.x + 20.0, y + 8.0, t.text_faint);
                y += 26.0;
            }
            let item = Rect::new(list.x + 6.0, y, list.w - 12.0, ROW_H);
            y += ROW_H;
            if item.bottom() < list.y || item.y > list.bottom() {
                continue;
            }
            let hovered = ui.hovered(item) && list.contains(ui.mouse);
            let selected = index == self.selected;
            if selected || hovered {
                p.rect(item, if selected { t.active } else { t.hover }, theme::RADIUS);
            }
            let mut title = p.layout(&row.title, theme::BODY, None);
            let mut detail = p.layout(&row.detail, theme::SMALL, None);
            detail.truncate(p.fonts, item.w * 0.4);
            title.truncate(p.fonts, item.w - detail.width() - 44.0);
            p.text(&title, item.x + 14.0, item.y + (ROW_H - title.height()) * 0.5, t.text);
            p.text(&detail, item.right() - 14.0 - detail.width(), item.y + (ROW_H - detail.height()) * 0.5, t.text_faint);
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(item) {
                    picked = Some(row.pick.clone());
                }
            }
        }
        p.set_clip(clip);

        // Footer with the keys.
        let footer = Rect::new(modal.x, modal.bottom() - 38.0, modal.w, 38.0);
        p.rect(Rect::new(modal.x, footer.y, modal.w, 1.0), t.border, 0.0);
        let mut right = footer.right() - 16.0;
        for (keys, label) in [("Esc", "close"), ("Enter", "open"), ("↑↓", "navigate")] {
            let text = p.layout(label, theme::TINY, None);
            right -= text.width();
            p.text(&text, right, footer.y + (footer.h - text.height()) * 0.5, t.text_faint);
            right -= 6.0;
            right -= keycap(p, keys, right, footer.y + 9.0) + 16.0;
        }

        if let Some(pick) = picked {
            return Outcome::Pick(pick);
        }
        // Clicking outside closes.
        if ui.released && !modal.contains(ui.press_pos) {
            return Outcome::Close;
        }
        Outcome::Stay
    }
}

fn group_count(rows: &[Row]) -> usize {
    let mut count = 0;
    let mut last = "";
    for row in rows {
        if row.group != last {
            count += 1;
            last = row.group;
        }
    }
    count
}

/// Top of row `index` within the results list.
fn row_y(rows: &[Row], index: usize) -> f32 {
    let mut y = 0.0;
    let mut last = "";
    for (i, row) in rows.iter().enumerate() {
        if row.group != last {
            y += 26.0;
            last = row.group;
        }
        if i == index {
            return y;
        }
        y += ROW_H;
    }
    y
}

fn ago(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{} min ago", secs / 60),
        3_600..86_400 => format!("{} h ago", secs / 3_600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

/// A small magnifying glass: a ring and a stepped handle.
fn magnifier(p: &mut Painter, x: f32, y: f32, color: crate::paint::Color) {
    p.bordered(Rect::new(x, y, 12.0, 12.0), [0.0; 4], 6.0, 1.6, color);
    for i in 0..4 {
        let d = i as f32 * 1.3;
        p.rect(Rect::new(x + 10.0 + d, y + 10.0 + d, 2.0, 2.0), color, 0.5);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_ranks_prefixes_and_word_starts() {
        assert!(fuzzy("set", "Settings").unwrap() > fuzzy("set", "Reset view").unwrap());
        assert!(fuzzy("nc", "New chat").is_some());
        assert!(fuzzy("xyz", "New chat").is_none());
        // Word-start runs beat scattered letters.
        assert!(fuzzy("nc", "New chat").unwrap() > fuzzy("nc", "banana cake").unwrap());
        assert_eq!(fuzzy("", "anything"), Some(0));
    }

    #[test]
    fn rows_group_and_filter() {
        let projects = [Project { path: "/p/alpha".into(), name: "alpha".into(), last_used: 0 }];
        let cx = Context {
            sessions: vec![SessionRef { id: "s1", title: "Refactor the parser", project: Some("/p/alpha"), updated: 0 }],
            projects: &projects,
            models: &[],
            model: "",
            scheme: Scheme::Dark,
        };
        let mut spotlight = Spotlight::default();
        spotlight.input.insert("pars");
        let rows = spotlight.rows(&cx);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].group, rows[0].pick.clone()), ("Sessions", Pick::Session("s1".into())));
        assert!(rows[0].detail.starts_with("alpha"));

        // Content hits for the same query are appended, without duplicates.
        spotlight.searched = "pars".into();
        spotlight.hits = vec![
            SearchHit { session: "s1".into(), title: "Refactor the parser".into(), snippet: "x".into(), updated: 0 },
            SearchHit { session: "s2".into(), title: "Other".into(), snippet: "…the parser…".into(), updated: 0 },
        ];
        let rows = spotlight.rows(&cx);
        assert_eq!(rows.iter().filter(|r| r.group == "Messages").count(), 1);
    }
}
