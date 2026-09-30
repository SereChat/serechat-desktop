//! Text layout and the glyph atlas.
//!
//! Layout happens in physical pixels so glyphs rasterise and land on the
//! pixel grid exactly; the public API speaks logical pixels. A layout is a
//! string plus optional styled runs (font, size, colour slot, decorations,
//! link), each character resolved through the font fallback chain in
//! `font.rs`. Emoji sequences collapse into single ligature glyphs, and lines
//! may break between CJK characters, which have no spaces.

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use crate::atlas::{NO_SHELF, Shelves};
pub use crate::font::{FontId, Fonts};
use crate::font::{Layer, is_cjk, is_emoji_joiner, is_invisible};

/// Side length of the square R8 glyph atlas texture. 2048 is the largest
/// size every backend (including GLES) guarantees.
pub const ATLAS_SIZE: u32 = 2048;

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
        Self { font: FontId::REGULAR, size, line_height: 1.55 }
    }

    /// Semibold text of `size` with a tight line height.
    #[must_use]
    pub const fn semibold(size: f32) -> Self {
        Self { font: FontId::SEMIBOLD, size, line_height: 1.3 }
    }

    /// Monospaced text of `size`, for code.
    #[must_use]
    pub const fn mono(size: f32) -> Self {
        Self { font: FontId::MONO, size, line_height: 1.6 }
    }
}

/// Decoration flag: a rounded background behind the run (inline code).
pub const BACKGROUND: u8 = 1;
/// Decoration flag: underline (links).
pub const UNDERLINE: u8 = 2;
/// Decoration flag: strike-through.
pub const STRIKE: u8 = 4;

/// Styling of one run inside a rich layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Run {
    /// Preferred face.
    pub font: FontId,
    /// Size relative to the layout's base size.
    pub size: f32,
    /// Colour slot, resolved against a palette when drawing.
    pub ink: u8,
    /// [`BACKGROUND`], [`UNDERLINE`] and [`STRIKE`] flags.
    pub decoration: u8,
    /// Link number plus one; zero when the run is not a link.
    pub link: u16,
}

impl Run {
    /// A plain run in `font`.
    #[must_use]
    pub const fn plain(font: FontId) -> Self {
        Self { font, size: 1.0, ink: 0, decoration: 0, link: 0 }
    }
}

/// A positioned glyph; coordinates are physical pixels relative to its line.
#[derive(Clone, Copy, Debug)]
struct Glyph {
    index: u16,
    font: FontId,
    /// Index into [`TextLayout::runs`].
    run: u16,
    /// Pen position (left edge of the advance box).
    x: f32,
    advance: f32,
    /// Pixels per em this glyph renders at.
    px: f32,
    /// Byte offset of the source character (cluster start).
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
    runs: Vec<Run>,
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

/// A decorated stretch of one line, in logical pixels relative to the layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decoration {
    /// Left edge.
    pub x: f32,
    /// Line top.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Line height.
    pub h: f32,
    /// Baseline offset from the line top.
    pub baseline: f32,
    /// The run's styling.
    pub run: Run,
}

fn is_regional_indicator(c: char) -> bool {
    matches!(c as u32, 0x1F1E6..=0x1F1FF)
}

/// Mutable state of a layout pass.
struct Builder<'a> {
    fonts: &'a Fonts,
    glyphs: Vec<Glyph>,
    lines: Vec<Line>,
    max_width: f32,
    line_start: usize,
    byte_start: usize,
    x: f32,
    /// First glyph of the next line if the current one wraps now.
    wrap_at: Option<usize>,
    /// Previous glyph, for kerning: (font, glyph, px).
    prev: Option<(FontId, u16, f32)>,
}

