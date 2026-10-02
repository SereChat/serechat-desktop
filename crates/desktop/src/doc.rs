//! Laid-out message documents.
//!
//! A [`Doc`] turns Markdown blocks into positioned text pieces (each a
//! [`TextLayout`] plus its plain text, for selection and copying) and
//! decorations (code frames, quote bars, rules, tables, list markers).
//! Rebuilding after a streamed delta reuses every block that did not change,
//! so long replies stay cheap to update.

use std::ops::Range;

use crate::highlight::{self, Token};
use crate::markdown::{self, Align as ColumnAlign, Block, Inline};
use crate::paint::{Color, Painter, Rect, fade};
use crate::text::{BACKGROUND, FontId, Fonts, Run, STRIKE, Style, TextLayout, UNDERLINE};
use crate::theme::{self, Palette};

/// Ink slots used by document runs; see [`inks`].
pub const INK_TEXT: u8 = 0;
const INK_LINK: u8 = 1;
const INK_CODE: u8 = 2;
const INK_SYNTAX: u8 = 3;
/// Secondary text (quotes, reasoning).
pub const INK_MUTED: u8 = 9;

/// The colours behind each ink slot for palette `t`, with `text` as slot 0.
#[must_use]
pub fn inks(t: &Palette, text: Color) -> [Color; 10] {
    let s = t.syntax;
    [text, t.link, text, s[0], s[1], s[2], s[3], s[4], s[5], t.text_muted]
}

/// Vertical space between blocks.
const BLOCK_GAP: f32 = 12.0;
/// Vertical space between list items.
const ITEM_GAP: f32 = 4.0;
/// Indentation of list item contents.
const LIST_INDENT: f32 = 24.0;
/// Height of a code block's header strip.
const CODE_HEADER: f32 = 32.0;
/// Inner padding of code blocks.
const CODE_PAD: f32 = 12.0;
/// Code font size.
const CODE_STYLE: Style = Style::mono(13.0);

/// A selectable run of text at a position inside the document.
pub struct TextPiece {
    /// Left edge relative to the document.
    pub x: f32,
    /// Top edge relative to the document.
    pub y: f32,
    /// The laid-out text.
    pub layout: TextLayout,
    /// Its plain text (what gets copied).
    pub text: String,
    /// Separator before this piece when copying across pieces.
    pub sep: &'static str,
    /// Link targets of this piece's link runs.
    links: Vec<String>,
    /// For code: the code block's number and its frame width.
    code: Option<(usize, f32)>,
}

impl TextPiece {
    /// The visible part, with code scrolled by `scroll`.
    fn rect(&self, scroll: f32) -> Rect {
        let w = self.layout.width().max(4.0);
        match self.code {
            Some((_, window)) => Rect::new(self.x, self.y, (w - scroll).min(window - 2.0 * CODE_PAD), self.layout.height()),
            None => Rect::new(self.x, self.y, w, self.layout.height()),
        }
    }
}

/// Non-text elements of a document.
pub enum Deco {
    /// Horizontal rule.
    Rule {
        /// Width.
        w: f32,
    },
    /// Bar left of a block quote.
    QuoteBar {
        /// Height.
        h: f32,
    },
    /// Frame, header and copy button of a code block.
    Code {
        /// Frame width.
        w: f32,
        /// Frame height.
        h: f32,
        /// Width of the code plus padding; more than `w` when it scrolls.
        content_w: f32,
        /// Language label.
        lang: String,
        /// The code, for the copy button.
        code: String,
        /// Number of this code block within the document.
        index: usize,
    },
    /// Table grid.
    Table {
        /// Column boundaries relative to the table's left edge.
        cols: Vec<f32>,
        /// Row boundaries relative to the table's top.
        rows: Vec<f32>,
    },
    /// List bullet or number.
    Marker(TextLayout),
    /// Task list checkbox.
    Checkbox {
        /// Ticked.
        checked: bool,
    },
}

/// A decoration at a position inside the document.
pub struct DecoPiece {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// What to draw.
    pub deco: Deco,
}

/// A position inside a document: text piece and byte offset.
pub type DocPos = (usize, usize);

/// What the user did with a document this frame.
#[derive(Default)]
pub struct DocEvent {
    /// Index of the code block whose copy button was clicked.
    pub copy_code: Option<usize>,
    /// A link that was clicked.
    pub open_link: Option<String>,
}

