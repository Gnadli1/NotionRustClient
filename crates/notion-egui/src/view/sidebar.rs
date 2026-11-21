//! The page tree on the left: ▸/▾ to expand, the page's emoji (or 📄), the
//! open page highlighted. `crate::sidebar` decides which rows are visible.

use std::collections::HashSet;

use eframe::egui::{self, Align2, FontId, Rect, RichText, ScrollArea, Sense, Ui};
use notion_core::store::PageEntry;

use crate::app::Action;
use crate::sidebar::{rows, TreeRow};
use crate::theme::Colors;

const ROW_H: f32 = 26.0;
const INDENT: f32 = 14.0;

pub fn show(
    ui: &mut Ui,
    c: &Colors,
    tree: &[PageEntry],
    open: &HashSet<String>,
    current: Option<&str>,
) -> Option<Action> {
    let mut act = None;
    let hint = RichText::new("Ctrl+K").small().color(c.secondary);
    let search = super::item(ui, c, false, 30.0, |ui| {
        ui.label("🔍");
        ui.label("Search");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(hint)
        });
    });
    if search.clicked() {
        act = Some(Action::Search);
    }
    let sync = super::item(ui, c, false, 30.0, |ui| {
        ui.label("⟳");
        ui.label("Sync everything");
    });
    if sync.clicked() {
        act = Some(Action::SyncAll);
    }
    ui.add_space(6.0);
    ui.label(RichText::new("  Pages").small().color(c.secondary));
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

fn tree_row(ui: &mut Ui, c: &Colors, r: &TreeRow, current: Option<&str>) -> Option<Action> {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), Sense::click());
    if current == Some(r.entry.id.as_str()) {
        ui.painter().rect_filled(rect, 4.0, c.hover.gamma_multiply(2.0));
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, c.hover);
    }
    let x = rect.left() + 4.0 + INDENT * r.depth as f32;
    let arrow = Rect::from_min_size(egui::pos2(x, rect.top() + 3.0), egui::vec2(20.0, 20.0));
    let mut act = None;
    if r.has_children {
        // registered after the row, so it wins where they overlap
        let a = ui.interact(arrow, resp.id.with("arrow"), Sense::click());
        if a.hovered() {
            ui.painter().rect_filled(arrow, 4.0, c.hover.gamma_multiply(2.0));
        }
        ui.painter().text(
            arrow.center(),
            Align2::CENTER_CENTER,
            if r.expanded { "▾" } else { "▸" },
            FontId::proportional(11.0),
            c.secondary,
        );
        if a.clicked() {
            act = Some(Action::ToggleTree(r.entry.id.clone()));
        }
    }
    let icon = r.entry.icon.as_deref().unwrap_or(if r.entry.database { "🗄" } else { "📄" });
    let title = super::untitled(&r.entry.title).replace('\n', " ");
    let clip = Rect::from_min_max(rect.min, egui::pos2(rect.right() - 4.0, rect.max.y));
    ui.painter().with_clip_rect(clip).text(
        egui::pos2(x + 22.0, rect.center().y),
        Align2::LEFT_CENTER,
        format!("{icon} {title}"),
        FontId::proportional(14.0),
        c.text,
    );
    if act.is_none() && resp.clicked() {
        // a database has no view until F4: the click expands it instead
        act = Some(if r.entry.database {
            Action::ToggleTree(r.entry.id.clone())
        } else {
            Action::Open(r.entry.id.clone())
        });
    }
    act
}
