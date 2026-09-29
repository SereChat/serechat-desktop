//! The message list: Markdown replies, prompts with attachments, the
//! reasoning block, tool-call cards with approvals, selection and scrolling.

use serechat::ToolStatus;
use winit::window::CursorIcon;

use super::composer::file_badge;
use super::{Chat, Decision, Entry, Load, PRIMARY_KEY, SelPos, model_name, usage_caption};
use crate::app::Action;
use crate::doc::{Doc, INK_MUTED, INK_TEXT};
use crate::paint::{Painter, Rect, fade, mix};
use crate::text::{Align, Style, TextLayout};
use crate::theme;
use crate::tools;
use crate::ui::{ButtonStyle, Ui, button, chevron, id, keycap, logo};

/// Vertical space between messages.
const MESSAGE_GAP: f32 = 20.0;
/// Inner padding of boxed messages.
const BOX_PAD: (f32, f32) = (12.0, 10.0);
/// Height of the caption row under a reply.
const META_H: f32 = 30.0;
/// Height of the "Reasoning" toggle above a reply.
const REASONING_ROW: f32 = 30.0;
/// Height of a tool card's header.
const TOOL_ROW: f32 = 34.0;
/// Tallest a tool card's body gets before it is clipped.
const TOOL_BODY_MAX: f32 = 260.0;
/// Height of the approval buttons row.
const APPROVAL_ROW: f32 = 44.0;
/// Height of a row of attachment chips under a prompt.
const CHIPS_ROW: f32 = 36.0;
/// Style of tool output.
const TOOL_STYLE: Style = Style { line_height: 1.5, ..Style::mono(12.0) };

/// Where a laid-out document was drawn, for hit-testing after the frame.
struct Target {
    entry: usize,
    doc: u8,
    origin: (f32, f32),
    rect: Rect,
}

impl Entry {
    /// Rebuilds stale documents and returns the entry's height at `width`.
    fn measure(&mut self, p: &Painter, width: f32, live: bool) -> f32 {
        let boxed = self.boxed();
        let wrap = if boxed { width - 2.0 * BOX_PAD.0 } else { width };
        let key = (self.message.content.len(), boxed);
        if self.doc.as_ref().is_none_or(|d| !d.fits(wrap, p.scale)) || self.doc_key != key {
            let previous = self.doc.take();
            self.doc = Some(if boxed {
                Doc::plain(p.fonts, &self.message.content, theme::BODY, wrap, p.scale)
            } else {
                Doc::markdown(p.fonts, &self.message.content, wrap, p.scale, INK_TEXT, previous)
            });
            self.doc_key = key;
        }
        let doc_h = self.doc.as_ref().map_or(0.0, |d| d.height);
        if boxed {
            let chips = if self.message.attachments.is_empty() { 0.0 } else { CHIPS_ROW * chip_rows(p, &self.message.attachments, wrap) as f32 };
            let text = if self.message.content.is_empty() { 0.0 } else { doc_h };
            return text + chips + 2.0 * BOX_PAD.1 - if text == 0.0 && chips > 0.0 { 6.0 } else { 0.0 };
        }

        let mut height = 0.0;
        if !self.message.reasoning.is_empty() {
            height += REASONING_ROW;
            if self.show_reasoning {
                let rwrap = width - 14.0;
                if self.reasoning_doc.as_ref().is_none_or(|d| !d.fits(rwrap, p.scale)) || self.reasoning_len != self.message.reasoning.len() {
                    let previous = self.reasoning_doc.take();
                    self.reasoning_doc = Some(Doc::markdown(p.fonts, &self.message.reasoning, rwrap, p.scale, INK_MUTED, previous));
                    self.reasoning_len = self.message.reasoning.len();
                }
                height += self.reasoning_doc.as_ref().map_or(0.0, |d| d.height) + 12.0;
            }
        }
        let thinking = live && self.message.content.is_empty() && self.message.tool_calls.is_empty();
        height += if thinking { 18.0 } else { doc_h };
        self.tool_bodies.resize_with(self.message.tool_calls.len(), || None);
        for index in 0..self.message.tool_calls.len() {
            height += 8.0 + self.tool_height(p, index, width);
        }
        if !live {
            height += META_H;
        } else if !self.message.tool_calls.is_empty() {
            height += 6.0;
        }
        height
    }