impl Builder<'_> {
    fn newline(&mut self, byte: usize) {
        self.lines.push(finish_line(&self.glyphs, self.line_start, self.glyphs.len(), self.byte_start, byte));
        self.line_start = self.glyphs.len();
        self.byte_start = byte + 1;
        self.x = 0.0;
        self.wrap_at = None;
        self.prev = None;
    }

    fn push(&mut self, font: FontId, index: u16, run: u16, px: f32, byte: usize, ch: char) {
        let face = self.fonts.face(font);
        let em = px / face.units_per_em();
        let space = ch.is_whitespace();
        let mut advance = face.advance(index) * em;
        if ch == '\t' {
            advance *= 4.0;
        }
        if let Some((pf, pg, ppx)) = self.prev
            && pf == font
            && (ppx - px).abs() < f32::EPSILON
        {
            self.x += face.kerning(pg, index) * em;
        }
        // CJK text has no spaces: a line may break before any ideograph.
        if is_cjk(ch) && self.glyphs.len() > self.line_start {
            self.wrap_at = Some(self.glyphs.len());
        }
        if self.x + advance > self.max_width && !space && self.glyphs.len() > self.line_start {
            let split = self.wrap_at.filter(|&w| w > self.line_start).unwrap_or(self.glyphs.len());
            let split_byte = self.glyphs.get(split).map_or(byte, |g| g.byte as usize);
            self.lines.push(finish_line(&self.glyphs, self.line_start, split, self.byte_start, split_byte));
            let shift = self.glyphs.get(split).map_or(self.x, |g| g.x);
            for glyph in &mut self.glyphs[split..] {
                glyph.x -= shift;
            }
            self.x -= shift;
            self.line_start = split;
            self.byte_start = split_byte;
            self.wrap_at = None;
        }
        self.glyphs.push(Glyph { index, font, run, x: self.x, advance, px, byte: byte as u32, space });
        self.x += advance;
        self.prev = Some((font, index, px));
        if space || is_cjk(ch) {
            self.wrap_at = Some(self.glyphs.len());
        }
    }
}

impl TextLayout {
    /// Lays out `text` in one style, wrapping at word boundaries to
    /// `max_width` logical pixels when given. Words wider than the line are
    /// broken anywhere.
    #[must_use]
    pub fn new(fonts: &Fonts, text: &str, style: Style, max_width: Option<f32>, scale: f32) -> Self {
        Self::rich(fonts, text, &[], style, max_width, scale)
    }

