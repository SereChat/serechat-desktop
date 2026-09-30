//! The composer: attachment chips, the text field (with input-method
//! preedit), and the toolbar with attach, model, reasoning and send.

use winit::window::CursorIcon;

use super::{Chat, Menu, model_name};
use crate::app::Action;
use crate::attachments::human_size;
use crate::image::{self, Lookup};
use crate::paint::{Painter, Rect, fade, mix};
use crate::text::{Style, TextLayout};
use crate::theme;
use crate::ui::{Ui, chevron, id};

/// Composer grows up to this many lines, then scrolls.
const MAX_LINES: usize = 8;
/// Height of an attachment chip.
const CHIP_H: f32 = 30.0;
/// Seconds a notice stays up.
const NOTICE_SECS: f32 = 7.0;

impl Chat {
    /// Draws the composer. Returns its top edge (including any notice) and
    /// the toolbar's model and reasoning buttons (menu anchors).
    pub(super) fn draw_composer(&mut self, p: &mut Painter, ui: &mut Ui, main: Rect, actions: &mut Vec<Action>) -> (f32, [Rect; 2]) {
        let t = p.theme;
        let (x, width) = Self::column(main);
        let pad = 12.0;
        let toolbar_h = 40.0;
        let text_w = width - 2.0 * pad;

        // With an input method composing, show its text at the caret.
        let cursor = self.composer.cursor();
        let (shown, preedit) = match &self.preedit {
            Some((text, _)) => {
                let mut shown = self.composer.text().to_owned();
                shown.insert_str(cursor, text);
                (shown, Some(cursor..cursor + text.len()))
            }
            None => (self.composer.text().to_owned(), None),
        };
        let layout = p.layout(&shown, theme::BODY, Some(text_w));
        let line_h = layout.line_height();
        let visible_h = layout.line_count().min(MAX_LINES) as f32 * line_h;

        // Attachment chips wrap above the text.
        let chips = self.chip_layout(p, text_w);
        let chips_h = chips.last().map_or(0.0, |(r, _)| r.bottom() + 8.0);
        let card_h = pad + chips_h + visible_h + toolbar_h;
        let card = Rect::new(x, main.bottom() - 20.0 - card_h, width, card_h);
        let text_area = Rect::new(card.x + pad, card.y + pad + chips_h, text_w, visible_h);

        // Keep the caret inside the visible part of a tall draft.
        let caret_byte = preedit.as_ref().map_or(cursor, |r| {
            let ime_cursor = self.preedit.as_ref().and_then(|(_, c)| *c).map_or(r.end - r.start, |(_, end)| end);
            r.start + ime_cursor
        });
        let (caret_x, caret_y) = layout.caret(caret_byte);
        self.composer_scroll = self
            .composer_scroll
            .clamp(caret_y + line_h - visible_h, caret_y)
            .clamp(0.0, (layout.height() - visible_h).max(0.0));
        let origin = (text_area.x, text_area.y - self.composer_scroll);
        self.caret_rect = Some(Rect::new(origin.0 + caret_x, origin.1 + caret_y, 2.0, line_h));

        // Mouse: click to place the caret, drag to select.
        let hit = |ui: &Ui| layout.hit(ui.mouse.0 - origin.0, ui.mouse.1 - origin.1);
        let text_zone = Rect::new(card.x, text_area.y - 4.0, card.w, text_area.h + 8.0);
        if ui.hovered(text_zone) {
            ui.cursor = CursorIcon::Text;
            if ui.pressed && preedit.is_none() {
                self.composer.set_cursor(hit(ui), ui.mods.shift_key());
                if ui.clicks == 2 {
                    self.composer.select_word();
                }
                self.selecting = true;
                ui.last_edit = ui.time;
            }
        }
        if self.selecting {
            if ui.down && ui.clicks < 2 {
                self.composer.set_cursor(hit(ui), true);
            } else if !ui.down {
                self.selecting = false;
            }
        }

        let notice_top = self.draw_notice(p, ui, x, width, card.y);

        let focus = ui.anim(id("composer-focus"), f32::from(u8::from(ui.focused && self.spotlight.is_none())));
        p.shadow(Rect::new(card.x, card.y + 4.0, card.w, card.h), t.shadow, theme::RADIUS, 14.0);
        p.bordered(card, t.surface, theme::RADIUS, 1.0, mix(t.border_strong, t.border_focus, focus));

        if let Some(index) = self.draw_chips(p, ui, &chips, (card.x + pad, card.y + pad)) {
            self.pending.remove(index);
        }

        let clip = p.push_clip(Rect::new(text_area.x - 2.0, text_area.y, text_area.w + 4.0, text_area.h));
        let selection = self.composer.selection();
        if !selection.is_empty() && preedit.is_none() {
            for (start, end, y) in layout.line_spans() {
                let (from, to) = (selection.start.max(start), selection.end.min(end));
                if from > to || (from == to && selection.end <= end) {
                    continue;
                }
                let x0 = layout.caret(from).0;
                // A selected line break shows as a small tail.
                let x1 = if selection.end > end { layout.caret(to).0 + 6.0 } else { layout.caret(to).0 };
                p.rect(Rect::new(origin.0 + x0, origin.1 + y, x1 - x0, line_h), t.selection, 2.0);
            }
        }
        if shown.is_empty() {
            let placeholder = if self.pending.is_empty() { "Message SereChat…" } else { "Add a message, or send the files as they are…" };
            p.label(placeholder, theme::BODY, origin.0, origin.1, t.text_faint);
        } else {
            p.text(&layout, origin.0, origin.1, t.text);
        }
        // The input method's text is underlined until committed.
        if let Some(range) = &preedit {
            for (start, end, y) in layout.line_spans() {
                let (from, to) = (range.start.max(start), range.end.min(end));
                if from < to {
                    let (x0, x1) = (layout.caret(from).0, layout.caret(to).0);
                    p.rect(Rect::new(origin.0 + x0, origin.1 + y + line_h - 5.0, x1 - x0, 1.5), t.accent, 0.0);
                }
            }
        }
        if ui.caret_visible() && (selection.is_empty() || preedit.is_some()) && self.spotlight.is_none() {
            p.rect(Rect::new(origin.0 + caret_x - 1.0, origin.1 + caret_y + 3.0, 2.0, line_h - 6.0), t.accent, 0.0);
        }
        p.set_clip(clip);

        // Toolbar: attach, model and reasoning on the left; send/stop on the right.
        let item_y = card.bottom() - 8.0 - 26.0;
        let attach = Rect::new(card.x + 6.0, item_y, 26.0, 26.0);
        let attach_hover = ui.anim(id("attach"), f32::from(u8::from(ui.hovered(attach))));
        p.rect(attach, fade(t.hover, attach_hover), theme::RADIUS_SM);
        p.label_centered("+", Style::regular(18.0), attach, mix(t.text_muted, t.text, attach_hover));
        if ui.hovered(attach) {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(attach) {
                actions.push(Action::PickFiles);
            }
        }
        let name = model_name(&self.models, &self.model).to_owned();
        let model = self.toolbar_button(p, ui, attach.right() + 4.0, item_y, &name, Menu::Model);
        let reasoning = format!("Reasoning: {}", self.reasoning_in_use().label());
        let reasoning = self.toolbar_button(p, ui, model.right() + 4.0, item_y, &reasoning, Menu::Reasoning);

        let busy = self.current().busy();
        let ready = !self.composer.text().trim().is_empty() || !self.pending.is_empty();
        let send = Rect::new(card.right() - 8.0 - 26.0, item_y, 26.0, 26.0);
        let hovered = ui.hovered(send) && (busy || ready);
        let hover = ui.anim(id("send"), f32::from(u8::from(hovered)));
        if busy {
            p.rect(send, mix(t.hover, t.active, hover), theme::RADIUS_SM);
            p.rect(Rect::new(send.x + 9.0, send.y + 9.0, 8.0, 8.0), t.text, 1.5);
        } else if ready {
            p.rect(send, fade(t.accent, 1.0 - 0.14 * hover), theme::RADIUS_SM);
            p.label_centered("↑", Style::semibold(15.0), send, t.on_accent);
        } else {
            p.rect(send, t.hover, theme::RADIUS_SM);
            p.label_centered("↑", Style::semibold(15.0), send, t.text_faint);
        }
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(send) {
                if busy {
                    self.stop(actions);
                } else {
                    self.send(actions);
                }
            }
        }

