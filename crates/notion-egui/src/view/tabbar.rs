//! The tab bar across the top, as in the Notion desktop app.

use eframe::egui::{self, Align2, FontId, Rect, Sense, Ui};

use crate::app::Action;
use crate::theme::Colors;

const H: f32 = 28.0;

/// `tabs`: (icon, title). Click switches, × (on hover) or a middle click
/// closes, `+` opens a new tab.
pub fn show(ui: &mut Ui, c: &Colors, tabs: &[(String, String)], active: usize) -> Option<Action> {
    let mut act = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let w = ((ui.available_width() - 40.0) / tabs.len().max(1) as f32).clamp(60.0, 200.0);
        for (i, (icon, title)) in tabs.iter().enumerate() {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, H), Sense::click());
            let fill = if i == active {
                c.bg
            } else if resp.hovered() {
                c.hover
            } else {
                egui::Color32::TRANSPARENT
            };
            ui.painter().rect_filled(rect, 6.0, fill);
            let x_rect = Rect::from_center_size(
                egui::pos2(rect.right() - 14.0, rect.center().y),
                egui::vec2(18.0, 18.0),
            );
            // registered after the tab, so it wins where they overlap
            let x = ui.interact(x_rect, resp.id.with("close"), Sense::click());
            let color = if i == active { c.text } else { c.secondary };
            let label = if icon.is_empty() {
                title.clone()
            } else {
                format!("{icon} {title}")
            };
            let clip = Rect::from_min_max(rect.min, egui::pos2(rect.right() - 24.0, rect.max.y));
            ui.painter().with_clip_rect(clip).text(
                egui::pos2(rect.left() + 10.0, rect.center().y),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(13.0),
                color,
            );
            if resp.hovered() || x.hovered() {
                if x.hovered() {
                    ui.painter().rect_filled(x_rect, 4.0, c.hover.gamma_multiply(2.0));
                }
                ui.painter().text(
                    x_rect.center(),
                    Align2::CENTER_CENTER,
                    "×",
                    FontId::proportional(14.0),
                    c.secondary,
                );
            }
            if x.clicked() || resp.middle_clicked() {
                act = Some(Action::CloseTab(i));
            } else if resp.clicked() {
                act = Some(Action::Switch(i));
            }
        }
        if ui.add(egui::Button::new("+").frame(false)).clicked() {
            act = Some(Action::NewTab);
        }
    });
    act
}
