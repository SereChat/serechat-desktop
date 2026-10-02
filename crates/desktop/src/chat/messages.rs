//! The message list: Markdown replies, prompts with attachments, generated
//! images, videos and audio, the reasoning block, tool-call cards with
//! approvals and diffs, selection and scrolling.

use std::sync::Arc;

use serechat::{MediaKind, StoredMessage, ToolStatus};
use winit::window::CursorIcon;

use super::composer::{capitalized, file_badge, file_icon};
use super::agent::MAX_RETRIES;
use super::{Chat, Decision, Entry, Load, PRIMARY_KEY, ReasoningView, SelPos, StreamingCall, format_cost, label_of, usage_caption};
use crate::app::Action;
use crate::attachments::human_size;
use crate::diff::{self, Diff, Kind};
use crate::doc::{Doc, INK_MUTED, INK_TEXT};
use crate::image::{self, Lookup};
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
/// Tallest an open tool card's screenshot gets.
const SHOT_MAX: f32 = 320.0;
/// Height of the approval buttons row.
const APPROVAL_ROW: f32 = 44.0;
/// Height of the retry status or Continue button under the messages.
const FOOTER_H: f32 = 44.0;
/// Height of a row of attachment chips under a prompt.
const CHIPS_ROW: f32 = 36.0;
/// Size of an image attachment's thumbnail tile.
const IMAGE_TILE: (f32, f32) = (160.0, 120.0);
/// Lines of a tool call's content shown while the model writes it.
const STREAM_LINES: usize = 14;
/// Style of tool output.
const TOOL_STYLE: Style = Style { line_height: 1.5, ..Style::mono(12.0) };
/// Height of a diff row (one line of [`TOOL_STYLE`]).
const DIFF_ROW: f32 = 18.0;
/// Diff rows shown before "Show all".
const DIFF_ROWS: usize = 16;
/// Most diff rows ever laid out.
const DIFF_MAX_ROWS: usize = 2000;
/// Height of the "Show all" row under a long diff.
const DIFF_FOOTER: f32 = 28.0;
/// Largest side of a generated image, before the thumbnail limit.
const IMAGE_MAX: f32 = 480.0;
/// Side of the tile an image is generated into.
const IMAGE_PENDING: f32 = 280.0;
/// Height of a generated video or audio file's card.
const MEDIA_CARD_H: f32 = 60.0;
/// Width of that card.
const MEDIA_CARD_W: f32 = 420.0;

/// A file change shown as a diff, with its rows laid out.
pub(super) struct DiffView {
    diff: Arc<Diff>,
    /// Diffed against the file itself, so its line numbers are real.
    exact: bool,
    /// Every row is shown, not only the first [`DIFF_ROWS`].
    full: bool,
    /// Rows laid out, and the (row count, scale) they were laid out for.
    rows: Option<((usize, u32), DiffRows)>,
}

/// Laid-out rows of a diff.
struct DiffRows {
    /// Width of each line-number column; 0 without numbers.
    gutter: f32,
    rows: Vec<RowText>,
}

/// One diff row's text: old and new line numbers, and the line.
struct RowText {
    old: Option<TextLayout>,
    new: Option<TextLayout>,
    text: TextLayout,
}

impl DiffView {
    /// A view of `diff`; `exact` when diffed against the whole file.
    pub(super) fn new(diff: Arc<Diff>, exact: bool, full: bool) -> Self {
        Self { diff, exact, full, rows: None }
    }

    /// Whether the user asked for every row.
    pub(super) fn full(&self) -> bool {
        self.full
    }

    /// Rows shown (an unchanged file shows one saying so).
    fn shown(&self) -> usize {
        let rows = self.diff.lines.len().max(1);
        rows.min(if self.full { DIFF_MAX_ROWS } else { DIFF_ROWS })
    }

    /// Whether a "Show all" row follows.
    fn footer(&self) -> bool {
        self.diff.lines.len() > DIFF_ROWS
    }

    fn height(&self) -> f32 {
        12.0 + self.shown() as f32 * DIFF_ROW + if self.footer() { DIFF_FOOTER } else { 0.0 }
    }

    /// Lays out the rows shown, unless done for this count and scale.
    fn layout(&mut self, p: &Painter) {
        let key = (self.shown(), p.scale.to_bits());
        if self.rows.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        let lines = &self.diff.lines[..key.0.min(self.diff.lines.len())];
        let digits = lines.iter().map(|l| l.old.max(l.new)).max().unwrap_or(0).to_string().len();
        let numbered = self.exact && lines.iter().any(|l| l.old > 0 || l.new > 0);
        let gutter = if numbered { p.layout(&"8".repeat(digits), TOOL_STYLE, None).width() + 16.0 } else { 0.0 };
        let number = |n: usize| (numbered && n > 0).then(|| p.layout(&n.to_string(), TOOL_STYLE, None));
        let mut rows: Vec<RowText> = lines
            .iter()
            .map(|line| {
                let text = match line.kind {
                    Kind::Skipped => format!("⋯ {} unchanged line{}", line.skipped, if line.skipped == 1 { "" } else { "s" }),
                    _ => line.text.clone(),
                };
                RowText { old: number(line.old), new: number(line.new), text: p.layout(&text, TOOL_STYLE, None) }
            })
            .collect();
        if rows.is_empty() {
            rows.push(RowText { old: None, new: None, text: p.layout("No changes.", TOOL_STYLE, None) });
        }
        self.rows = Some((key, DiffRows { gutter, rows }));
    }
}

/// Where a laid-out document was drawn, for hit-testing after the frame.
struct Target {
    entry: usize,
    doc: u8,
    origin: (f32, f32),
    rect: Rect,
}

impl Entry {
    /// Waiting for the first words of a live reply.
    fn thinking(&self, live: bool) -> bool {
        live && self.message.content.is_empty() && self.message.tool_calls.is_empty() && self.streaming_calls.is_empty()
    }

    /// Whether the reasoning header row is drawn: while a live reply
    /// thinks, and above a finished reply that reasoned.
    fn reasoning_row(&self, live: bool, view: ReasoningView) -> bool {
        view != ReasoningView::Hidden && (self.thinking(live) || !self.message.reasoning.is_empty())
    }

