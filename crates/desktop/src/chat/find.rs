//! Find in the open chat (Ctrl/Cmd+F): a bar over the messages that
//! highlights every match of its text, case-insensitively, and steps
//! through them (Enter, Shift+Enter, F3), scrolling each into view.
//!
//! It searches what the messages show: prompts, replies, summaries and
//! the reasoning blocks that are open. Matches are found again only when
//! the text, the conversation or the query changes.

use std::ops::Range;

use winit::window::CursorIcon;

use super::{Conversation, ReasoningView};
use crate::doc::Doc;
use crate::paint::{Painter, Rect, fade};
use crate::text::Style;
use crate::theme;
use crate::ui::{FieldStyle, TextField, Ui, id, text_field};

/// Width of the find bar.
const BAR_W: f32 = 380.0;
/// Its height.
const BAR_H: f32 = 40.0;

/// One match: entry, document (0 reasoning, 1 content), text piece and
/// byte range in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Match {
    pub entry: usize,
    pub doc: u8,
    pub piece: usize,
    pub range: Range<usize>,
}

/// The find bar's state while open.
#[derive(Default)]
pub(super) struct Find {
    pub field: TextField,
    /// Whether typing goes to the bar.
    pub focused: bool,
    matches: Vec<Match>,
    /// Index of the match being looked at.
    current: usize,
    /// What `matches` were found for.
    key: Option<(String, u64, usize, usize, usize, bool)>,
    /// Scroll the current match into view on the next frame.
    pub reveal: bool,
    /// Where the bar was drawn, to keep clicks from reaching the messages.
    pub rect: Rect,
    /// The caret, for the input method's window.
    pub caret: Option<Rect>,
}

/// What the bar asks for after a frame.
#[derive(PartialEq, Eq)]
pub(super) enum BarEvent {
    None,
    Close,
}

impl Find {
    /// The match being looked at.
    pub(super) fn current(&self) -> Option<&Match> {
        self.matches.get(self.current)
    }

    /// Steps to the next match (or the previous one), wrapping around.
    pub(super) fn step(&mut self, back: bool) {
        if self.matches.is_empty() {
            return;
        }
        let n = self.matches.len();
        self.current = if back { (self.current + n - 1) % n } else { (self.current + 1) % n };
        self.reveal = true;
    }

    /// Finds the matches again if the query or the text changed. `first`
    /// is the entry at the top of the view, where a new query starts.
    pub(super) fn update(&mut self, conversation: &Conversation, view: ReasoningView, first: usize) {
        let query: String = self.field.editor.text().chars().flat_map(char::to_lowercase).collect();
        let content: usize = conversation.entries.iter().map(|e| e.message.content.len()).sum();
        let reasoning: usize = conversation.entries.iter().filter(|e| e.reasoning_shown(view)).map(|e| e.message.reasoning.len()).sum();
        let key = (query.clone(), conversation.id, conversation.entries.len(), content, reasoning, view == ReasoningView::Hidden);
        if self.key.as_ref() == Some(&key) {
            return;
        }
        let same_query = self.key.as_ref().is_some_and(|k| k.0 == key.0 && k.1 == key.1);
        let previous = self.current().cloned();
        self.key = Some(key);
        self.matches.clear();
        if !query.trim().is_empty() {
            for (index, entry) in conversation.entries.iter().enumerate() {
                let reasoning = entry.reasoning_doc.as_ref().filter(|_| entry.reasoning_shown(view));
                for (doc_id, doc) in [(0u8, reasoning), (1, entry.doc.as_ref())] {
                    let Some(doc) = doc else { continue };
                    for (piece, text) in doc.texts.iter().enumerate() {
                        for range in find_all(&text.text, &query) {
                            self.matches.push(Match { entry: index, doc: doc_id, piece, range });
                        }
                    }
                }
            }
        }
        // While the text grows, stay on the same match; a new query starts
        // at the first match in view.
        self.current = if let (true, Some(previous)) = (same_query, previous) {
            self.matches.iter().position(|m| *m == previous).unwrap_or(0)
        } else {
            self.reveal = true;
            self.matches.iter().position(|m| m.entry >= first).unwrap_or(0)
        };
    }

