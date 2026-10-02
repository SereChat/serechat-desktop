//! Visual design tokens. Every colour and size in the UI comes from here.
//!
//! Colours live in a [`Palette`] so the scheme can change at runtime; the
//! painter carries the active one (`p.theme`). Sizes and text styles are the
//! same in every scheme. The look is Zed-inspired: flat surfaces, hairline
//! borders, small radii and a single accent.

use crate::paint::{Color, hex, hexa, mix};
use crate::text::Style;

/// Every colour the UI uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// Chat area background.
    pub bg: Color,
    /// Sidebar and other panels.
    pub panel: Color,
    /// Raised surfaces: composer, cards, menus.
    pub surface: Color,
    /// Hovered element.
    pub hover: Color,
    /// Selected or pressed element.
    pub active: Color,
    /// Hairline dividers.
    pub border: Color,
    /// Borders of controls and floating surfaces.
    pub border_strong: Color,
    /// Border of the focused input.
    pub border_focus: Color,
    /// Primary text.
    pub text: Color,
    /// Secondary text.
    pub text_muted: Color,
    /// Hints, placeholders and captions.
    pub text_faint: Color,
    /// The single accent colour.
    pub accent: Color,
    /// Text and icons drawn on the accent colour.
    pub on_accent: Color,
    /// Text selection highlight.
    pub selection: Color,
    /// Errors and destructive actions.
    pub danger: Color,
    /// Lines a change adds (diffs).
    pub added: Color,
    /// Lines a change removes (diffs).
    pub removed: Color,
    /// Drop shadows under floating surfaces.
    pub shadow: Color,
    /// Background of code blocks.
    pub code_bg: Color,
    /// Links in replies.
    pub link: Color,
    /// Syntax colours: keyword, string, number, comment, function, type.
    pub syntax: [Color; 6],
}

impl Palette {
    /// Blends every colour towards `other` by `t` (for theme transitions).
    #[must_use]
    pub fn mix(&self, other: &Self, t: f32) -> Self {
        let m = |a, b| mix(a, b, t);
        Self {
            bg: m(self.bg, other.bg),
            panel: m(self.panel, other.panel),
            surface: m(self.surface, other.surface),
            hover: m(self.hover, other.hover),
            active: m(self.active, other.active),
            border: m(self.border, other.border),
            border_strong: m(self.border_strong, other.border_strong),
            border_focus: m(self.border_focus, other.border_focus),
            text: m(self.text, other.text),
            text_muted: m(self.text_muted, other.text_muted),
            text_faint: m(self.text_faint, other.text_faint),
            accent: m(self.accent, other.accent),
            on_accent: m(self.on_accent, other.on_accent),
            selection: m(self.selection, other.selection),
            danger: m(self.danger, other.danger),
            added: m(self.added, other.added),
            removed: m(self.removed, other.removed),
            shadow: m(self.shadow, other.shadow),
            code_bg: m(self.code_bg, other.code_bg),
            link: m(self.link, other.link),
            syntax: std::array::from_fn(|i| m(self.syntax[i], other.syntax[i])),
        }
    }
}

/// Neutral graphite with a white accent. The default.
pub const DARK: Palette = Palette {
    bg: hex(0x161618),
    panel: hex(0x1C1C1F),
    surface: hex(0x212124),
    hover: hex(0x2A2A2E),
    active: hex(0x333338),
    border: hex(0x2A2A2E),
    border_strong: hex(0x3A3A40),
    border_focus: hex(0x5A5A63),
    text: hex(0xEDEDEF),
    text_muted: hex(0xA0A0AB),
    text_faint: hex(0x6B6B76),
    accent: hex(0xEDEDEF),
    on_accent: hex(0x161618),
    selection: hexa(0xFFFFFF, 0.16),
    danger: hex(0xEB6F6F),
    added: hex(0x3FB950),
    removed: hex(0xF85149),
    shadow: hexa(0x000000, 0.5),
    code_bg: hex(0x1B1B1E),
    link: hex(0x8AB4F8),
    // GitHub Dark.
    syntax: [hex(0xFF7B72), hex(0xA5D6FF), hex(0x79C0FF), hex(0x8B949E), hex(0xD2A8FF), hex(0xFFA657)],
};