/// A laid-out document.
#[derive(Default)]
pub struct Doc {
    /// Selectable text, in reading order.
    pub texts: Vec<TextPiece>,
    /// Decorations, drawn beneath the text.
    pub decos: Vec<DecoPiece>,
    /// Total height in logical pixels.
    pub height: f32,
    /// Blocks this document was built from, for incremental rebuilds.
    blocks: Vec<Block>,
    /// After each top-level block: text count, deco count, code count and `y`.
    block_ends: Vec<(usize, usize, usize, f32)>,
    /// Width and scale the layout is valid for.
    key: (u32, u32),
    /// Sideways scroll of each code block, which never wraps.
    code_scroll: Vec<f32>,
    /// Code block whose scrollbar is being dragged, and its scroll at the press.
    code_drag: Option<(usize, f32)>,
}

/// Layout state while building.
struct Builder<'a> {
    fonts: &'a Fonts,
    scale: f32,
    texts: Vec<TextPiece>,
    decos: Vec<DecoPiece>,
    codes: usize,
}

impl Doc {
    /// A document of plain text in `style` (user messages, errors).
    #[must_use]
    pub fn plain(fonts: &Fonts, text: &str, style: Style, width: f32, scale: f32) -> Self {
        let layout = TextLayout::new(fonts, text, style, Some(width), scale);
        let height = layout.height();
        Self {
            texts: vec![TextPiece { x: 0.0, y: 0.0, layout, text: text.to_owned(), sep: "", links: Vec::new(), code: None }],
            height,
            key: (width.to_bits(), scale.to_bits()),
            ..Self::default()
        }
    }

    /// Lays out Markdown `src` at `width`. `previous`, when laid out at the
    /// same width and scale, donates every leading block that is unchanged.
    /// `base_ink` colours plain text (e.g. [`INK_MUTED`] for reasoning).
    #[must_use]
    pub fn markdown(fonts: &Fonts, src: &str, width: f32, scale: f32, base_ink: u8, mut previous: Option<Self>) -> Self {
        let blocks = markdown::parse(src);
        let key = (width.to_bits(), scale.to_bits());
        let mut b = Builder { fonts, scale, texts: Vec::new(), decos: Vec::new(), codes: 0 };
        let mut block_ends = Vec::with_capacity(blocks.len());
        let mut y = 0.0;
        let mut reused = 0;
        // Code blocks keep their scroll while a reply streams in.
        let (mut code_scroll, code_drag) = previous.as_mut().map(|p| (std::mem::take(&mut p.code_scroll), p.code_drag)).unwrap_or_default();
        if let Some(prev) = previous.filter(|p| p.key == key) {
            reused = prev.blocks.iter().zip(&blocks).take_while(|(a, b)| a == b).count();
            // The last reused block's end marks where rebuilding resumes.
            if let Some(&(texts, decos, codes, end_y)) = reused.checked_sub(1).and_then(|i| prev.block_ends.get(i)) {
                b.texts = prev.texts;
                b.texts.truncate(texts);
                b.decos = prev.decos;
                b.decos.truncate(decos);
                b.codes = codes;
                y = end_y;
                block_ends.extend_from_slice(&prev.block_ends[..reused]);
            } else {
                reused = 0;
            }
        }
        for block in &blocks[reused..] {
            let first = b.texts.is_empty() && b.decos.is_empty();
            if !first {
                y += BLOCK_GAP;
            }
            let sep = if first { "" } else { "\n\n" };
            y += b.block(block, 0.0, y, width, base_ink, sep);
            block_ends.push((b.texts.len(), b.decos.len(), b.codes, y));
        }
        code_scroll.resize(b.codes, 0.0);
        Self { texts: b.texts, decos: b.decos, height: y, blocks, block_ends, key, code_scroll, code_drag }
    }

    /// How far a piece is scrolled sideways (code only).
    fn scroll_of(&self, piece: &TextPiece) -> f32 {
        piece.code.and_then(|(index, _)| self.code_scroll.get(index)).copied().unwrap_or(0.0)
    }

    /// Whether the layout was made for this width and scale.
    #[must_use]
    pub fn fits(&self, width: f32, scale: f32) -> bool {
        self.key == (width.to_bits(), scale.to_bits())
    }

