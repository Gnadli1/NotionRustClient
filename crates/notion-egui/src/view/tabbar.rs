//! The tab bar across the top, as in the Notion desktop app.

use eframe::egui::{self, Align2, FontId, Rect, Sense, Ui};

use crate::app::Action;
use crate::theme::Colors;

const H: f32 = 28.0;
/// The `+` button: a square hit target, and the gap before it.
const PLUS: f32 = 26.0;
const PLUS_GAP: f32 = 8.0;

/// `tabs`: (icon, title). Click switches, × (on hover) or a middle click
/// closes, `+` opens a new tab.
pub fn show(ui: &mut Ui, c: &Colors, tabs: &[(String, String)], active: usize) -> Option<Action> {
    let mut act = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let n = tabs.len().max(1) as f32;
        // `+` keeps its room even when the tabs shrink to their minimum
        let w = ((ui.available_width() - PLUS - PLUS_GAP - 12.0 - 2.0 * n) / n).clamp(24.0, 200.0);
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
        ui.add_space(PLUS_GAP - 2.0);
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(PLUS, PLUS), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect, 6.0, c.hover);
        }
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, "+", FontId::proportional(20.0), c.secondary);
        if resp.clicked() {
            act = Some(Action::NewTab);
        }
    });
    act
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{epaint::Shape, Event, Pos2, RawInput};

    /// Hovering paints the widget's hover fill: read the hit rects from those.
    #[test]
    fn plus_is_a_big_target_with_a_gap_after_the_last_tab() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let tabs = vec![(String::new(), "one".to_string()), (String::new(), "two".to_string())];
        // the filled rects of a frame with the pointer at x (a warm-up frame first: hover is a frame late)
        let hover = |x: f32| {
            let mut rects = Vec::new();
            for _ in 0..2 {
                let raw = RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 100.0))),
                    events: vec![Event::PointerMoved(Pos2::new(x, 14.0))],
                    ..Default::default()
                };
                let out = ctx.run_ui(raw, |ui| {
                    let c = crate::theme::colors(ui);
                    show(ui, c, &tabs, 0);
                });
                rects = out.shapes.iter().filter_map(|s| match &s.shape { Shape::Rect(r) => Some(r.rect), _ => None }).collect();
                out.drop_without_applying_deltas();
            }
            rects
        };
        // tabs are 200 wide here: the second one spans ~202..402
        let tab = hover(300.0).into_iter().rfind(|r| r.width() > 100.0).expect("hovered tab rect");
        let plus = hover(tab.right() + 20.0).into_iter().find(|r| r.width() > 0.0 && r.width() < 50.0).expect("hovered plus rect");
        assert!(plus.width() >= 24.0 && plus.height() >= 24.0, "plus hit rect {plus:?}");
        assert!(plus.left() - tab.right() >= 8.0 - 0.01, "gap {} between tab {tab:?} and plus {plus:?}", plus.left() - tab.right());
    }
}
