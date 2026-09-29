//! Font faces and fallback.
//!
//! Embedded: Inter (regular, semibold, italic) for UI and prose, JetBrains
//! Mono for code, and Twemoji for colour emoji. Scripts those don't cover
//! (Chinese, Japanese, Korean, and so on) come from the operating system's
//! fonts, loaded lazily the first time a character needs them.
//!
//! Faces are parsed by `ttf-parser`, which reads tables on demand, so even a
//! 20 MB CJK collection costs only its file read. Kerning comes from GPOS
//! (falling back to `kern`); emoji sequences (ZWJ families, flags, skin
//! tones, keycaps) are formed with the emoji font's GSUB ligatures.
//!
//! ponytail: no full shaping (no Arabic joining, Indic reordering or bidi).
//! Those scripts render as isolated glyphs; `rustybuzz` is the upgrade path.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;

use ttf_parser::gpos::{PairAdjustment, PositioningSubtable};
use ttf_parser::gsub::SubstitutionSubtable;
use ttf_parser::{GlyphId, RgbaColor, Tag, colr};

use crate::raster::{self, Bitmap};

/// Identifies a face: the embedded ones by constant, system fallbacks after.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FontId(u8);

impl FontId {
    /// Inter Regular: body text.
    pub const REGULAR: Self = Self(0);
    /// Inter SemiBold: titles, labels and emphasis.
    pub const SEMIBOLD: Self = Self(1);
    /// Inter Italic.
    pub const ITALIC: Self = Self(2);
    /// JetBrains Mono: code.
    pub const MONO: Self = Self(3);
    /// Twemoji: colour emoji.
    pub const EMOJI: Self = Self(4);
    const EMBEDDED: usize = 5;
}

/// A colour-glyph layer: glyph id and its colour, or `None` for "text colour".
pub type Layer = (u16, Option<[u8; 4]>);

/// One parsed face with the lookups the layout needs.
pub struct Face {
    inner: ttf_parser::Face<'static>,
    /// GPOS lookups behind the `kern` feature.
    kern_lookups: Vec<u16>,
    /// GSUB lookups behind ligature-forming features.
    liga_lookups: Vec<u16>,
    kern_cache: RefCell<HashMap<u32, i16>>,
}