    /// The text a tool card's body shows, if any: the preview while waiting
    /// for approval, the output once expanded.
    fn tool_body(&self, index: usize) -> Option<String> {
        let record = &self.message.tool_calls[index];
        let text = match record.status {
            ToolStatus::Pending if tools::needs_approval(&record.call.name) => tools::view(&record.call).preview?,
            ToolStatus::Done | ToolStatus::Failed | ToolStatus::Denied if self.open_tools.contains(&index) => record.output.clone(),
            _ => return None,
        };
        // Long bodies are clipped anyway; don't lay out more than fits.
        let mut lines: Vec<&str> = text.lines().take(120).collect();
        if text.lines().count() > 120 {
            lines.push("…");
        }
        Some(lines.join("\n"))
    }

    /// Height of tool card `index`, refreshing its body layout.
    fn tool_height(&mut self, p: &Painter, index: usize, width: f32) -> f32 {
        let record = &self.message.tool_calls[index];
        let status = record.status;
        let approval = status == ToolStatus::Pending && tools::needs_approval(&record.call.name);
        let body = self.tool_body(index);
        let slot = &mut self.tool_bodies[index];
        let mut height = TOOL_ROW;
        if let Some(body) = body {
            let key = (body.len(), status, (width - 24.0).to_bits());
            if slot.as_ref().is_none_or(|(k, _)| *k != key) {
                *slot = Some((key, TextLayout::new(p.fonts, &body, TOOL_STYLE, Some(width - 24.0), p.scale)));
            }
            height += slot.as_ref().map_or(0.0, |(_, l)| l.height().min(TOOL_BODY_MAX)) + 14.0;
        } else {
            *slot = None;
        }
        if approval {
            height += APPROVAL_ROW;
        }
        height
    }
}

/// How many rows of chips `attachments` need at `width`.
fn chip_rows(p: &Painter, attachments: &[serechat::Attachment], width: f32) -> usize {
    let mut rows = 1;
    let mut x = 0.0;
    for attachment in attachments {
        let chip_w = p.layout(&attachment.name, theme::SMALL, None).width().min(170.0) + 60.0;
        if x > 0.0 && x + chip_w > width {
            rows += 1;
            x = 0.0;
        }
        x += chip_w + 6.0;
    }
    rows
}