    /// Rebuilds stale documents and returns the entry's height at `width`.
    fn measure(&mut self, p: &Painter, width: f32, live: bool, view: ReasoningView) -> f32 {
        let boxed = self.boxed();
        let summary = self.message.compaction && !boxed;
        let wrap = if boxed {
            width - 2.0 * BOX_PAD.0
        } else if summary {
            width - 14.0
        } else {
            width
        };
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
        if self.message.media.is_some() && !boxed {
            return media_size(&self.message, width, p.scale).1 + META_H;
        }
        if boxed {
            let chips = attachment_layout(p, &self.message.attachments, wrap).1;
            let text = if self.message.content.is_empty() { 0.0 } else { doc_h };
            return text + chips + 2.0 * BOX_PAD.1 - if text == 0.0 && chips > 0.0 { 6.0 } else { 0.0 };
        }
        if summary {
            let open = !live && self.reasoning_open == Some(true);
            return REASONING_ROW + if open { doc_h + 12.0 } else { 0.0 };
        }

        let mut height = 0.0;
        let thinking = self.thinking(live);
        if self.reasoning_row(live, view) {
            height += REASONING_ROW;
            if self.reasoning_shown(view) {
                let rwrap = width - 14.0;
                if self.reasoning_doc.as_ref().is_none_or(|d| !d.fits(rwrap, p.scale)) || self.reasoning_len != self.message.reasoning.len() {
                    let previous = self.reasoning_doc.take();
                    self.reasoning_doc = Some(Doc::markdown(p.fonts, &self.message.reasoning, rwrap, p.scale, INK_MUTED, previous));
                    self.reasoning_len = self.message.reasoning.len();
                }
                height += self.reasoning_doc.as_ref().map_or(0.0, |d| d.height) + 12.0;
            }
        } else if thinking {
            // The bare "thinking" dots.
            height += 18.0;
        }
        if !thinking {
            height += doc_h;
        }
        self.tool_bodies.resize_with(self.message.tool_calls.len(), || None);
        for index in 0..self.message.tool_calls.len() {
            height += 8.0 + self.tool_height(p, index, width);
        }
        if live {
            for call in &mut self.streaming_calls {
                height += 8.0 + call.measure(p, width);
            }
        }
        if !live {
            height += META_H;
        } else if !self.message.tool_calls.is_empty() || !self.streaming_calls.is_empty() {
            height += 6.0;
        }
        height
    }

