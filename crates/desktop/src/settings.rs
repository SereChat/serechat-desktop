//! Settings page: appearance, chat, usage, data and account.

use crate::app::Action;
use crate::chat::{ReasoningView, format_cost, group_digits};
use crate::skills::Catalog;
use crate::paint::{Painter, Rect, fade, mix};
use crate::text::{Align, Style};
use crate::theme::{self, Palette, Scheme};
use crate::ui::{ButtonStyle, Ui, button, id, keycap};

/// Widest the settings column gets.
const CONTENT_WIDTH: f32 = 680.0;
/// Space between sections.
const SECTION_GAP: f32 = 40.0;
/// Height of a row inside a settings group.
const ROW_H: f32 = 68.0;

/// Usage summed over every saved session.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    /// USD spent.
    pub cost: f64,
    /// Tokens billed.
    pub tokens: u64,
    /// Sessions with at least one message.
    pub sessions: usize,
}

/// The settings tabs, in header order.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Appearance,
    Skills,
    Usage,
    Account,
}

impl Tab {
    const ALL: [Self; 4] = [Self::Appearance, Self::Skills, Self::Usage, Self::Account];

    fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Skills => "Skills",
            Self::Usage => "Usage",
            Self::Account => "Account",
        }
    }
}

/// Open tab and scroll state of the settings page, and the skills it lists.
#[derive(Default)]
pub struct SettingsView {
    tab: Tab,
    scroll: f32,
    /// Content height measured last frame, for clamping the scroll.
    content_h: f32,
    /// The skills found; `None` while looking.
    skills: Option<Catalog>,
}

impl SettingsView {
    /// Lists the skills the open chat can use (`None` while looking).
    pub fn set_skills(&mut self, report: Option<Catalog>) {
        self.skills = report;
    }