impl Face {
    fn parse(data: &'static [u8], index: u32) -> Option<Self> {
        let face = ttf_parser::Face::parse(data, index).ok()?;
        let lookups = |table: Option<ttf_parser::opentype_layout::LayoutTable<'_>>, tags: &[&[u8; 4]]| {
            let mut out: Vec<u16> = table
                .into_iter()
                .flat_map(|t| t.features.into_iter())
                .filter(|f| tags.iter().any(|tag| f.tag == Tag::from_bytes(tag)))
                .flat_map(|f| f.lookup_indices.into_iter())
                .collect();
            out.sort_unstable();
            out.dedup();
            out
        };
        let kern_lookups = lookups(face.tables().gpos, &[b"kern"]);
        let liga_lookups = lookups(face.tables().gsub, &[b"ccmp", b"liga", b"rlig", b"clig"]);
        Some(Self { inner: face, kern_lookups, liga_lookups, kern_cache: RefCell::default() })
    }

    /// Glyph for `c`, if the face has one.
    pub fn glyph(&self, c: char) -> Option<u16> {
        self.inner.glyph_index(c).map(|g| g.0).filter(|&g| g != 0)
    }

    /// Font units per em.
    pub fn units_per_em(&self) -> f32 {
        f32::from(self.inner.units_per_em())
    }

    /// Ascender and descender (negative) in font units.
    pub fn vertical_metrics(&self) -> (f32, f32) {
        (f32::from(self.inner.ascender()), f32::from(self.inner.descender()))
    }

    /// Advance width in font units.
    pub fn advance(&self, glyph: u16) -> f32 {
        f32::from(self.inner.glyph_hor_advance(GlyphId(glyph)).unwrap_or(0))
    }

    /// Pair kerning in font units.
    pub fn kerning(&self, left: u16, right: u16) -> f32 {
        let key = u32::from(left) << 16 | u32::from(right);
        if let Some(&k) = self.kern_cache.borrow().get(&key) {
            return f32::from(k);
        }
        let k = self.gpos_kerning(left, right).or_else(|| self.kern_table(left, right)).unwrap_or(0);
        self.kern_cache.borrow_mut().insert(key, k);
        f32::from(k)
    }

    fn gpos_kerning(&self, left: u16, right: u16) -> Option<i16> {
        let gpos = self.inner.tables().gpos?;
        let (l, r) = (GlyphId(left), GlyphId(right));
        for &index in &self.kern_lookups {
            let Some(lookup) = gpos.lookups.get(index) else { continue };
            for i in 0..lookup.subtables.len() {
                let Some(PositioningSubtable::Pair(pair)) = lookup.subtables.get::<PositioningSubtable>(i) else { continue };
                let found = match pair {
                    PairAdjustment::Format1 { coverage, sets } => {
                        coverage.get(l).and_then(|set| sets.get(set)).and_then(|set| set.get(r))
                    }
                    PairAdjustment::Format2 { coverage, classes, matrix } => {
                        coverage.contains(l).then(|| matrix.get((classes.0.get(l), classes.1.get(r)))).flatten()
                    }
                };
                if let Some((first, _)) = found {
                    return Some(first.x_advance);
                }
            }
        }
        None
    }

    fn kern_table(&self, left: u16, right: u16) -> Option<i16> {
        let kern = self.inner.tables().kern?;
        kern.subtables
            .into_iter()
            .filter(|s| s.horizontal && !s.variable)
            .find_map(|s| s.glyphs_kerning(GlyphId(left), GlyphId(right)))
    }

    /// The longest GSUB ligature starting at `glyphs[0]`, as the ligature
    /// glyph and the number of input glyphs it replaces.
    pub fn ligature(&self, glyphs: &[u16]) -> Option<(u16, usize)> {
        let gsub = self.inner.tables().gsub?;
        let first = GlyphId(*glyphs.first()?);
        let mut best: Option<(u16, usize)> = None;
        for &index in &self.liga_lookups {
            let Some(lookup) = gsub.lookups.get(index) else { continue };
            for i in 0..lookup.subtables.len() {
                let Some(SubstitutionSubtable::Ligature(sub)) = lookup.subtables.get::<SubstitutionSubtable>(i) else { continue };
                let Some(set) = sub.coverage.get(first).and_then(|c| sub.ligature_sets.get(c)) else { continue };
                for ligature in set {
                    let len = usize::from(ligature.components.len()) + 1;
                    let matches = len <= glyphs.len()
                        && ligature.components.into_iter().zip(&glyphs[1..]).all(|(component, &g)| component.0 == g);
                    if matches && best.is_none_or(|(_, n)| len > n) {
                        best = Some((ligature.glyph.0, len));
                    }
                }
            }
        }
        best
    }

    /// Colour layers of a COLR glyph, bottom first; `None` for plain glyphs.
    pub fn layers(&self, glyph: u16) -> Option<Vec<Layer>> {
        /// Stand-in for the text colour, mapped back to `None`.
        const FOREGROUND: RgbaColor = RgbaColor { red: 1, green: 2, blue: 3, alpha: 4 };
        struct Collect {
            current: Option<u16>,
            layers: Vec<Layer>,
        }
        impl<'a> colr::Painter<'a> for Collect {
            fn outline_glyph(&mut self, glyph: GlyphId) {
                self.current = Some(glyph.0);
            }
            fn paint(&mut self, paint: colr::Paint<'a>) {
                // COLRv0 only paints solids; gradients (v1) are skipped.
                if let (Some(glyph), colr::Paint::Solid(c)) = (self.current, paint) {
                    let color = (c != FOREGROUND).then_some([c.red, c.green, c.blue, c.alpha]);
                    self.layers.push((glyph, color));
                }
            }
            fn push_clip(&mut self) {}
            fn push_clip_box(&mut self, _: colr::ClipBox) {}
            fn pop_clip(&mut self) {}
            fn push_layer(&mut self, _: colr::CompositeMode) {}
            fn pop_layer(&mut self) {}
            fn push_transform(&mut self, _: ttf_parser::Transform) {}
            fn pop_transform(&mut self) {}
        }
        if !self.inner.is_color_glyph(GlyphId(glyph)) {
            return None;
        }
        let mut collect = Collect { current: None, layers: Vec::new() };
        self.inner.paint_color_glyph(GlyphId(glyph), 0, FOREGROUND, &mut collect)?;
        (!collect.layers.is_empty()).then_some(collect.layers)
    }

    /// Rasterises a glyph at `px` pixels per em.
    pub fn rasterize(&self, glyph: u16, px: f32) -> Option<Bitmap> {
        raster::rasterize(&self.inner, GlyphId(glyph), px)
    }
}

