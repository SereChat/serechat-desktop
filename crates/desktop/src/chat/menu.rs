//! Drop-down menus: model, generation model and reasoning (above the composer toolbar),
//! slash commands (above the composer) and project (below the header's
//! folder chip).

use accesskit::Role;
use winit::window::CursorIcon;

use super::composer::capitalized;
use super::{Chat, Menu, MenuItem, PRIMARY_KEY, Reasoning, display_path, price};
use crate::a11y::Node;
use crate::app::Action;
use crate::paint::{Painter, Rect, fade};
use crate::text::TextLayout;
use crate::theme;
use crate::ui::{Ui, id};

impl Chat {
    /// Draws the open menu (if any) and applies a choice. `anchors` holds
    /// the model and reasoning buttons and the composer card.
    pub(super) fn draw_open_menu(&mut self, p: &mut Painter, ui: &mut Ui, anchors: [Rect; 3], actions: &mut Vec<Action>) {
        let Some(menu) = self.menu else {
            self.menu_rect = None;
            return;
        };
        let media_title;
        let (anchor, width, below, header, items) = match menu {
            Menu::Media(kind) => {
                let chosen = &self.media_model[kind.index()];
                let items = self.media_models[kind.index()]
                    .iter()
                    .map(|m| MenuItem { label: m.label().to_owned(), detail: m.pricing.clone(), selected: &m.id == chosen })
                    .collect();
                media_title = format!("{} model", capitalized(kind.noun()));
                (anchors[0], 380.0, false, (media_title.as_str(), "Price"), items)
            }
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
                (anchors[0], 360.0, false, ("Model", "Input / output per 1M tokens"), items)
            }
            Menu::Reasoning => {
                let in_use = self.reasoning_in_use();
                let items = Reasoning::choices(self.selected_model())
                    .into_iter()
                    .map(|r| MenuItem { label: r.label().to_owned(), detail: r.detail().to_owned(), selected: r == in_use })
                    .collect();
                (anchors[1], 280.0, false, ("Reasoning effort", ""), items)
            }
            Menu::Commands => {
                let items = self
                    .commands()
                    .into_iter()
                    .map(|c| MenuItem { label: format!("/{}", c.name()), detail: c.detail().to_owned(), selected: false })
                    .collect();
                (anchors[2], 320.0, false, ("Commands", "Tab to complete"), items)
            }
            Menu::Project => {
                let current = self.current().project.clone();
                let mut items = vec![MenuItem { label: "No project".into(), detail: "Chat without tools".into(), selected: current.is_none() }];
                items.extend(self.projects.iter().map(|p| MenuItem {
                    label: p.name.clone(),
                    detail: display_path(&p.path),
                    selected: current.as_deref() == Some(p.path.as_str()),
                }));
                items.push(MenuItem { label: "Open folder…".into(), detail: format!("{PRIMARY_KEY}+O"), selected: false });
                (self.project_button, 320.0, true, ("Project", ""), items)
            }
        };
        if let Some(index) = self.draw_menu(p, ui, anchor, width, below, header, &items) {
            self.menu = None;
            ui.released = false;
            match menu {
                Menu::Media(kind) => {
                    if let Some(model) = self.media_models[kind.index()].get(index) {
                        self.media_model[kind.index()].clone_from(&model.id);
                        actions.push(Action::SelectMediaModel(kind, model.id.clone()));
                    }
                }
                Menu::Model => {
                    if let Some(model) = self.models.get(index) {
                        self.model.clone_from(&model.id);
                        actions.push(Action::SelectModel(model.id.clone()));
                    }
                }
                Menu::Reasoning => {
                    if let Some(choice) = Reasoning::choices(self.selected_model()).get(index) {
                        self.reasoning = *choice;
                        actions.push(Action::SetReasoning(self.reasoning));
                    }
                }
                Menu::Commands => {
                    if let Some(command) = self.commands().get(index) {
                        self.run_command(*command, actions);
                    }
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
            self.close_menu();
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
        // Kept inside the window when the anchor sits near its right edge.
        let x = anchor.x.min(p.clip().right() - width - 8.0).max(8.0);
        let area = Rect::new(x, y, width, height);
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
            // The command Enter would run is highlighted.
            let picked = self.menu == Some(Menu::Commands) && i == self.command_pick;
            let hover = ui.anim(id(("menu", header.0, i)), f32::from(u8::from(hovered || picked)));
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
            ui.describe(|| Node::new(Role::MenuItem, row, &format!("{}, {}", item.label, item.detail)));

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