    /// Lays out `text` with styled `spans` (sorted, non-overlapping byte
    /// ranges); text outside every span uses `base` with ink 0. Line metrics
    /// always come from `base`.
    #[must_use]
    pub fn rich(fonts: &Fonts, text: &str, spans: &[(Range<usize>, Run)], base: Style, max_width: Option<f32>, scale: f32) -> Self {
        let primary = fonts.face(base.font);
        let px = base.size * scale;
        let (ascent, descent) = primary.vertical_metrics();
        let (ascent, descent) = (ascent / primary.units_per_em() * px, descent / primary.units_per_em() * px);
        let line_height = (base.size * base.line_height * scale).round();
        let baseline = ((line_height - (ascent - descent)) * 0.5 + ascent).round();

        let mut runs = Vec::with_capacity(spans.len() + 1);
        runs.push(Run::plain(base.font));
        runs.extend(spans.iter().map(|(_, run)| *run));

        let mut b = Builder {
            fonts,
            glyphs: Vec::with_capacity(text.len()),
            lines: Vec::new(),
            max_width: max_width.map_or(f32::INFINITY, |w| (w * scale).floor()),
            line_start: 0,
            byte_start: 0,
            x: 0.0,
            wrap_at: None,
            prev: None,
        };
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut span = 0;
        let mut i = 0;
        while i < chars.len() {
            let (byte, ch) = chars[i];
            while span < spans.len() && spans[span].0.end <= byte {
                span += 1;
            }
            let run_index = if spans.get(span).is_some_and(|(r, _)| r.start <= byte) { span + 1 } else { 0 };
            let run = runs[run_index];
            let run_index = run_index as u16;
            i += 1;
            if ch == '\n' {
                b.newline(byte);
                continue;
            }
            if is_invisible(ch) {
                continue;
            }
            let glyph_px = px * run.size;
            let next = chars.get(i).map(|&(_, c)| c);
            let (font, index) = fonts.resolve(run.font, if ch == '\t' { ' ' } else { ch }, next);
            if font != FontId::EMOJI {
                b.push(font, index, run_index, glyph_px, byte, ch);
                continue;
            }

            // An emoji cluster: joiners, whatever follows a ZWJ, and the
            // second half of a flag all belong to the first character.
            let face = fonts.face(font);
            let mut sequence = vec![index];
            while let Some(&(_, c)) = chars.get(i) {
                let prev = chars[i - 1].1;
                let flag_pair = is_regional_indicator(ch) && is_regional_indicator(c) && sequence.len() == 1;
                if !(is_emoji_joiner(c) || prev == '\u{200D}' || flag_pair) {
                    break;
                }
                sequence.extend(face.glyph(c));
                i += 1;
            }
            let mut k = 0;
            while k < sequence.len() {
                let (glyph, used) = face.ligature(&sequence[k..]).unwrap_or((sequence[k], 1));
                b.push(font, glyph, run_index, glyph_px, byte, ch);
                k += used;
            }
        }
        let end = text.len();
        b.lines.push(finish_line(&b.glyphs, b.line_start, b.glyphs.len(), b.byte_start, end));

        Self { glyphs: b.glyphs, lines: b.lines, runs, scale, baseline, line_height }
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

    /// Distance from a line's top to its baseline, logical pixels.
    #[must_use]
    pub fn baseline(&self) -> f32 {
        self.baseline / self.scale
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
        let Some(last) = self.glyphs.last().copied() else { return };
        let (font, index) = fonts.resolve(self.runs[usize::from(last.run)].font, '…', None);
        let face = fonts.face(font);
        let advance = face.advance(index) * last.px / face.units_per_em();
        while self.glyphs.last().is_some_and(|g| g.x + g.advance + advance > max || g.space) {
            self.glyphs.pop();
        }
        let line = &mut self.lines[0];
        let x = self.glyphs.last().map_or(0.0, |g| g.x + g.advance);
        self.glyphs.push(Glyph { index, font, x, advance, byte: line.byte_end, space: false, ..last });
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

    /// The link number (as given in [`Run::link`], minus one) of the glyph
    /// under the logical point `(x, y)`, if any.
    #[must_use]
    pub fn link_at(&self, x: f32, y: f32) -> Option<u16> {
        if y < 0.0 || x < 0.0 {
            return None;
        }
        let line = self.lines.get((y / self.line_height()) as usize)?;
        let x = x * self.scale;
        let glyph = self.glyphs[line.start as usize..line.end as usize].iter().find(|g| x >= g.x && x < g.x + g.advance)?;
        self.runs[usize::from(glyph.run)].link.checked_sub(1)
    }

    /// The source byte range of each visual line, with its top `y` in
    /// logical pixels. Used to draw selections.
    pub fn line_spans(&self) -> impl Iterator<Item = (usize, usize, f32)> + '_ {
        self.lines
            .iter()
            .enumerate()
            .map(|(row, l)| (l.byte_start as usize, l.byte_end as usize, row as f32 * self.line_height()))
    }

    /// Stretches of decorated runs, one per line and run.
    #[must_use]
    pub fn decorations(&self) -> Vec<Decoration> {
        let mut out = Vec::new();
        let s = self.scale;
        for (row, line) in self.lines.iter().enumerate() {
            let glyphs = &self.glyphs[line.start as usize..line.end as usize];
            let mut i = 0;
            while i < glyphs.len() {
                let run = self.runs[usize::from(glyphs[i].run)];
                let mut j = i + 1;
                while j < glyphs.len() && glyphs[j].run == glyphs[i].run {
                    j += 1;
                }
                if run.decoration != 0 {
                    let (x0, x1) = (glyphs[i].x, glyphs[j - 1].x + glyphs[j - 1].advance);
                    out.push(Decoration {
                        x: x0 / s,
                        y: row as f32 * self.line_height() ,
                        w: (x1 - x0) / s,
                        h: self.line_height(),
                        baseline: self.baseline(),
                        run,
                    });
                }
                i = j;
            }
        }
        out
    }
}

fn finish_line(glyphs: &[Glyph], start: usize, end: usize, byte_start: usize, byte_end: usize) -> Line {
    // Trailing whitespace has advances but no ink; exclude it so centred and
    // right-aligned text looks balanced.
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
    /// Atlas shelf holding it ([`NO_SHELF`] for glyphs without ink).
    pub shelf: u16,
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

/// CPU side of the glyph atlas: a shelf packer plus lookup tables. When
/// full, glyphs not drawn recently are evicted a shelf at a time; only if a
/// single frame needs more than the whole atlas is it wiped and the frame
/// redrawn.
pub struct GlyphAtlas {
    entries: HashMap<(FontId, u16, u32), AtlasEntry>,
    /// Colour layers per glyph (`None`: not a colour glyph).
    layers: HashMap<(FontId, u16), Option<Rc<[Layer]>>>,
    shelves: Shelves,
    /// Rasterised glyphs not yet copied to the GPU texture.
    pub uploads: Vec<Upload>,
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        Self { entries: HashMap::new(), layers: HashMap::new(), shelves: Shelves::new(ATLAS_SIZE), uploads: Vec::new() }
    }
}

impl GlyphAtlas {
    /// Forgets every glyph. Pending uploads are dropped too since their
    /// texels are about to be reused.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Starts a frame, for least-recently-used eviction.
    pub fn next_frame(&mut self) {
        self.shelves.next_frame();
    }