/// A system font tried when the embedded ones lack a character.
struct Fallback {
    paths: &'static [&'static str],
    /// Only characters passing this are looked up, so unrelated scripts never
    /// load a large font.
    covers: fn(char) -> bool,
    face: OnceCell<Option<Face>>,
}

impl Fallback {
    fn face(&self) -> Option<&Face> {
        self.face.get_or_init(|| self.paths.iter().find_map(|path| load_system_font(path))).as_ref()
    }
}

/// Reads a font file for the process lifetime. Paths may start with `%FONTS%`
/// (the Windows fonts folder) and end with `#index` for collections.
fn load_system_font(spec: &str) -> Option<Face> {
    let (path, index) = spec.split_once('#').map_or((spec, 0), |(p, i)| (p, i.parse().unwrap_or(0)));
    let path = match path.strip_prefix("%FONTS%") {
        Some(rest) => {
            let windir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
            windir.join("Fonts").join(rest.trim_start_matches(['/', '\\']))
        }
        None => PathBuf::from(path),
    };
    let data = std::fs::read(path).ok()?;
    // ponytail: fallback fonts are read fully and kept for the process
    // lifetime (tens of MB for CJK); memory-map them if that matters.
    Face::parse(Box::leak(data.into_boxed_slice()), index)
}

/// Han, kana, hangul, bopomofo and CJK punctuation / full-width forms.
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF | 0x2E80..=0x2FDF | 0x3000..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xA960..=0xA97F | 0xAC00..=0xD7FF | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFFEF
        | 0x20000..=0x3134F)
}

fn is_hangul(c: char) -> bool {
    matches!(c as u32, 0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7FF)
}

fn not_cjk(c: char) -> bool {
    !is_cjk(c)
}

