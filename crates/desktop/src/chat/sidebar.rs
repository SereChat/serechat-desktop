//! The sidebar: new chat, every saved session, and settings.

use serechat::unix_now;
use accesskit::Role;
use winit::window::CursorIcon;

use super::{Chat, Page, PRIMARY_KEY, ago};
use crate::a11y::Node;
use crate::app::Action;
use crate::paint::{Painter, Rect, fade, hexa, mix};
use crate::text::Style;
use crate::theme;
use crate::ui::{ButtonStyle, Ui, button, id};

impl Chat {
    pub(super) fn draw_sidebar(&mut self, p: &mut Painter, ui: &mut Ui, area: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        p.rect(area, t.panel, 0.0);
        p.rect(Rect::new(area.right() - 1.0, 0.0, 1.0, area.h), t.border, 0.0);

        // macOS draws the traffic lights over our (transparent) title bar.
        let top = if cfg!(target_os = "macos") { 36.0 } else { 8.0 };
        let brand = p.layout("SereChat", Style::semibold(14.5), None);
        p.text(&brand, 18.0, top + 18.0 - brand.height() * 0.5, t.text);

        let on_chat = self.page == Page::Chat;
        let fresh = self.conversations.iter().any(|c| c.id == self.current && c.is_fresh());
        let new_chat = Rect::new(8.0, top + 40.0, area.w - 16.0, 30.0);
        if list_row(p, ui, new_chat, id("new-chat"), "New chat", Some(&format!("{PRIMARY_KEY}+N")), on_chat && fresh) {
            self.new_conversation();
        }

        let list_top = new_chat.bottom() + 18.0;
        p.label("Sessions", theme::CAPTION, 18.0, list_top, t.text_faint);
        // An installed update adds a row above Settings.
        let footer_h = if self.update_ready.is_some() { 86.0 } else { 52.0 };
        let list = Rect::new(0.0, list_top + 24.0, area.w, area.h - list_top - 24.0 - footer_h);
        let item_h = 30.0;
        let shown = |c: &&super::Conversation| !c.is_fresh();
        let content_h = self.conversations.iter().filter(shown).count() as f32 * (item_h + 1.0);
        if ui.hovered(list) {
            self.sidebar_scroll += ui.scroll;
        }
        self.sidebar_scroll = self.sidebar_scroll.clamp(0.0, (content_h - list.h).max(0.0));
        if content_h == 0.0 {
            p.label("No sessions yet", theme::SMALL, 18.0, list.y + 6.0, t.text_faint);
        }

        let clip = p.push_clip(list);
        let now = unix_now();
        let (mut open, mut delete) = (None, None);
        let mut y = list.y - self.sidebar_scroll;
        for conversation in self.conversations.iter().filter(shown) {
            let item = Rect::new(8.0, y, area.w - 16.0, item_h);
            y += item_h + 1.0;
            if item.bottom() < list.y || item.y > list.bottom() {
                continue;
            }
            // Rows scrolled under the list edges must not react.
            let hovered = ui.hovered(item) && list.contains(ui.mouse);
            let hover = ui.anim(id(("conversation", conversation.id)), f32::from(u8::from(hovered)));
            let selected = on_chat && conversation.id == self.current;
            p.rect(item, if selected { t.active } else { fade(t.hover, hover) }, theme::RADIUS_SM);

            ui.describe(|| Node::new(Role::Button, item, &conversation.title));
            // Hidden until hovered, but always there for screen readers.
            ui.describe(|| Node::new(Role::Button, Rect::new(item.right() - 26.0, item.y + 5.0, 20.0, 20.0), &format!("Delete {}", conversation.title)));
            // Right side: age, or the delete control while hovered.
            let mut on_control = false;
            let right_w = if hovered {
                let x = Rect::new(item.right() - 26.0, item.y + 5.0, 20.0, 20.0);
                let over = x.contains(ui.mouse);
                if over {
                    p.rect(x, t.active, theme::RADIUS_SM);
                }
                p.label_centered("×", Style::regular(15.0), x, if over { t.text } else { t.text_faint });
                on_control = over;
                if over && ui.clicked(x) {
                    delete = Some((conversation.id, conversation.busy()));
                }
                30.0
            } else if conversation.busy() {
                let pulse = 0.5 + 0.5 * (ui.time * 4.0).sin();
                p.rect(Rect::new(item.right() - 16.0, item.y + item_h * 0.5 - 3.0, 6.0, 6.0), fade(t.accent, 0.4 + 0.6 * pulse), 3.0);
                24.0
            } else {
                let age = p.layout(&ago(now, conversation.updated), theme::TINY, None);
                p.text(&age, item.right() - 10.0 - age.width(), item.y + (item_h - age.height()) * 0.5, t.text_faint);
                age.width() + 18.0
            };

            let mut title = p.layout(&conversation.title, theme::SMALL, None);
            title.truncate(p.fonts, item.w - 10.0 - right_w);
            let color = if selected { t.text } else { mix(t.text_muted, t.text, hover) };
            p.text(&title, item.x + 10.0, item.y + (item_h - title.height()) * 0.5, color);
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if !on_control && ui.clicked(item) {
                    open = Some(conversation.id);
                }
            }
        }
        p.set_clip(clip);

