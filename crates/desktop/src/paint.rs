//! Immediate-mode drawing API on top of [`Instance`]s.

use crate::gpu::Instance;
use crate::text::{Align, Fonts, GlyphAtlas, Style, TextLayout};
use crate::theme::Palette;

/// Straight-alpha sRGB colour.
pub type Color = [f32; 4];

/// Opaque colour from `0xRRGGBB`.
#[must_use]
pub const fn hex(rgb: u32) -> Color {
    hexa(rgb, 1.0)
}

/// Colour from `0xRRGGBB` with alpha.
#[must_use]
pub const fn hexa(rgb: u32, alpha: f32) -> Color {
    [
        ((rgb >> 16) & 0xff) as f32 / 255.0,
        ((rgb >> 8) & 0xff) as f32 / 255.0,
        (rgb & 0xff) as f32 / 255.0,
        alpha,
    ]
}

/// `color` with its alpha multiplied by `factor`.
#[must_use]
pub fn fade(color: Color, factor: f32) -> Color {
    [color[0], color[1], color[2], color[3] * factor]
}

/// Linear interpolation between two colours.
#[must_use]
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

/// Axis-aligned rectangle in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// Creates a rectangle.
    #[must_use]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Right edge.
    #[must_use]
    pub fn right(self) -> f32 {
        self.x + self.w
    }

    /// Bottom edge.
    #[must_use]
    pub fn bottom(self) -> f32 {
        self.y + self.h
    }

    /// Whether the point lies inside.
    #[must_use]
    pub fn contains(self, (x, y): (f32, f32)) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// Overlapping area of two rectangles (empty if disjoint).
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        Self::new(x, y, (self.right().min(other.right()) - x).max(0.0), (self.bottom().min(other.bottom()) - y).max(0.0))
    }
}

/// Primitive kinds understood by the shader.
const KIND_SHAPE: f32 = 0.0;
const KIND_GLYPH: f32 = 1.0;

/// Records draw calls for one frame.
pub struct Painter<'a> {
    /// Fonts used for layout.
    pub fonts: &'a Fonts,
    atlas: &'a mut GlyphAtlas,
    out: &'a mut Vec<Instance>,
    /// Physical pixels per logical pixel.
    pub scale: f32,
    clip: Rect,
    /// Colours of the active scheme.
    pub theme: Palette,
    /// Set when the glyph atlas ran out of space mid-frame.
    pub atlas_full: bool,
}

impl<'a> Painter<'a> {
    /// Starts a frame covering `viewport`.
    pub fn new(fonts: &'a Fonts, atlas: &'a mut GlyphAtlas, out: &'a mut Vec<Instance>, scale: f32, viewport: Rect, theme: &Palette) -> Self {
        out.clear();
        Self { fonts, atlas, out, scale, clip: viewport, theme: *theme, atlas_full: false }
    }

    /// Restricts subsequent drawing to `rect` (intersected with the current
    /// clip) and returns the previous clip for [`Painter::set_clip`].
    pub fn push_clip(&mut self, rect: Rect) -> Rect {
        let clip = self.clip.intersect(rect);
        std::mem::replace(&mut self.clip, clip)
    }

    /// The current clip rectangle.
    #[must_use]
    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// Restores a clip returned by [`Painter::push_clip`].
    pub fn set_clip(&mut self, clip: Rect) {
        self.clip = clip;
    }

    fn push(&mut self, rect: Rect, color: Color, color2: Color, params: [f32; 4], uv: [f32; 4]) {
        let c = self.clip;
        // Cheap CPU cull; the shader clips exactly. Pad for blur falloff.
        let pad = params[2] + 1.0;
        if rect.right() + pad < c.x || rect.x - pad > c.right() || rect.bottom() + pad < c.y || rect.y - pad > c.bottom() {
            return;
        }
        self.out.push(Instance {
            rect: [rect.x, rect.y, rect.w, rect.h],
            clip: [c.x, c.y, c.right(), c.bottom()],
            color,
            color2,
            uv,
            params,
        });
    }

    /// Filled rounded rectangle.
    pub fn rect(&mut self, rect: Rect, color: Color, radius: f32) {
        self.push(rect, color, color, [radius, 0.0, 0.0, KIND_SHAPE], [0.0; 4]);
    }

