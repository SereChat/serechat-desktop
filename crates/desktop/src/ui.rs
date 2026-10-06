//! Per-frame input state, animation bookkeeping and shared widgets.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use arboard::Clipboard;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use accesskit::Role;
use winit::window::CursorIcon;

use crate::a11y::Node;
use crate::editor::Editor;
use crate::paint::{Color, Painter, Rect, fade, mix};
use crate::theme;

/// Input and bookkeeping shared by every widget during a frame.
#[derive(Default)]
pub struct Ui {
    /// Mouse position in logical pixels.
    pub mouse: (f32, f32),
    /// Where the left button last went down.
    pub press_pos: (f32, f32),
    /// Left button held.
    pub down: bool,
    /// Left button went down this frame.
    pub pressed: bool,
    /// Clicks in the current burst: 1 single, 2 double, 3 triple…
    pub clicks: u32,
    /// When the last press happened (for click bursts).
    last_press: f32,
    /// Left button went up this frame.
    pub released: bool,
    /// Accumulated wheel movement in logical pixels; positive scrolls down.
    pub scroll: f32,
    /// Accumulated sideways wheel movement; positive scrolls right.
    pub scroll_x: f32,
    /// Keyboard modifiers.
    pub mods: ModifiersState,
    /// Seconds since the previous frame.
    pub dt: f32,
    /// Seconds since startup.
    pub time: f32,
    /// Moment of the last text edit, used to restart the caret blink.
    pub last_edit: f32,
    /// Whether the window has keyboard focus.
    pub focused: bool,
    /// Cursor requested by whatever the mouse is over.
    pub cursor: CursorIcon,
    /// An overlay covering this area swallows hover for widgets beneath it.
    pub blocker: Option<Rect>,
    /// Set when something is mid-animation and needs another frame.
    pub animating: bool,
    /// Widgets drawn this frame, while a screen reader listens.
    pub access: Option<Vec<Node>>,
    /// The real mouse position while a screen reader's click is replayed.
    replayed: Option<(f32, f32)>,
    /// Animated values with the frame they were last used in.
    anims: HashMap<u64, (f32, u64)>,
    frame: u64,
}

impl Ui {
    /// Prepares for a new frame.
    pub fn begin(&mut self, dt: f32, time: f32) {
        self.dt = dt;
        self.time = time;
        self.cursor = CursorIcon::Default;
        self.animating = false;
        // Forget widgets that were not drawn last frame so the map stays small.
        let frame = self.frame;
        self.anims.retain(|_, (_, used)| *used == frame);
        self.frame += 1;
    }

    /// Records a left-button press at the current mouse position, counting
    /// quick presses in the same spot as one burst (double/triple click).
    pub fn press(&mut self, now: f32) {
        let near = (self.mouse.0 - self.press_pos.0).abs() < 4.0 && (self.mouse.1 - self.press_pos.1).abs() < 4.0;
        self.clicks = if near && now - self.last_press < 0.4 { self.clicks + 1 } else { 1 };
        self.last_press = now;
        self.down = true;
        self.pressed = true;
        self.press_pos = self.mouse;
    }

    /// Clears one-shot input once a frame has consumed it.
    pub fn end(&mut self) {
        self.pressed = false;
        self.released = false;
        self.scroll = 0.0;
        self.scroll_x = 0.0;
        if let Some(mouse) = self.replayed.take() {
            self.mouse = mouse;
        }
    }

    /// Plays a screen reader's click at `at` (logical pixels) in the next
    /// frame: a press and release there, as if the mouse had done it.
    pub fn replay_click(&mut self, at: (f32, f32)) {
        self.replayed.get_or_insert(self.mouse);
        self.mouse = at;
        self.press_pos = at;
        self.pressed = true;
        self.released = true;
        self.down = false;
        self.clicks = 1;
    }

    /// Describes a widget to screen readers. `node` runs only while one
    /// listens, so describing costs nothing otherwise.
    pub fn describe(&mut self, node: impl FnOnce() -> Node) {
        if let Some(nodes) = &mut self.access {
            nodes.push(node());
        }
    }

    /// Whether the mouse is over `rect` and not over an overlay.
    #[must_use]
    pub fn hovered(&self, rect: Rect) -> bool {
        rect.contains(self.mouse) && !self.blocker.is_some_and(|b| b.contains(self.mouse))
    }

    /// Whether `rect` was clicked (pressed and released inside) this frame.
    #[must_use]
    pub fn clicked(&self, rect: Rect) -> bool {
        self.released && self.hovered(rect) && rect.contains(self.press_pos)
    }