/// Zed's "One Dark": slate greys with a blue accent.
pub const BLUE: Palette = Palette {
    bg: hex(0x282C33),
    panel: hex(0x2F343E),
    surface: hex(0x2F343E),
    hover: hex(0x363C46),
    active: hex(0x454A56),
    border: hex(0x363C46),
    border_strong: hex(0x464B57),
    border_focus: hex(0x47679E),
    text: hex(0xDCE0E5),
    text_muted: hex(0xA9AFBC),
    text_faint: hex(0x6F7581),
    accent: hex(0x74ADE8),
    on_accent: hex(0x1B1F25),
    selection: hexa(0x74ADE8, 0.24),
    danger: hex(0xD07277),
    added: hex(0xA1C181),
    removed: hex(0xD07277),
    shadow: hexa(0x000000, 0.35),
    code_bg: hex(0x23272E),
    link: hex(0x74ADE8),
    // Zed One Dark.
    syntax: [hex(0xB477CF), hex(0xA1C181), hex(0xBF956A), hex(0x5D636F), hex(0x73ADE9), hex(0x6EB4BF)],
};

/// Paper white with an ink accent.
pub const LIGHT: Palette = Palette {
    bg: hex(0xFFFFFF),
    panel: hex(0xF7F7F8),
    surface: hex(0xFFFFFF),
    hover: hex(0xEDEDF0),
    active: hex(0xE3E3E8),
    border: hex(0xE8E8EC),
    border_strong: hex(0xD6D6DC),
    border_focus: hex(0xA0A0AB),
    text: hex(0x18181B),
    text_muted: hex(0x5B5B66),
    text_faint: hex(0x9A9AA5),
    accent: hex(0x18181B),
    on_accent: hex(0xFFFFFF),
    selection: hexa(0x2563EB, 0.18),
    danger: hex(0xD93636),
    added: hex(0x1A7F37),
    removed: hex(0xCF222E),
    shadow: hexa(0x000000, 0.1),
    code_bg: hex(0xF6F6F8),
    link: hex(0x2563EB),
    // GitHub Light.
    syntax: [hex(0xCF222E), hex(0x0A3069), hex(0x0550AE), hex(0x6E7781), hex(0x8250DF), hex(0x953800)],
};

/// The colour schemes offered in settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scheme {
    /// [`DARK`].
    #[default]
    Dark,
    /// [`BLUE`].
    Blue,
    /// [`LIGHT`].
    Light,
}

impl Scheme {
    /// Every scheme, in settings order.
    pub const ALL: [Self; 3] = [Self::Dark, Self::Blue, Self::Light];

    /// Its colours.
    #[must_use]
    pub fn palette(self) -> &'static Palette {
        match self {
            Self::Dark => &DARK,
            Self::Blue => &BLUE,
            Self::Light => &LIGHT,
        }
    }

    /// Name shown in settings.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Blue => "One Dark",
            Self::Light => "Light",
        }
    }

    /// Value stored in the config file.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Blue => "blue",
            Self::Light => "light",
        }
    }

    /// Parses a config value; unknown values fall back to the default.
    #[must_use]
    pub fn from_key(key: Option<&str>) -> Self {
        Self::ALL.into_iter().find(|s| Some(s.key()) == key).unwrap_or_default()
    }

    /// Whether the OS chrome (title bar) should be light.
    #[must_use]
    pub fn is_light(self) -> bool {
        self == Self::Light
    }
}

/// Corner radius of small elements: list rows, buttons.
pub const RADIUS_SM: f32 = 4.0;
/// Corner radius of containers: composer, menus, cards.
pub const RADIUS: f32 = 6.0;
/// Sidebar width.
pub const SIDEBAR_WIDTH: f32 = 256.0;
/// Height of the header bar above the main area.
pub const HEADER_HEIGHT: f32 = 44.0;
/// Maximum width of the message column.
pub const COLUMN_WIDTH: f32 = 760.0;

/// Message body text.
pub const BODY: Style = Style::regular(14.5);
/// Secondary UI text.
pub const SMALL: Style = Style::regular(13.0);
/// Captions and hints.
pub const TINY: Style = Style::regular(11.5);
/// Buttons and list items.
pub const LABEL: Style = Style::semibold(13.0);
/// Small headings and captions that need weight.
pub const CAPTION: Style = Style::semibold(11.5);
/// Page and section titles.
pub const TITLE: Style = Style::semibold(22.0);

#[cfg(test)]
mod tests {
    use super::Scheme;

    #[test]
    fn scheme_keys_round_trip() {
        for scheme in Scheme::ALL {
            assert_eq!(Scheme::from_key(Some(scheme.key())), scheme);
        }
        assert_eq!(Scheme::from_key(Some("neon")), Scheme::Dark);
        assert_eq!(Scheme::from_key(None), Scheme::Dark);
    }
}
