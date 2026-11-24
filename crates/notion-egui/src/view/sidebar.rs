//! The left column: the app/sidebar header, the workspace picker and the page
//! tree. Notion puts its logo, sidebar toggle and back/forward above the page
//! list, with the tab bar only over the content; this mirrors that.

use std::collections::HashSet;

use eframe::egui::{
    self, vec2, Align, FontId, Layout, Popup, Rect, RichText, ScrollArea, Sense, Ui, UiBuilder,
};
use notion_core::store::PageEntry;

use crate::app::Action;
use crate::icons::{self, Icon};
use crate::sidebar::{rows, TreeRow};
use crate::theme::Colors;

const ROW_H: f32 = 30.0;
const INDENT: f32 = 14.0;
const HEADER_H: f32 = 32.0;

pub struct Header<'a> {
    pub workspace: &'a str,
    /// Every profile the picker can switch to.
    pub names: &'a [String],
    pub no_token: bool,
    pub can_back: bool,
    pub can_forward: bool,
}

/// The logo, sidebar toggle and back/forward: the top-left cluster Notion keeps
/// above the sidebar. Returns the action a click asked for.
pub fn header(ui: &mut Ui, c: &Colors, h: &Header<'_>) -> Option<Action> {
    let mut act = None;
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), HEADER_H),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            // the mark itself: a rounded N tile, the app's one piece of branding
            let (r, logo) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
            if logo.hovered() {
                ui.painter().rect_filled(r, 4.0, c.hover);
            }
            let tile = r.shrink(4.0);
            ui.painter()
                .rect_filled(tile, 4.0, c.text.gamma_multiply(0.14));
            ui.painter().text(
                tile.center(),
                egui::Align2::CENTER_CENTER,
                "N",
                FontId::proportional(16.0),
                c.text,
            );
            if logo.on_hover_text("Toggle sidebar").clicked() {
                act = Some(Action::ToggleSidebar);
            }
            let toggle = icons::button(
                ui,
                28.0,
                Icon::Sidebar,
                c.secondary,
                c.hover,
                Some("Toggle sidebar"),
            );
            if toggle.clicked() {
                act = Some(Action::ToggleSidebar);
            }
            ui.add_space(4.0);
            let back_c = if h.can_back {
                c.secondary
            } else {
                c.secondary.gamma_multiply(0.35)
            };
            if icons::button(ui, 24.0, Icon::Back, back_c, c.hover, Some("Back")).clicked()
                && h.can_back
            {
                act = Some(Action::Back);
            }
            let fwd_c = if h.can_forward {
                c.secondary
            } else {
                c.secondary.gamma_multiply(0.35)
            };
            if icons::button(ui, 24.0, Icon::Forward, fwd_c, c.hover, Some("Forward")).clicked()
                && h.can_forward
            {
                act = Some(Action::Forward);
            }
        },
    );
    act
}