    /// The text position nearest to `(x, y)` (document coordinates).
    #[must_use]
    pub fn hit(&self, x: f32, y: f32) -> Option<DocPos> {
        let distance = |r: Rect| {
            let dx = (r.x - x).max(x - r.right()).max(0.0);
            let dy = (r.y - y).max(y - r.bottom()).max(0.0);
            // Rows matter more than columns: prefer the piece on this line.
            dy * 4.0 + dx
        };
        let rect = |piece: &TextPiece| piece.rect(self.scroll_of(piece));
        let (index, piece) = self.texts.iter().enumerate().min_by(|a, b| distance(rect(a.1)).total_cmp(&distance(rect(b.1))))?;
        let byte = piece.layout.hit(x - piece.x + self.scroll_of(piece), (y - piece.y).clamp(0.0, piece.layout.height() - 0.01));
        Some((index, byte))
    }

    /// The word (or single other character) around a position.
    #[must_use]
    pub fn word(&self, (piece, byte): DocPos) -> Range<usize> {
        let Some(text) = self.texts.get(piece).map(|p| p.text.as_str()) else { return 0..0 };
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let at = text[byte.min(text.len())..].chars().next();
        if !at.is_some_and(is_word) {
            return byte..byte + at.map_or(0, char::len_utf8);
        }
        let start = text[..byte].rfind(|c: char| !is_word(c)).map_or(0, |i| i + text[i..].chars().next().map_or(1, char::len_utf8));
        let end = text[byte..].find(|c: char| !is_word(c)).map_or(text.len(), |i| byte + i);
        start..end
    }

    /// Plain text between two positions, pieces joined by their separators.
    #[must_use]
    pub fn text_between(&self, from: DocPos, to: DocPos) -> String {
        let mut out = String::new();
        for (index, piece) in self.texts.iter().enumerate().take(to.0 + 1).skip(from.0) {
            let start = if index == from.0 { from.1 } else { 0 };
            let end = if index == to.0 { to.1 } else { piece.text.len() };
            if index != from.0 {
                out.push_str(piece.sep);
            }
            out.push_str(piece.text.get(start..end).unwrap_or_default());
        }
        out
    }

    /// End position of the document.
    #[must_use]
    pub fn end(&self) -> DocPos {
        self.texts.last().map_or((0, 0), |p| (self.texts.len() - 1, p.text.len()))
    }

    /// Draws the selection between two ordered positions.
    pub fn draw_selection(&self, p: &mut Painter, origin: (f32, f32), from: DocPos, to: DocPos) {
        let color = p.theme.selection;
        self.draw_highlight(p, origin, from, to, color);
    }

    /// Where position `pos` is, relative to the document: the top of its
    /// line and the line's height.
    #[must_use]
    pub fn position(&self, (piece, byte): DocPos) -> Option<(f32, f32)> {
        let piece = self.texts.get(piece)?;
        let (_, y) = piece.layout.caret(byte.min(piece.text.len()));
        Some((piece.y + y, piece.layout.line_height()))
    }

    /// Fills the text between two ordered positions with `color`.
    pub fn draw_highlight(&self, p: &mut Painter, origin: (f32, f32), from: DocPos, to: DocPos, color: Color) {
        for (index, piece) in self.texts.iter().enumerate().take(to.0 + 1).skip(from.0) {
            let first = if index == from.0 { from.1 } else { 0 };
            let last = if index == to.0 { to.1 } else { piece.text.len() };
            let continues = index < to.0;
            let line_h = piece.layout.line_height();
            let left = origin.0 + piece.x - self.scroll_of(piece);
            // Scrolled code stays inside its frame.
            let clip = piece.code.map(|(_, window)| p.push_clip(code_window(origin, piece, window)));
            for (start, end, y) in piece.layout.line_spans() {
                let (s, e) = (first.max(start), last.min(end));
                let past_line = last > end || continues;
                if s > e || (s == e && !past_line) {
                    continue;
                }
                let x0 = piece.layout.caret(s).0;
                let x1 = piece.layout.caret(e).0 + if past_line && e == end { 6.0 } else { 0.0 };
                p.rect(Rect::new(left + x0, origin.1 + piece.y + y, x1 - x0, line_h), color, 2.0);
            }
            if let Some(clip) = clip {
                p.set_clip(clip);
            }
        }
    }