    /// Draws the page into `area` (the whole main area, header included).
    #[allow(clippy::too_many_arguments, reason = "one argument per setting shown; a struct would only rename them")]
    pub fn draw(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        area: Rect,
        scheme: Scheme,
        reasoning: ReasoningView,
        totals: Totals,
        actions: &mut Vec<Action>,
    ) {
        let t = p.theme;
        let bar = Rect::new(area.x, area.y, area.w, theme::HEADER_HEIGHT);
        p.rect(Rect::new(bar.x, bar.bottom() - 1.0, bar.w, 1.0), t.border, 0.0);
        let title = p.layout("Settings", theme::LABEL, None);
        p.text(&title, bar.x + 16.0, bar.y + (bar.h - title.height()) * 0.5, t.text);
        let close = p.layout("to close", theme::TINY, None);
        let close_x = bar.right() - 16.0 - close.width();
        p.text(&close, close_x, bar.y + (bar.h - close.height()) * 0.5, t.text_faint);
        keycap(p, "Esc", close_x - 6.0, bar.y + (bar.h - 20.0) * 0.5);
        if let Some(tab) = tabs(p, ui, bar.x + 16.0 + title.width() + 20.0, bar, self.tab) {
            self.tab = tab;
            self.scroll = 0.0;
        }

        let body = Rect::new(area.x, bar.bottom(), area.w, area.h - bar.h);
        if ui.hovered(body) {
            self.scroll += ui.scroll;
        }
        self.scroll = self.scroll.clamp(0.0, (self.content_h - body.h).max(0.0));
        let width = CONTENT_WIDTH.min(body.w - 64.0);
        let x = body.x + ((body.w - width) * 0.5).round();

        // Content scrolled under the header must not react to the mouse.
        ui.blocker = Some(bar);
        let clip = p.push_clip(body);
        let top = body.y + 32.0 - self.scroll.round();
        let mut y = top;
        // Theme cards and usage stats share a three-column grid.
        let card_w = ((width - 24.0) / 3.0).floor();

        match self.tab {
            Tab::Appearance => {
                y = section(p, x, y, "Colour scheme", "It applies instantly.");
                let preview_h = (card_w * 0.58).round();
                let card_h = preview_h + 52.0;
                for (i, choice) in Scheme::ALL.into_iter().enumerate() {
                    let card = Rect::new(x + i as f32 * (card_w + 12.0), y, card_w, card_h);
                    if theme_card(p, ui, card, preview_h, choice, choice == scheme) && choice != scheme {
                        actions.push(Action::SetTheme(choice));
                    }
                }
                y += card_h + SECTION_GAP;

                y = section(p, x, y, "Chat", "How replies from thinking models look.");
                let row = group(p, x, y, width);
                setting_row(p, row, "Model reasoning", "Show what the model considered before answering.");
                let labels = ReasoningView::ALL.map(ReasoningView::label);
                let selected = ReasoningView::ALL.iter().position(|v| *v == reasoning).unwrap_or(0);
                if let Some(choice) = segmented(p, ui, control(row, 264.0), &labels, selected) {
                    actions.push(Action::SetReasoningView(ReasoningView::ALL[choice]));
                }
                y += ROW_H;
            }
            Tab::Skills => y = self.draw_skills(p, x, y, width),
            Tab::Usage => {
                y = section(p, x, y, "Usage", "Totals across every session saved on this device.");
                let stats = [
                    ("Total spent", format_cost(totals.cost)),
                    ("Tokens", group_digits(totals.tokens)),
                    ("Sessions", group_digits(totals.sessions as u64)),
                ];
                for (i, (label, value)) in stats.iter().enumerate() {
                    let stat = Rect::new(x + i as f32 * (card_w + 12.0), y, card_w, 84.0);
                    p.bordered(stat, t.surface, theme::RADIUS, 1.0, t.border);
                    p.label(label, theme::CAPTION, stat.x + 16.0, stat.y + 16.0, t.text_faint);
                    p.label(value, Style::semibold(24.0), stat.x + 16.0, stat.y + 38.0, t.text);
                }
                y += 84.0;
            }
            Tab::Account => {
                y = section(p, x, y, "Data", "Your sessions never leave this device except to reach the model.");
                let row = group(p, x, y, width);
                setting_row(p, row, "Saved sessions", "Stored as JSON files in ~/.serechat/sessions");
                if button(p, ui, control(row, 116.0), "Open folder", ButtonStyle::Secondary, true) {
                    actions.push(Action::OpenDataDir);
                }
                y += ROW_H + SECTION_GAP;

                y = section(p, x, y, "Account", "Manage how this device is signed in to SereChat.");
                let row = group(p, x, y, width);
                setting_row(p, row, "SereChat account", "Signed in on this device. Signing out keeps your sessions.");
                if button(p, ui, control(row, 96.0), "Sign out", ButtonStyle::Danger, true) {
                    actions.push(Action::SignOut);
                }
                y += ROW_H + 32.0;

                let footer = p.layout(concat!("SereChat Desktop ", env!("CARGO_PKG_VERSION")), theme::TINY, None);
                p.text_aligned(&footer, x, y, Align::Center, width, t.text_faint);
                y += footer.height();
            }
        }

        p.set_clip(clip);
        ui.blocker = None;
        self.content_h = y - top + 64.0;
    }
}

impl SettingsView {
    /// The skills the agent can load in the open chat, and the problems
    /// found reading them. Returns where the section ends.
    fn draw_skills(&self, p: &mut Painter, x: f32, y: f32, width: f32) -> f32 {
        let t = p.theme;
        let mut y = section(p, x, y, "Skills", "Yours from ~/.agents/skills, plus a project's own from its .agents/skills.");
        let Some(report) = &self.skills else {
            let row = group(p, x, y, width);
            setting_row(p, row, "Looking for skills…", "Reading .agents/skills folders.");
            return y + ROW_H;
        };
        // (title, description, badge)
        let mut rows: Vec<(String, String, &str)> =
            report.skills.iter().map(|s| (s.name.clone(), s.description.replace('\n', " "), s.scope.label())).collect();
        if report.skills.is_empty() {
            rows.push(("No skills found".to_owned(), "Add one as .agents/skills/<name>/SKILL.md, in a project or your home folder.".to_owned(), ""));
        }
        let list = Rect::new(x, y, width, ROW_H * rows.len() as f32);
        p.bordered(list, t.surface, theme::RADIUS, 1.0, t.border);
        for (i, (title, text, badge)) in rows.iter().enumerate() {
            let row = Rect::new(x, y, width, ROW_H);
            if i > 0 {
                p.rect(Rect::new(row.x + 1.0, row.y, row.w - 2.0, 1.0), t.border, 0.0);
            }
            let badge = p.layout(badge, theme::TINY, None);
            p.text(&badge, row.right() - 16.0 - badge.width(), row.y + 17.0, t.text_faint);
            let mut title = p.layout(title, theme::LABEL, None);
            title.truncate(p.fonts, row.w - 48.0 - badge.width());
            p.text(&title, row.x + 16.0, row.y + 15.0, t.text);
            let mut text = p.layout(text, theme::SMALL, None);
            text.truncate(p.fonts, row.w - 32.0);
            p.text(&text, row.x + 16.0, row.y + 36.0, t.text_muted);
            y += ROW_H;
        }
        for warning in &report.warnings {
            let text = p.layout(warning, theme::SMALL, Some(width));
            p.text(&text, x, y + 8.0, t.danger);
            y += text.height() + 8.0;
        }
        y
    }
}

