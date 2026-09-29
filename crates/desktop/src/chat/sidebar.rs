//! The sidebar: project switcher, search, new chat, the sessions of the
//! current project, and settings.

use serechat::unix_now;
use winit::window::CursorIcon;

use super::{Chat, Menu, Page, PRIMARY_KEY, ago};
use crate::app::Action;
use crate::paint::{Painter, Rect, fade, mix};
use crate::text::Style;
use crate::theme;
use crate::ui::{Ui, chevron, folder_icon, id};

impl Chat {
    pub(super) fn draw_sidebar(&mut self, p: &mut Painter, ui: &mut Ui, area: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        p.rect(area, t.panel, 0.0);
        p.rect(Rect::new(area.right() - 1.0, 0.0, 1.0, area.h), t.border, 0.0);

        // macOS draws the traffic lights over our (transparent) title bar.
        let top = if cfg!(target_os = "macos") { 36.0 } else { 8.0 };
        let brand = p.layout("SereChat", Style::semibold(14.5), None);
        p.text(&brand, 18.0, top + 18.0 - brand.height() * 0.5, t.text);

        self.draw_project_switcher(p, ui, Rect::new(8.0, top + 40.0, area.w - 16.0, 36.0));

        let on_chat = self.page == Page::Chat;
        let search = Rect::new(8.0, top + 84.0, area.w - 16.0, 30.0);
        if list_row(p, ui, search, id("search"), "Search", Some(&format!("{PRIMARY_KEY}+K")), self.spotlight.is_some()) {
            self.open_spotlight();
        }
        let fresh = self.conversations.iter().any(|c| c.id == self.current && c.is_fresh());
        let new_chat = Rect::new(8.0, search.bottom() + 2.0, area.w - 16.0, 30.0);
        if list_row(p, ui, new_chat, id("new-chat"), "New chat", Some(&format!("{PRIMARY_KEY}+N")), on_chat && fresh) {
            self.new_conversation();
        }

        let list_top = new_chat.bottom() + 18.0;
        p.label("Sessions", theme::CAPTION, 18.0, list_top, t.text_faint);
        let list = Rect::new(0.0, list_top + 24.0, area.w, area.h - list_top - 24.0 - 52.0);
        let item_h = 30.0;
        let project = self.project.clone();
        let shown = |c: &&super::Conversation| !c.is_fresh() && c.project == project;
        let content_h = self.conversations.iter().filter(shown).count() as f32 * (item_h + 1.0);
        if ui.hovered(list) {
            self.sidebar_scroll += ui.scroll;
        }
        self.sidebar_scroll = self.sidebar_scroll.clamp(0.0, (content_h - list.h).max(0.0));
        if content_h == 0.0 {
            let empty = if project.is_some() { "No sessions in this project yet" } else { "No sessions yet" };
            p.label(empty, theme::SMALL, 18.0, list.y + 6.0, t.text_faint);
        }

        let clip = p.push_clip(list);
        let now = unix_now();
        let (mut open, mut confirm, mut delete, mut confirm_hovered) = (None, None, None, false);
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

            // Right side: age, or the delete control while hovered.
            let confirming = self.confirm_delete == Some(conversation.id);
            confirm_hovered |= confirming && hovered;
            let mut on_control = false;
            let right_w = if confirming {
                let del = Rect::new(item.right() - 60.0, item.y + 4.0, 56.0, item_h - 8.0);
                let over = hovered && del.contains(ui.mouse);
                p.rect(del, fade(t.danger, if over { 0.24 } else { 0.14 }), theme::RADIUS_SM);
                p.label_centered("Delete", theme::CAPTION, del, t.danger);
                on_control = over;
                if over && ui.clicked(del) {
                    delete = Some(conversation.id);
                }
                64.0
            } else if hovered {
                let x = Rect::new(item.right() - 26.0, item.y + 5.0, 20.0, 20.0);
                let over = x.contains(ui.mouse);
                if over {
                    p.rect(x, t.active, theme::RADIUS_SM);
                }
                p.label_centered("×", Style::regular(15.0), x, if over { t.text } else { t.text_faint });
                on_control = over;
                if over && ui.clicked(x) {
                    confirm = Some(conversation.id);
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

        // Leaving the row cancels a pending delete.
        if !confirm_hovered {
            self.confirm_delete = None;
        }
        if confirm.is_some() {
            self.confirm_delete = confirm;
        }
        if let Some(id) = delete {
            self.confirm_delete = None;
            self.delete_conversation(id, actions);
        }
        if let Some(id) = open {
            self.open(id, actions);
        }

        p.rect(Rect::new(0.0, area.h - 47.0, area.w - 1.0, 1.0), t.border, 0.0);
        let settings = Rect::new(8.0, area.h - 39.0, area.w - 16.0, 30.0);
        if list_row(p, ui, settings, id("settings"), "Settings", Some(&format!("{PRIMARY_KEY}+,")), !on_chat) {
            self.toggle_settings();
        }
    }

    /// The current project (or "No project") with a menu to switch.
    fn draw_project_switcher(&mut self, p: &mut Painter, ui: &mut Ui, rect: Rect) {
        let t = p.theme;
        self.project_button = rect;
        let open = self.menu == Some(Menu::Project);
        let hovered = ui.hovered(rect);
        let hover = ui.anim(id("project-switcher"), f32::from(u8::from(hovered || open)));
        p.bordered(rect, mix(t.surface, t.hover, hover), theme::RADIUS, 1.0, t.border);
        folder_icon(p, rect.x + 11.0, rect.y + (rect.h - 10.0) * 0.5, if self.project.is_some() { t.text_muted } else { t.text_faint });
        let name = self
            .project
            .as_deref()
            .map(|path| self.projects.iter().find(|p| p.path == path).map_or_else(|| super::display_path(path), |p| p.name.clone()));
        let (label, color) = match &name {
            Some(name) => (name.as_str(), t.text),
            None => ("No project", t.text_muted),
        };
        let mut layout = p.layout(label, theme::LABEL, None);
        layout.truncate(p.fonts, rect.w - 56.0);
        p.text(&layout, rect.x + 30.0, rect.y + (rect.h - layout.height()) * 0.5, color);
        chevron(p, rect.right() - 20.0, rect.y + rect.h * 0.5 - 2.0, true, t.text_faint);
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(rect) {
                self.menu = if open { None } else { Some(Menu::Project) };
                self.menu_scroll = 0.0;
            }
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
    ui.clicked(rect)
}