impl Chat {
    pub(super) fn draw_messages(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        let (x, width) = Self::column(Rect::new(view.x, 0.0, view.w, 0.0));
        let dt = ui.dt;
        let copied = self.copied.filter(|(_, _, at)| ui.time - at < 1.5);
        if ui.pressed && !view.contains(ui.mouse) {
            self.selection = None;
        }
        let selection = self.selection.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        let current = self.current;
        let models = &self.models;
        let Some(conversation) = self.conversations.iter_mut().find(|c| c.id == current) else { return };

        match &conversation.load {
            // Reading a file takes milliseconds; a spinner would only flash.
            Load::Summary | Load::Loading => return,
            Load::Failed(message) => {
                let layout = p.layout(message, theme::SMALL, Some(width));
                p.text_aligned(&layout, x, view.y + view.h * 0.4, Align::Center, width, t.danger);
                return;
            }
            Load::Loaded if conversation.entries.is_empty() => {
                draw_empty(p, view, conversation.project.as_deref());
                return;
            }
            Load::Loaded => {}
        }
        let streaming = conversation.stream.is_some();
        let last = conversation.entries.len() - 1;

        // Measure everything (layouts are cached) to know the scroll range.
        let mut content_h = 24.0;
        for (index, entry) in conversation.entries.iter_mut().enumerate() {
            content_h += entry.measure(p, width, streaming && index == last) + MESSAGE_GAP;
        }
        let max_scroll = (content_h - view.h).max(0.0);

        if ui.hovered(view) && ui.scroll != 0.0 {
            self.scroll_target = (self.scroll_target + ui.scroll).clamp(0.0, max_scroll);
            self.stick_to_bottom = self.scroll_target >= max_scroll - 1.0;
        }
        // Dragging a selection past the edges scrolls.
        if self.dragging && ui.down {
            let over = if ui.mouse.1 < view.y { ui.mouse.1 - view.y } else if ui.mouse.1 > view.bottom() { ui.mouse.1 - view.bottom() } else { 0.0 };
            if over != 0.0 {
                self.scroll_target = (self.scroll_target + over.clamp(-40.0, 40.0) * 0.5).clamp(0.0, max_scroll);
                self.stick_to_bottom = false;
                ui.animating = true;
            }
        }
        if self.stick_to_bottom {
            self.scroll_target = max_scroll;
        }
        self.scroll_target = self.scroll_target.min(max_scroll);
        self.scroll += (self.scroll_target - self.scroll) * (1.0 - (-dt * 18.0).exp());
        if (self.scroll_target - self.scroll).abs() < 0.5 {
            self.scroll = self.scroll_target;
        } else {
            ui.animating = true;
        }

        // Widgets scrolled under the header must not react.
        let in_view = view.contains(ui.mouse) && ui.hovered(view);
        let clip = p.push_clip(view);
        let mut y = view.y + 24.0 - self.scroll.round();
        let mut targets = Vec::new();
        let mut effects = Effects::default();
        for (index, entry) in conversation.entries.iter_mut().enumerate() {
            let live = streaming && index == last;
            let height = entry.measure(p, width, live);
            let area = Rect::new(x, y, width, height);
            y += height + MESSAGE_GAP;
            if area.bottom() < view.y || area.y > view.bottom() {
                continue;
            }
            let sel = |doc: u8, d: &Doc| selected_range(selection, index, doc, d);
            if entry.boxed() {
                let (fill, border, color) = if entry.message.failed {
                    (fade(t.danger, 0.08), fade(t.danger, 0.4), t.danger)
                } else {
                    (t.surface, t.border, t.text)
                };
                p.bordered(area, fill, theme::RADIUS, 1.0, border);
                let origin = (area.x + BOX_PAD.0, area.y + BOX_PAD.1);
                let mut chips_y = origin.1;
                if let Some(doc) = entry.doc.as_ref().filter(|_| !entry.message.content.is_empty()) {
                    if let Some((a, b)) = sel(1, doc) {
                        doc.draw_selection(p, origin, a, b);
                    }
                    doc.draw(p, ui, origin, color, in_view, None);
                    targets.push(Target { entry: index, doc: 1, origin, rect: Rect::new(origin.0, origin.1, width, doc.height) });
                    chips_y += doc.height + 4.0;
                }
                if let Some(path) = draw_attachment_chips(p, ui, &entry.message.attachments, (origin.0, chips_y), width - 2.0 * BOX_PAD.0, in_view) {
                    effects.open_path = Some(path);
                }
                continue;
            }

            let mut top = area.y;
            if !entry.message.reasoning.is_empty() {
                let label = p.layout("Reasoning", theme::SMALL, None);
                let toggle = Rect::new(x - 6.0, top, label.width() + 34.0, 26.0);
                let hovered = in_view && ui.hovered(toggle);
                let hover = ui.anim(id(("reasoning", entry.id)), f32::from(u8::from(hovered)));
                p.rect(toggle, fade(t.hover, hover), theme::RADIUS_SM);
                let color = mix(t.text_muted, t.text, hover);
                let (cx, cy) = if entry.show_reasoning { (toggle.x + 8.0, toggle.y + 11.0) } else { (toggle.x + 10.0, toggle.y + 9.5) };
                chevron(p, cx, cy, entry.show_reasoning, color);
                p.text(&label, toggle.x + 24.0, toggle.y + (26.0 - label.height()) * 0.5, color);
                if hovered {
                    ui.cursor = CursorIcon::Pointer;
                    if ui.clicked(toggle) {
                        entry.show_reasoning = !entry.show_reasoning;
                        ui.animating = true;
                    }
                }
                top += REASONING_ROW;
                if let Some(doc) = entry.reasoning_doc.as_ref().filter(|_| entry.show_reasoning) {
                    let origin = (x + 14.0, top);
                    p.rect(Rect::new(x, top, 2.0, doc.height), t.border_strong, 1.0);
                    if let Some((a, b)) = sel(0, doc) {
                        doc.draw_selection(p, origin, a, b);
                    }
                    let event = doc.draw(p, ui, origin, t.text_muted, in_view, None);
                    effects.link = effects.link.take().or(event.open_link);
                    targets.push(Target { entry: index, doc: 0, origin, rect: Rect::new(origin.0, origin.1, width - 14.0, doc.height) });
                    top += doc.height + 12.0;
                }
            }

            if live && entry.message.content.is_empty() && entry.message.tool_calls.is_empty() {
                // "Thinking" dots.
                for dot in 0..3 {
                    let phase = (ui.time * 5.0 - dot as f32 * 0.7).sin() * 0.5 + 0.5;
                    let dot_rect = Rect::new(x + dot as f32 * 11.0, top + 9.0 - phase * 3.0, 6.0, 6.0);
                    p.rect(dot_rect, fade(t.text_muted, 0.3 + 0.7 * phase), 3.0);
                }
                ui.animating = true;
                continue;
            }
            if let Some(doc) = &entry.doc {
                let origin = (x, top);
                if let Some((a, b)) = sel(1, doc) {
                    doc.draw_selection(p, origin, a, b);
                }
                if in_view && ui.hovered(Rect::new(x, top, width, doc.height)) {
                    ui.cursor = CursorIcon::Text;
                }
                let code_copied = copied.and_then(|(id, code, _)| (id == entry.id).then_some(code).flatten());
                let event = doc.draw(p, ui, origin, t.text, in_view, code_copied);
                if let Some(code) = event.copy_code.and_then(|i| doc.code(i).map(|c| (i, c.to_owned()))) {
                    effects.copy = Some((entry.id, Some(code.0), code.1));
                }
                effects.link = effects.link.take().or(event.open_link);
                targets.push(Target { entry: index, doc: 1, origin, rect: Rect::new(x, top, width, doc.height) });
                top += doc.height;
            }

            for tool in 0..entry.message.tool_calls.len() {
                top += 8.0;
                let card_h = entry.tool_height(p, tool, width);
                if let Some(decision) = draw_tool_card(p, ui, entry, tool, Rect::new(x, top, width, card_h), in_view) {
                    effects.decision = Some((index, tool, decision));
                }
                top += card_h;
            }

            if live {
                continue;
            }
            // Caption and hover actions under a finished reply.
            let meta_y = top + 6.0;
            if let Some(model) = &entry.message.model {
                let text = usage_caption(model_name(models, model), entry.message.usage, entry.message.cost);
                let caption = p.layout(&text, theme::TINY, None);
                p.text(&caption, x, meta_y + (24.0 - caption.height()) * 0.5, t.text_faint);
            }
            let is_copied = copied.is_some_and(|(id, code, _)| id == entry.id && code.is_none());
            if !entry.message.content.is_empty() && ((in_view && ui.hovered(area)) || is_copied) {
                let label = if is_copied { "✓ Copied" } else { "Copy" };
                if button(p, ui, Rect::new(area.right() - 72.0, meta_y, 72.0, 24.0), label, ButtonStyle::Ghost, true) {
                    effects.copy = Some((entry.id, None, entry.message.content.clone()));
                }
            }
        }
        p.set_clip(clip);

        // Selection: press to start, drag to extend, double/triple click for
        // a word/paragraph. Shift+click extends an existing selection.
        let hit = |mouse: (f32, f32)| -> Option<SelPos> {
            let target = targets.iter().min_by(|a, b| {
                let gap = |r: Rect| (r.y - mouse.1).max(mouse.1 - r.bottom()).max(0.0);
                gap(a.rect).total_cmp(&gap(b.rect))
            })?;
            let entry = &conversation.entries[target.entry];
            let doc = if target.doc == 0 { entry.reasoning_doc.as_ref()? } else { entry.doc.as_ref()? };
            let (piece, byte) = doc.hit(mouse.0 - target.origin.0, mouse.1 - target.origin.1)?;
            Some((target.entry, target.doc, piece, byte))
        };
        let on_text = targets.iter().any(|t| t.rect.contains(ui.mouse));
        if ui.pressed && in_view && ui.cursor != CursorIcon::Pointer {
            match hit(ui.mouse) {
                Some(pos) if on_text || ui.mods.shift_key() => {
                    let entry = &conversation.entries[pos.0];
                    let doc = if pos.1 == 0 { entry.reasoning_doc.as_ref() } else { entry.doc.as_ref() };
                    self.selection = match (ui.clicks, doc) {
                        (2, Some(doc)) => {
                            let word = doc.word((pos.2, pos.3));
                            Some(((pos.0, pos.1, pos.2, word.start), (pos.0, pos.1, pos.2, word.end)))
                        }
                        (clicks, Some(doc)) if clicks >= 3 => {
                            let len = doc.texts.get(pos.2).map_or(0, |p| p.text.len());
                            Some(((pos.0, pos.1, pos.2, 0), (pos.0, pos.1, pos.2, len)))
                        }
                        _ if ui.mods.shift_key() => self.selection.map(|(anchor, _)| (anchor, pos)).or(Some((pos, pos))),
                        _ => Some((pos, pos)),
                    };
                    self.dragging = ui.clicks == 1;
                }
                _ => self.selection = None,
            }
        }
        if self.dragging {
            if ui.down {
                if let (Some(pos), Some((anchor, _))) = (hit(ui.mouse), self.selection) {
                    self.selection = Some((anchor, pos));
                }
            } else {
                self.dragging = false;
            }
        }

        if let Some((id, code, text)) = effects.copy {
            self.copied = Some((id, code, ui.time));
            actions.push(Action::Copy(text));
        }
        if let Some(url) = effects.link {
            actions.push(Action::OpenLink(url));
        }
        if let Some(path) = effects.open_path {
            actions.push(Action::OpenPath(path));
        }
        if copied.is_some() {
            ui.animating = true;
        }
        if let Some((entry, tool, decision)) = effects.decision {
            self.decide(entry, tool, decision, actions);
        }

        // Scrollbar.
        if max_scroll > 0.0 {
            let thumb_h = (view.h * view.h / content_h).max(32.0);
            let thumb_y = view.y + (view.h - thumb_h) * (self.scroll / max_scroll);
            let track = Rect::new(view.right() - 12.0, view.y, 12.0, view.h);
            let hover = ui.anim(id("scrollbar"), f32::from(u8::from(ui.hovered(track))));
            p.rect(Rect::new(view.right() - 9.0, thumb_y, 6.0, thumb_h), fade(t.text, 0.1 + 0.1 * hover), 3.0);
        }
    }
}