/// The workspace row: avatar, name, chevron and the new-page button. Clicking
/// the row opens the same profile menu the top bar used to show.
pub fn workspace_row(ui: &mut Ui, c: &Colors, h: &Header<'_>) -> Option<Action> {
    let mut act = None;
    let full = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(full, ROW_H), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, c.hover);
    }
    let name = if h.no_token {
        format!("⚠ {}", h.workspace)
    } else {
        h.workspace.to_string()
    };
    let name_color = if h.no_token { c.error } else { c.text };
    let mut row = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(7.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    row.style_mut().interaction.selectable_labels = false;
    row.spacing_mut().item_spacing.x = 6.0;
    icons::workspace_avatar(&mut row, h.workspace, 20.0);
    row.label(RichText::new(name).size(14.0).strong().color(name_color));
    let chev = Rect::from_center_size(
        egui::pos2(row.cursor().left() + 7.0, rect.center().y),
        vec2(14.0, 14.0),
    );
    row.advance_cursor_after_rect(chev);
    icons::paint(&row, chev, Icon::ChevronDown, c.secondary);

    // the compose button sits at the far right, like Notion's "new page"
    let compose = Rect::from_center_size(
        egui::pos2(rect.right() - 16.0, rect.center().y),
        vec2(22.0, 22.0),
    );
    let compose_resp = ui.interact(compose, resp.id.with("compose"), Sense::click());
    if compose_resp.hovered() {
        ui.painter()
            .rect_filled(compose, 4.0, c.hover.gamma_multiply(1.6));
    }
    icons::paint(ui, compose.shrink(4.0), Icon::Compose, c.secondary);
    if compose_resp.on_hover_text("New page").clicked() {
        act = Some(Action::NewPage(None));
    }

    // `Popup::menu` keeps its own open state in egui's memory; it has to be
    // shown every frame, not only on the frame of the click.
    let popup = Popup::menu(&resp)
        .width(248.0)
        .frame(egui::Frame::popup(ui.style()).inner_margin(6));
    popup.show(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        if h.names.is_empty() {
            ui.label(RichText::new("No workspace yet").small().color(c.secondary));
        }
        for n in h.names {
            let selected = n == h.workspace;
            let r = ui.add_sized(
                [ui.available_width(), 28.0],
                egui::Button::new(RichText::new(n).size(14.0))
                    .selected(selected)
                    .frame(false)
                    .wrap_mode(egui::TextWrapMode::Truncate),
            );
            if selected {
                let cr = Rect::from_center_size(
                    egui::pos2(r.rect.right() - 12.0, r.rect.center().y),
                    vec2(14.0, 14.0),
                );
                icons::paint(ui, cr, Icon::Check, c.accent);
            }
            if r.clicked() && !selected {
                act = Some(Action::SwitchProfile(n.clone()));
                ui.close();
            }
        }
        ui.separator();
        if menu_item(ui, c, "Browser session (token_v2)…") {
            act = Some(Action::BrowserLoginStart);
            ui.close();
        }
        if menu_item(ui, c, "Add workspace with a token…") {
            act = Some(Action::NewProfile);
            ui.close();
        }
        if !h.names.is_empty() && menu_item(ui, c, "Edit current workspace…") {
            act = Some(Action::EditProfile);
            ui.close();
        }
        ui.separator();
        ui.label(
            RichText::new("Browser profiles use token_v2 and see every shared workspace.")
                .small()
                .color(c.secondary),
        );
    });
    act
}

fn menu_item(ui: &mut Ui, _c: &Colors, text: &str) -> bool {
    ui.add_sized(
        [ui.available_width(), 28.0],
        egui::Button::new(RichText::new(text).size(14.0))
            .frame(false)
            .wrap_mode(egui::TextWrapMode::Truncate),
    )
    .clicked()
}

pub fn show(
    ui: &mut Ui,
    c: &Colors,
    tree: &[PageEntry],
    open: &HashSet<String>,
    current: Option<&str>,
    unread: usize,
    no_person: bool,
) -> Option<Action> {
    let mut act = None;
    let hint = RichText::new("Ctrl+K").small().color(c.secondary);
    let search = nav_row(ui, c, Icon::Search, "Search", |ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(hint));
    });
    if search.clicked() {
        act = Some(Action::Search);
    }
    let inbox = nav_row(ui, c, Icon::Bell, "Updates", |ui| {
        if unread > 0 {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(unread.to_string()).small().color(c.secondary));
            });
        }
    });
    if inbox.clicked() {
        act = Some(Action::Updates);
    }
    if no_person {
        ui.label(
            RichText::new(crate::inbox::NO_PERSON)
                .small()
                .color(c.secondary),
        );
    }
    let sync = nav_row(ui, c, Icon::Sync, "Sync everything", |_| {});
    if sync.clicked() {
        act = Some(Action::SyncAll);
    }
    let bin = nav_row(ui, c, Icon::Trash, "Trash", |_| {});
    if bin.clicked() {
        act = Some(Action::Dlg(crate::pages::Dlg::Bin { sel: 0 }));
    }
    let ai = nav_row(ui, c, Icon::Comment, "AI Agent", |_| {});
    if ai.on_hover_text("Ask AI about the open page (Ctrl+Shift+H)").clicked() {
        act = Some(Action::Ai);
    }
    let settings = nav_row(ui, c, Icon::Dots, "Settings", |_| {});
    if settings.on_hover_text("Theme, text size, font, key binds, AI (Ctrl+,)").clicked() {
        act = Some(Action::Settings);
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(7.0);
        ui.label(RichText::new("Pages").size(12.0).color(c.secondary));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(2.0);
            if icons::button(ui, 22.0, Icon::Plus, c.secondary, c.hover, Some("New page")).clicked()
            {
                act = Some(Action::NewPage(None));
            }
        });
    });
    ui.add_space(2.0);
    let list = rows(tree, open);
    ui.spacing_mut().item_spacing.y = 0.0;
    ScrollArea::vertical()
        .auto_shrink(false)
        .show_rows(ui, ROW_H, list.len(), |ui, range| {
            for r in &list[range] {
                if let Some(a) = tree_row(ui, c, r, current) {
                    act = Some(a);
                }
            }
        });
    act
}