    /// Eases the value stored under `id` towards `target` and returns it.
    pub fn anim(&mut self, id: u64, target: f32) -> f32 {
        let (value, used) = self.anims.entry(id).or_insert((target, self.frame));
        *used = self.frame;
        let t = 1.0 - (-self.dt * 16.0).exp();
        *value += (target - *value) * t;
        if (target - *value).abs() < 0.002 {
            *value = target;
        } else {
            self.animating = true;
        }
        *value
    }

    /// Whether the caret should be drawn right now.
    #[must_use]
    pub fn caret_visible(&self) -> bool {
        self.focused && (self.time - self.last_edit).rem_euclid(1.0) < 0.55
    }

    /// Seconds until the caret blink state next flips.
    #[must_use]
    pub fn next_blink(&self) -> f32 {
        let phase = (self.time - self.last_edit).rem_euclid(1.0);
        if phase < 0.55 { 0.55 - phase } else { 1.0 - phase }
    }
}

/// Stable widget identity from any hashable key.
#[must_use]
pub fn id(key: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

/// Visual weight of a button.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    /// Accent-filled call to action.
    Primary,
    /// Outlined neutral.
    Secondary,
    /// Text only until hovered.
    Ghost,
    /// Outlined in the danger colour, for destructive actions.
    Danger,
}

/// Draws a button and returns whether it was clicked.
pub fn button(p: &mut Painter, ui: &mut Ui, rect: Rect, label: &str, style: ButtonStyle, enabled: bool) -> bool {
    let t = p.theme;
    let hovered = enabled && ui.hovered(rect);
    let hover = ui.anim(id((label, "button", rect.x.to_bits(), rect.y.to_bits())), f32::from(u8::from(hovered)));
    let radius = theme::RADIUS_SM;
    let text_color = match style {
        _ if !enabled => t.text_faint,
        ButtonStyle::Primary => t.on_accent,
        ButtonStyle::Secondary => t.text,
        ButtonStyle::Ghost => mix(t.text_muted, t.text, hover),
        ButtonStyle::Danger => t.danger,
    };
    match style {
        ButtonStyle::Primary if enabled => p.rect(rect, fade(t.accent, 1.0 - 0.14 * hover), radius),
        ButtonStyle::Primary => p.rect(rect, t.hover, radius),
        ButtonStyle::Secondary => p.bordered(rect, mix(t.surface, t.hover, hover), radius, 1.0, t.border_strong),
        ButtonStyle::Ghost => p.rect(rect, fade(t.hover, hover), radius),
        ButtonStyle::Danger => p.bordered(rect, fade(t.danger, 0.1 * hover), radius, 1.0, fade(t.danger, 0.45 + 0.3 * hover)),
    }
    p.label_centered(label, theme::LABEL, rect, text_color);
    if hovered {
        ui.cursor = CursorIcon::Pointer;
    }
    ui.describe(|| Node::new(Role::Button, rect, label));
    enabled && ui.clicked(rect)
}

/// Draws the brand mark: a flat accent square with an "S".
pub fn logo(p: &mut Painter, rect: Rect) {
    p.rect(rect, p.theme.accent, rect.w * 0.22);
    p.label_centered("S", crate::text::Style::semibold(rect.w * 0.56), rect, p.theme.on_accent);
}

/// A small chevron built from pixel-snapped squares (no icon font needed),
/// pointing down when `open`, right otherwise. `(x, y)` is its top-left.
pub fn chevron(p: &mut Painter, x: f32, y: f32, open: bool, color: Color) {
    for i in 0..4 {
        let d = i as f32;
        if open {
            p.rect(Rect::new(x + d, y + d, 1.5, 1.5), color, 0.0);
            p.rect(Rect::new(x + 6.0 - d, y + d, 1.5, 1.5), color, 0.0);
        } else {
            p.rect(Rect::new(x + d, y + d, 1.5, 1.5), color, 0.0);
            p.rect(Rect::new(x + d, y + 6.0 - d, 1.5, 1.5), color, 0.0);
        }
    }
}

/// A 12×10 folder glyph with its top-left at `(x, y)`.
pub fn folder_icon(p: &mut Painter, x: f32, y: f32, color: Color) {
    p.rect(Rect::new(x, y, 5.0, 3.0), color, 1.0);
    p.rect(Rect::new(x, y + 2.0, 12.0, 8.0), color, 1.5);
}