    /// Draws the document at `origin`. `interactive` enables hover and clicks;
    /// `copied` is the code block currently showing "Copied".
    pub fn draw(
        &mut self,
        p: &mut Painter,
        ui: &mut crate::ui::Ui,
        origin: (f32, f32),
        text: Color,
        interactive: bool,
        copied: Option<usize>,
    ) -> DocEvent {
        let t = p.theme;
        let clip = p.clip();
        let (top, bottom) = (clip.y - origin.1, clip.bottom() - origin.1);
        let mut event = DocEvent::default();
        for piece in &self.decos {
            let (x, y) = (origin.0 + piece.x, origin.1 + piece.y);
            match &piece.deco {
                Deco::Rule { w } => p.rect(Rect::new(x, y, *w, 1.0), t.border, 0.0),
                Deco::QuoteBar { h } => p.rect(Rect::new(x, y, 3.0, *h), t.border_strong, 1.5),
                Deco::Code { w, h, content_w, lang, index, .. } => {
                    if piece.y > bottom || piece.y + h < top {
                        continue;
                    }
                    let frame = Rect::new(x, y, *w, *h);
                    p.bordered(frame, t.code_bg, theme::RADIUS, 1.0, t.border);
                    p.rect(Rect::new(x, y + CODE_HEADER - 1.0, *w, 1.0), t.border, 0.0);
                    let label = if lang.is_empty() { "code" } else { lang.as_str() };
                    let label = p.layout(label, theme::CAPTION, None);
                    p.text(&label, x + CODE_PAD, y + (CODE_HEADER - label.height()) * 0.5, t.text_faint);
                    let copy = if copied == Some(*index) { "✓ Copied" } else { "Copy" };
                    let button = Rect::new(x + w - 78.0, y + 4.0, 72.0, CODE_HEADER - 8.0);
                    // Only refuse clicks through the header onto a button scrolled beneath it.
                    let enabled = interactive || !ui.hovered(button);
                    let clicked = crate::ui::button(p, ui, button, copy, crate::ui::ButtonStyle::Ghost, enabled);
                    if clicked {
                        event.copy_code = Some(*index);
                    }
                    if content_w > w {
                        let body = Rect::new(x, y + CODE_HEADER, *w, h - CODE_HEADER);
                        let scroll = &mut self.code_scroll[*index];
                        let dragging = &mut self.code_drag;
                        scroll_code(p, ui, body, *content_w, (scroll, dragging, *index), interactive);
                    }
                }
                Deco::Table { cols, rows } => {
                    let (table_w, table_h) = (cols.last().copied().unwrap_or(0.0), rows.last().copied().unwrap_or(0.0));
                    let header = rows.get(1).copied().unwrap_or(table_h);
                    p.bordered(Rect::new(x, y, table_w, table_h), [0.0; 4], theme::RADIUS, 1.0, t.border_strong);
                    p.rect(Rect::new(x + 1.0, y + 1.0, table_w - 2.0, header - 1.0), fade(t.text, 0.04), theme::RADIUS - 1.0);
                    for &row in &rows[1..rows.len().saturating_sub(1)] {
                        p.rect(Rect::new(x, y + row, table_w, 1.0), t.border, 0.0);
                    }
                    for &col in &cols[1..cols.len().saturating_sub(1)] {
                        p.rect(Rect::new(x + col, y, 1.0, table_h), t.border, 0.0);
                    }
                }
                Deco::Marker(layout) => p.text(layout, x, y, t.text_faint),
                Deco::Checkbox { checked } => {
                    let check = Rect::new(x, y + 4.0, 14.0, 14.0);
                    if *checked {
                        p.rect(check, t.accent, 3.0);
                        p.label_centered("✓", Style::semibold(11.0), check, t.on_accent);
                    } else {
                        p.bordered(check, [0.0; 4], 3.0, 1.5, t.border_strong);
                    }
                }
            }
        }

        let inks = inks(&t, text);
        let code_bg = fade(t.text, 0.08);
        for piece in &self.texts {
            if piece.y > bottom || piece.y + piece.layout.height() < top {
                continue;
            }
            let (x, y) = (origin.0 + piece.x - self.scroll_of(piece), origin.1 + piece.y);
            let clip = piece.code.map(|(_, window)| p.push_clip(code_window(origin, piece, window)));
            p.rich(&piece.layout, x, y, &inks, code_bg);
            if let Some(clip) = clip {
                p.set_clip(clip);
            }
            if interactive && !piece.links.is_empty() && ui.hovered(Rect::new(x, y, piece.layout.width(), piece.layout.height())) {
                let under = |(mx, my): (f32, f32)| piece.layout.link_at(mx - x, my - y);
                if let Some(index) = under(ui.mouse) {
                    ui.cursor = winit::window::CursorIcon::Pointer;
                    // A click, not the end of a drag-selection, on the same link.
                    let still = (ui.press_pos.0 - ui.mouse.0).hypot(ui.press_pos.1 - ui.mouse.1) < 4.0;
                    if ui.released && still && under(ui.press_pos) == Some(index) {
                        event.open_link = piece.links.get(usize::from(index)).cloned();
                    }
                }
            }
        }
        event
    }