/// A top sidebar row: a 30 px clickable line with a 18 px icon and its label.
fn nav_row(
    ui: &mut Ui,
    c: &Colors,
    icon: Icon,
    label: &str,
    right: impl FnOnce(&mut Ui),
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, c.hover);
    }
    let icon_rect = Rect::from_center_size(
        egui::pos2(rect.left() + 17.0, rect.center().y),
        vec2(18.0, 18.0),
    );
    icons::paint(ui, icon_rect, icon, c.secondary);
    let mut row = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(31.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    row.style_mut().interaction.selectable_labels = false;
    row.label(RichText::new(label).size(14.0).color(c.text));
    row.with_layout(Layout::right_to_left(Align::Center), |ui| right(ui));
    resp
}

fn tree_row(ui: &mut Ui, c: &Colors, r: &TreeRow, current: Option<&str>) -> Option<Action> {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    if current == Some(r.entry.id.as_str()) {
        ui.painter()
            .rect_filled(rect, 6.0, c.hover.gamma_multiply(2.4));
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, c.hover);
    }
    let x = rect.left() + 5.0 + INDENT * r.depth as f32;
    let arrow = Rect::from_center_size(egui::pos2(x + 8.0, rect.center().y), vec2(18.0, 18.0));
    let mut act = None;
    if r.has_children {
        let a = ui.interact(arrow, resp.id.with("arrow"), Sense::click());
        if a.hovered() {
            ui.painter()
                .rect_filled(arrow, 4.0, c.hover.gamma_multiply(1.8));
        }
        icons::paint(
            ui,
            arrow.shrink(5.0),
            if r.expanded {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            },
            c.secondary,
        );
        if a.clicked() {
            act = Some(Action::ToggleTree(r.entry.id.clone()));
        }
    }
    let glyph = Rect::from_center_size(egui::pos2(x + 28.0, rect.center().y), vec2(18.0, 18.0));
    icons::page_glyph(ui, glyph, r.entry.icon.as_deref(), r.entry.database, c.text);
    let title = super::untitled(&r.entry.title).replace('\n', " ");
    let clip = Rect::from_min_max(
        egui::pos2(glyph.right() + 4.0, rect.top()),
        egui::pos2(rect.right() - 6.0, rect.max.y),
    );
    ui.painter().with_clip_rect(clip).text(
        egui::pos2(glyph.right() + 7.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        FontId::proportional(14.0),
        if current == Some(r.entry.id.as_str()) {
            c.text
        } else {
            c.text.gamma_multiply(0.92)
        },
    );
    if act.is_none() && resp.clicked() {
        act = Some(Action::Open(r.entry.id.clone()));
    }
    act
}
#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Context, Event, Modifiers, PointerButton, Pos2, RawInput};

    #[test]
    fn the_workspace_menu_stays_open_after_the_click_frame() {
        let ctx = Context::default();
        crate::setup_fonts(&ctx);
        crate::theme::install(&ctx);
        let names = vec!["Mine".to_string(), "Other".to_string()];
        let h = Header {
            workspace: &names[0],
            names: &names,
            no_token: false,
            can_back: false,
            can_forward: false,
        };
        let frame = |events: Vec<Event>| {
            let raw = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 400.0))),
                events,
                ..Default::default()
            };
            ctx.run_ui(raw, |ui| {
                ui.set_width(260.0);
                let c = crate::theme::colors(ui);
                let _ = workspace_row(ui, c, &h);
            })
            .drop_without_applying_deltas();
        };
        let pos = Pos2::new(100.0, ROW_H / 2.0);
        frame(vec![Event::PointerMoved(pos)]);
        frame(vec![
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ]);
        assert!(Popup::is_any_open(&ctx), "the click did not open the menu");
        frame(vec![]);
        assert!(
            Popup::is_any_open(&ctx),
            "the menu closed after the click frame"
        );
    }
}