/// The tab strip in the header, starting at `x`; the open tab is underlined
/// in the accent. Returns a newly clicked tab.
fn tabs(p: &mut Painter, ui: &mut Ui, mut x: f32, bar: Rect, open: Tab) -> Option<Tab> {
    let t = p.theme;
    let mut clicked = None;
    for tab in Tab::ALL {
        let label = p.layout(tab.label(), theme::LABEL, None);
        let cell = Rect::new(x, bar.y, label.width() + 24.0, bar.h);
        let hovered = tab != open && ui.hovered(cell);
        let hover = ui.anim(id(("settings-tab", tab.label())), f32::from(u8::from(hovered)));
        p.rect(Rect::new(cell.x, cell.y + 8.0, cell.w, cell.h - 16.0), fade(t.hover, hover), theme::RADIUS_SM);
        let color = if tab == open { t.text } else { mix(t.text_muted, t.text, hover) };
        p.text(&label, cell.x + 12.0, cell.y + (cell.h - label.height()) * 0.5, color);
        if tab == open {
            p.rect(Rect::new(cell.x + 8.0, cell.bottom() - 2.0, cell.w - 16.0, 2.0), t.accent, 1.0);
        }
        if hovered {
            ui.cursor = winit::window::CursorIcon::Pointer;
            if ui.clicked(cell) {
                clicked = Some(tab);
            }
        }
        x += cell.w;
    }
    clicked
}

/// Section heading with a description; returns where its content starts.
fn section(p: &mut Painter, x: f32, y: f32, title: &str, description: &str) -> f32 {
    let t = p.theme;
    p.label(title, Style::semibold(15.0), x, y, t.text);
    p.label(description, theme::SMALL, x, y + 24.0, t.text_muted);
    y + 58.0
}

/// A bordered group holding one row; returns the row's rect.
fn group(p: &mut Painter, x: f32, y: f32, width: f32) -> Rect {
    let t = p.theme;
    let rect = Rect::new(x, y, width, ROW_H);
    p.bordered(rect, t.surface, theme::RADIUS, 1.0, t.border);
    rect
}

/// Title and description on the left of a settings row.
fn setting_row(p: &mut Painter, row: Rect, title: &str, description: &str) {
    let t = p.theme;
    p.label(title, theme::LABEL, row.x + 16.0, row.y + 15.0, t.text);
    p.label(description, theme::SMALL, row.x + 16.0, row.y + 36.0, t.text_muted);
}

/// A control of `width` right-aligned in `row`.
fn control(row: Rect, width: f32) -> Rect {
    Rect::new(row.right() - 16.0 - width, row.y + (row.h - 30.0) * 0.5, width, 30.0)
}

/// Equal-width options in one outlined control, the selected one filled.
/// Returns a newly clicked option.
fn segmented(p: &mut Painter, ui: &mut Ui, rect: Rect, labels: &[&str], selected: usize) -> Option<usize> {
    let t = p.theme;
    p.bordered(rect, t.surface, theme::RADIUS_SM, 1.0, t.border_strong);
    let w = rect.w / labels.len() as f32;
    let mut clicked = None;
    for (i, label) in labels.iter().enumerate() {
        let cell = Rect::new((rect.x + i as f32 * w).round(), rect.y, w.round(), rect.h);
        let inner = Rect::new(cell.x + 3.0, cell.y + 3.0, cell.w - 6.0, cell.h - 6.0);
        let hovered = ui.hovered(cell) && i != selected;
        let hover = ui.anim(id(("segment", label, rect.y.to_bits())), f32::from(u8::from(hovered)));
        if i == selected {
            p.rect(inner, t.active, theme::RADIUS_SM - 1.0);
        } else {
            p.rect(inner, fade(t.hover, hover), theme::RADIUS_SM - 1.0);
        }
        let color = if i == selected { t.text } else { mix(t.text_muted, t.text, hover) };
        p.label_centered(label, theme::LABEL, cell, color);
        if hovered {
            ui.cursor = winit::window::CursorIcon::Pointer;
            if ui.clicked(cell) {
                clicked = Some(i);
            }
        }
    }
    clicked
}