/// Draws a keyboard shortcut in a key cap whose right edge is at `right`.
/// Returns the cap's width.
pub fn keycap(p: &mut Painter, keys: &str, right: f32, y: f32) -> f32 {
    let t = p.theme;
    let text = p.layout(keys, theme::TINY, None);
    let cap = Rect::new(right - text.width() - 12.0, y, text.width() + 12.0, 20.0);
    p.bordered(cap, t.surface, theme::RADIUS_SM, 1.0, t.border_strong);
    p.text(&text, cap.x + 6.0, cap.y + (cap.h - text.height()) * 0.5, t.text_muted);
    cap.w
}

/// An on/off switch in `rect` (about 32×18), named `label` for screen
/// readers. Returns whether it was clicked.
pub fn switch(p: &mut Painter, ui: &mut Ui, rect: Rect, on: bool, key: u64, label: &str) -> bool {
    let t = p.theme;
    let hovered = ui.hovered(rect);
    let at = ui.anim(key, f32::from(u8::from(on)));
    let track = mix(t.border_strong, t.accent, at);
    p.rect(rect, if hovered { mix(track, t.text, 0.08) } else { track }, rect.h * 0.5);
    let knob = rect.h - 4.0;
    let x = rect.x + 2.0 + (rect.w - knob - 4.0) * at;
    p.rect(Rect::new(x, rect.y + 2.0, knob, knob), mix(t.surface, t.on_accent, at), knob * 0.5);
    if hovered {
        ui.cursor = CursorIcon::Pointer;
    }
    ui.describe(|| Node::switch(rect, label, on));
    ui.clicked(rect)
}

/// A text input's editing state, plus how far it is scrolled.
#[derive(Default)]
pub struct TextField {
    /// The text and caret.
    pub editor: Editor,
    scroll: f32,
    selecting: bool,
}

/// How a [`text_field`] looks.
pub struct FieldStyle<'a> {
    /// Shown while empty.
    pub placeholder: &'a str,
    /// Wraps and grows to several lines (Enter is the caller's to handle).
    pub multiline: bool,
    /// Uses the monospace font (commands, URLs, keys).
    pub mono: bool,
}

/// Draws `field` in `rect`: a click places the caret (and asks for focus),
/// a drag selects. Returns `(pressed, caret)`: whether it was pressed this
/// frame, and where the caret is when `focused` (for input methods).
pub fn text_field(p: &mut Painter, ui: &mut Ui, rect: Rect, field: &mut TextField, focused: bool, style: &FieldStyle<'_>) -> (bool, Option<Rect>) {
    let t = p.theme;
    let focus = ui.anim(id(("field-focus", rect.x.to_bits(), rect.y.to_bits())), f32::from(u8::from(focused && ui.focused)));
    p.bordered(rect, t.bg, theme::RADIUS_SM, 1.0, mix(t.border_strong, t.border_focus, focus));
    let text_style = if style.mono { crate::text::Style::mono(12.5) } else { theme::SMALL };
    let pad = 10.0;
    let inner = Rect::new(rect.x + pad, rect.y + 6.0, rect.w - 2.0 * pad, rect.h - 12.0);
    let wrap = style.multiline.then_some(inner.w);
    let layout = p.layout(field.editor.text(), text_style, wrap);
    let line_h = layout.line_height();
    let (caret_x, caret_y) = layout.caret(field.editor.cursor());
    // Keep the caret in view: sideways for one line, down for several.
    if style.multiline {
        field.scroll = field.scroll.clamp(caret_y + line_h - inner.h, caret_y).clamp(0.0, (layout.height() - inner.h).max(0.0));
    } else {
        field.scroll = field.scroll.clamp(caret_x + 2.0 - inner.w, caret_x).clamp(0.0, (layout.width() + 2.0 - inner.w).max(0.0));
    }
    let origin = if style.multiline {
        (inner.x, inner.y - field.scroll)
    } else {
        (inner.x - field.scroll, rect.y + (rect.h - line_h) * 0.5)
    };

    let hovered = ui.hovered(rect);
    let hit = |ui: &Ui| layout.hit(ui.mouse.0 - origin.0, ui.mouse.1 - origin.1);
    let mut pressed = false;
    if hovered {
        ui.cursor = CursorIcon::Text;
        if ui.pressed {
            pressed = true;
            field.editor.set_cursor(hit(ui), ui.mods.shift_key() && focused);
            if ui.clicks == 2 {
                field.editor.select_word();
            } else if ui.clicks >= 3 {
                field.editor.select_all();
            }
            field.selecting = true;
            ui.last_edit = ui.time;
        }
    }
    if field.selecting {
        if ui.down && ui.clicks < 2 {
            field.editor.set_cursor(hit(ui), true);
        } else if !ui.down {
            field.selecting = false;
        }
    }

    let clip = p.push_clip(Rect::new(inner.x - 1.0, rect.y + 1.0, inner.w + 2.0, rect.h - 2.0));
    let selection = field.editor.selection();
    if focused && !selection.is_empty() {
        for (start, end, y) in layout.line_spans() {
            let (from, to) = (selection.start.max(start), selection.end.min(end));
            if from > to || (from == to && selection.end <= end) {
                continue;
            }
            let x0 = layout.caret(from).0;
            let x1 = if selection.end > end { layout.caret(to).0 + 6.0 } else { layout.caret(to).0 };
            p.rect(Rect::new(origin.0 + x0, origin.1 + y, x1 - x0, line_h), t.selection, 2.0);
        }
    }
    if field.editor.text().is_empty() {
        p.label(style.placeholder, text_style, origin.0, origin.1, t.text_faint);
    } else {
        p.text(&layout, origin.0, origin.1, t.text);
    }
    let caret = Rect::new(origin.0 + caret_x - 1.0, origin.1 + caret_y + 2.0, 2.0, line_h - 4.0);
    if focused && ui.caret_visible() && selection.is_empty() {
        p.rect(caret, t.accent, 0.0);
    }
    p.set_clip(clip);
    ui.describe(|| Node::input(rect, style.placeholder, field.editor.text(), style.multiline, focused));
    (pressed, focused.then_some(caret))
}