    /// Highlights the matches inside document `doc_id` of entry `entry`,
    /// drawn at `origin`.
    pub(super) fn highlight(&self, p: &mut Painter, entry: usize, doc_id: u8, doc: &Doc, origin: (f32, f32)) {
        let t = p.theme;
        for (index, m) in self.matches.iter().enumerate().filter(|(_, m)| m.entry == entry && m.doc == doc_id) {
            let color = if index == self.current { t.find_current } else { t.find };
            doc.draw_highlight(p, origin, (m.piece, m.range.start), (m.piece, m.range.end), color);
        }
    }

    /// Draws the bar at the top right of `area`, for conversation
    /// `conversation`. Returns what the user asked for: closing, and a step
    /// back (`true`) or forward.
    pub(super) fn draw(&mut self, p: &mut Painter, ui: &mut Ui, area: Rect, conversation: u64) -> (BarEvent, Option<bool>) {
        let t = p.theme;
        // Matches of another chat (this one shows no messages to search).
        if self.key.as_ref().is_some_and(|k| k.1 != conversation) {
            self.matches.clear();
            self.key = None;
        }
        let width = BAR_W.min(area.w - 32.0);
        let bar = Rect::new(area.right() - 16.0 - width, area.y + 8.0, width, BAR_H);
        self.rect = bar;
        p.shadow(Rect::new(bar.x, bar.y + 4.0, bar.w, bar.h), t.shadow, theme::RADIUS, 14.0);
        p.bordered(bar, t.surface, theme::RADIUS, 1.0, t.border_strong);

        // Right to left: close, next, previous, the count.
        let mut event = BarEvent::None;
        let mut step = None;
        let close = Rect::new(bar.right() - 32.0, bar.y + 8.0, 24.0, 24.0);
        let next = Rect::new(close.x - 28.0, close.y, 24.0, 24.0);
        let previous = Rect::new(next.x - 26.0, close.y, 24.0, 24.0);
        for (rect, label, key) in [(close, "×", 0), (next, "↓", 1), (previous, "↑", 2)] {
            let hovered = ui.hovered(rect);
            let hover = ui.anim(id(("find-button", key)), f32::from(u8::from(hovered)));
            p.rect(rect, fade(t.hover, hover), theme::RADIUS_SM);
            let style = if key == 0 { Style::regular(16.0) } else { Style::semibold(13.0) };
            p.label_centered(label, style, rect, if hovered { t.text } else { t.text_muted });
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(rect) {
                    match key {
                        0 => event = BarEvent::Close,
                        1 => step = Some(false),
                        _ => step = Some(true),
                    }
                }
            }
        }
        let count = match (self.matches.len(), self.field.editor.text().trim().is_empty()) {
            (_, true) => String::new(),
            (0, false) => "No results".to_owned(),
            (n, false) => format!("{} of {n}", self.current + 1),
        };
        let count = p.layout(&count, theme::TINY, None);
        let count_x = previous.x - 8.0 - count.width();
        p.text(&count, count_x, bar.y + (bar.h - count.height()) * 0.5, if self.matches.is_empty() { t.text_faint } else { t.text_muted });

        let field = Rect::new(bar.x + 6.0, bar.y + 6.0, count_x - bar.x - 14.0, bar.h - 12.0);
        let style = FieldStyle { placeholder: "Find in chat", multiline: false, mono: false };
        let (pressed, caret) = text_field(p, ui, field, &mut self.field, self.focused, &style);
        self.caret = caret;
        if ui.pressed {
            // A click on the bar keeps typing here; anywhere else gives it back.
            self.focused = pressed || (self.focused && ui.hovered(bar));
        }
        (event, step)
    }
}

