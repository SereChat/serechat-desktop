//! Drop-down menus: model and reasoning (above the composer toolbar) and
//! project (below the sidebar's switcher).

use winit::window::CursorIcon;

use super::{Chat, Menu, MenuItem, PRIMARY_KEY, Reasoning, display_path, price};
use crate::app::Action;
use crate::paint::{Painter, Rect, fade};
use crate::text::TextLayout;
use crate::theme;
use crate::ui::{Ui, id};

impl Chat {
    /// Draws the open menu (if any) and applies a choice. `toolbar` holds
    /// the model and reasoning buttons.
    pub(super) fn draw_open_menu(&mut self, p: &mut Painter, ui: &mut Ui, toolbar: [Rect; 2], actions: &mut Vec<Action>) {
        let Some(menu) = self.menu else {
            self.menu_rect = None;
            return;
        };
        let (anchor, width, below, header, items) = match menu {
            Menu::Model => {
                let items = self
                    .models
                    .iter()
                    .map(|m| MenuItem {
                        label: if m.name.is_empty() { m.id.clone() } else { m.name.clone() },
                        detail: price(m),
                        selected: m.id == self.model,
                    })
                    .collect::<Vec<_>>();
                (toolbar[0], 360.0, false, ("Model", "Input / output per 1M tokens"), items)
            }
            Menu::Reasoning => {
                let items = Reasoning::ALL
                    .iter()
                    .map(|r| MenuItem { label: r.label().to_owned(), detail: r.detail().to_owned(), selected: *r == self.reasoning })
                    .collect();
                (toolbar[1], 280.0, false, ("Reasoning effort", ""), items)
            }
            Menu::Project => {
                let mut items = vec![MenuItem { label: "No project".into(), detail: "Chat without tools".into(), selected: self.project.is_none() }];
                items.extend(self.projects.iter().map(|p| MenuItem {
                    label: p.name.clone(),
                    detail: display_path(&p.path),
                    selected: self.project.as_deref() == Some(p.path.as_str()),
                }));
                items.push(MenuItem { label: "Open folder…".into(), detail: format!("{PRIMARY_KEY}+O"), selected: false });
                (self.project_button, 320.0, true, ("Project", ""), items)
            }
        };
        if let Some(index) = self.draw_menu(p, ui, anchor, width, below, header, &items) {
            self.menu = None;
            ui.released = false;
            match menu {
                Menu::Model => {
                    if let Some(model) = self.models.get(index) {
                        self.model.clone_from(&model.id);
                        actions.push(Action::SelectModel(model.id.clone()));
                    }
                }
                Menu::Reasoning => {
                    self.reasoning = Reasoning::ALL[index];
                    actions.push(Action::SetReasoning(self.reasoning));
                }
                Menu::Project if index == 0 => self.set_project(None, actions),
                Menu::Project if index > self.projects.len() => actions.push(Action::OpenProject(None)),
                Menu::Project => {
                    let path = self.projects[index - 1].path.clone();
                    if std::path::Path::new(&path).is_dir() {
                        self.set_project(Some(path), actions);
                    } else {
                        self.projects.remove(index - 1);
                        actions.push(Action::ForgetProject(path));
                        self.notify("That folder no longer exists, so it was removed from your projects.");
                    }
                }
            }
            return;
        }
        let inside = self.menu_rect.is_some_and(|r| r.contains(ui.press_pos)) || anchor.contains(ui.press_pos);
        if ui.released && !inside {
            self.menu = None;
        }
    }

    /// Draws a menu next to `anchor` (below it, or above when `below` is
    /// false). Returns the clicked row.
    #[allow(clippy::too_many_arguments, reason = "menu geometry and content; a struct would only rename them")]
    fn draw_menu(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        anchor: Rect,
        width: f32,
        below: bool,
        header: (&str, &str),
        items: &[MenuItem],
    ) -> Option<usize> {
        let t = p.theme;
        let (row_h, header_h, pad) = (30.0, 30.0, 4.0);
        let content_h = items.len().max(1) as f32 * row_h;
        let view_h = p.clip().bottom();
        // Scrolls when the window is too short for every row.
        let room = if below { view_h - anchor.bottom() - 16.0 } else { anchor.y - 16.0 };
        let height = (header_h + content_h + 2.0 * pad).min(room.max(header_h + row_h * 3.0));
        let y = if below { anchor.bottom() + 6.0 } else { anchor.y - 6.0 - height };
        let area = Rect::new(anchor.x, y, width, height);
        self.menu_rect = Some(area);

        p.shadow(Rect::new(area.x, area.y + 6.0, area.w, area.h), t.shadow, theme::RADIUS, 18.0);
        p.bordered(area, t.surface, theme::RADIUS, 1.0, t.border_strong);
        p.label(header.0, theme::CAPTION, area.x + 12.0, area.y + 9.0, t.text_faint);
        let right = p.layout(header.1, theme::CAPTION, None);
        p.text(&right, area.right() - 12.0 - right.width(), area.y + 9.0, t.text_faint);
        p.rect(Rect::new(area.x, area.y + header_h, area.w, 1.0), t.border, 0.0);

        let list = Rect::new(area.x, area.y + header_h + pad, area.w, area.h - header_h - 2.0 * pad);
        if items.is_empty() {
            p.label("Loading…", theme::SMALL, list.x + 12.0, list.y + 7.0, t.text_muted);
            return None;
        }
        if ui.hovered(area) {
            self.menu_scroll += ui.scroll;
            ui.scroll = 0.0;
        }
        self.menu_scroll = self.menu_scroll.clamp(0.0, (content_h - list.h).max(0.0));

        let clip = p.push_clip(list);
        let mut chosen = None;
        for (i, item) in items.iter().enumerate() {
            let row = Rect::new(area.x + 4.0, list.y + i as f32 * row_h - self.menu_scroll, area.w - 8.0, row_h);
            if row.bottom() < list.y || row.y > list.bottom() {
                continue;
            }
            let hovered = ui.hovered(row) && list.contains(ui.mouse);
            let hover = ui.anim(id(("menu", header.0, i)), f32::from(u8::from(hovered)));
            p.rect(row, fade(t.hover, hover), theme::RADIUS_SM);

            let centre = |layout: &TextLayout| row.y + (row.h - layout.height()) * 0.5;
            if item.selected {
                p.label_centered("✓", theme::SMALL, Rect::new(row.x + 4.0, row.y, 16.0, row.h), t.accent);
            }
            let mut detail = p.layout(&item.detail, theme::SMALL, None);
            detail.truncate(p.fonts, (row.w * 0.5).max(60.0));
            p.text(&detail, row.right() - 10.0 - detail.width(), centre(&detail), t.text_faint);
            let mut label = p.layout(&item.label, theme::SMALL, None);
            label.truncate(p.fonts, row.w - 50.0 - detail.width());
            p.text(&label, row.x + 24.0, centre(&label), t.text);

            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.clicked(row) {
                    chosen = Some(i);
                }
            }
        }
        p.set_clip(clip);
        chosen
    }
}
