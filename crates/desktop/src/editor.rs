//! Text-editing state for input fields, independent of rendering.
//!
//! Positions are byte offsets that always sit on `char` boundaries.
//! ponytail: movement is per `char`, not per grapheme cluster; combined
//! emoji or accents need two presses. Add `unicode-segmentation` if it matters.

use std::ops::Range;

/// An editable string with a caret and an optional selection.
#[derive(Debug, Clone, Default)]
pub struct Editor {
    text: String,
    /// Caret position.
    cursor: usize,
    /// Other end of the selection; equal to `cursor` when nothing is selected.
    anchor: usize,
    /// Maximum number of chars, if limited.
    max_chars: Option<usize>,
    /// Only chars passing this filter are inserted.
    filter: Option<fn(char) -> bool>,
}

impl Editor {
    /// An editor limited to `max_chars` chars that pass `filter`.
    #[must_use]
    pub fn restricted(max_chars: usize, filter: fn(char) -> bool) -> Self {
        Self { max_chars: Some(max_chars), filter: Some(filter), ..Self::default() }
    }

    /// Current contents.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Caret byte offset.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Selected byte range (empty when nothing is selected).
    #[must_use]
    pub fn selection(&self) -> Range<usize> {
        self.cursor.min(self.anchor)..self.cursor.max(self.anchor)
    }

    /// The selected text.
    #[must_use]
    pub fn selected_text(&self) -> &str {
        &self.text[self.selection()]
    }

    /// Empties the editor and returns what it held.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.anchor = 0;
        std::mem::take(&mut self.text)
    }

    /// Replaces the selection with `input`, honouring the filter and limit.
    pub fn insert(&mut self, input: &str) {
        self.delete_selection();
        let budget = self.max_chars.map_or(usize::MAX, |max| max.saturating_sub(self.text.chars().count()));
        let allowed = |c: char| self.filter.map_or(c == '\n' || c == '\t' || !c.is_control(), |f| f(c));
        let filtered: String = input
            .replace("\r\n", "\n")
            .chars()
            .map(|c| if c == '\r' { '\n' } else { c })
            .filter(|&c| allowed(c))
            .take(budget)
            .collect();
        self.text.insert_str(self.cursor, &filtered);
        self.cursor += filtered.len();
        self.anchor = self.cursor;
    }

    /// Deletes the selection, or the char/word before the caret.
    pub fn backspace(&mut self, word: bool) {
        if self.selection().is_empty() {
            self.anchor = if word { self.word_left(self.cursor) } else { self.prev(self.cursor) };
        }
        self.delete_selection();
    }

    /// Deletes the selection, or the char/word after the caret.
    pub fn delete(&mut self, word: bool) {
        if self.selection().is_empty() {
            self.anchor = if word { self.word_right(self.cursor) } else { self.next(self.cursor) };
        }
        self.delete_selection();
    }

    /// Moves left by a char or word; collapses an unextended selection to its start.
    pub fn left(&mut self, word: bool, select: bool) {
        let target = if !select && !self.selection().is_empty() {
            self.selection().start
        } else if word {
            self.word_left(self.cursor)
        } else {
            self.prev(self.cursor)
        };
        self.set_cursor(target, select);
    }

    /// Moves right by a char or word; collapses an unextended selection to its end.
    pub fn right(&mut self, word: bool, select: bool) {
        let target = if !select && !self.selection().is_empty() {
            self.selection().end
        } else if word {
            self.word_right(self.cursor)
        } else {
            self.next(self.cursor)
        };
        self.set_cursor(target, select);
    }

    /// Moves to the start of the current logical line.
    pub fn home(&mut self, select: bool) {
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        self.set_cursor(start, select);
    }

    /// Moves to the end of the current logical line.
    pub fn end(&mut self, select: bool) {
        let end = self.text[self.cursor..].find('\n').map_or(self.text.len(), |i| self.cursor + i);
        self.set_cursor(end, select);
    }

    /// Selects everything.
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
    }

    /// Places the caret at `byte` (snapped to a char boundary), extending the
    /// selection when `select` is set.
    pub fn set_cursor(&mut self, byte: usize, select: bool) {
        let mut byte = byte.min(self.text.len());
        while !self.text.is_char_boundary(byte) {
            byte -= 1;
        }
        self.cursor = byte;
        if !select {
            self.anchor = byte;
        }
    }

    fn delete_selection(&mut self) {
        let range = self.selection();
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = range.start;
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i].char_indices().next_back().map_or(0, |(j, _)| j)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..].chars().next().map_or(i, |c| i + c.len_utf8())
    }

    /// Start of the word before `i`, skipping whitespace first.
    fn word_left(&self, i: usize) -> usize {
        let head = self.text[..i].trim_end();
        head.rfind(char::is_whitespace).map_or(0, |j| j + head[j..].chars().next().map_or(1, char::len_utf8))
    }

    /// End of the word after `i`, skipping whitespace first.
    fn word_right(&self, i: usize) -> usize {
        let tail = &self.text[i..];
        let skipped = tail.len() - tail.trim_start().len();
        let rest = &tail[skipped..];
        i + skipped + rest.find(char::is_whitespace).unwrap_or(rest.len())
    }
}

#[cfg(test)]
mod tests {
    use super::Editor;

    #[test]
    fn typing_and_deleting() {
        let mut e = Editor::default();
        e.insert("héllo world");
        e.backspace(false);
        assert_eq!(e.text(), "héllo worl");
        e.backspace(true);
        assert_eq!(e.text(), "héllo ");
        e.left(false, false);
        e.left(false, false);
        e.delete(false);
        assert_eq!(e.text(), "héll ");
        e.home(false);
        e.delete(true);
        assert_eq!(e.text(), " ");
    }

    #[test]
    fn selection_replace_and_navigation() {
        let mut e = Editor::default();
        e.insert("one two\nthree");
        e.home(false);
        e.end(true);
        assert_eq!(e.selected_text(), "three");
        e.insert("3");
        assert_eq!(e.text(), "one two\n3");
        e.select_all();
        e.left(false, false);
        assert_eq!(e.cursor(), 0);
        e.right(true, true);
        assert_eq!(e.selected_text(), "one");
        assert_eq!(e.take(), "one two\n3");
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn restricted_input() {
        let mut e = Editor::restricted(6, |c| c.is_ascii_digit());
        e.insert("12a3-45 678");
        assert_eq!(e.text(), "123456");
        e.insert("9");
        assert_eq!(e.text(), "123456");
    }

    #[test]
    fn line_endings_and_control_chars() {
        let mut e = Editor::default();
        e.insert("a\r\n\r\nb\u{1}\tc\rd");
        assert_eq!(e.text(), "a\n\nb\tc\nd");
    }
}
