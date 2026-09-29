//! Fonts, text layout and the glyph atlas.
//!
//! Layout happens in physical pixels so glyphs rasterise and land on the
//! pixel grid exactly; the public API speaks logical pixels.
//!
//! ponytail: no shaping, bidi or font fallback. Latin/Cyrillic/Greek render
//! correctly (Inter covers them); emoji and CJK show as missing-glyph boxes.
//! Upgrade path: swap this module's internals for `swash`/`rustybuzz`.

use std::collections::HashMap;

use fontdue::{Font, FontSettings};

/// Side length of the square R8 glyph atlas texture. 2048 is the largest
/// size every backend (including GLES) guarantees.
pub const ATLAS_SIZE: u32 = 2048;

/// Font faces bundled with the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FontId {
    /// Inter Regular: body text.
    Regular,
    /// Inter SemiBold: titles, labels and emphasis.
    SemiBold,
}

/// The loaded font faces.
pub struct Fonts {
    regular: Font,
    semibold: Font,
}

impl Fonts {
    /// Parses the embedded fonts.
    ///
    /// # Panics
    /// Only if the embedded font files are corrupt, which is a build defect.
    #[must_use]
    pub fn load() -> Self {
        let load = |bytes: &'static [u8]| {
            Font::from_bytes(bytes, FontSettings::default()).expect("embedded font is a valid TrueType file")
        };
        Self {
            regular: load(include_bytes!("../assets/Inter-Regular.ttf")),
            semibold: load(include_bytes!("../assets/Inter-SemiBold.ttf")),
        }
    }

    fn get(&self, id: FontId) -> &Font {
        match id {
            FontId::Regular => &self.regular,
            FontId::SemiBold => &self.semibold,
        }
    }
}

/// How a piece of text should look.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    /// Face to use.
    pub font: FontId,
    /// Font size in logical pixels.
    pub size: f32,
    /// Line height as a multiple of `size`.
    pub line_height: f32,
}

impl Style {
    /// Regular text of `size` with a comfortable reading line height.
    #[must_use]
    pub const fn regular(size: f32) -> Self {
        Self { font: FontId::Regular, size, line_height: 1.55 }
    }

    /// Semibold text of `size` with a tight line height.
    #[must_use]
    pub const fn semibold(size: f32) -> Self {
        Self { font: FontId::SemiBold, size, line_height: 1.3 }
    }
}

/// A positioned glyph; coordinates are physical pixels relative to its line.
#[derive(Clone, Copy, Debug)]
struct Glyph {
    index: u16,
    /// Pen position (left edge of the advance box).
    x: f32,
    advance: f32,
    /// Byte offset of the source character.
    byte: u32,
    /// Whitespace: has an advance but no ink.
    space: bool,
}

/// One visual line.
#[derive(Clone, Copy, Debug)]
struct Line {
    /// Glyph range `start..end`.
    start: u32,
    end: u32,
    /// Source byte range `byte_start..byte_end` (excludes a trailing `\n`).
    byte_start: u32,
    byte_end: u32,
    /// Ink width excluding trailing whitespace, physical pixels.
    width: f32,
}

/// Text broken into lines and positioned, ready to draw or hit-test.
#[derive(Clone, Debug)]
pub struct TextLayout {
    glyphs: Vec<Glyph>,
    lines: Vec<Line>,
    font: FontId,
    /// Physical pixel size the glyphs were measured at.
    px: f32,
    scale: f32,
    /// Baseline offset from the top of a line, physical pixels.
    baseline: f32,
    /// Physical pixels between line tops.
    line_height: f32,
}

/// Horizontal alignment of each line within a box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    /// Flush left.
    Left,
    /// Centred.
    Center,
}