    fn layers(&mut self, fonts: &Fonts, font: FontId, index: u16) -> Option<Rc<[Layer]>> {
        self.layers.entry((font, index)).or_insert_with(|| fonts.face(font).layers(index).map(Rc::from)).clone()
    }

    /// Returns the atlas entry for a glyph, rasterising it on first use.
    /// `None` means the atlas is full.
    fn get(&mut self, fonts: &Fonts, font: FontId, index: u16, px: f32) -> Option<AtlasEntry> {
        let key = (font, index, px.to_bits());
        if let Some(entry) = self.entries.get(&key) {
            self.shelves.touch(entry.shelf);
            return Some(*entry);
        }
        let mut entry = AtlasEntry { x: 0, y: 0, w: 0, h: 0, xmin: 0, ymin: 0, shelf: NO_SHELF };
        if let Some(bitmap) = fonts.face(font).rasterize(index, px) {
            let (w, h) = (bitmap.w, bitmap.h);
            // One texel of padding keeps linear filtering from bleeding.
            let slot = self.shelves.alloc(w + 1, h + 1)?;
            if let Some(shelf) = slot.evicted {
                self.entries.retain(|_, e| e.shelf != shelf);
            }
            entry = AtlasEntry {
                x: slot.x as u16,
                y: slot.y as u16,
                w: w as u16,
                h: h as u16,
                xmin: bitmap.xmin as i16,
                ymin: bitmap.ymin as i16,
                shelf: slot.shelf,
            };
            // The padding is uploaded too: an evicted glyph may have left ink there.
            self.uploads.push(Upload { x: slot.x, y: slot.y, w: w + 1, h: h + 1, data: pad(&bitmap.data, w, h, 1) });
        }
        self.entries.insert(key, entry);
        Some(entry)
    }
}

/// `data` (`w`×`h` texels of `bpp` bytes) with a transparent column on the
/// right and row at the bottom.
#[must_use]
pub fn pad(data: &[u8], w: u32, h: u32, bpp: usize) -> Vec<u8> {
    let (row, padded) = (w as usize * bpp, (w as usize + 1) * bpp);
    let mut out = vec![0; padded * (h as usize + 1)];
    for (src, dst) in data.chunks_exact(row).zip(out.chunks_exact_mut(padded)) {
        dst[..row].copy_from_slice(src);
    }
    out
}

/// A glyph ready to be turned into a GPU instance, in logical pixels.
pub struct PlacedGlyph {
    /// Quad position and size.
    pub rect: [f32; 4],
    /// Atlas texel rectangle.
    pub uv: [f32; 4],
    /// Colour slot of the glyph's run.
    pub ink: u8,
    /// Fixed colour of an emoji layer (straight sRGB), overriding `ink`.
    pub color: Option<[f32; 4]>,
}

impl TextLayout {
    /// Resolves glyphs inside the `visible` rectangle `[x0, y0, x1, y1]` to
    /// atlas quads at logical origin `(x, y)`, aligned within `box_width`.
    ///
    /// Returns `false` if the atlas overflowed; the frame must be redrawn
    /// after [`GlyphAtlas::clear`].
    pub fn place(
        &self,
        fonts: &Fonts,
        atlas: &mut GlyphAtlas,
        origin: (f32, f32),
        align: (Align, f32),
        visible: [f32; 4],
        mut emit: impl FnMut(PlacedGlyph),
    ) -> bool {
        let s = self.scale;
        let ox = (origin.0 * s).round();
        let oy = (origin.1 * s).round();
        let [x_min, y_min, x_max, y_max] = visible.map(|v| v * s);
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
                if glyph.space {
                    continue;
                }
                let ink = self.runs[usize::from(glyph.run)].ink;
                let pen = ox + dx + glyph.x.round();
                // Ink may overhang the advance a little; an em of margin covers it.
                if pen + glyph.advance + glyph.px < x_min || pen - glyph.px > x_max {
                    continue;
                }
                let mut place = |atlas: &mut GlyphAtlas, index: u16, color: Option<[f32; 4]>| -> bool {
                    let Some(entry) = atlas.get(fonts, glyph.font, index, glyph.px) else {
                        return false;
                    };
                    if entry.w > 0 {
                        let (w, h) = (f32::from(entry.w), f32::from(entry.h));
                        let gx = pen + f32::from(entry.xmin);
                        let gy = baseline - h - f32::from(entry.ymin);
                        emit(PlacedGlyph { rect: [gx / s, gy / s, w / s, h / s], uv: [f32::from(entry.x), f32::from(entry.y), w, h], ink, color });
                    }
                    true
                };
                let layers = if glyph.font == FontId::EMOJI { atlas.layers(fonts, glyph.font, glyph.index) } else { None };
                let ok = match layers {
                    Some(layers) => layers.iter().all(|&(index, rgba)| {
                        place(atlas, index, rgba.map(|c| c.map(|v| f32::from(v) / 255.0)))
                    }),
                    None => place(atlas, glyph.index, None),
                };
                if !ok {
                    return false;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn emoji_sequences_are_single_clusters() {
        let text = "a👨\u{200D}👩\u{200D}👧b🇳🇱c";
        let l = layout(text, None);
        // a, family, b, flag, c.
        assert_eq!(l.glyphs.len(), 5);
        assert!(l.glyphs.iter().filter(|g| g.font == FontId::EMOJI).count() == 2);
        // The caret skips over a whole sequence.
        let b = text.find('b').unwrap();
        assert_eq!(l.hit(l.caret(b).0 + 0.1, 1.0), b);
    }

    #[test]
    fn cjk_wraps_without_spaces() {
        let l = layout(&"汉字".repeat(40), Some(120.0));
        assert!(l.line_count() > 1);
        assert!(l.width() <= 120.0);
    }

    #[test]
    fn runs_style_ranges_and_links() {
        let fonts = Fonts::load();
        let link = Run { ink: 2, decoration: UNDERLINE, link: 1, ..Run::plain(FontId::REGULAR) };
        let code = Run { decoration: BACKGROUND, ..Run::plain(FontId::MONO) };
        let l = TextLayout::rich(&fonts, "see docs and code", &[(4..8, link), (13..17, code)], Style::regular(16.0), None, 1.0);
        assert!(l.glyphs[4..8].iter().all(|g| g.run == 1));
        assert!(l.glyphs[13..17].iter().all(|g| g.font == FontId::MONO));
        let decorations = l.decorations();
        assert_eq!(decorations.len(), 2);
        let (x, _) = l.caret(5);
        assert_eq!(l.link_at(x + 1.0, 4.0), Some(0));
        assert_eq!(l.link_at(1.0, 4.0), None);
    }
}