    /// Code of the block with this index, for copying.
    #[must_use]
    pub fn code(&self, index: usize) -> Option<&str> {
        self.decos.iter().find_map(|d| match &d.deco {
            Deco::Code { code, index: i, .. } if *i == index => Some(code.as_str()),
            _ => None,
        })
    }
}

impl Builder<'_> {
    fn inline(&self, inline: &Inline, style: Style, width: f32, base_ink: u8) -> TextLayout {
        let base = Run { ink: base_ink, ..Run::plain(style.font) };
        let mut spans: Vec<(Range<usize>, Run)> = Vec::with_capacity(inline.spans.len() * 2 + 1);
        let mut at = 0;
        let plain = |spans: &mut Vec<(Range<usize>, Run)>, range: Range<usize>| {
            if !range.is_empty() && base != Run::plain(style.font) {
                spans.push((range, base));
            }
        };
        for (range, mark) in &inline.spans {
            plain(&mut spans, at..range.start);
            let font = if mark.code {
                FontId::MONO
            } else if mark.strong || style.font == FontId::SEMIBOLD {
                FontId::SEMIBOLD
            } else if mark.emphasis {
                FontId::ITALIC
            } else {
                style.font
            };
            let mut decoration = 0;
            if mark.code {
                decoration |= BACKGROUND;
            }
            if mark.link != 0 {
                decoration |= UNDERLINE;
            }
            if mark.strike {
                decoration |= STRIKE;
            }
            let ink = if mark.link != 0 { INK_LINK } else if mark.code { INK_CODE } else { base_ink };
            let size = if mark.code { 0.88 } else { 1.0 };
            spans.push((range.clone(), Run { font, size, ink, decoration, link: mark.link }));
            at = range.end;
        }
        plain(&mut spans, at..inline.text.len());
        TextLayout::rich(self.fonts, &inline.text, &spans, style, Some(width), self.scale)
    }

    fn text(&mut self, x: f32, y: f32, layout: TextLayout, inline: &Inline, sep: &'static str) -> f32 {
        let h = layout.height();
        self.texts.push(TextPiece { x, y, layout, text: inline.text.clone(), sep, links: inline.links.clone(), code: None });
        h
    }

    /// Lays out `block` at `(x, y)`; returns its height.
    fn block(&mut self, block: &Block, x: f32, y: f32, width: f32, ink: u8, sep: &'static str) -> f32 {
        match block {
            Block::Paragraph(inline) => {
                let layout = self.inline(inline, theme::BODY, width, ink);
                self.text(x, y, layout, inline, sep)
            }
            Block::Heading { level, text } => {
                let size = match level {
                    1 => 22.0,
                    2 => 19.0,
                    3 => 16.5,
                    _ => 15.0,
                };
                // Headings get extra air above, unless they open the document.
                let above = if y > 0.0 { 6.0 } else { 0.0 };
                let layout = self.inline(text, Style::semibold(size), width, ink);
                above + self.text(x, y + above, layout, text, sep)
            }
            Block::Code { lang, code } => {
                let spans: Vec<(Range<usize>, Run)> = highlight::highlight(lang, code)
                    .into_iter()
                    .map(|(range, token)| {
                        let slot = match token {
                            Token::Keyword => 0,
                            Token::String => 1,
                            Token::Number => 2,
                            Token::Comment => 3,
                            Token::Function => 4,
                            Token::Type => 5,
                        };
                        (range, Run { ink: INK_SYNTAX + slot, ..Run::plain(FontId::MONO) })
                    })
                    .collect();
                // Code never wraps: lines keep their shape and the block scrolls.
                let layout = TextLayout::rich(self.fonts, code, &spans, CODE_STYLE, None, self.scale);
                let content_w = layout.width() + 2.0 * CODE_PAD;
                // Room for the scrollbar when the code is wider than the frame.
                let bar = if content_w > width { 6.0 } else { 0.0 };
                let h = CODE_HEADER + layout.height() + 2.0 * CODE_PAD - 4.0 + bar;
                let index = self.codes;
                self.decos.push(DecoPiece { x, y, deco: Deco::Code { w: width, h, content_w, lang: lang.clone(), code: code.clone(), index } });
                self.codes += 1;
                let piece = TextPiece {
                    x: x + CODE_PAD,
                    y: y + CODE_HEADER + CODE_PAD - 2.0,
                    layout,
                    text: code.clone(),
                    sep,
                    links: Vec::new(),
                    code: Some((index, width)),
                };
                self.texts.push(piece);
                h
            }
            Block::Quote(blocks) => {
                let h = self.blocks(blocks, x + 16.0, y, width - 16.0, markdown_quote_ink(ink), sep, BLOCK_GAP);
                self.decos.push(DecoPiece { x, y, deco: Deco::QuoteBar { h } });
                h
            }
            Block::List { start, items } => {
                let mut h = 0.0;
                for (n, item) in items.iter().enumerate() {
                    if n > 0 {
                        h += ITEM_GAP;
                    }
                    let item_y = y + h;
                    let item_sep = if n == 0 { sep } else { "\n" };
                    let indent = if item.task.is_some() { LIST_INDENT + 2.0 } else { LIST_INDENT };
                    match (item.task, start) {
                        (Some(checked), _) => self.decos.push(DecoPiece { x: x + 2.0, y: item_y, deco: Deco::Checkbox { checked } }),
                        (None, Some(first)) => {
                            let marker = TextLayout::new(self.fonts, &format!("{}.", first + n as u64), theme::BODY, None, self.scale);
                            let mx = x + LIST_INDENT - 7.0 - marker.width();
                            self.decos.push(DecoPiece { x: mx, y: item_y, deco: Deco::Marker(marker) });
                        }
                        (None, None) => {
                            let marker = TextLayout::new(self.fonts, "•", theme::BODY, None, self.scale);
                            self.decos.push(DecoPiece { x: x + 7.0, y: item_y, deco: Deco::Marker(marker) });
                        }
                    }
                    h += self.blocks(&item.blocks, x + indent, item_y, width - indent, ink, item_sep, ITEM_GAP + 2.0).max(18.0);
                }
                h
            }
            Block::Rule => {
                self.decos.push(DecoPiece { x, y: y + 8.0, deco: Deco::Rule { w: width } });
                17.0
            }
            Block::Table { align, header, rows } => self.table(x, y, width, align, header, rows, ink, sep),
        }
    }

    /// Stacks `blocks` vertically; returns their total height.
    #[allow(clippy::too_many_arguments, reason = "internal layout helper; a struct would only rename the arguments")]
    fn blocks(&mut self, blocks: &[Block], x: f32, y: f32, width: f32, ink: u8, sep: &'static str, gap: f32) -> f32 {
        let mut h = 0.0;
        for (i, block) in blocks.iter().enumerate() {
            if i > 0 {
                h += gap;
            }
            h += self.block(block, x, y + h, width, ink, if i == 0 { sep } else { "\n\n" });
        }
        h
    }

    #[allow(clippy::too_many_arguments, reason = "internal layout helper; a struct would only rename the arguments")]
    fn table(&mut self, x: f32, y: f32, width: f32, align: &[ColumnAlign], header: &[Inline], rows: &[Vec<Inline>], ink: u8, sep: &'static str) -> f32 {
        const PAD: (f32, f32) = (10.0, 7.0);
        let style = Style::regular(13.5);
        let columns = header.len().max(1);
        let all_rows = || std::iter::once(header).chain(rows.iter().map(Vec::as_slice));
        // Natural width of each column, then shrink the widest to fit.
        let mut natural = vec![0.0f32; columns];
        for row in all_rows() {
            for (c, cell) in row.iter().enumerate().take(columns) {
                let w = TextLayout::new(self.fonts, &cell.text, style, None, self.scale).width();
                natural[c] = natural[c].max(w + 2.0 * PAD.0 + 1.0);
            }
        }
        let widths = fit_columns(&natural, width);

        let mut cols = vec![0.0];
        for w in &widths {
            cols.push(cols.last().copied().unwrap_or(0.0) + w);
        }
        let mut row_y = vec![0.0];
        for (r, row) in all_rows().enumerate() {
            let top = row_y.last().copied().unwrap_or(0.0);
            let cell_style = if r == 0 { Style { font: FontId::SEMIBOLD, ..style } } else { style };
            let mut row_h = 0.0f32;
            for (c, cell) in row.iter().enumerate().take(columns) {
                let inner = widths[c] - 2.0 * PAD.0;
                let layout = self.inline(cell, cell_style, inner, ink);
                let offset = match align.get(c) {
                    Some(ColumnAlign::Center) => ((inner - layout.width()) * 0.5).max(0.0),
                    Some(ColumnAlign::Right) => (inner - layout.width()).max(0.0),
                    _ => 0.0,
                };
                row_h = row_h.max(layout.height());
                let cell_sep = match (r, c) {
                    (0, 0) => sep,
                    (_, 0) => "\n",
                    _ => "\t",
                };
                self.text(x + cols[c] + PAD.0 + offset.round(), y + top + PAD.1, layout, cell, cell_sep);
            }
            row_y.push(top + row_h + 2.0 * PAD.1);
        }
        let h = row_y.last().copied().unwrap_or(0.0);
        self.decos.push(DecoPiece { x, y, deco: Deco::Table { cols, rows: row_y } });
        h
    }
}