impl TextLayout {
    /// Lays out `text`, wrapping at word boundaries to `max_width` logical
    /// pixels when given. Words wider than the line are broken anywhere.
    #[must_use]
    pub fn new(fonts: &Fonts, text: &str, style: Style, max_width: Option<f32>, scale: f32) -> Self {
        let font = fonts.get(style.font);
        let px = style.size * scale;
        let metrics = font.horizontal_line_metrics(px).unwrap_or(fontdue::LineMetrics {
            ascent: px * 0.8,
            descent: -px * 0.2,
            line_gap: 0.0,
            new_line_size: px,
        });
        let line_height = (style.size * style.line_height * scale).round();
        let baseline = ((line_height - (metrics.ascent - metrics.descent)) * 0.5 + metrics.ascent).round();
        let max_width = max_width.map_or(f32::INFINITY, |w| (w * scale).floor());

        let mut glyphs: Vec<Glyph> = Vec::with_capacity(text.len());
        let mut lines = Vec::new();
        let mut line_start = 0usize;
        let mut byte_start = 0usize;
        let mut x = 0.0f32;
        // First glyph after the latest whitespace: where a wrap would split.
        let mut wrap_at: Option<usize> = None;
        let mut prev: Option<u16> = None;

        for (byte, ch) in text.char_indices() {
            if ch == '\n' {
                lines.push(finish_line(&glyphs, line_start, glyphs.len(), byte_start, byte));
                line_start = glyphs.len();
                byte_start = byte + 1;
                x = 0.0;
                wrap_at = None;
                prev = None;
                continue;
            }
            let index = font.lookup_glyph_index(if ch == '\t' { ' ' } else { ch });
            let mut advance = font.metrics_indexed(index, px).advance_width;
            if ch == '\t' {
                advance *= 4.0;
            }
            if let Some(prev) = prev {
                x += font.horizontal_kern_indexed(prev, index, px).unwrap_or(0.0);
            }

            if x + advance > max_width && !ch.is_whitespace() && glyphs.len() > line_start {
                let split = wrap_at.filter(|&w| w > line_start).unwrap_or(glyphs.len());
                let split_byte = glyphs.get(split).map_or(byte, |g| g.byte as usize);
                lines.push(finish_line(&glyphs, line_start, split, byte_start, split_byte));
                let shift = glyphs.get(split).map_or(x, |g| g.x);
                for glyph in &mut glyphs[split..] {
                    glyph.x -= shift;
                }
                x -= shift;
                line_start = split;
                byte_start = split_byte;
                wrap_at = None;
            }

            glyphs.push(Glyph { index, x, advance, byte: byte as u32, space: ch.is_whitespace() });
            x += advance;
            prev = Some(index);
            if ch.is_whitespace() {
                wrap_at = Some(glyphs.len());
            }
        }
        lines.push(finish_line(&glyphs, line_start, glyphs.len(), byte_start, text.len()));

        Self { glyphs, lines, font: style.font, px, scale, baseline, line_height }
    }

    /// Widest line in logical pixels.
    #[must_use]
    pub fn width(&self) -> f32 {
        self.lines.iter().map(|l| l.width).fold(0.0, f32::max) / self.scale
    }

    /// Total height in logical pixels (at least one line).
    #[must_use]
    pub fn height(&self) -> f32 {
        self.lines.len() as f32 * self.line_height()
    }

    /// Height of a single line in logical pixels.
    #[must_use]
    pub fn line_height(&self) -> f32 {
        self.line_height / self.scale
    }

    /// Number of visual lines (at least one).
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Shortens a single-line layout to `max_width` logical pixels, ending it
    /// with an ellipsis if anything was cut.
    pub fn truncate(&mut self, fonts: &Fonts, max_width: f32) {
        let max = max_width * self.scale;
        if self.lines.len() != 1 || self.lines[0].width <= max {
            return;
        }
        let line = &mut self.lines[0];
        let font = fonts.get(self.font);
        let index = font.lookup_glyph_index('…');
        let advance = font.metrics_indexed(index, self.px).advance_width;
        while self.glyphs.last().is_some_and(|g| g.x + g.advance + advance > max || g.space) {
            self.glyphs.pop();
        }
        let x = self.glyphs.last().map_or(0.0, |g| g.x + g.advance);
        self.glyphs.push(Glyph { index, x, advance, byte: line.byte_end, space: false });
        line.end = self.glyphs.len() as u32;
        line.width = x + advance;
    }

    /// Caret position for byte offset `byte`: `(x, y)` of the caret's top
    /// in logical pixels relative to the layout origin.
    #[must_use]
    pub fn caret(&self, byte: usize) -> (f32, f32) {
        let byte = byte as u32;
        let row = self.lines.partition_point(|l| l.byte_start <= byte).saturating_sub(1);
        let line = self.lines[row];
        let glyphs = &self.glyphs[line.start as usize..line.end as usize];
        let x = glyphs
            .iter()
            .find(|g| g.byte >= byte)
            .map_or_else(|| glyphs.last().map_or(0.0, |g| g.x + g.advance), |g| g.x);
        (x / self.scale, row as f32 * self.line_height())
    }