/// Opens the clipboard lazily; `None` when the platform has none.
fn clipboard(slot: &mut Option<Clipboard>) -> Option<&mut Clipboard> {
    if slot.is_none() {
        *slot = Clipboard::new().ok();
    }
    slot.as_mut()
}

/// The text on the system clipboard, if there is any.
pub fn paste(slot: &mut Option<Clipboard>) -> Option<String> {
    clipboard(slot).and_then(|cb| cb.get_text().ok())
}

/// Copies `text` to the system clipboard, ignoring failures.
pub fn copy(slot: &mut Option<Clipboard>, text: &str) {
    if let Some(cb) = clipboard(slot) {
        let _ = cb.set_text(text);
    }
}

/// Applies standard text-editing keys to `editor`. Returns `false` when the
/// key is not an editing key, so the caller may handle it.
///
/// Enter is never handled here; callers decide between submit and newline.
pub fn edit_key(editor: &mut Editor, event: &KeyEvent, mods: ModifiersState, cb: &mut Option<Clipboard>) -> bool {
    let mac = cfg!(target_os = "macos");
    let primary = if mac { mods.super_key() } else { mods.control_key() };
    let word = if mac { mods.alt_key() } else { mods.control_key() };
    let shift = mods.shift_key();
    match &event.logical_key {
        Key::Named(NamedKey::Enter) => return false,
        Key::Named(NamedKey::Backspace) => editor.backspace(word),
        Key::Named(NamedKey::Delete) => editor.delete(word),
        Key::Named(NamedKey::ArrowLeft) if mac && primary => editor.home(shift),
        Key::Named(NamedKey::ArrowRight) if mac && primary => editor.end(shift),
        Key::Named(NamedKey::ArrowLeft) => editor.left(word, shift),
        Key::Named(NamedKey::ArrowRight) => editor.right(word, shift),
        Key::Named(NamedKey::Home) => editor.home(shift),
        Key::Named(NamedKey::End) => editor.end(shift),
        Key::Character(c) if primary && c.eq_ignore_ascii_case("a") => editor.select_all(),
        Key::Character(c) if primary && c.eq_ignore_ascii_case("c") => copy(cb, editor.selected_text()),
        Key::Character(c) if primary && c.eq_ignore_ascii_case("x") => {
            copy(cb, editor.selected_text());
            editor.insert("");
        }
        Key::Character(c) if primary && c.eq_ignore_ascii_case("v") => {
            if let Some(text) = clipboard(cb).and_then(|cb| cb.get_text().ok()) {
                editor.insert(&text);
            }
        }
        _ => match &event.text {
            // Ctrl+Alt is AltGr on Windows/Linux and produces real characters.
            Some(text) if !primary || mods.alt_key() => editor.insert(text),
            _ => return false,
        },
    }
    true
}