        // A session that is still working asks first.
        match delete {
            Some((id, true)) => self.confirm_delete = Some(id),
            Some((id, false)) => self.delete_conversation(id, actions),
            None => {}
        }
        if let Some(id) = open {
            self.open(id, actions);
        }

        p.rect(Rect::new(0.0, area.h - footer_h + 5.0, area.w - 1.0, 1.0), t.border, 0.0);
        if let Some(version) = &self.update_ready {
            let restart = Rect::new(8.0, area.h - 73.0, area.w - 16.0, 30.0);
            let clicked = list_row(p, ui, restart, id("restart-update"), &format!("Restart to update to {version}"), None, false);
            p.rect(Rect::new(restart.right() - 18.0, restart.y + 12.0, 6.0, 6.0), t.accent, 3.0);
            if clicked {
                actions.push(Action::RestartToUpdate);
            }
        }
        let settings = Rect::new(8.0, area.h - 39.0, area.w - 16.0, 30.0);
        if list_row(p, ui, settings, id("settings"), "Settings", Some(&format!("{PRIMARY_KEY}+,")), !on_chat) {
            self.toggle_settings(actions);
        }
    }

    /// Draws the "delete a working session?" dialog over `view` and acts on
    /// the answer. Clicking outside cancels.
    pub(super) fn draw_confirm_delete(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, actions: &mut Vec<Action>) {
        let Some(id) = self.confirm_delete else { return };
        let t = p.theme;
        p.rect(view, hexa(0x000000, 0.4), 0.0);
        let width = 380.0f32.min(view.w - 48.0);
        let body = p.layout("A reply is still being generated. Deleting the session stops it.", theme::SMALL, Some(width - 40.0));
        let height = 20.0 + 22.0 + 8.0 + body.height() + 20.0 + 30.0 + 20.0;
        let modal = Rect::new(view.x + ((view.w - width) * 0.5).round(), view.y + ((view.h - height) * 0.4).round(), width, height);
        p.shadow(Rect::new(modal.x, modal.y + 12.0, modal.w, modal.h), hexa(0x000000, 0.45), theme::RADIUS * 2.0, 40.0);
        p.bordered(modal, t.surface, theme::RADIUS * 1.5, 1.0, t.border_strong);
        p.label("Delete this session?", Style::semibold(14.5), modal.x + 20.0, modal.y + 20.0, t.text);
        p.text(&body, modal.x + 20.0, modal.y + 50.0, t.text_muted);

        let row_y = modal.bottom() - 50.0;
        let delete = Rect::new(modal.right() - 20.0 - 80.0, row_y, 80.0, 30.0);
        let cancel = Rect::new(delete.x - 8.0 - 80.0, row_y, 80.0, 30.0);
        if button(p, ui, delete, "Delete", ButtonStyle::Danger, true) {
            self.confirm_delete = None;
            self.delete_conversation(id, actions);
        } else if button(p, ui, cancel, "Cancel", ButtonStyle::Secondary, true) || (ui.released && !modal.contains(ui.press_pos)) {
            self.confirm_delete = None;
        }
    }
}

/// Draws a left-aligned list row with an optional right-hand hint and
/// returns whether it was clicked.
pub(super) fn list_row(p: &mut Painter, ui: &mut Ui, rect: Rect, key: u64, label: &str, hint: Option<&str>, selected: bool) -> bool {
    let t = p.theme;
    let hovered = ui.hovered(rect);
    let hover = ui.anim(key, f32::from(u8::from(hovered)));
    p.rect(rect, if selected { t.active } else { fade(t.hover, hover) }, theme::RADIUS_SM);
    let mut reserved = 24.0;
    if let Some(hint) = hint {
        let hint = p.layout(hint, theme::TINY, None);
        reserved = hint.width() + 28.0;
        p.text(&hint, rect.right() - 10.0 - hint.width(), rect.y + (rect.h - hint.height()) * 0.5, t.text_faint);
    }
    let mut layout = p.layout(label, theme::SMALL, None);
    layout.truncate(p.fonts, rect.w - 10.0 - reserved);
    let color = if selected { t.text } else { mix(t.text_muted, t.text, hover) };
    p.text(&layout, rect.x + 10.0, rect.y + (rect.h - layout.height()) * 0.5, color);
    if hovered {
        ui.cursor = CursorIcon::Pointer;
    }
    ui.describe(|| Node::new(Role::Button, rect, label));
    ui.clicked(rect)
}