/// Things the message loop asks for once drawing is done.
#[derive(Default)]
struct Effects {
    /// Entry id, code block (or whole message) and the text to copy.
    copy: Option<(u64, Option<usize>, String)>,
    link: Option<String>,
    open_path: Option<String>,
    decision: Option<(usize, usize, Decision)>,
}

/// The part of `doc` (entry `entry`, document `doc_id`) inside the selection.
fn selected_range(selection: Option<(SelPos, SelPos)>, entry: usize, doc_id: u8, doc: &Doc) -> Option<((usize, usize), (usize, usize))> {
    let (a, b) = selection?;
    if a == b || (entry, doc_id) < (a.0, a.1) || (entry, doc_id) > (b.0, b.1) {
        return None;
    }
    let from = if (entry, doc_id) == (a.0, a.1) { (a.2, a.3) } else { (0, 0) };
    let to = if (entry, doc_id) == (b.0, b.1) { (b.2, b.3) } else { doc.end() };
    (from != to).then_some((from, to))
}

/// Draws a prompt's attachment chips; returns a file the user clicked.
fn draw_attachment_chips(p: &mut Painter, ui: &mut Ui, attachments: &[serechat::Attachment], origin: (f32, f32), width: f32, interactive: bool) -> Option<String> {
    let t = p.theme;
    let mut clicked = None;
    let (mut x, mut y) = (0.0, 0.0);
    for attachment in attachments {
        let mut name = p.layout(&attachment.name, theme::SMALL, None);
        name.truncate(p.fonts, 170.0);
        let chip_w = name.width() + 60.0;
        if x > 0.0 && x + chip_w > width {
            x = 0.0;
            y += CHIPS_ROW;
        }
        let chip = Rect::new(origin.0 + x, origin.1 + y + 2.0, chip_w, CHIPS_ROW - 8.0);
        let hovered = interactive && ui.hovered(chip);
        p.bordered(chip, if hovered { t.hover } else { t.bg }, theme::RADIUS_SM, 1.0, t.border_strong);
        file_badge(p, &attachment.mime, &attachment.name, Rect::new(chip.x + 4.0, chip.y + 4.0, 34.0, chip.h - 8.0));
        p.text(&name, chip.x + 46.0, chip.y + (chip.h - name.height()) * 0.5, t.text);
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(chip) {
                clicked = Some(attachment.path.clone());
            }
        }
        x += chip_w + 6.0;
    }
    clicked
}

