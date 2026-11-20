//! Drawing of the shell: tabs, sidebar, status bar, switcher. Each function
//! returns the `Action` the user triggered, if any; `App::apply` runs it.

pub mod sidebar;
pub mod status;
pub mod switcher;
pub mod tabbar;

use eframe::egui::{self, Align, Label, Layout, RichText, Sense, Ui, UiBuilder};

use crate::app::Action;
use crate::theme::Colors;

pub fn untitled(t: &str) -> &str {
    if t.is_empty() {
        "Untitled"
    } else {
        t
    }
}

/// A full-width clickable row: highlighted when `selected` or hovered, with
/// `content` laid out left to right, vertically centred, inside it.
pub fn item(
    ui: &mut Ui,
    c: &Colors,
    selected: bool,
    height: f32,
    content: impl FnOnce(&mut Ui),
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 4.0, c.hover.gamma_multiply(2.0));
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, c.hover);
    }
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(8.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    // labels must not take the click from the row
    child.style_mut().interaction.selectable_labels = false;
    child.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
    content(&mut child);
    resp
}

/// Icon and title of each page above the open one (clickable), then the open
/// page's own title.
pub fn breadcrumb(
    ui: &mut Ui,
    c: &Colors,
    crumbs: &[(String, String, String)],
    current: &str,
) -> Option<Action> {
    let mut act = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (id, icon, title) in crumbs {
            let text = RichText::new(format!("{icon} {title}"))
                .size(13.0)
                .color(c.secondary);
            let r = ui.add(Label::new(text).sense(Sense::click()).selectable(false));
            if r.hovered() {
                ui.painter().rect_filled(r.rect.expand(2.0), 3.0, c.hover);
            }
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                act = Some(Action::Open(id.clone()));
            }
            ui.label(RichText::new("›").size(13.0).color(c.secondary));
        }
        ui.label(RichText::new(current).size(13.0).color(c.secondary));
    });
    act
}

/// The centre of a tab with no page.
pub fn empty_tab(ui: &mut Ui, c: &Colors) {
    ui.centered_and_justified(|ui| {
        ui.label(RichText::new("New tab. Press Ctrl+K to open a page.").color(c.secondary));
    });
}