/// The part of a code block's frame its text shows through.
fn code_window(origin: (f32, f32), piece: &TextPiece, frame_w: f32) -> Rect {
    Rect::new(origin.0 + piece.x - CODE_PAD + 1.0, origin.1 + piece.y - 4.0, frame_w - 2.0, piece.layout.height() + 8.0)
}

/// Scrolls a code block sideways (wheel, trackpad or its scrollbar) and draws
/// the scrollbar along the bottom of `body`. `state` is the block's scroll,
/// the document's drag (block number and scroll at the press) and the
/// block's number.
fn scroll_code(p: &mut Painter, ui: &mut crate::ui::Ui, body: Rect, content_w: f32, state: (&mut f32, &mut Option<(usize, f32)>, usize), interactive: bool) {
    let (scroll, drag, index) = state;
    let max = content_w - body.w;
    if interactive && ui.hovered(body) && ui.scroll_x != 0.0 {
        *scroll += ui.scroll_x;
        ui.scroll_x = 0.0;
    }
    let track = Rect::new(body.x + 6.0, body.bottom() - 9.0, body.w - 12.0, 6.0);
    let thumb_w = (track.w * body.w / content_w).max(24.0);
    // Easier to grab than it looks.
    let grab = Rect::new(track.x, track.y - 4.0, track.w, track.h + 6.0);
    let over = interactive && ui.hovered(grab);
    if over {
        ui.cursor = winit::window::CursorIcon::Pointer;
        if ui.pressed {
            *drag = Some((index, *scroll));
        }
    }
    let dragging = drag.is_some_and(|(i, _)| i == index);
    if dragging {
        if ui.down {
            let start = drag.map_or(0.0, |(_, s)| s);
            *scroll = start + (ui.mouse.0 - ui.press_pos.0) * max / (track.w - thumb_w).max(1.0);
            // Keep the pointer cursor so a drag never turns into a text selection.
            ui.cursor = winit::window::CursorIcon::Pointer;
        } else {
            *drag = None;
        }
    }
    *scroll = scroll.clamp(0.0, max);
    let t = p.theme;
    let alpha = if over || dragging { 0.3 } else if ui.hovered(body) { 0.16 } else { 0.08 };
    let thumb = Rect::new(track.x + (track.w - thumb_w) * (*scroll / max), track.y + 1.0, thumb_w, 4.0);
    p.rect(thumb, fade(t.text, alpha), 2.0);
}