    /// The text a tool card's body shows, if any: the preview while waiting
    /// for approval, the output once expanded.
    fn tool_body(&self, index: usize) -> Option<String> {
        let record = &self.message.tool_calls[index];
        if shows_plan(record) {
            return tools::plan_text(&record.call);
        }
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

    /// Whether tool card `index` shows its file change as a diff: while it
    /// waits for approval, and once done when opened.
    fn shows_diff(&self, index: usize) -> bool {
        let record = &self.message.tool_calls[index];
        tools::changes_file(&record.call.name)
            && match record.status {
                ToolStatus::Pending => tools::needs_approval(&record.call.name),
                ToolStatus::Done => self.open_tools.contains(&index),
                ToolStatus::Running | ToolStatus::Failed | ToolStatus::Denied => false,
            }
    }

    /// The diff of tool card `index` if it changes a file (and the change is
    /// pending or made). Until the file's own arrives from a worker, it is
    /// made from the call's text.
    fn diff_view(&mut self, index: usize) -> Option<&mut DiffView> {
        let record = &self.message.tool_calls[index];
        if !tools::changes_file(&record.call.name) || !matches!(record.status, ToolStatus::Pending | ToolStatus::Done) {
            return None;
        }
        let id = &record.call.call_id;
        if !self.diffs.contains_key(id) {
            let (before, after, whole) = tools::snippet_change(&record.call)?;
            self.diffs.insert(id.clone(), DiffView::new(Arc::new(diff::diff(&before, &after, whole)), false, false));
        }
        self.diffs.get_mut(id)
    }

    /// Size of tool card `index`'s screenshot, shown under its output while
    /// the card is open, in a card `width` wide.
    fn shot_size(&self, index: usize, width: f32) -> Option<(f32, f32)> {
        let image = self.message.tool_calls[index].image.as_ref().filter(|_| self.open_tools.contains(&index))?;
        let (w, h) = image.dimensions.map_or((16.0, 10.0), |(w, h)| (w as f32, h as f32));
        let height = ((width - 24.0).max(1.0) * h / w).min(SHOT_MAX);
        Some((height * w / h, height))
    }

    /// Height of tool card `index`, refreshing its body layout.
    fn tool_height(&mut self, p: &Painter, index: usize, width: f32) -> f32 {
        let shows_diff = self.shows_diff(index);
        let record = &self.message.tool_calls[index];
        let status = record.status;
        let approval = status == ToolStatus::Pending && tools::needs_approval(&record.call.name);
        // The header shows a change's size even while its diff is closed.
        if let Some(view) = self.diff_view(index)
            && shows_diff
        {
            view.layout(p);
            let height = TOOL_ROW + view.height() + if approval { APPROVAL_ROW } else { 0.0 };
            self.tool_bodies[index] = None;
            return height;
        }
        let body = self.tool_body(index);
        let screenshot = self.shot_size(index, width);
        let slot = &mut self.tool_bodies[index];
        let mut height = TOOL_ROW + screenshot.map_or(0.0, |(_, h)| h + 12.0);
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

impl StreamingCall {
    /// Refreshes the card for the arguments so far and returns its height.
    /// The body shows the end of what is being written.
    fn measure(&mut self, p: &Painter, width: f32) -> f32 {
        if self.shown.as_ref().is_none_or(|(len, ..)| *len != self.arguments.len()) {
            let view = tools::view_partial(&self.name, &self.arguments);
            let body = view.preview.as_deref().filter(|text| !text.is_empty()).map(|text| {
                let lines: Vec<&str> = text.lines().collect();
                let tail = lines[lines.len().saturating_sub(STREAM_LINES)..].join("\n");
                TextLayout::new(p.fonts, &tail, TOOL_STYLE, Some(width - 24.0), p.scale)
            });
            self.shown = Some((self.arguments.len(), view, body));
        }
        let body = self.shown.as_ref().and_then(|(_, _, body)| body.as_ref());
        TOOL_ROW + body.map_or(0.0, |b| b.height().min(TOOL_BODY_MAX) + 14.0)
    }
}

/// Where each of a prompt's attachments goes, relative to the attachment
/// area: images with thumbnails as tiles first, then the other files as
/// chips, each group wrapping to `width`. Also returns the total height.
fn attachment_layout(p: &Painter, attachments: &[serechat::Attachment], width: f32) -> (Vec<Rect>, f32) {
    let mut rects = vec![Rect::default(); attachments.len()];
    let (mut x, mut y, mut row_h) = (0.0, 0.0, 0.0f32);
    for tiles in [true, false] {
        for (index, attachment) in attachments.iter().enumerate() {
            if image::supported(&attachment.mime) != tiles {
                continue;
            }
            let (item_w, item_h) = if tiles { IMAGE_TILE } else { (p.layout(&attachment.name, theme::SMALL, None).width().min(170.0) + 60.0, CHIPS_ROW) };
            if x > 0.0 && x + item_w > width {
                (x, y, row_h) = (0.0, y + row_h, 0.0);
            }
            rects[index] = Rect::new(x, y, item_w, item_h);
            x += item_w + 6.0;
            row_h = row_h.max(item_h + if tiles { 6.0 } else { 0.0 });
        }
        // Chips start on a row of their own.
        if x > 0.0 {
            (x, y, row_h) = (0.0, y + row_h, 0.0);
        }
    }
    // Tile rows carry a gap below them; the last one doesn't need it.
    let only_tiles = !attachments.is_empty() && attachments.iter().all(|a| image::supported(&a.mime));
    (rects, if only_tiles { y - 6.0 } else { y })
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
        let reasoning_view = self.reasoning_view;
        let (models, media_models) = (&self.models, &self.media_models);
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
        let live_entry = conversation.stream.as_ref().map(|s| s.entry);
        let started = conversation.stream.as_ref().map(|s| s.started);
        // Under the messages: a retry in progress, or the Continue button.
        let retry = conversation.retry.as_ref().map(|r| format!("Retrying ({} of {MAX_RETRIES}): {}", r.attempt, r.error));
        let resumable = conversation.resumable();
        let busy = conversation.busy();
        // What the latest run of tool rounds has done and cost.
        let run = match conversation.run_stats() {
            (0, _) => None,
            (1, cost) => Some(format!("1 tool round  ·  {}", format_cost(cost))),
            (steps, cost) => Some(format!("{steps} tool rounds  ·  {}", format_cost(cost))),
        };
        let footer_h = if retry.is_some() || resumable || run.is_some() { FOOTER_H } else { 0.0 };

        // Measure everything (layouts are cached) to know the scroll range,
        // and where each entry starts.
        let mut tops = Vec::with_capacity(conversation.entries.len());
        let mut top = 0.0;
        for entry in &mut conversation.entries {
            tops.push(top);
            top += entry.measure(p, width, live_entry == Some(entry.id), reasoning_view) + MESSAGE_GAP;
        }
        let content_h = 24.0 + footer_h + top;
        let max_scroll = (content_h - view.h).max(0.0);
        if let Some(find) = &mut self.find {
            let first = tops.iter().position(|t| t + 24.0 >= self.scroll).unwrap_or(0);
            find.update(conversation, reasoning_view, first);
        }

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
        // Scrollbar: drag the thumb, or press the track to jump there.
        let thumb_h = (view.h * view.h / content_h).max(32.0);
        let travel = (view.h - thumb_h).max(1.0);
        let track = Rect::new(view.right() - 12.0, view.y, 12.0, view.h);
        if max_scroll > 0.0 && ui.pressed && ui.hovered(track) {
            let thumb_y = view.y + travel * (self.scroll / max_scroll);
            if !(thumb_y..thumb_y + thumb_h).contains(&ui.mouse.1) {
                self.scroll = ((ui.mouse.1 - view.y - thumb_h * 0.5) / travel * max_scroll).clamp(0.0, max_scroll);
            }
            self.bar_drag = Some(self.scroll);
        }
        if let Some(start) = self.bar_drag {
            if !ui.down {
                self.bar_drag = None;
            } else if ui.mouse.1 > f32::MIN {
                // (`f32::MIN` means the pointer left the window: hold still.)
                self.scroll =(start + (ui.mouse.1 - ui.press_pos.1) * max_scroll / travel).clamp(0.0, max_scroll);
                self.scroll_target = self.scroll;
                self.stick_to_bottom = self.scroll >= max_scroll - 1.0;
            }
        }
        // Jump near the find match being looked at; once drawn, it is
        // centred exactly (below).
        let reveal = self.find.as_ref().filter(|f| f.reveal).and_then(|f| f.current().cloned());
        if let Some(m) = &reveal {
            let entry = &conversation.entries[m.entry];
            let doc = if m.doc == 0 { entry.reasoning_doc.as_ref() } else { entry.doc.as_ref() };
            let inside = doc.and_then(|d| d.position((m.piece, m.range.start))).map_or(0.0, |(y, _)| y);
            self.scroll = (tops[m.entry] + inside + 24.0 - view.h * 0.35).clamp(0.0, max_scroll);
            self.scroll_target = self.scroll;
            self.stick_to_bottom = false;
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
            let live = live_entry == Some(entry.id);
            let height = entry.measure(p, width, live, reasoning_view);
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
                if let Some(doc) = entry.doc.as_mut().filter(|_| !entry.message.content.is_empty()) {
                    if let Some(find) = &self.find {
                        find.highlight(p, index, 1, doc, origin);
                    }
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
            if entry.message.media.is_some() {
                // A generation: the file it made, or a placeholder while it waits.
                let model = entry.message.model.as_deref().map_or("", |m| label_of(models, media_models, m));
                let (media_w, media_h) = media_size(&entry.message, width, p.scale);
                if let Some(path) = draw_media(p, ui, entry, Rect::new(x, top, media_w, media_h), model, in_view) {
                    effects.open_path = Some(path);
                }
                if !entry.message.media_pending() {
                    let caption = p.layout(&usage_caption(model, entry.message.usage, entry.message.cost), theme::TINY, None);
                    p.text(&caption, x, top + media_h + 6.0 + (24.0 - caption.height()) * 0.5, t.text_faint);
                }
                continue;
            }
            if entry.message.compaction {
                // A summary replaced the messages above for the model; it
                // opens like a reasoning block.
                let open = !live && entry.reasoning_open == Some(true);
                let label = if live { "Summarising the conversation to free up context" } else { "Summarised the messages above to free up context" };
                let row = Toggle { label, expandable: !live, open, pulse: live, key: id(("summary", entry.id)) };
                if toggle_row(p, ui, (x, top), &row, in_view) {
                    entry.reasoning_open = Some(!open);
                    ui.animating = true;
                }
                if let Some(doc) = entry.doc.as_mut().filter(|_| open) {
                    let origin = (x + 14.0, top + REASONING_ROW);
                    p.rect(Rect::new(x, origin.1, 2.0, doc.height), t.border_strong, 1.0);
                    if let Some(find) = &self.find {
                        find.highlight(p, index, 1, doc, origin);
                    }
                    if let Some((a, b)) = sel(1, doc) {
                        doc.draw_selection(p, origin, a, b);
                    }
                    let event = doc.draw(p, ui, origin, t.text_muted, in_view, None);
                    effects.link = effects.link.take().or(event.open_link);
                    targets.push(Target { entry: index, doc: 1, origin, rect: Rect::new(origin.0, origin.1, width - 14.0, doc.height) });
                }
                continue;
            }
            let thinking = entry.thinking(live);
            if entry.reasoning_row(live, reasoning_view) {
                // "Thinking for 4s" while it thinks, "Thought for 12s" after.
                let open = entry.reasoning_shown(reasoning_view);
                let ms = if thinking { started.map_or(0, |s| u64::try_from(s.elapsed().as_millis()).unwrap_or(u64::MAX)) } else { entry.message.reasoning_ms };
                let label = reasoning_label(thinking, ms);
                let row = Toggle { label: &label, expandable: !entry.message.reasoning.is_empty(), open, pulse: thinking, key: id(("reasoning", entry.id)) };
                if toggle_row(p, ui, (x, top), &row, in_view) {
                    entry.reasoning_open = Some(!open);
                    ui.animating = true;
                }
                top += REASONING_ROW;
                if let Some(doc) = entry.reasoning_doc.as_mut().filter(|_| open) {
                    let origin = (x + 14.0, top);
                    p.rect(Rect::new(x, top, 2.0, doc.height), t.border_strong, 1.0);
                    if let Some(find) = &self.find {
                        find.highlight(p, index, 0, doc, origin);
                    }
                    if let Some((a, b)) = sel(0, doc) {
                        doc.draw_selection(p, origin, a, b);
                    }
                    let event = doc.draw(p, ui, origin, t.text_muted, in_view, None);
                    effects.link = effects.link.take().or(event.open_link);
                    targets.push(Target { entry: index, doc: 0, origin, rect: Rect::new(origin.0, origin.1, width - 14.0, doc.height) });
                    top += doc.height + 12.0;
                }
            }

            if thinking {
                if reasoning_view != ReasoningView::Hidden {
                    continue;
                }
                // Bare "thinking" dots when reasoning is hidden.
                for dot in 0..3 {
                    let phase = (ui.time * 5.0 - dot as f32 * 0.7).sin() * 0.5 + 0.5;
                    let dot_rect = Rect::new(x + dot as f32 * 11.0, top + 9.0 - phase * 3.0, 6.0, 6.0);
                    p.rect(dot_rect, fade(t.text_muted, 0.3 + 0.7 * phase), 3.0);
                }
                ui.animating = true;
                continue;
            }
            if let Some(doc) = &mut entry.doc {
                let origin = (x, top);
                if let Some(find) = &self.find {
                    find.highlight(p, index, 1, doc, origin);
                }
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
                for call in &mut entry.streaming_calls {
                    top += 8.0;
                    let card_h = call.measure(p, width);
                    draw_streaming_card(p, ui, call, Rect::new(x, top, width, card_h));
                    top += card_h;
                }
            }

            if live {
                continue;
            }
            // Caption and hover actions under a finished reply.
            let meta_y = top + 6.0;
            if let Some(model) = &entry.message.model {
                let text = usage_caption(label_of(models, media_models, model), entry.message.usage, entry.message.cost);
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
        if let Some(status) = &retry {
            // A slow pulse: the run is waiting, not stuck.
            let pulse = (ui.time * 2.6).sin() * 0.5 + 0.5;
            p.rect(Rect::new(x, y + 12.0, 6.0, 6.0), fade(t.accent, 0.4 + 0.6 * pulse), 3.0);
            let mut text = p.layout(status, theme::SMALL, None);
            text.truncate(p.fonts, width - 16.0);
            p.text(&text, x + 16.0, y + 15.0 - text.height() * 0.5, t.text_muted);
            ui.animating = true;
        } else if resumable {
            let enabled = in_view || !ui.hovered(Rect::new(x, y, 96.0, 30.0));
            if button(p, ui, Rect::new(x, y, 96.0, 30.0), "Continue", ButtonStyle::Primary, enabled) {
                effects.resume = true;
            }
            let hint = match &run {
                Some(stats) => format!("The run stopped before it finished  ·  {stats}"),
                None => "The run stopped before it finished.".to_owned(),
            };
            let mut hint = p.layout(&hint, theme::SMALL, None);
            hint.truncate(p.fonts, width - 110.0);
            p.text(&hint, x + 110.0, y + (30.0 - hint.height()) * 0.5, t.text_faint);
        } else if let Some(stats) = &run {
            let text = if live_entry.is_some() || busy { format!("Working  ·  {stats} so far") } else { format!("Done  ·  {stats}") };
            let text = p.layout(&text, theme::SMALL, None);
            p.text(&text, x, y + (30.0 - text.height()) * 0.5, t.text_faint);
        }
        p.set_clip(clip);

        // Centre the find match being looked at, now that its place is known.
        if let (Some(m), Some(find)) = (&reveal, &mut self.find) {
            let entry = &conversation.entries[m.entry];
            let doc = if m.doc == 0 { entry.reasoning_doc.as_ref() } else { entry.doc.as_ref() };
            let target = targets.iter().find(|t| t.entry == m.entry && t.doc == m.doc);
            if let (Some(target), Some((y, line_h))) = (target, doc.and_then(|d| d.position((m.piece, m.range.start)))) {
                let wanted = (self.scroll + target.origin.1 + y + line_h * 0.5 - (view.y + view.h * 0.4)).clamp(0.0, max_scroll);
                self.scroll = wanted;
                self.scroll_target = wanted;
                ui.animating = true;
            }
            find.reveal = false;
        }

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
        if ui.pressed && in_view && ui.cursor != CursorIcon::Pointer && self.bar_drag.is_none() {
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
        if effects.resume {
            self.resume(current, actions);
        }

        if max_scroll > 0.0 {
            let thumb_y = view.y + travel * (self.scroll / max_scroll);
            let active = self.bar_drag.is_some() || ui.hovered(track);
            if self.bar_drag.is_some() {
                ui.cursor = CursorIcon::Default;
            }
            let hover = ui.anim(id("scrollbar"), f32::from(u8::from(active)));
            p.rect(Rect::new(view.right() - 9.0, thumb_y, 6.0, thumb_h), fade(t.text, 0.1 + 0.1 * hover), 3.0);
        }
    }
}

/// The reasoning header: `Thinking` (then `Thinking for 4s`) while a reply
/// thinks, `Thought for 12s` once it answered, or `Reasoning` when the time
/// is unknown (replies saved by older versions).
fn reasoning_label(thinking: bool, ms: u64) -> String {
    let secs = (ms + 500) / 1000;
    let time = if secs < 60 { format!("{}s", secs.max(1)) } else { format!("{}m {}s", secs / 60, secs % 60) };
    match (thinking, ms) {
        (true, 0..1000) => "Thinking".to_owned(),
        (true, _) => format!("Thinking for {time}"),
        (false, 0) => "Reasoning".to_owned(),
        (false, _) => format!("Thought for {time}"),
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
    /// The Continue button was clicked.
    resume: bool,
}

/// A row that opens and closes a block, like "Thought for 12s".
struct Toggle<'a> {
    label: &'a str,
    /// Has something to open; otherwise a dot replaces the chevron.
    expandable: bool,
    open: bool,
    /// Breathes while waiting for the model.
    pulse: bool,
    /// Animation key.
    key: u64,
}

/// Draws `row` at `origin`; returns whether it was clicked.
fn toggle_row(p: &mut Painter, ui: &mut Ui, origin: (f32, f32), row: &Toggle<'_>, interactive: bool) -> bool {
    let t = p.theme;
    let label = p.layout(row.label, theme::SMALL, None);
    let toggle = Rect::new(origin.0 - 6.0, origin.1, label.width() + 34.0, 26.0);
    let hovered = interactive && row.expandable && ui.hovered(toggle);
    let hover = ui.anim(row.key, f32::from(u8::from(hovered)));
    p.rect(toggle, fade(t.hover, hover), theme::RADIUS_SM);
    let color = if row.pulse {
        ui.animating = true;
        mix(t.text_faint, t.text, ((ui.time * 2.6).sin() * 0.5 + 0.5) * 0.75)
    } else {
        mix(t.text_muted, t.text, hover)
    };
    if row.expandable {
        let (cx, cy) = if row.open { (toggle.x + 8.0, toggle.y + 11.0) } else { (toggle.x + 10.0, toggle.y + 9.5) };
        chevron(p, cx, cy, row.open, color);
    } else {
        p.rect(Rect::new(toggle.x + 9.0, toggle.y + 10.0, 6.0, 6.0), color, 3.0);
    }
    p.text(&label, toggle.x + 24.0, toggle.y + (26.0 - label.height()) * 0.5, color);
    if hovered {
        ui.cursor = CursorIcon::Pointer;
        return ui.clicked(toggle);
    }
    false
}

/// An `update_plan` call whose checklist is always shown.
fn shows_plan(record: &serechat::ToolRecord) -> bool {
    record.call.name == "update_plan" && record.status != ToolStatus::Failed
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

/// Draws a prompt's image tiles and file chips; returns a file the user clicked.
fn draw_attachment_chips(p: &mut Painter, ui: &mut Ui, attachments: &[serechat::Attachment], origin: (f32, f32), width: f32, interactive: bool) -> Option<String> {
    let t = p.theme;
    let mut clicked = None;
    let (rects, _) = attachment_layout(p, attachments, width);
    for (attachment, rect) in attachments.iter().zip(rects) {
        let tile = image::supported(&attachment.mime);
        let rect = if tile {
            Rect::new(origin.0 + rect.x, origin.1 + rect.y, rect.w, rect.h)
        } else {
            Rect::new(origin.0 + rect.x, origin.1 + rect.y + 2.0, rect.w, CHIPS_ROW - 8.0)
        };
        let hovered = interactive && ui.hovered(rect);
        if tile {
            match p.image(&attachment.path, rect, theme::RADIUS) {
                Lookup::Ready(_) => {}
                // A quiet placeholder; loading takes a moment at most.
                Lookup::Loading => p.rect(rect, t.hover, theme::RADIUS),
                Lookup::Failed => {
                    p.rect(rect, t.hover, theme::RADIUS);
                    let mut name = p.layout(&attachment.name, theme::TINY, None);
                    name.truncate(p.fonts, rect.w - 16.0);
                    file_badge(p, &attachment.mime, &attachment.name, Rect::new(rect.x + (rect.w - 34.0) * 0.5, rect.y + rect.h * 0.5 - 22.0, 34.0, 20.0));
                    p.text(&name, rect.x + (rect.w - name.width()) * 0.5, rect.y + rect.h * 0.5 + 6.0, t.text_muted);
                }
            }
            // Hairline frame, brighter on hover.
            p.bordered(rect, [0.0; 4], theme::RADIUS, 1.0, if hovered { t.border_focus } else { t.border });
        } else {
            let mut name = p.layout(&attachment.name, theme::SMALL, None);
            name.truncate(p.fonts, 170.0);
            p.bordered(rect, if hovered { t.hover } else { t.bg }, theme::RADIUS_SM, 1.0, t.border_strong);
            file_icon(p, attachment, Rect::new(rect.x + 4.0, rect.y + 4.0, 34.0, rect.h - 8.0));
            p.text(&name, rect.x + 46.0, rect.y + (rect.h - name.height()) * 0.5, t.text);
        }
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(rect) {
                clicked = Some(attachment.path.clone());
            }
        }
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
        ToolStatus::Pending | ToolStatus::Running => spinner(p, ui, icon),
        ToolStatus::Done => p.label_centered("✓", Style::semibold(12.5), icon, t.syntax[1]),
        ToolStatus::Failed => p.label_centered("×", Style::semibold(14.0), icon, t.danger),
        ToolStatus::Denied => p.rect(Rect::new(icon.x + 4.0, icon.y + 7.5, 8.0, 1.5), t.text_faint, 0.5),
    }
    let verb = p.layout(view.verb, theme::LABEL, None);
    p.text(&verb, header.x + 34.0, header.y + (TOOL_ROW - verb.height()) * 0.5, t.text);
    let expandable = status.is_finished() && !record.output.is_empty() && !shows_plan(record);
    let mut right = if approval {
        let waiting = p.layout("Needs approval", theme::TINY, None);
        p.text(&waiting, header.right() - 12.0 - waiting.width(), header.y + (TOOL_ROW - waiting.height()) * 0.5, t.accent);
        waiting.width() + 24.0
    } else if expandable {
        chevron(p, header.right() - 22.0, header.y + TOOL_ROW * 0.5 - 3.0, entry.open_tools.contains(&index), t.text_faint);
        34.0
    } else {
        12.0
    };
    // A file change's size: `+12 −3`.
    if let Some(change) = entry.diffs.get(&record.call.call_id).filter(|_| matches!(status, ToolStatus::Pending | ToolStatus::Done)) {
        let (added, removed) = (change.diff.added, change.diff.removed);
        for (count, sign, color) in [(removed, '−', t.removed), (added, '+', t.added)] {
            if count > 0 || (added == 0 && removed == 0 && sign == '+') {
                let label = p.layout(&format!("{sign}{count}"), Style::mono(11.5), None);
                right += label.width() + 6.0;
                p.text(&label, header.right() - right, header.y + (TOOL_ROW - label.height()) * 0.5, color);
            }
        }
        right += 6.0;
    }
    let mut target = p.layout(&view.target.replace('\n', " "), TOOL_STYLE, None);
    target.truncate(p.fonts, (rect.w - 46.0 - verb.width() - right).max(20.0));
    p.text(&target, header.x + 42.0 + verb.width(), header.y + (TOOL_ROW - target.height()) * 0.5, t.text_muted);
    if expandable && interactive && ui.hovered(header) {
        ui.cursor = CursorIcon::Pointer;
        if ui.clicked(header) && !entry.open_tools.remove(&index) {
            entry.open_tools.insert(index);
        }
    }

    // Body: the change as a diff, the preview awaiting approval, or the expanded output.
    let mut y = header.bottom();
    if let Some(change) = entry.diffs.get(&record.call.call_id).filter(|_| entry.shows_diff(index)) {
        p.rect(Rect::new(rect.x, y, rect.w, 1.0), t.border, 0.0);
        let body = Rect::new(rect.x, y, rect.w, change.height());
        if draw_diff(p, ui, change, body, interactive)
            && let Some(change) = entry.diffs.get_mut(&record.call.call_id)
        {
            change.full = !change.full;
            ui.animating = true;
        }
        y = body.bottom();
    } else if let Some((_, layout)) = entry.tool_bodies.get(index).and_then(Option::as_ref) {
        p.rect(Rect::new(rect.x, y, rect.w, 1.0), t.border, 0.0);
        let body_h = layout.height().min(TOOL_BODY_MAX);
        let clip = p.push_clip(Rect::new(rect.x, y + 7.0, rect.w, body_h));
        let color = if status == ToolStatus::Failed { t.danger } else { t.text_muted };
        p.text(layout, rect.x + 12.0, y + 7.0, color);
        p.set_clip(clip);
        y += body_h + 14.0;
    }
    if let (Some((w, h)), Some(image)) = (entry.shot_size(index, rect.w), &record.image) {
        let shot = Rect::new(rect.x + 12.0, y, w, h);
        if !matches!(p.image(&image.path, shot, theme::RADIUS_SM), Lookup::Ready(_)) {
            p.rect(shot, t.hover, theme::RADIUS_SM);
        }
        p.bordered(shot, [0.0; 4], theme::RADIUS_SM, 1.0, t.border);
        y += h + 12.0;
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

/// Draws a diff into `rect` (its rows laid out by [`DiffView::layout`]):
/// tinted rows with a coloured edge for added and removed lines, line
/// numbers, and the changed part of edited lines picked out. Returns whether
/// "Show all" (or "Show fewer") was clicked.
fn draw_diff(p: &mut Painter, ui: &mut Ui, view: &DiffView, rect: Rect, interactive: bool) -> bool {
    let t = p.theme;
    let Some((_, rows)) = &view.rows else { return false };
    let gutter = rows.gutter;
    let sign_x = rect.x + gutter * 2.0 + 10.0;
    let text_x = sign_x + 16.0;
    let clip = p.push_clip(Rect::new(rect.x, rect.y, rect.w, rect.h - if view.footer() { DIFF_FOOTER } else { 0.0 }));
    let mut y = rect.y + 6.0;
    for (index, row) in rows.rows.iter().enumerate() {
        let line = view.diff.lines.get(index);
        let kind = line.map_or(Kind::Skipped, |l| l.kind);
        // Inside the card's hairline border.
        let band = Rect::new(rect.x + 1.0, y, rect.w - 2.0, DIFF_ROW);
        let middle = |layout: &TextLayout| y + (DIFF_ROW - layout.height()) * 0.5;
        let (ink, text_color) = match kind {
            Kind::Added => (Some(t.added), t.text),
            Kind::Removed => (Some(t.removed), t.text),
            Kind::Same => (None, t.text_muted),
            Kind::Skipped => (None, t.text_faint),
        };
        match ink {
            Some(color) => {
                p.rect(band, fade(color, 0.1), 0.0);
                p.rect(Rect::new(band.x, y, 2.0, DIFF_ROW), color, 0.0);
            }
            None if kind == Kind::Skipped => p.rect(band, fade(t.text, 0.03), 0.0),
            None => {}
        }
        // Line numbers, right-aligned in their columns.
        for (number, column) in [(&row.old, 0.0), (&row.new, 1.0)] {
            if let Some(number) = number {
                p.text(number, rect.x + gutter * (column + 1.0) - 6.0 - number.width(), middle(number), t.text_faint);
            }
        }
        if let Some(color) = ink {
            let sign = if kind == Kind::Added { "+" } else { "−" };
            p.label(sign, TOOL_STYLE, sign_x, y + 1.0, color);
            if let Some((start, end)) = line.and_then(|l| l.changed) {
                let (from, to) = (row.text.caret(start).0, row.text.caret(end).0);
                p.rect(Rect::new(text_x + from, y + 1.0, (to - from).max(2.0), DIFF_ROW - 2.0), fade(color, 0.3), 2.0);
            }
        }
        p.text(&row.text, text_x, middle(&row.text), text_color);
        y += DIFF_ROW;
    }
    p.set_clip(clip);
    if !view.footer() {
        return false;
    }
    let footer = Rect::new(rect.x, rect.bottom() - DIFF_FOOTER, rect.w, DIFF_FOOTER);
    p.rect(Rect::new(footer.x, footer.y, footer.w, 1.0), t.border, 0.0);
    let total = view.diff.lines.len();
    let label = if view.full {
        "Show fewer lines".to_owned()
    } else {
        format!("Show all {total} lines")
    };
    let hovered = interactive && ui.hovered(footer);
    let label = p.layout(&label, theme::TINY, None);
    p.text(&label, text_x, footer.y + (DIFF_FOOTER - label.height()) * 0.5, if hovered { t.text } else { t.text_muted });
    if view.full && total > DIFF_MAX_ROWS {
        let more = p.layout(&format!("{} more not shown", total - DIFF_MAX_ROWS), theme::TINY, None);
        p.text(&more, footer.right() - 12.0 - more.width(), footer.y + (DIFF_FOOTER - more.height()) * 0.5, t.text_faint);
    }
    if hovered {
        ui.cursor = CursorIcon::Pointer;
        return ui.clicked(footer);
    }
    false
}

/// Size of a generation's block at column `width`: a generated image at its
/// own aspect ratio (within the thumbnail limit at `scale`), a square tile
/// while one is made, or a file card.
fn media_size(message: &StoredMessage, width: f32, scale: f32) -> (f32, f32) {
    let card = (width.min(MEDIA_CARD_W), MEDIA_CARD_H);
    let Some(job) = &message.media else { return card };
    if message.media_pending() {
        return if job.kind == MediaKind::Image { (IMAGE_PENDING.min(width), IMAGE_PENDING) } else { card };
    }
    match message.attachments.first() {
        Some(file) if image::supported(&file.mime) => {
            let limit = image::MAX_THUMB as f32 / scale.max(1.0);
            let (w, h) = file.dimensions.map_or((1.0, 1.0), |(w, h)| (w as f32, h as f32));
            let (max_w, max_h) = (width.min(IMAGE_MAX).min(limit), IMAGE_MAX.min(limit));
            // Fit inside the box; small images keep their pixel size.
            let fit = (max_w / w).min(max_h / h).min(if file.dimensions.is_some() { 1.0 } else { f32::MAX });
            ((w * fit).floor().max(16.0), (h * fit).floor().max(16.0))
        }
        _ => card,
    }
}

/// Draws a generation into `rect`: the waiting placeholder, the image, or a
/// card for other files. Returns the file to open when clicked.
fn draw_media(p: &mut Painter, ui: &mut Ui, entry: &Entry, rect: Rect, model: &str, interactive: bool) -> Option<String> {
    let t = p.theme;
    let message = &entry.message;
    let job = message.media.as_ref()?;
    if message.media_pending() {
        let secs = entry.media_since.map_or(0, |since| since.elapsed().as_secs());
        let what = format!("Generating {}", job.kind.noun());
        let detail = if model.is_empty() { clock(secs) } else { format!("{model}  ·  {}", clock(secs)) };
        if job.kind == MediaKind::Image {
            // A tile that breathes while the picture is made.
            let pulse = (ui.time * 1.8).sin() * 0.5 + 0.5;
            p.rect(rect, mix(t.surface, t.hover, 0.3 + 0.5 * pulse), theme::RADIUS);
            p.bordered(rect, [0.0; 4], theme::RADIUS, 1.0, t.border);
            spinner(p, ui, Rect::new(rect.x + rect.w * 0.5 - 8.0, rect.y + rect.h * 0.5 - 24.0, 16.0, 16.0));
            let label = p.layout(&what, theme::LABEL, None);
            p.text_aligned(&label, rect.x, rect.y + rect.h * 0.5 + 2.0, Align::Center, rect.w, t.text_muted);
            let mut detail = p.layout(&detail, theme::TINY, None);
            detail.truncate(p.fonts, rect.w - 24.0);
            p.text_aligned(&detail, rect.x, rect.y + rect.h * 0.5 + 24.0, Align::Center, rect.w, t.text_faint);
        } else {
            p.bordered(rect, t.surface, theme::RADIUS, 1.0, t.border);
            spinner(p, ui, Rect::new(rect.x + 24.0, rect.y + (rect.h - 16.0) * 0.5, 16.0, 16.0));
            p.label(&what, theme::LABEL, rect.x + 64.0, rect.y + 13.0, t.text);
            let mut detail = p.layout(&detail, theme::TINY, None);
            detail.truncate(p.fonts, rect.w - 80.0);
            p.text(&detail, rect.x + 64.0, rect.y + 33.0, t.text_faint);
        }
        ui.animating = true;
        return None;
    }
    let file = message.attachments.first()?;
    let hovered = interactive && ui.hovered(rect);
    if image::supported(&file.mime) {
        match p.image(&file.path, rect, theme::RADIUS) {
            Lookup::Ready(_) => {}
            Lookup::Loading => p.rect(rect, t.hover, theme::RADIUS),
            Lookup::Failed => {
                p.rect(rect, t.hover, theme::RADIUS);
                file_badge(p, &file.mime, &file.name, Rect::new(rect.x + (rect.w - 34.0) * 0.5, rect.y + (rect.h - 20.0) * 0.5, 34.0, 20.0));
            }
        }
        p.bordered(rect, [0.0; 4], theme::RADIUS, 1.0, if hovered { t.border_focus } else { t.border });
    } else {
        p.bordered(rect, if hovered { t.hover } else { t.surface }, theme::RADIUS, 1.0, if hovered { t.border_strong } else { t.border });
        file_badge(p, &file.mime, &file.name, Rect::new(rect.x + 12.0, rect.y + (rect.h - 30.0) * 0.5, 40.0, 30.0));
        let open = p.layout("Open", theme::SMALL, None);
        let mut name = p.layout(&file.name, theme::LABEL, None);
        name.truncate(p.fonts, rect.w - 96.0 - open.width());
        p.text(&name, rect.x + 64.0, rect.y + 13.0, t.text);
        let detail = format!("{}  ·  {}", capitalized(job.kind.noun()), human_size(file.size));
        p.label(&detail, theme::TINY, rect.x + 64.0, rect.y + 33.0, t.text_faint);
        p.text(&open, rect.right() - 16.0 - open.width(), rect.y + (rect.h - open.height()) * 0.5, if hovered { t.text } else { t.text_muted });
    }
    if hovered {
        ui.cursor = CursorIcon::Pointer;
        if ui.clicked(rect) {
            return Some(file.path.clone());
        }
    }
    None
}

/// A wait in seconds as `12s` or `3m 05s`.
fn clock(secs: u64) -> String {
    if secs < 60 { format!("{secs}s") } else { format!("{}m {:02}s", secs / 60, secs % 60) }
}

/// A small spinner in the 16×16 `icon`: eight dots fading around a circle.
fn spinner(p: &mut Painter, ui: &mut Ui, icon: Rect) {
    let t = p.theme;
    for i in 0..8 {
        let a = i as f32 / 8.0 * std::f32::consts::TAU;
        let phase = (1.0 - (ui.time * 1.4 - i as f32 / 8.0).rem_euclid(1.0)).powi(2);
        let (cx, cy) = (icon.x + 8.0 + a.cos() * 5.5, icon.y + 8.0 + a.sin() * 5.5);
        p.rect(Rect::new(cx - 1.25, cy - 1.25, 2.5, 2.5), fade(t.text_muted, 0.25 + 0.75 * phase), 1.25);
    }
    ui.animating = true;
}

/// A tool call the model is still writing: what it will do, and the end of
/// its content so far.
fn draw_streaming_card(p: &mut Painter, ui: &mut Ui, call: &StreamingCall, rect: Rect) {
    let t = p.theme;
    let Some((_, view, body)) = &call.shown else { return };
    p.bordered(rect, t.code_bg, theme::RADIUS, 1.0, t.border);
    spinner(p, ui, Rect::new(rect.x + 10.0, rect.y + (TOOL_ROW - 16.0) * 0.5, 16.0, 16.0));
    let verb = p.layout(view.verb, theme::LABEL, None);
    p.text(&verb, rect.x + 34.0, rect.y + (TOOL_ROW - verb.height()) * 0.5, t.text);
    let mut target = p.layout(&view.target.replace('\n', " "), TOOL_STYLE, None);
    target.truncate(p.fonts, (rect.w - 58.0 - verb.width()).max(20.0));
    p.text(&target, rect.x + 42.0 + verb.width(), rect.y + (TOOL_ROW - target.height()) * 0.5, t.text_muted);
    if let Some(body) = body {
        let y = rect.y + TOOL_ROW;
        p.rect(Rect::new(rect.x, y, rect.w, 1.0), t.border, 0.0);
        let body_h = body.height().min(TOOL_BODY_MAX);
        let clip = p.push_clip(Rect::new(rect.x, y + 7.0, rect.w, body_h));
        // The newest lines stay in view.
        p.text(body, rect.x + 12.0, y + 7.0 + body_h - body.height(), t.text_muted);
        p.set_clip(clip);
    }
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

#[cfg(test)]
mod tests {
    use super::{IMAGE_MAX, IMAGE_PENDING, MEDIA_CARD_H, clock, media_size, reasoning_label};

    #[test]
    fn media_sizes() {
        use serechat::{Attachment, MediaJob, MediaKind, Role, StoredMessage};
        let mut message = StoredMessage::new(Role::Assistant, String::new());
        message.media = Some(MediaJob { kind: MediaKind::Image, id: "j".into() });
        assert_eq!(media_size(&message, 760.0, 1.0), (IMAGE_PENDING, IMAGE_PENDING), "a square tile while it is made");
        let file = |mime: &str, dimensions| Attachment { name: "a".into(), mime: mime.into(), size: 1, path: "a".into(), dimensions };
        message.attachments = vec![file("image/png", Some((1536, 1024)))];
        assert_eq!(media_size(&message, 760.0, 1.0), (IMAGE_MAX, 320.0), "wide images keep their shape");
        assert_eq!(media_size(&message, 760.0, 4.0), (256.0, 170.0), "never more pixels than a thumbnail holds");
        message.attachments = vec![file("image/png", Some((64, 32)))];
        assert_eq!(media_size(&message, 760.0, 1.0), (64.0, 32.0), "small images are not blown up");
        message.attachments = vec![file("video/mp4", None)];
        assert_eq!(media_size(&message, 300.0, 1.0), (300.0, MEDIA_CARD_H));
        assert_eq!((clock(9), clock(185)), ("9s".to_owned(), "3m 05s".to_owned()));
    }

    #[test]
    fn reasoning_labels() {
        assert_eq!(reasoning_label(true, 400), "Thinking");
        assert_eq!(reasoning_label(true, 4_200), "Thinking for 4s");
        assert_eq!(reasoning_label(false, 0), "Reasoning");
        assert_eq!(reasoning_label(false, 300), "Thought for 1s");
        assert_eq!(reasoning_label(false, 125_000), "Thought for 2m 5s");
    }
}