    /// Byte offset closest to the logical point `(x, y)`.
    #[must_use]
    pub fn hit(&self, x: f32, y: f32) -> usize {
        let row = ((y / self.line_height()).floor().max(0.0) as usize).min(self.lines.len() - 1);
        let line = self.lines[row];
        let x = x * self.scale;
        self.glyphs[line.start as usize..line.end as usize]
            .iter()
            .find(|g| g.x + g.advance * 0.5 > x)
            .map_or(line.byte_end, |g| g.byte) as usize
    }

    /// The source byte range of each visual line, with its top `y` in
    /// logical pixels. Used to draw selections.
    pub fn line_spans(&self) -> impl Iterator<Item = (usize, usize, f32)> + '_ {
        self.lines
            .iter()
            .enumerate()
            .map(|(row, l)| (l.byte_start as usize, l.byte_end as usize, row as f32 * self.line_height()))
    }
}

fn finish_line(glyphs: &[Glyph], start: usize, end: usize, byte_start: usize, byte_end: usize) -> Line {
    // Trailing whitespace glyphs have zero-width bitmaps but real advances;
    // exclude them so right-aligned and centred text looks balanced.
    let width = glyphs[start..end]
        .iter()
        .rev()
        .find(|g| !g.space)
        .map_or(0.0, |g| g.x + g.advance);
    Line { start: start as u32, end: end as u32, byte_start: byte_start as u32, byte_end: byte_end as u32, width }
}

/// Where a rasterised glyph lives in the atlas, plus its bearing.
#[derive(Clone, Copy, Debug)]
pub struct AtlasEntry {
    /// Texel rectangle in the atlas.
    pub x: u16,
    /// See `x`.
    pub y: u16,
    /// Bitmap width; zero for glyphs without ink.
    pub w: u16,
    /// Bitmap height.
    pub h: u16,
    /// Horizontal bearing from the pen position.
    pub xmin: i16,
    /// Bottom of the bitmap relative to the baseline (y up).
    pub ymin: i16,
}

/// A rectangle of atlas texels waiting to be copied to the GPU.
#[derive(Debug)]
pub struct Upload {
    /// Destination texel x.
    pub x: u32,
    /// Destination texel y.
    pub y: u32,
    /// Width in texels (= bytes per row).
    pub w: u32,
    /// Height in texels.
    pub h: u32,
    /// Coverage values, row-major.
    pub data: Vec<u8>,
}

/// CPU side of the glyph atlas: a shelf packer plus a lookup table.
///
/// ponytail: when the atlas fills up it is wiped and the frame redrawn;
/// fine for UI text volumes, add an LRU if many sizes/scripts appear.
#[derive(Default)]
pub struct GlyphAtlas {
    entries: HashMap<(FontId, u16, u32), AtlasEntry>,
    cursor_x: u32,
    cursor_y: u32,
    row_height: u32,
    /// Rasterised glyphs not yet copied to the GPU texture.
    pub uploads: Vec<Upload>,
}

impl GlyphAtlas {
    /// Forgets every glyph. Pending uploads are dropped too since their
    /// texels are about to be reused.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Returns the atlas entry for a glyph, rasterising it on first use.
    /// `None` means the atlas is full.
    fn get(&mut self, fonts: &Fonts, font: FontId, index: u16, px: f32) -> Option<AtlasEntry> {
        let key = (font, index, px.to_bits());
        if let Some(entry) = self.entries.get(&key) {
            return Some(*entry);
        }
        let (metrics, data) = fonts.get(font).rasterize_indexed(index, px);
        let (w, h) = (metrics.width as u32, metrics.height as u32);
        let mut entry = AtlasEntry { x: 0, y: 0, w: w as u16, h: h as u16, xmin: metrics.xmin as i16, ymin: metrics.ymin as i16 };
        if w > 0 && h > 0 {
            // One texel of padding keeps linear filtering from bleeding.
            if self.cursor_x + w + 1 > ATLAS_SIZE {
                self.cursor_x = 0;
                self.cursor_y += self.row_height;
                self.row_height = 0;
            }
            if self.cursor_y + h + 1 > ATLAS_SIZE || w + 1 > ATLAS_SIZE {
                return None;
            }
            entry.x = self.cursor_x as u16;
            entry.y = self.cursor_y as u16;
            self.uploads.push(Upload { x: self.cursor_x, y: self.cursor_y, w, h, data });
            self.cursor_x += w + 1;
            self.row_height = self.row_height.max(h + 1);
        }
        self.entries.insert(key, entry);
        Some(entry)
    }
}