/// Byte ranges of `text` where `needle` (lower-cased already) occurs,
/// ignoring case. Offsets stay on `text`'s character boundaries even where
/// lower-casing changes a character's length.
pub(super) fn find_all(text: &str, needle: &str) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    // The lower-cased text, and for each of its bytes the original offset.
    let mut lowered = String::with_capacity(text.len());
    let mut origin = Vec::with_capacity(text.len() + 1);
    for (offset, c) in text.char_indices() {
        for l in c.to_lowercase() {
            let before = lowered.len();
            lowered.push(l);
            origin.resize(origin.len() + lowered.len() - before, offset);
        }
    }
    origin.push(text.len());
    let char_end = |at: usize| at + text[at..].chars().next().map_or(0, char::len_utf8);
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = lowered[from..].find(needle) {
        let start = from + i;
        let end = start + needle.len();
        // A match ending inside one character's lower-case form covers that character.
        let original_end = if end < lowered.len() && origin[end] == origin[end - 1] { char_end(origin[end]) } else { origin[end] };
        out.push(origin[start]..original_end);
        from = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_ignoring_case() {
        let one = |found: Vec<Range<usize>>| (found.len() == 1).then(|| found[0].clone());
        assert_eq!(find_all("Hello hello HELLO", "hello"), [0..5, 6..11, 12..17]);
        assert_eq!(one(find_all("aaa", "aa")), Some(0..2), "matches don't overlap");
        assert_eq!(one(find_all("Ünïcödé text", "ïcö")), Some(3..8));
        assert!(find_all("abc", "").is_empty() && find_all("abc", "x").is_empty());
        // 'İ' lower-cases to two characters; ranges stay on boundaries.
        let text = "xİy";
        for range in find_all(text, "i") {
            assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end));
            assert_eq!(&text[range], "İ");
        }
        assert_eq!(one(find_all("STRASSE straße", "straße")), Some(8..15));
    }

    #[test]
    fn matches_follow_the_text() {
        use serechat::{Role, StoredMessage};
        let fonts = crate::text::Fonts::load();
        let mut conversation = Conversation::new(1, None);
        for (id, (role, text)) in [(Role::User, "Where is the Parser?"), (Role::Assistant, "The **parser** lives in `parser.rs`.")].into_iter().enumerate() {
            let mut entry = super::super::Entry::new(id as u64, StoredMessage::new(role, text.into()));
            entry.doc = Some(Doc::markdown(&fonts, text, 400.0, 1.0, crate::doc::INK_TEXT, None));
            conversation.entries.push(entry);
        }
        let mut find = Find::default();
        find.field.editor.insert("PARSER");
        find.update(&conversation, ReasoningView::Collapsed, 1);
        // Markdown is searched as shown: "parser" and "parser.rs" in the reply.
        assert_eq!(find.matches.iter().map(|m| m.entry).collect::<Vec<_>>(), [0, 1, 1]);
        assert_eq!(find.current().map(|m| m.entry), Some(1), "a new query starts in view");
        find.step(false);
        let looking_at = find.current().cloned();
        // The reply grows; the same match stays current.
        let entry = &mut conversation.entries[1];
        entry.message.content.push_str(" Also see parser_test.rs.");
        entry.doc = Some(Doc::markdown(&fonts, &entry.message.content, 400.0, 1.0, crate::doc::INK_TEXT, None));
        find.update(&conversation, ReasoningView::Collapsed, 0);
        assert_eq!(find.matches.len(), 4);
        assert_eq!(find.current().cloned(), looking_at);
    }

    #[test]
    fn steps_wrap_around() {
        let mut find = Find { matches: vec![Match { entry: 0, doc: 1, piece: 0, range: 0..1 }; 3], ..Find::default() };
        find.step(true);
        assert_eq!(find.current, 2);
        find.step(false);
        assert_eq!(find.current, 0);
        assert!(find.reveal);
    }
}