    /// Rounded rectangle with an inner border.
    pub fn bordered(&mut self, rect: Rect, fill: Color, radius: f32, border: f32, border_color: Color) {
        self.push(rect, fill, border_color, [radius, border, 0.0, KIND_SHAPE], [0.0; 4]);
    }

    /// Soft shadow or glow: `rect` blurred by `blur` logical pixels.
    pub fn shadow(&mut self, rect: Rect, color: Color, radius: f32, blur: f32) {
        self.push(rect, color, color, [radius, 0.0, blur.max(0.01), KIND_SHAPE], [0.0; 4]);
    }

    /// Lays out text with this frame's scale.
    #[must_use]
    pub fn layout(&self, text: &str, style: Style, max_width: Option<f32>) -> TextLayout {
        TextLayout::new(self.fonts, text, style, max_width, self.scale)
    }

    /// Draws a laid-out block with its top-left at `(x, y)`.
    pub fn text(&mut self, layout: &TextLayout, x: f32, y: f32, color: Color) {
        self.text_aligned(layout, x, y, Align::Left, 0.0, color);
    }

    /// Draws a laid-out block, aligning each line inside `width`.
    pub fn text_aligned(&mut self, layout: &TextLayout, x: f32, y: f32, align: Align, width: f32, color: Color) {
        self.glyphs(layout, (x, y), (align, width), &[color]);
    }

    /// Draws a rich layout: run decorations first (code backgrounds,
    /// underlines, strike-throughs), then glyphs. A run's `ink` indexes
    /// `inks`; out-of-range slots use `inks[0]`. `code_bg` fills
    /// [`BACKGROUND`](crate::text::BACKGROUND) runs.
    pub fn rich(&mut self, layout: &TextLayout, x: f32, y: f32, inks: &[Color], code_bg: Color) {
        use crate::text::{BACKGROUND, STRIKE, UNDERLINE};
        for d in layout.decorations() {
            let ink = inks.get(usize::from(d.run.ink)).or(inks.first()).copied().unwrap_or([1.0; 4]);
            if d.run.decoration & BACKGROUND != 0 {
                let pad = (d.h * 0.12).round();
                self.rect(Rect::new(x + d.x - 3.0, y + d.y + pad, d.w + 6.0, d.h - 2.0 * pad), code_bg, 4.0);
            }
            if d.run.decoration & UNDERLINE != 0 {
                self.rect(Rect::new(x + d.x, y + d.y + d.baseline + 2.0, d.w, 1.0), fade(ink, 0.5), 0.0);
            }
            if d.run.decoration & STRIKE != 0 {
                self.rect(Rect::new(x + d.x, y + d.y + d.baseline * 0.68, d.w, 1.0), ink, 0.0);
            }
        }
        self.glyphs(layout, (x, y), (Align::Left, 0.0), inks);
    }

    fn glyphs(&mut self, layout: &TextLayout, origin: (f32, f32), align: (Align, f32), inks: &[Color]) {
        let clip = self.clip;
        let out = &mut *self.out;
        let fallback = inks.first().copied().unwrap_or([1.0; 4]);
        let ok = layout.place(self.fonts, self.atlas, origin, align, (clip.y, clip.bottom()), |g| {
            let ink = inks.get(usize::from(g.ink)).copied().unwrap_or(fallback);
            // Emoji layers keep their own colours but follow the text's fade.
            let color = g.color.map_or(ink, |c| [c[0], c[1], c[2], c[3] * ink[3]]);
            out.push(Instance {
                rect: g.rect,
                clip: [clip.x, clip.y, clip.right(), clip.bottom()],
                color,
                color2: color,
                uv: g.uv,
                params: [0.0, 0.0, 0.0, KIND_GLYPH],
            });
        });
        self.atlas_full |= !ok;
    }

    /// Lays out and draws a single line; returns its width.
    pub fn label(&mut self, text: &str, style: Style, x: f32, y: f32, color: Color) -> f32 {
        let layout = self.layout(text, style, None);
        self.text(&layout, x, y, color);
        layout.width()
    }

    /// Draws a single line centred in `rect`.
    pub fn label_centered(&mut self, text: &str, style: Style, rect: Rect, color: Color) {
        let layout = self.layout(text, style, None);
        let x = rect.x + (rect.w - layout.width()) * 0.5;
        let y = rect.y + (rect.h - layout.height()) * 0.5;
        self.text(&layout, x, y, color);
    }
}