/// A selectable card previewing `scheme`. Returns whether it was clicked.
fn theme_card(p: &mut Painter, ui: &mut Ui, card: Rect, preview_h: f32, scheme: Scheme, selected: bool) -> bool {
    let t = p.theme;
    let hovered = ui.hovered(card);
    let hover = ui.anim(id(("theme-card", scheme.key())), f32::from(u8::from(hovered)));
    let border = if selected { t.accent } else { mix(t.border, t.border_strong, hover) };
    p.bordered(card, t.surface, theme::RADIUS + 2.0, if selected { 2.0 } else { 1.0 }, border);
    preview(p, Rect::new(card.x + 8.0, card.y + 8.0, card.w - 16.0, preview_h), scheme.palette());

    // Radio button and name.
    let label_y = card.y + preview_h + 16.0;
    let radio = Rect::new(card.x + 12.0, label_y + 7.0, 14.0, 14.0);
    p.bordered(radio, [0.0; 4], 7.0, 1.5, if selected { t.accent } else { t.border_strong });
    if selected {
        p.rect(Rect::new(radio.x + 4.0, radio.y + 4.0, 6.0, 6.0), t.accent, 3.0);
    }
    let name = p.layout(scheme.label(), theme::LABEL, None);
    p.text(&name, card.x + 34.0, label_y + (28.0 - name.height()) * 0.5, t.text);

    if hovered {
        ui.cursor = winit::window::CursorIcon::Pointer;
    }
    ui.clicked(card)
}

/// A miniature of the app drawn in `c`'s colours.
fn preview(p: &mut Painter, r: Rect, c: &Palette) {
    p.bordered(r, c.bg, theme::RADIUS_SM, 1.0, c.border_strong);
    let inner = Rect::new(r.x + 1.0, r.y + 1.0, r.w - 2.0, r.h - 2.0);

    // Sidebar: rounded on the left only, with a hairline on its right.
    let side = Rect::new(inner.x, inner.y, (inner.w * 0.3).round(), inner.h);
    p.rect(side, c.panel, theme::RADIUS_SM - 1.0);
    p.rect(Rect::new(side.right() - 4.0, side.y, 4.0, side.h), c.panel, 0.0);
    p.rect(Rect::new(side.right(), side.y, 1.0, side.h), c.border, 0.0);
    p.rect(Rect::new(side.x + 4.0, side.y + 17.0, side.w - 8.0, 10.0), c.active, 2.0);
    for (i, w) in [0.5, 0.7, 0.55, 0.62, 0.45].into_iter().enumerate() {
        let alpha = if i == 1 { 0.9 } else { 0.45 };
        p.rect(Rect::new(side.x + 7.0, side.y + 8.0 + i as f32 * 10.0, (side.w - 14.0) * w, 3.0), fade(c.text_muted, alpha), 1.5);
    }

    // Chat: a prompt box, reply lines and the composer with its send button.
    let main = Rect::new(side.right() + 1.0, inner.y, inner.right() - side.right() - 1.0, inner.h);
    let pad = (main.w * 0.1).round();
    let col = Rect::new(main.x + pad, main.y, main.w - 2.0 * pad, main.h);
    p.bordered(Rect::new(col.x, col.y + 10.0, col.w, 12.0), c.surface, 2.0, 1.0, c.border);
    p.rect(Rect::new(col.x + 5.0, col.y + 14.5, col.w * 0.4, 3.0), fade(c.text, 0.7), 1.5);
    for (i, w) in [0.92, 0.84, 0.6].into_iter().enumerate() {
        p.rect(Rect::new(col.x, col.y + 30.0 + i as f32 * 8.0, col.w * w, 3.0), fade(c.text, 0.55), 1.5);
    }
    let composer = Rect::new(col.x, main.bottom() - 22.0, col.w, 16.0);
    p.bordered(composer, c.surface, 3.0, 1.0, c.border_strong);
    p.rect(Rect::new(composer.right() - 13.0, composer.y + 3.0, 10.0, 10.0), c.accent, 2.0);
}