/// Fallback fonts in preference order for this platform.
fn system_fallbacks() -> Vec<Fallback> {
    let fallback = |paths, covers| Fallback { paths, covers, face: OnceCell::new() };
    if cfg!(target_os = "windows") {
        vec![
            fallback(&["%FONTS%/msyh.ttc#0", "%FONTS%/msjh.ttc#0", "%FONTS%/simsun.ttc#0"], is_cjk),
            fallback(&["%FONTS%/YuGothM.ttc#0", "%FONTS%/meiryo.ttc#0", "%FONTS%/msgothic.ttc#0"], is_cjk),
            fallback(&["%FONTS%/malgun.ttf"], is_hangul),
            fallback(&["%FONTS%/segoeui.ttf"], not_cjk),
            fallback(&["%FONTS%/seguisym.ttf"], not_cjk),
            fallback(&["%FONTS%/Nirmala.ttc#0", "%FONTS%/Nirmala.ttf"], not_cjk),
            fallback(&["%FONTS%/arialuni.ttf", "%FONTS%/arial.ttf"], not_cjk),
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            fallback(
                &[
                    "/System/Library/Fonts/PingFang.ttc#0",
                    "/System/Library/Fonts/Hiragino Sans GB.ttc#0",
                    "/System/Library/Fonts/STHeiti Light.ttc#0",
                ],
                is_cjk,
            ),
            fallback(&["/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc#0"], is_cjk),
            fallback(&["/System/Library/Fonts/AppleSDGothicNeo.ttc#0"], is_hangul),
            fallback(
                &["/System/Library/Fonts/Supplemental/Arial Unicode.ttf", "/Library/Fonts/Arial Unicode.ttf"],
                |_| true,
            ),
            fallback(&["/System/Library/Fonts/Apple Symbols.ttf"], not_cjk),
        ]
    } else {
        vec![
            fallback(
                &[
                    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc#0",
                    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc#0",
                    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc#0",
                    "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
                    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc#0",
                    "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc#0",
                    "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc#0",
                    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
                ],
                is_cjk,
            ),
            fallback(
                &[
                    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
                    "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
                ],
                not_cjk,
            ),
            fallback(
                &["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", "/usr/share/fonts/TTF/DejaVuSans.ttf", "/usr/share/fonts/dejavu/DejaVuSans.ttf"],
                not_cjk,
            ),
        ]
    }
}

/// Characters that attach to the preceding emoji rather than standing alone:
/// variation selectors, ZWJ, skin tones, tag characters and the keycap mark.
pub fn is_emoji_joiner(c: char) -> bool {
    matches!(c as u32, 0xFE0E | 0xFE0F | 0x200D | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F | 0x20E3)
}

/// Zero-width format characters that never draw anything by themselves.
pub fn is_invisible(c: char) -> bool {
    matches!(c as u32, 0x200B..=0x200F | 0x2060..=0x2064 | 0xFE00..=0xFE0F | 0xFEFF | 0xE0000..=0xE007F)
}

/// Every face the app can draw with.
pub struct Fonts {
    embedded: [Face; FontId::EMBEDDED],
    fallbacks: Vec<Fallback>,
    /// Resolution results for characters the requested face lacks.
    resolved: RefCell<HashMap<(FontId, char), (FontId, u16)>>,
}

impl Fonts {
    /// Parses the embedded fonts; system fallbacks load on first use.
    ///
    /// # Panics
    /// Only if an embedded font file is corrupt, which is a build defect.
    #[must_use]
    pub fn load() -> Self {
        let embedded = |bytes: &'static [u8]| Face::parse(bytes, 0).expect("embedded font is a valid OpenType file");
        Self {
            embedded: [
                embedded(include_bytes!("../assets/Inter-Regular.ttf")),
                embedded(include_bytes!("../assets/Inter-SemiBold.ttf")),
                embedded(include_bytes!("../assets/Inter-Italic.ttf")),
                embedded(include_bytes!("../assets/JetBrainsMono-Regular.ttf")),
                embedded(include_bytes!("../assets/Twemoji.Mozilla.ttf")),
            ],
            fallbacks: system_fallbacks(),
            resolved: RefCell::default(),
        }
    }

    /// The face behind `id`. Ids only come from [`Fonts::resolve`], so a
    /// fallback id always refers to a loaded face; anything else maps to
    /// Inter Regular rather than panicking.
    pub fn face(&self, id: FontId) -> &Face {
        let index = usize::from(id.0);
        self.embedded.get(index).or_else(|| self.fallbacks.get(index - FontId::EMBEDDED)?.face.get()?.as_ref()).unwrap_or(&self.embedded[0])
    }