/// Draws tool card `index` of `entry` into `rect`; returns an approval decision.
fn draw_tool_card(p: &mut Painter, ui: &mut Ui, entry: &mut Entry, index: usize, rect: Rect, interactive: bool) -> Option<Decision> {
    let t = p.theme;
    let record = &entry.message.tool_calls[index];
    let view = tools::view(&record.call);
    let status = record.status;
    let approval = status == ToolStatus::Pending && tools::needs_approval(&record.call.name);
    let border = if approval { fade(t.accent, 0.6) } else { t.border };
    p.bordered(rect, t.code_bg, theme::RADIUS, 1.0, border);

    // Header: status, verb and target, expand chevron.
    let header = Rect::new(rect.x, rect.y, rect.w, TOOL_ROW);
    let icon = Rect::new(header.x + 10.0, header.y + (TOOL_ROW - 16.0) * 0.5, 16.0, 16.0);
    match status {
        ToolStatus::Pending if approval => p.rect(Rect::new(icon.x + 4.0, icon.y + 4.0, 8.0, 8.0), t.accent, 4.0),
        ToolStatus::Pending | ToolStatus::Running => {
            // A small spinner: eight dots fading around a circle.
            for i in 0..8 {
                let a = i as f32 / 8.0 * std::f32::consts::TAU;
                let phase = (1.0 - (ui.time * 1.4 - i as f32 / 8.0).rem_euclid(1.0)).powi(2);
                let (cx, cy) = (icon.x + 8.0 + a.cos() * 5.5, icon.y + 8.0 + a.sin() * 5.5);
                p.rect(Rect::new(cx - 1.25, cy - 1.25, 2.5, 2.5), fade(t.text_muted, 0.25 + 0.75 * phase), 1.25);
            }
            ui.animating = true;
        }
        ToolStatus::Done => p.label_centered("✓", Style::semibold(12.5), icon, t.syntax[1]),
        ToolStatus::Failed => p.label_centered("×", Style::semibold(14.0), icon, t.danger),
        ToolStatus::Denied => p.rect(Rect::new(icon.x + 4.0, icon.y + 7.5, 8.0, 1.5), t.text_faint, 0.5),
    }
    let verb = p.layout(view.verb, theme::LABEL, None);
    p.text(&verb, header.x + 34.0, header.y + (TOOL_ROW - verb.height()) * 0.5, t.text);
    let expandable = status.is_finished() && !record.output.is_empty();
    let right = if approval {
        let waiting = p.layout("Needs approval", theme::TINY, None);
        p.text(&waiting, header.right() - 12.0 - waiting.width(), header.y + (TOOL_ROW - waiting.height()) * 0.5, t.accent);
        waiting.width() + 24.0
    } else if expandable {
        chevron(p, header.right() - 22.0, header.y + TOOL_ROW * 0.5 - 3.0, entry.open_tools.contains(&index), t.text_faint);
        34.0
    } else {
        12.0
    };
    let mut target = p.layout(&view.target.replace('\n', " "), TOOL_STYLE, None);
    target.truncate(p.fonts, (rect.w - 46.0 - verb.width() - right).max(20.0));
    p.text(&target, header.x + 42.0 + verb.width(), header.y + (TOOL_ROW - target.height()) * 0.5, t.text_muted);
    if expandable && interactive && ui.hovered(header) {
        ui.cursor = CursorIcon::Pointer;
        if ui.clicked(header) && !entry.open_tools.remove(&index) {
            entry.open_tools.insert(index);
        }
    }

    // Body: the preview awaiting approval, or the expanded output.
    let mut y = header.bottom();
    if let Some((_, layout)) = entry.tool_bodies.get(index).and_then(Option::as_ref) {
        p.rect(Rect::new(rect.x, y, rect.w, 1.0), t.border, 0.0);
        let body_h = layout.height().min(TOOL_BODY_MAX);
        let clip = p.push_clip(Rect::new(rect.x, y + 7.0, rect.w, body_h));
        let color = if status == ToolStatus::Failed { t.danger } else { t.text_muted };
        p.text(layout, rect.x + 12.0, y + 7.0, color);
        p.set_clip(clip);
        y += body_h + 14.0;
    }
    if approval {
        let row = Rect::new(rect.x + 12.0, y + 6.0, rect.w - 24.0, 30.0);
        let allow = Rect::new(row.x, row.y, 76.0, row.h);
        let always = Rect::new(allow.right() + 8.0, row.y, 150.0, row.h);
        let deny = Rect::new(always.right() + 8.0, row.y, 70.0, row.h);
        let hint = p.layout(&format!("Always allow skips asking again for {} in this chat", view.verb.to_lowercase()), theme::TINY, None);
        if deny.right() + 16.0 + hint.width() < row.right() {
            p.text(&hint, deny.right() + 16.0, row.y + (row.h - hint.height()) * 0.5, t.text_faint);
        }
        // Only refuse clicks through the header onto buttons scrolled beneath it.
        let enabled = |ui: &Ui, r: Rect| interactive || !ui.hovered(r);
        if button(p, ui, allow, "Allow", ButtonStyle::Primary, enabled(ui, allow)) {
            return Some(Decision::Allow);
        }
        if button(p, ui, always, "Always allow", ButtonStyle::Secondary, enabled(ui, always)) {
            return Some(Decision::Always);
        }
        if button(p, ui, deny, "Deny", ButtonStyle::Ghost, enabled(ui, deny)) {
            return Some(Decision::Deny);
        }
    }
    None
}