        self.composer_layout = if preedit.is_none() { Some((layout, origin)) } else { None };
        (notice_top.min(card.y), [model, reasoning])
    }

    /// Positions of the attachment chips (plus loading placeholders),
    /// relative to the chip area's top-left, wrapped to `width`.
    fn chip_layout(&self, p: &Painter, width: f32) -> Vec<(Rect, Option<TextLayout>)> {
        let mut out = Vec::new();
        let (mut x, mut y) = (0.0, 0.0);
        let mut place = |w: f32, label: Option<TextLayout>| {
            if x > 0.0 && x + w > width {
                x = 0.0;
                y += CHIP_H + 6.0;
            }
            out.push((Rect::new(x, y, w.min(width), CHIP_H), label));
            x += w + 6.0;
        };
        for attachment in &self.pending {
            let mut name = p.layout(&attachment.name, theme::SMALL, None);
            name.truncate(p.fonts, 170.0);
            let w = name.width() + 112.0;
            place(w, Some(name));
        }
        for _ in 0..self.importing {
            place(110.0, None);
        }
        out
    }

    /// Draws the chips at `origin`; returns the index of one whose remove
    /// button was clicked.
    fn draw_chips(&self, p: &mut Painter, ui: &mut Ui, chips: &[(Rect, Option<TextLayout>)], origin: (f32, f32)) -> Option<usize> {
        let t = p.theme;
        let mut removed = None;
        for (index, (rect, name)) in chips.iter().enumerate() {
            let chip = Rect::new(origin.0 + rect.x, origin.1 + rect.y, rect.w, rect.h);
            p.bordered(chip, t.bg, theme::RADIUS_SM, 1.0, t.border_strong);
            let Some(name) = name else {
                let pulse = 0.4 + 0.3 * (ui.time * 5.0).sin();
                p.label("Loading…", theme::SMALL, chip.x + 12.0, chip.y + 6.0, fade(t.text_muted, pulse + 0.3));
                ui.animating = true;
                continue;
            };
            let attachment = &self.pending[index];
            file_icon(p, attachment, Rect::new(chip.x + 5.0, chip.y + 5.0, 34.0, 20.0));
            p.text(name, chip.x + 45.0, chip.y + (CHIP_H - name.height()) * 0.5, t.text);
            let size = p.layout(&human_size(attachment.size), theme::TINY, None);
            p.text(&size, chip.x + 51.0 + name.width(), chip.y + (CHIP_H - size.height()) * 0.5, t.text_faint);
            let close = Rect::new(chip.right() - 26.0, chip.y + 5.0, 20.0, 20.0);
            let over = ui.hovered(close);
            if over {
                p.rect(close, t.hover, theme::RADIUS_SM);
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(close) {
                    removed = Some(index);
                }
            }
            p.label_centered("×", Style::regular(15.0), close, if over { t.text } else { t.text_faint });
        }
        removed
    }

    /// Draws the transient notice above `bottom`; returns its top edge.
    fn draw_notice(&mut self, p: &mut Painter, ui: &mut Ui, x: f32, width: f32, bottom: f32) -> f32 {
        let Some((text, since)) = &mut self.notice else { return bottom };
        let since = *since.get_or_insert(ui.time);
        let age = ui.time - since;
        if age > NOTICE_SECS {
            self.notice = None;
            return bottom;
        }
        ui.animating = true;
        let t = p.theme;
        let fade_out = ((NOTICE_SECS - age) / 0.4).min(1.0);
        let layout = p.layout(text, theme::SMALL, Some(width - 48.0));
        let rect = Rect::new(x, bottom - 10.0 - layout.height() - 16.0, width, layout.height() + 16.0);
        p.bordered(rect, fade(t.surface, fade_out), theme::RADIUS, 1.0, fade(t.danger, 0.5 * fade_out));
        p.text(&layout, rect.x + 14.0, rect.y + 8.0, fade(t.text, fade_out));
        let close = Rect::new(rect.right() - 28.0, rect.y + 4.0, 22.0, 22.0);
        p.label_centered("×", Style::regular(15.0), close, fade(t.text_faint, fade_out));
        if ui.hovered(rect) {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(rect) {
                self.notice = None;
            }
        }
        rect.y
    }

    /// A ghost button with a chevron that toggles `menu`. Returns its rect.
    fn toolbar_button(&mut self, p: &mut Painter, ui: &mut Ui, x: f32, y: f32, label: &str, menu: Menu) -> Rect {
        let t = p.theme;
        let text = p.layout(label, theme::SMALL, None);
        let rect = Rect::new(x, y, text.width() + 34.0, 26.0);
        let open = self.menu == Some(menu);
        let hovered = ui.hovered(rect);
        let hover = ui.anim(id(("toolbar", menu == Menu::Model)), f32::from(u8::from(hovered || open)));
        p.rect(rect, fade(t.hover, hover), theme::RADIUS_SM);
        let color = mix(t.text_muted, t.text, hover);
        p.text(&text, rect.x + 8.0, y + (26.0 - text.height()) * 0.5, color);
        chevron(p, rect.right() - 17.0, y + 11.0, true, color);
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(rect) {
                self.menu = if open { None } else { Some(menu) };
                self.menu_scroll = 0.0;
            }
        }
        rect
    }
}

/// An image's thumbnail, or [`file_badge`] while it loads and for other files.
pub(super) fn file_icon(p: &mut Painter, attachment: &serechat::Attachment, rect: Rect) {
    if !(image::supported(&attachment.mime) && matches!(p.image(&attachment.path, rect, 3.0), Lookup::Ready(_))) {
        file_badge(p, &attachment.mime, &attachment.name, rect);
    }
}

/// A small coloured badge naming a file's kind: IMG, PDF, or its extension.
pub(super) fn file_badge(p: &mut Painter, mime: &str, name: &str, rect: Rect) {
    let t = p.theme;
    let ext = std::path::Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("txt");
    let (label, color) = if mime.starts_with("image/") {
        ("IMG".to_owned(), t.syntax[1])
    } else if mime == "application/pdf" {
        ("PDF".to_owned(), t.danger)
    } else {
        (ext.chars().take(4).collect::<String>().to_uppercase(), t.syntax[4])
    };
    p.rect(rect, fade(color, 0.16), 3.0);
    p.label_centered(&label, Style::semibold(9.5), rect, color);
}