    /// Picks the face and glyph for `c`, preferring `primary`. `next` is the
    /// following character: a U+FE0F after it asks for emoji presentation.
    pub fn resolve(&self, primary: FontId, c: char, next: Option<char>) -> (FontId, u16) {
        let emoji_style = next == Some('\u{FE0F}');
        if !emoji_style && let Some(glyph) = self.face(primary).glyph(c) {
            return (primary, glyph);
        }
        let key = (primary, c);
        if !emoji_style && let Some(&hit) = self.resolved.borrow().get(&key) {
            return hit;
        }
        let found = self
            .face(FontId::EMOJI)
            .glyph(c)
            .map(|g| (FontId::EMOJI, g))
            .or_else(|| self.face(primary).glyph(c).map(|g| (primary, g)))
            // Italic and mono lack some symbols the regular face has.
            .or_else(|| self.face(FontId::REGULAR).glyph(c).map(|g| (FontId::REGULAR, g)))
            .or_else(|| {
                self.fallbacks.iter().enumerate().filter(|(_, f)| (f.covers)(c)).find_map(|(i, fallback)| {
                    let glyph = fallback.face()?.glyph(c)?;
                    Some((FontId((FontId::EMBEDDED + i) as u8), glyph))
                })
            })
            .unwrap_or((primary, 0));
        if !emoji_style {
            self.resolved.borrow_mut().insert(key, found);
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_faces_cover_what_the_ui_draws() {
        let fonts = Fonts::load();
        for id in [FontId::REGULAR, FontId::SEMIBOLD] {
            for c in ['✓', '↑', '…', '·', '→', '$', '×'] {
                assert!(fonts.face(id).glyph(c).is_some(), "{c:?} missing from {id:?}");
            }
        }
        assert!(fonts.face(FontId::MONO).glyph('{').is_some());
    }

    #[test]
    fn emoji_resolve_to_colour_glyphs() {
        let fonts = Fonts::load();
        let (font, glyph) = fonts.resolve(FontId::REGULAR, '😀', None);
        assert_eq!(font, FontId::EMOJI);
        assert!(fonts.face(font).layers(glyph).is_some_and(|l| l.len() > 1));
        // Text presentation stays in Inter unless U+FE0F asks otherwise.
        assert_eq!(fonts.resolve(FontId::REGULAR, '#', None).0, FontId::REGULAR);
        assert_eq!(fonts.resolve(FontId::REGULAR, '#', Some('\u{FE0F}')).0, FontId::EMOJI);
    }

    #[test]
    fn emoji_sequences_form_ligatures() {
        let emoji = &Fonts::load().embedded[usize::from(FontId::EMOJI.0)];
        for sequence in ["👨\u{200D}👩\u{200D}👧", "🇳🇱", "👍🏽"] {
            let glyphs: Vec<u16> = sequence.chars().filter_map(|c| emoji.glyph(c)).collect();
            let (_, len) = emoji.ligature(&glyphs).unwrap_or_else(|| panic!("no ligature for {sequence}"));
            assert_eq!(len, glyphs.len(), "{sequence} should become one glyph");
        }
    }

    #[test]
    fn cjk_uses_a_system_font_when_one_is_installed() {
        let fonts = Fonts::load();
        for c in ['汉', 'か', '한'] {
            let (font, glyph) = fonts.resolve(FontId::REGULAR, c, None);
            // Machines without CJK fonts get the missing-glyph box instead.
            if glyph != 0 {
                assert!(usize::from(font.0) >= FontId::EMBEDDED, "{c:?} should come from a fallback");
                let bitmap = fonts.face(font).rasterize(glyph, 16.0).expect("CJK glyphs have ink");
                assert!(bitmap.w > 4 && bitmap.h > 4);
            }
            eprintln!("{c:?} -> font {font:?}, glyph {glyph}");
        }
    }

    #[test]
    fn kerning_comes_from_gpos() {
        let face = &Fonts::load().embedded[0];
        let (a, v) = (face.glyph('A').unwrap(), face.glyph('V').unwrap());
        assert!(face.kerning(a, v) < 0.0, "Inter kerns AV");
        assert!(face.kerning(a, v) < 0.0, "cached value matches");
    }
}