/// The welcome state of an empty conversation.
fn draw_empty(p: &mut Painter, view: Rect, project: Option<&str>) {
    let t = p.theme;
    let cx = view.x + view.w * 0.5;
    let top = view.y + (view.h * 0.5 - 170.0).max(24.0);
    logo(p, Rect::new(cx - 18.0, top, 36.0, 36.0));
    let title = p.layout(if project.is_some() { "What should we build?" } else { "What can I help with?" }, theme::TITLE, None);
    p.text_aligned(&title, view.x, top + 56.0, Align::Center, view.w, t.text);
    let subtitle = match project {
        Some(path) => format!("Working in {}. SereChat can read, search and (with your approval) change files here.", super::display_path(path)),
        None => "Ask anything, attach files, or open a folder to let SereChat work on a project.".to_owned(),
    };
    let subtitle = p.layout(&subtitle, theme::SMALL, Some((view.w - 64.0).min(520.0)));
    p.text_aligned(&subtitle, view.x + (view.w - subtitle.width()) * 0.5 - 0.0, top + 92.0, Align::Center, subtitle.width(), t.text_muted);

    // Keyboard shortcuts, the way Zed's welcome page lists them.
    let key = |k: &str| format!("{PRIMARY_KEY}+{k}");
    let shortcuts = [
        ("Search everything", key("K")),
        ("New chat", key("N")),
        ("Open a project folder", key("O")),
        ("New line", "Shift+Enter".to_owned()),
        ("Stop reply", "Esc".to_owned()),
        ("Settings", key(",")),
    ];
    let width = 280.0;
    let mut y = top + 110.0 + subtitle.height();
    for (label, keys) in shortcuts {
        let text = p.layout(label, theme::SMALL, None);
        p.text(&text, cx - width * 0.5, y + (26.0 - text.height()) * 0.5, t.text_muted);
        keycap(p, &keys, cx + width * 0.5, y + 3.0);
        y += 30.0;
    }
}