/// Quotes render their text muted unless already coloured.
fn markdown_quote_ink(ink: u8) -> u8 {
    if ink == INK_TEXT { INK_MUTED } else { ink }
}

/// Column widths: natural where everything fits, otherwise narrow columns
/// keep their width and the rest share what remains (at least 60 px each).
fn fit_columns(natural: &[f32], available: f32) -> Vec<f32> {
    let total: f32 = natural.iter().sum();
    if total <= available {
        return natural.to_vec();
    }
    let mut widths = natural.to_vec();
    let mut open: Vec<usize> = (0..natural.len()).collect();
    let mut remaining = available;
    loop {
        let fair = (remaining / open.len().max(1) as f32).max(60.0);
        let (fixed, rest): (Vec<usize>, Vec<usize>) = open.iter().partition(|&&c| natural[c] <= fair);
        if fixed.is_empty() {
            for c in rest {
                widths[c] = fair.floor();
            }
            return widths;
        }
        for &c in &fixed {
            remaining -= natural[c];
        }
        open = rest;
        if open.is_empty() {
            return widths;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(src: &str) -> Doc {
        Doc::markdown(&Fonts::load(), src, 400.0, 1.0, INK_TEXT, None)
    }

    #[test]
    fn builds_pieces_in_reading_order() {
        let d = doc("# Title\n\nSome **bold** text.\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |");
        let texts: Vec<&str> = d.texts.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, ["Title", "Some bold text.", "one", "two", "fn main() {}", "a", "b", "1", "2"]);
        assert!(d.texts.windows(2).all(|w| w[0].y <= w[1].y + 0.01));
        assert!(d.height > 200.0);
        assert_eq!(d.code(0), Some("fn main() {}"));
    }

    #[test]
    fn selection_text_uses_separators() {
        let d = doc("Para one.\n\n- a\n- b\n\n| x | y |\n|---|---|\n| 1 | 2 |");
        assert_eq!(d.text_between((0, 5), d.end()), "one.\n\na\nb\n\nx\ty\n1\t2");
        assert_eq!(d.word((0, 1)), 0..4);
    }

    #[test]
    fn streaming_rebuild_reuses_unchanged_blocks() {
        let fonts = Fonts::load();
        let first = Doc::markdown(&fonts, "Intro paragraph.\n\nSecond", 400.0, 1.0, INK_TEXT, None);
        let intro_y = first.texts[0].y;
        let next = Doc::markdown(&fonts, "Intro paragraph.\n\nSecond part grows", 400.0, 1.0, INK_TEXT, Some(first));
        assert_eq!(next.texts.len(), 2);
        assert!((next.texts[0].y - intro_y).abs() < f32::EPSILON);
        assert_eq!(next.texts[1].text, "Second part grows");
        // Same result as building from scratch.
        let fresh = Doc::markdown(&fonts, "Intro paragraph.\n\nSecond part grows", 400.0, 1.0, INK_TEXT, None);
        assert!((fresh.height - next.height).abs() < 0.01);
    }

    #[test]
    fn hit_finds_the_nearest_piece() {
        let d = doc("First line.\n\nSecond line.");
        let second = &d.texts[1];
        assert_eq!(d.hit(1.0, second.y + 2.0).map(|p| p.0), Some(1));
        assert_eq!(d.hit(1.0, -50.0), Some((0, 0)));
    }

    #[test]
    fn code_scrolls_instead_of_wrapping() {
        let fonts = Fonts::load();
        let src = format!("```\n{}\n```", "x".repeat(300));
        let mut d = Doc::markdown(&fonts, &src, 400.0, 1.0, INK_TEXT, None);
        assert_eq!(d.texts[0].layout.line_count(), 1);
        assert!(matches!(d.decos[0].deco, Deco::Code { content_w, .. } if content_w > 400.0));
        let (x, y) = (d.texts[0].x + 10.0, d.texts[0].y + 2.0);
        let unscrolled = d.hit(x, y).unwrap().1;
        d.code_scroll[0] = 200.0;
        assert!(d.hit(x, y).unwrap().1 > unscrolled + 10, "hits land on the scrolled text");
        // A streamed rebuild keeps the scroll.
        let next = Doc::markdown(&fonts, &format!("{src}\n\nmore"), 400.0, 1.0, INK_TEXT, Some(d));
        assert!((next.code_scroll[0] - 200.0).abs() < f32::EPSILON);
    }

    #[test]
    fn columns_fit() {
        assert_eq!(fit_columns(&[50.0, 60.0], 400.0), [50.0, 60.0]);
        let w = fit_columns(&[50.0, 600.0, 900.0], 400.0);
        assert!((w[0] - 50.0).abs() < f32::EPSILON);
        assert!(w.iter().sum::<f32>() <= 401.0);
    }
}