/// A glyph ready to be turned into a GPU instance, in logical pixels.
pub struct PlacedGlyph {
    /// Quad position and size.
    pub rect: [f32; 4],
    /// Atlas texel rectangle.
    pub uv: [f32; 4],
}

impl TextLayout {
    /// Resolves glyphs of rows intersecting `y_min..y_max` to atlas quads at
    /// logical origin `(x, y)`, aligned within `box_width`.
    ///
    /// Returns `false` if the atlas overflowed; the frame must be redrawn
    /// after [`GlyphAtlas::clear`].
    pub fn place(
        &self,
        fonts: &Fonts,
        atlas: &mut GlyphAtlas,
        origin: (f32, f32),
        align: (Align, f32),
        visible: (f32, f32),
        mut emit: impl FnMut(PlacedGlyph),
    ) -> bool {
        let s = self.scale;
        let ox = (origin.0 * s).round();
        let oy = (origin.1 * s).round();
        let (y_min, y_max) = (visible.0 * s, visible.1 * s);
        for (row, line) in self.lines.iter().enumerate() {
            let top = oy + row as f32 * self.line_height;
            if top + self.line_height < y_min {
                continue;
            }
            if top > y_max {
                break;
            }
            let dx = match align.0 {
                Align::Left => 0.0,
                Align::Center => ((align.1 * s - line.width) * 0.5).round(),
            };
            let baseline = top + self.baseline;
            for glyph in &self.glyphs[line.start as usize..line.end as usize] {
                let Some(entry) = atlas.get(fonts, self.font, glyph.index, self.px) else {
                    return false;
                };
                if entry.w == 0 {
                    continue;
                }
                let gx = ox + dx + glyph.x.round() + f32::from(entry.xmin);
                let gy = baseline - f32::from(entry.h) - f32::from(entry.ymin);
                let (w, h) = (f32::from(entry.w), f32::from(entry.h));
                emit(PlacedGlyph {
                    rect: [gx / s, gy / s, w / s, h / s],
                    uv: [f32::from(entry.x), f32::from(entry.y), w, h],
                });
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_symbols_exist_in_both_weights() {
        // There is no font fallback, so every symbol the UI draws must be in Inter.
        let fonts = Fonts::load();
        for font in [FontId::Regular, FontId::SemiBold] {
            for ch in ['✓', '↑', '…', '·', '→', '$', '×'] {
                assert_ne!(fonts.get(font).lookup_glyph_index(ch), 0, "{ch:?} missing from {font:?}");
            }
        }
    }

    fn layout(text: &str, width: Option<f32>) -> TextLayout {
        TextLayout::new(&Fonts::load(), text, Style::regular(16.0), width, 1.0)
    }

    #[test]
    fn wraps_at_word_boundaries() {
        let l = layout("hello world hello world", Some(100.0));
        assert!(l.line_count() >= 2);
        assert!(l.width() <= 100.0);
        // Every line starts at a word, never mid-word.
        for (start, _, _) in l.line_spans().skip(1) {
            assert!(start == 0 || "hello world hello world".as_bytes()[start - 1] == b' ');
        }
    }

    #[test]
    fn breaks_long_words_and_hard_newlines() {
        assert!(layout(&"x".repeat(200), Some(50.0)).line_count() > 1);
        let l = layout("a\n\nb\n", None);
        assert_eq!(l.line_count(), 4);
        assert!((l.caret(5).1 - l.line_height() * 3.0).abs() < 1e-3);
    }

    #[test]
    fn caret_and_hit_round_trip() {
        let l = layout("hello world", None);
        for byte in 0..=11 {
            let (x, y) = l.caret(byte);
            assert_eq!(l.hit(x + 0.1, y + 1.0), byte, "byte {byte}");
        }
        assert_eq!(layout("", None).caret(0), (0.0, 0.0));
    }

    #[test]
    fn truncates_with_ellipsis() {
        let fonts = Fonts::load();
        let mut l = TextLayout::new(&fonts, "a fairly long conversation title", Style::regular(14.0), None, 1.0);
        l.truncate(&fonts, 80.0);
        assert!(l.width() <= 80.0);
    }
}
